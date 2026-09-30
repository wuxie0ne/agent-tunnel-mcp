package relay

import (
	"agent-tunnel/internal/config"
	"agent-tunnel/internal/mcpserver"
	"agent-tunnel/internal/relayproto"
	"agent-tunnel/internal/target"
	"bytes"
	"context"
	"errors"
	"fmt"
	"github.com/coder/websocket"
	"log"
	"math/rand/v2"
	"net/http"
	"net/url"
	"strings"
	"sync"
	"time"
)

type Client struct {
	Node                *target.Node
	Handler             http.Handler
	Base                *url.URL
	Key                 string
	mu                  sync.Mutex
	boot, route, resume string
	Ready               func(string)
}

func NewClient(n *target.Node, h http.Handler, base, key string, ready func(string)) (*Client, error) {
	u, e := url.Parse(base)
	if e != nil || u.Host == "" || u.Scheme != "http" && u.Scheme != "https" || u.User != nil || u.RawQuery != "" || u.Fragment != "" || u.Path != "" && u.Path != "/" {
		return nil, errors.New("relay URL must be a plain http(s) origin")
	}
	return &Client{Node: n, Handler: h, Base: u, Key: key, Ready: ready}, nil
}
func (c *Client) Run(ctx context.Context) error {
	delay := time.Second
	for {
		if !c.Node.Alive() {
			return nil
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		default:
		}
		err, fatal, registered := c.connect(ctx)
		c.Node.SetConnection("reconnecting", "")
		if fatal {
			return err
		}
		if ctx.Err() != nil {
			return ctx.Err()
		}
		if registered {
			delay = time.Second
		}
		if err != nil {
			log.Printf("relay connection lost; reconnecting (no command replay)")
		}
		wait := delay + time.Duration(rand.Int64N(int64(delay/4+1)))
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(wait):
		}
		delay *= 2
		if delay > 10*time.Second {
			delay = 10 * time.Second
		}
	}
}
func (c *Client) connect(owner context.Context) (error, bool, bool) {
	u := *c.Base
	u.Path = "/connect"
	if u.Scheme == "http" {
		u.Scheme = "ws"
	} else {
		u.Scheme = "wss"
	}
	ctx, cancel := context.WithCancel(owner)
	defer cancel()
	dialctx, stop := context.WithTimeout(ctx, 10*time.Second)
	conn, _, err := websocket.Dial(dialctx, u.String(), nil)
	stop()
	if err != nil {
		return err, false, false
	}
	defer conn.CloseNow()
	wire := &relayproto.Connection{Conn: conn}
	wire.Limit()
	registration := relayproto.Message{Type: "register", Key: c.Key, Instance: c.Node.InstanceID, Name: c.Node.Config.Name, TokenHash: Hash(c.Node.Token), Expires: c.Node.Expires, Timeout: c.Node.Config.Timeout}
	c.mu.Lock()
	if c.resume != "" {
		registration = relayproto.Message{Type: "resume", Boot: c.boot, Route: c.route, Resume: c.resume, Instance: c.Node.InstanceID}
	}
	c.mu.Unlock()
	handshake, stop := context.WithTimeout(ctx, 5*time.Second)
	defer stop()
	if err = wire.Write(handshake, registration); err != nil {
		return err, false, false
	}
	response, err := wire.Read(handshake)
	if err != nil {
		return err, false, false
	}
	if response.Type == "resume_rejected" {
		if response.Code == "RE_REGISTER" {
			c.mu.Lock()
			c.boot = ""
			c.route = ""
			c.resume = ""
			c.mu.Unlock()
			return errors.New("relay restarted: registering new URL"), false, false
		}
		return fmt.Errorf("registration rejected: %s", response.Code), true, false
	}
	if response.Type != "registered" || !validID(response.Boot) || !validID(response.Route) || !validID(response.Resume) {
		return errors.New("invalid registration response"), true, false
	}
	c.mu.Lock()
	c.boot = response.Boot
	c.route = response.Route
	c.resume = response.Resume
	c.mu.Unlock()
	endpoint := *c.Base
	endpoint.Path = "/t/" + response.Boot + "/" + response.Route + "/mcp"
	query := endpoint.Query()
	query.Set("token", c.Node.Token)
	endpoint.RawQuery = query.Encode()
	c.Node.SetConnection("ready", endpoint.String())
	if c.Ready != nil {
		c.Ready(endpoint.String())
	}
	go wire.Heartbeat(ctx, cancel)
	generation := target.ID()
	var requests sync.Map
	slots := make(chan struct{}, 16)
	go func() {
		tick := time.NewTicker(time.Second)
		defer tick.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-tick.C:
				if c.Node.Expired() && c.Node.Idle() && len(slots) == 0 {
					cancel()
					conn.CloseNow()
					return
				}
			}
		}
	}()
	defer requests.Range(func(_, v any) bool { v.(context.CancelFunc)(); return true })
	for {
		msg, e := wire.Read(ctx)
		if e != nil {
			return e, false, true
		}
		switch msg.Type {
		case "request_abandoned":
			if value, ok := requests.Load(msg.ID); ok {
				value.(context.CancelFunc)()
			}
		case "http_request":
			if !validID(msg.ID) || msg.Method != "POST" || len(msg.Body) > config.MaxHTTPBody {
				return errors.New("invalid HTTP envelope"), true, true
			}
			parsed, e := url.ParseRequestURI(msg.Path)
			if e != nil || parsed.Path != "/mcp" || parsed.Host != "" || strings.ContainsAny(msg.Host, "\r\n/") {
				return errors.New("invalid forwarded path"), true, true
			}
			select {
			case slots <- struct{}{}:
			default:
				wire.Write(ctx, relayproto.Message{Type: "http_response", ID: msg.ID, Status: 503, Body: []byte("BUSY")})
				continue
			}
			reqctx, reqcancel := context.WithCancel(mcpserver.WithForwardID(ctx, generation+":"+msg.ID))
			if _, loaded := requests.LoadOrStore(msg.ID, reqcancel); loaded {
				reqcancel()
				<-slots
				return errors.New("duplicate forwarding identifier"), true, true
			}
			go func(msg relayproto.Message, reqctx context.Context, reqcancel context.CancelFunc) {
				defer func() { requests.Delete(msg.ID); reqcancel(); <-slots }()
				r, e := http.NewRequestWithContext(reqctx, msg.Method, "http://target"+msg.Path, bytes.NewReader(msg.Body))
				if e != nil {
					return
				}
				r.Host = msg.Host
				r.Header = relayproto.FilterHeaders(msg.Header)
				buffer := mcpserver.NewBuffer()
				c.Handler.ServeHTTP(buffer, r)
				body := buffer.Body.Bytes()
				if reqctx.Err() != nil {
					return
				}
				if err := wire.Write(ctx, relayproto.Message{Type: "http_response", ID: msg.ID, Status: buffer.Status(), Header: buffer.Header(), Body: body}); err != nil {
					cancel()
					conn.CloseNow()
				}
			}(msg, reqctx, reqcancel)
		default:
			return errors.New("unexpected relay envelope"), true, true
		}
	}
}
