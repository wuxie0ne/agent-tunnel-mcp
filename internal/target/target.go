package target

import (
	"context"
	"crypto/rand"
	"crypto/sha256"
	"crypto/subtle"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net/url"
	"os"
	"sync"
	"time"

	"agent-tunnel/internal/config"
	"agent-tunnel/internal/executor"
	"agent-tunnel/internal/history"
)

func ID() string { var b [32]byte; rand.Read(b[:]); return base64.RawURLEncoding.EncodeToString(b[:]) }

type pending struct {
	spec        executor.Spec
	fingerprint [32]byte
	expires     time.Time
	generation  uint64
}
type record struct{ fingerprint [32]byte }
type Node struct {
	httpActive             int
	Config                 config.Target
	InstanceID, Token, Cwd string
	Expires                time.Time
	History                history.Sink
	Runner                 executor.Runner
	ApprovalTTL            time.Duration
	mu                     sync.Mutex
	owner                  context.Context
	cancel                 context.CancelFunc
	stopping               bool
	state, url             string
	generation             uint64
	pending                map[string]pending
	recent                 map[string]record
	order                  []string
	active                 map[string]record
	unhealthy              bool
	wg                     sync.WaitGroup
}

func New(c config.Target) (*Node, error) {
	if err := c.Validate(); err != nil {
		return nil, err
	}
	id := ID()
	sink, err := history.Open(c.LogDir, id)
	if err != nil {
		return nil, err
	}
	n, err := NewWithHistory(c, id, sink)
	if err != nil {
		sink.Close()
	}
	return n, err
}
func NewWithHistory(c config.Target, id string, sink history.Sink) (*Node, error) {
	cwd, err := os.Getwd()
	if err != nil {
		return nil, err
	}
	owner, cancel := context.WithCancel(context.Background())
	n := &Node{Config: c, InstanceID: id, Token: ID(), Cwd: cwd, Expires: time.Now().Add(c.TTL), History: sink, Runner: executor.DefaultRunner(), ApprovalTTL: config.ApprovalTTL, owner: owner, cancel: cancel, state: "connecting", pending: map[string]pending{}, active: map[string]record{}, recent: map[string]record{}}
	go n.sweep()
	return n, nil
}
func (n *Node) Authenticate(token string) bool {
	return token != "" && subtle.ConstantTimeCompare([]byte(token), []byte(n.Token)) == 1
}
func (n *Node) Expired() bool              { return !time.Now().Before(n.Expires) }
func fingerprint(s executor.Spec) [32]byte { b, _ := json.Marshal(s); return sha256.Sum256(b) }
func (n *Node) checkLocked() executor.Result {
	if n.stopping {
		return executor.Failure("TARGET_STOPPED", "target is stopping")
	}
	if n.Expired() {
		return executor.Failure("TOKEN_EXPIRED", "token expired")
	}
	if n.unhealthy || n.History.Err() != nil {
		return executor.Failure("HISTORY_UNAVAILABLE", "execution disabled: history or process cleanup is unhealthy; restart after repair")
	}
	if n.state != "ready" {
		return executor.Failure("TARGET_OFFLINE", "target is not connected")
	}
	return executor.Result{}
}
func (n *Node) remember(id string, fp [32]byte) {
	n.recent[id] = record{fp}
	n.order = append(n.order, id)
	if len(n.order) > config.RecentRequests {
		delete(n.recent, n.order[0])
		n.order = n.order[1:]
	}
}
func (n *Node) write(event, id string, data any) error {
	err := n.History.Write(history.Event{Event: event, RequestID: id, Data: data})
	if err != nil {
		n.unhealthy = true
		log.Printf("command history write failed; new executions disabled (instance=%s)", n.InstanceID)
	}
	return err
}
func (n *Node) rejectLocked(id string, s executor.Spec, r executor.Result) executor.Result {
	r.RequestID = id
	if err := n.write("execution_rejected", id, map[string]any{"command": s, "code": r.Code, "message": r.Message}); err != nil {
		r.HistoryStatus = "failed"
	} else {
		r.HistoryStatus = "ok"
	}
	return r
}
func (n *Node) Reject(s executor.Spec, code, message string) executor.Result {
	n.mu.Lock()
	defer n.mu.Unlock()
	return n.rejectLocked(ID(), s, executor.Failure(code, message))
}
func (n *Node) SetConnection(state, url string) {
	n.mu.Lock()
	defer n.mu.Unlock()
	if n.state != state || n.url != url {
		n.generation++
		for id, p := range n.pending {
			n.rejectLocked(id, p.spec, executor.Failure("APPROVAL_UNAVAILABLE", "connection changed before execution"))
			n.remember(id, p.fingerprint)
			delete(n.pending, id)
		}
	}
	n.state = state
	n.url = url
}
func (n *Node) Prepare(s executor.Spec) (string, executor.Result) {
	n.mu.Lock()
	defer n.mu.Unlock()
	if r := n.checkLocked(); r.Code != "" {
		return "", n.rejectLocked(ID(), s, r)
	}
	if len(n.pending) >= config.MaxPending {
		return "", n.rejectLocked(ID(), s, executor.Failure("BUSY", "too many pending confirmations"))
	}
	id := ID()
	n.pending[id] = pending{s, fingerprint(s), time.Now().Add(n.ApprovalTTL), n.generation}
	return id, executor.Result{}
}
func (n *Node) Consume(id string, s executor.Spec, approved bool) (string, executor.Result) {
	n.mu.Lock()
	defer n.mu.Unlock()
	p, ok := n.pending[id]
	if !ok {
		return "", executor.Failure("APPROVAL_EXPIRED", "confirmation state is unknown, consumed, or expired")
	}
	delete(n.pending, id)
	var fail executor.Result
	if p.fingerprint != fingerprint(s) {
		fail = executor.Failure("REQUEST_CONFLICT", "confirmation does not match complete command")
	} else if p.generation != n.generation {
		fail = executor.Failure("APPROVAL_UNAVAILABLE", "connection changed")
	} else if !time.Now().Before(p.expires) {
		fail = executor.Failure("APPROVAL_EXPIRED", "confirmation expired")
	} else if !approved {
		fail = executor.Failure("APPROVAL_REJECTED", "confirmation declined or cancelled")
	}
	if fail.Code != "" {
		n.remember(id, p.fingerprint)
		return "", n.rejectLocked(id, p.spec, fail)
	}
	return id, executor.Result{}
}
func (n *Node) Execute(ctx context.Context, id string, s executor.Spec, approved bool) executor.Result {
	if id == "" {
		id = ID()
	}
	fp := fingerprint(s)
	n.mu.Lock()
	for _, m := range []map[string]record{n.active, n.recent} {
		if rec, ok := m[id]; ok {
			n.mu.Unlock()
			if rec.fingerprint != fp {
				return executor.Failure("REQUEST_CONFLICT", "request id reused with different command")
			}
			return executor.Failure("REQUEST_DUPLICATE", "not executed again; previous output is not available")
		}
	}
	if r := n.checkLocked(); r.Code != "" {
		r = n.rejectLocked(id, s, r)
		n.mu.Unlock()
		return r
	}
	if n.Config.Mode == "strict" || (n.Config.Mode == "review" && !approved) {
		r := n.rejectLocked(id, s, executor.Failure("MODE_DENIED", "command is not authorized"))
		n.mu.Unlock()
		return r
	}
	if len(n.active) >= n.Config.Concurrency {
		r := n.rejectLocked(id, s, executor.Failure("BUSY", "command concurrency limit reached"))
		n.mu.Unlock()
		return r
	}
	if ctx.Err() != nil {
		r := n.rejectLocked(id, s, executor.Failure("REQUEST_CANCELLED", "request cancelled before execution"))
		n.mu.Unlock()
		return r
	}
	if err := n.write("execution_intent", id, map[string]any{"command": s, "mode": n.Config.Mode, "approved": approved}); err != nil {
		n.mu.Unlock()
		return executor.Failure("HISTORY_UNAVAILABLE", "unable to persist execution intent; command not started")
	}
	if r := n.checkLocked(); r.Code != "" || ctx.Err() != nil {
		if r.Code == "" {
			r = executor.Failure("REQUEST_CANCELLED", "cancelled after recording intent")
		}
		r.RequestID = id
		r.EndTime = time.Now().UTC()
		n.write("execution_result", id, r.Metadata())
		n.remember(id, fp)
		n.mu.Unlock()
		return r
	}
	n.active[id] = record{fp}
	n.wg.Add(1)
	n.mu.Unlock()
	r := n.Runner.Run(n.owner, s)
	r.RequestID = id
	n.mu.Lock()
	if err := n.write("execution_result", id, r.Metadata()); err != nil {
		r.HistoryStatus = "failed"
	} else {
		r.HistoryStatus = "ok"
	}
	if r.CleanupStatus == "unconfirmed" {
		n.unhealthy = true
	}
	delete(n.active, id)
	n.remember(id, fp)
	n.mu.Unlock()
	n.wg.Done()
	return r
}
func (n *Node) Info() map[string]any {
	n.mu.Lock()
	defer n.mu.Unlock()
	health := "ok"
	if n.unhealthy || n.History.Err() != nil {
		health = "failed"
	}
	state := n.state
	if n.Expired() {
		state = "expired"
	}
	return map[string]any{"name": n.Config.Name, "instance_id": n.InstanceID, "mode": n.Config.Mode, "expires_at": n.Expires.UTC(), "transport": n.Config.Transport, "connection_state": state, "mcp_url_available": n.url != "", "default_cwd": n.Cwd, "shell": n.Config.Shell, "uid": os.Geteuid(), "gid": os.Getegid(), "command_timeout_seconds": n.Config.Timeout.Seconds(), "concurrency": n.Config.Concurrency, "active_commands": len(n.active), "active_http_requests": n.httpActive, "pending_confirmations": len(n.pending), "capture_bytes_per_stream": config.CaptureLimit, "default_output_bytes": config.DefaultOutput, "max_output_bytes": config.MaxOutput, "history_path": n.History.Path(), "history_status": health}
}
func (n *Node) sweep() {
	ticker := time.NewTicker(time.Second)
	defer ticker.Stop()
	notified := false
	for {
		select {
		case <-n.owner.Done():
			return
		case <-ticker.C:
			n.mu.Lock()
			for id, p := range n.pending {
				if n.Expired() || !time.Now().Before(p.expires) {
					n.rejectLocked(id, p.spec, executor.Failure("APPROVAL_EXPIRED", "confirmation or token expired"))
					n.remember(id, p.fingerprint)
					delete(n.pending, id)
				}
			}
			if n.Expired() && !notified {
				log.Printf("token expired (instance=%s); running commands continue, new operations rejected", n.InstanceID)
				notified = true
			}
			n.mu.Unlock()
		}
	}
}
func (n *Node) Stop(ctx context.Context) error {
	n.mu.Lock()
	n.stopping = true
	n.cancel()
	for id, p := range n.pending {
		n.rejectLocked(id, p.spec, executor.Failure("TARGET_STOPPED", "target stopped"))
		delete(n.pending, id)
	}
	n.mu.Unlock()
	done := make(chan struct{})
	go func() { n.wg.Wait(); close(done) }()
	select {
	case <-done:
		return n.History.Close()
	case <-ctx.Done():
		return fmt.Errorf("shutdown incomplete: %w", ctx.Err())
	}
}
func (n *Node) Alive() bool { n.mu.Lock(); defer n.mu.Unlock(); return !n.stopping && !n.Expired() }
func (n *Node) Validate(in executor.Input) (executor.Spec, error) {
	if in.MaxOutputBytes < 0 {
		return executor.Spec{}, errors.New("negative output budget")
	}
	return executor.Normalize(in, n.Cwd, n.Config.Shell, n.Config.Timeout)
}

func (n *Node) AllowOrigin(origin string) bool {
	n.mu.Lock()
	defer n.mu.Unlock()
	u, err := url.Parse(n.url)
	return err == nil && u.Host != "" && origin == u.Scheme+"://"+u.Host
}

func (n *Node) HTTPBegin() { n.mu.Lock(); n.httpActive++; n.mu.Unlock() }
func (n *Node) HTTPEnd()   { n.mu.Lock(); n.httpActive--; n.mu.Unlock() }
func (n *Node) Idle() bool {
	n.mu.Lock()
	defer n.mu.Unlock()
	return len(n.active) == 0 && n.httpActive == 0
}
