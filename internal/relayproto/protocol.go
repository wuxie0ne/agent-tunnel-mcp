package relayproto

import (
	"agent-tunnel/internal/config"
	"context"
	"fmt"
	"github.com/coder/websocket"
	"github.com/coder/websocket/wsjson"
	"net/http"
	"sync"
	"time"
)

const Version = 1

type Message struct {
	Version   int           `json:"version"`
	Type      string        `json:"type"`
	ID        string        `json:"id,omitempty"`
	Code      string        `json:"code,omitempty"`
	Key       string        `json:"key,omitempty"`
	Instance  string        `json:"instance,omitempty"`
	Name      string        `json:"name,omitempty"`
	Boot      string        `json:"boot,omitempty"`
	Route     string        `json:"route,omitempty"`
	Resume    string        `json:"resume,omitempty"`
	TokenHash string        `json:"token_hash,omitempty"`
	Expires   time.Time     `json:"expires,omitempty"`
	Timeout   time.Duration `json:"timeout_ns,omitempty"`
	Method    string        `json:"method,omitempty"`
	Path      string        `json:"path,omitempty"`
	Host      string        `json:"host,omitempty"`
	Header    http.Header   `json:"header,omitempty"`
	Body      []byte        `json:"body,omitempty"`
	Status    int           `json:"status,omitempty"`
}
type Connection struct {
	Conn *websocket.Conn
	mu   sync.Mutex
}

func (c *Connection) Write(ctx context.Context, m Message) error {
	m.Version = Version
	ctx, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()
	c.mu.Lock()
	defer c.mu.Unlock()
	return wsjson.Write(ctx, c.Conn, m)
}
func (c *Connection) Read(ctx context.Context) (Message, error) {
	var m Message
	err := wsjson.Read(ctx, c.Conn, &m)
	if err == nil && m.Version != Version {
		err = fmt.Errorf("unsupported relay protocol")
	}
	return m, err
}
func (c *Connection) Limit() { c.Conn.SetReadLimit(config.MaxWSMessage) }
func (c *Connection) Heartbeat(ctx context.Context, cancel context.CancelFunc) {
	tick := time.NewTicker(10 * time.Second)
	defer tick.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-tick.C:
			pctx, stop := context.WithTimeout(ctx, 20*time.Second)
			err := c.Conn.Ping(pctx)
			stop()
			if err != nil {
				cancel()
				c.Conn.CloseNow()
				return
			}
		}
	}
}
func FilterHeaders(h http.Header) http.Header {
	out := make(http.Header)
	for _, key := range []string{"Content-Type", "Accept", "Authorization", "Origin", "MCP-Protocol-Version", "Mcp-Method", "Mcp-Name"} {
		if values := h.Values(key); len(values) > 0 {
			out[http.CanonicalHeaderKey(key)] = append([]string(nil), values...)
		}
	}
	return out
}
