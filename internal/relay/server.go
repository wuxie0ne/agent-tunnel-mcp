package relay

import (
	"agent-tunnel/internal/config"
	"agent-tunnel/internal/mcpserver"
	"agent-tunnel/internal/relayproto"
	"agent-tunnel/internal/target"
	"bytes"
	"context"
	"crypto/sha256"
	"crypto/subtle"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"github.com/coder/websocket"
	"io"
	"net/http"
	"strings"
	"sync"
	"time"
)

type route struct {
	instance, name, id, resume, tokenHash string
	expires                               time.Time
	timeout                               time.Duration
	peer                                  *peer
}
type peer struct {
	c       *relayproto.Connection
	ctx     context.Context
	cancel  context.CancelFunc
	mu      sync.Mutex
	pending map[string]chan relayproto.Message
	done    chan struct{}
	once    sync.Once
}

func (p *peer) close()     { p.once.Do(func() { p.cancel(); p.c.Conn.CloseNow(); close(p.done) }) }
func (p *peer) count() int { p.mu.Lock(); defer p.mu.Unlock(); return len(p.pending) }

type Server struct {
	Key, Boot string
	mu        sync.Mutex
	routes    map[string]*route
	slots     chan struct{}
	closed    bool
}

func New(key string) (*Server, error) {
	if len(key) < 16 || len(key) > 4096 {
		return nil, errors.New("registration key must be 16..4096 characters")
	}
	return &Server{Key: key, Boot: target.ID(), routes: map[string]*route{}, slots: make(chan struct{}, 64)}, nil
}
func Hash(s string) string   { h := sha256.Sum256([]byte(s)); return hex.EncodeToString(h[:]) }
func equal(a, b string) bool { return subtle.ConstantTimeCompare([]byte(a), []byte(b)) == 1 }
func validID(s string) bool {
	if len(s) < 16 || len(s) > 128 {
		return false
	}
	for _, r := range s {
		if !(r >= 'a' && r <= 'z' || r >= 'A' && r <= 'Z' || r >= '0' && r <= '9' || r == '_' || r == '-') {
			return false
		}
	}
	return true
}
func (s *Server) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	if r.URL.Path == "/connect" {
		s.connect(w, r)
		return
	}
	if r.URL.Path == "/healthz" && r.Method == "GET" {
		w.Write([]byte("ok\n"))
		return
	}
	s.forward(w, r)
}
func (s *Server) connect(w http.ResponseWriter, r *http.Request) {
	if r.Header.Get("Origin") != "" {
		http.Error(w, "browser registration not allowed", 403)
		return
	}
	conn, err := websocket.Accept(w, r, nil)
	if err != nil {
		return
	}
	defer conn.CloseNow()
	c := &relayproto.Connection{Conn: conn}
	conn.SetReadLimit(config.MaxHTTPBody)
	handshake, cancel := context.WithTimeout(r.Context(), 5*time.Second)
	m, err := c.Read(handshake)
	cancel()
	if err != nil {
		return
	}
	s.mu.Lock()
	if s.closed {
		s.mu.Unlock()
		return
	}
	s.pruneLocked()
	var rt *route
	code := ""
	if m.Type == "register" {
		if !equal(m.Key, s.Key) {
			code = "UNAUTHORIZED"
		} else if !validID(m.Instance) || len(m.Name) > 256 || len(m.TokenHash) != 64 || !time.Now().Before(m.Expires) || m.Timeout <= 0 || m.Timeout > 30*time.Minute {
			code = "INVALID_REGISTRATION"
		} else if _, e := hex.DecodeString(m.TokenHash); e != nil {
			code = "INVALID_REGISTRATION"
		} else {
			for _, existing := range s.routes {
				if existing.instance == m.Instance {
					code = "INSTANCE_CONFLICT"
					break
				}
			}
			if code == "" && len(s.routes) >= 16 {
				code = "BUSY"
			}
			if code == "" {
				rt = &route{instance: m.Instance, name: m.Name, id: target.ID(), resume: target.ID(), tokenHash: m.TokenHash, expires: m.Expires, timeout: m.Timeout}
				s.routes[rt.id] = rt
			}
		}
	} else if m.Type == "resume" {
		rt = s.routes[m.Route]
		if m.Boot != s.Boot || rt == nil {
			code = "RE_REGISTER"
		} else if rt.instance != m.Instance || !equal(Hash(m.Resume), rt.resume) {
			code = "UNAUTHORIZED"
		} else if !time.Now().Before(rt.expires) {
			code = "TOKEN_EXPIRED"
		} else if rt.peer != nil {
			select {
			case <-rt.peer.done:
			default:
				code = "INSTANCE_CONFLICT"
			}
		}
	} else {
		code = "INVALID_REGISTRATION"
	}
	if code != "" {
		s.mu.Unlock()
		c.Write(r.Context(), relayproto.Message{Type: "resume_rejected", Code: code})
		return
	}
	ctx, stop := context.WithCancel(r.Context())
	p := &peer{c: c, ctx: ctx, cancel: stop, pending: map[string]chan relayproto.Message{}, done: make(chan struct{})}
	rt.peer = p
	resume := rt.resume
	if m.Type == "register" {
		rt.resume = Hash(resume)
	} else {
		resume = m.Resume
	}
	id := rt.id
	boot := s.Boot
	s.mu.Unlock()
	defer p.close()
	if err = c.Write(ctx, relayproto.Message{Type: "registered", Boot: boot, Route: id, Resume: resume}); err != nil {
		return
	}
	c.Limit()
	go c.Heartbeat(ctx, stop)
	for {
		msg, e := c.Read(ctx)
		if e != nil {
			return
		}
		if msg.Type != "http_response" || msg.Status < 100 || msg.Status > 599 || len(msg.Body) > config.MaxHTTPResponse {
			return
		}
		p.mu.Lock()
		ch := p.pending[msg.ID]
		p.mu.Unlock()
		if ch != nil {
			select {
			case ch <- msg:
			default:
			}
		}
	}
}
func (s *Server) pruneLocked() {
	for id, rt := range s.routes {
		if !time.Now().Before(rt.expires) && (rt.peer == nil || rt.peer.count() == 0) {
			if rt.peer != nil {
				rt.peer.close()
			}
			delete(s.routes, id)
		}
	}
}
func (s *Server) forward(w http.ResponseWriter, r *http.Request) {
	parts := strings.Split(strings.Trim(r.URL.Path, "/"), "/")
	if len(parts) != 4 || parts[0] != "t" || parts[1] != s.Boot || parts[3] != "mcp" {
		http.NotFound(w, r)
		return
	}
	if r.Method != "POST" {
		http.Error(w, "JSON MCP POST only", 405)
		return
	}
	token, err := mcpserver.Token(r)
	if err != nil || token == "" {
		http.Error(w, "UNAUTHORIZED", 401)
		return
	}
	s.mu.Lock()
	s.pruneLocked()
	rt := s.routes[parts[2]]
	if rt == nil {
		s.mu.Unlock()
		http.NotFound(w, r)
		return
	}
	if !equal(Hash(token), rt.tokenHash) {
		s.mu.Unlock()
		http.Error(w, "UNAUTHORIZED", 401)
		return
	}
	if !time.Now().Before(rt.expires) {
		s.mu.Unlock()
		http.Error(w, "TOKEN_EXPIRED", 401)
		return
	}
	p := rt.peer
	timeout := rt.timeout
	s.mu.Unlock()
	r.Body = http.MaxBytesReader(w, r.Body, config.MaxHTTPBody)
	body, err := io.ReadAll(r.Body)
	if err != nil {
		http.Error(w, "request body too large or unreadable", 413)
		return
	}
	if p == nil {
		problem(w, body, "TARGET_OFFLINE", "target offline; command not forwarded", false)
		return
	}
	select {
	case <-p.done:
		problem(w, body, "TARGET_OFFLINE", "target offline; command not forwarded", false)
		return
	default:
	}
	select {
	case s.slots <- struct{}{}:
		defer func() { <-s.slots }()
	default:
		problem(w, body, "BUSY", "relay capacity reached; command not forwarded", false)
		return
	}
	id := target.ID()
	ch := make(chan relayproto.Message, 1)
	p.mu.Lock()
	if len(p.pending) >= 16 {
		p.mu.Unlock()
		problem(w, body, "BUSY", "target HTTP capacity reached; command not forwarded", false)
		return
	}
	p.pending[id] = ch
	p.mu.Unlock()
	defer func() { p.mu.Lock(); delete(p.pending, id); p.mu.Unlock() }()
	msg := relayproto.Message{Type: "http_request", ID: id, Method: r.Method, Path: "/mcp", Host: r.Host, Header: relayproto.FilterHeaders(r.Header), Body: body}
	if q := r.URL.RawQuery; q != "" {
		msg.Path += "?" + q
	}
	if err = p.c.Write(p.ctx, msg); err != nil {
		p.close()
		unknown(w, body)
		return
	}
	timer := time.NewTimer(timeout + 15*time.Second)
	defer timer.Stop()
	select {
	case res := <-ch:
		for k, v := range res.Header {
			if strings.EqualFold(k, "Content-Type") || strings.EqualFold(k, "MCP-Protocol-Version") {
				w.Header()[k] = v
			}
		}
		w.WriteHeader(res.Status)
		w.Write(res.Body)
	case <-p.done:
		unknown(w, body)
	case <-timer.C:
		unknown(w, body)
		p.c.Write(p.ctx, relayproto.Message{Type: "request_abandoned", ID: id})
	case <-r.Context().Done():
		go func() { p.c.Write(p.ctx, relayproto.Message{Type: "request_abandoned", ID: id}) }()
	}
}
func unknown(w http.ResponseWriter, body []byte) {
	problem(w, body, "EXECUTION_UNKNOWN", "可能已执行、结果未知; do not retry automatically", nil)
}

// Translate transport failures into a tool result when its JSON-RPC envelope is
// available. This does not execute tools or authorize them at the relay.
func problem(w http.ResponseWriter, body []byte, code, message string, started any) {
	var req struct {
		ID     json.RawMessage `json:"id"`
		Method string          `json:"method"`
		Params struct {
			Name string `json:"name"`
		} `json:"params"`
	}
	if json.Unmarshal(body, &req) != nil || len(req.ID) == 0 || bytes.Equal(req.ID, []byte("null")) {
		http.Error(w, code+": "+message, http.StatusBadGateway)
		return
	}
	response := map[string]any{"jsonrpc": "2.0", "id": req.ID}
	if req.Method == "tools/call" && req.Params.Name == "exec" {
		response["result"] = map[string]any{"resultType": "complete", "isError": true, "content": []any{map[string]any{"type": "text", "text": code + ": " + message}}, "structuredContent": map[string]any{"status": "rejected", "code": code, "message": message, "started": started, "history_status": "unknown"}}
		if code == "EXECUTION_UNKNOWN" {
			response["result"].(map[string]any)["structuredContent"].(map[string]any)["status"] = "unknown"
		}
	} else {
		response["error"] = map[string]any{"code": -32000, "message": code + ": " + message}
	}
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(response)
}
func (s *Server) Close() {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.closed = true
	for _, rt := range s.routes {
		if rt.peer != nil {
			rt.peer.close()
		}
	}
}
func (s *Server) Stats() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return fmt.Sprintf("%d registered targets", len(s.routes))
}
