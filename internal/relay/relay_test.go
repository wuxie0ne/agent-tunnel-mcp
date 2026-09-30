package relay

import (
	"agent-tunnel/internal/config"
	"agent-tunnel/internal/executor"
	"agent-tunnel/internal/mcpserver"
	"agent-tunnel/internal/target"
	"context"
	"encoding/json"
	"errors"
	"github.com/modelcontextprotocol/go-sdk/mcp"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

const testKey = "test-only-registration-key-123456"

func startNode(t *testing.T, base, mode, key string) (*target.Node, chan string) {
	t.Helper()
	c := config.DefaultTarget()
	c.Mode = mode
	c.Transport = "relay"
	c.RelayURL = base
	c.RegistrationKey = key
	c.LogDir = t.TempDir()
	c.Name = "same-name"
	n, e := target.New(c)
	if e != nil {
		t.Fatal(e)
	}
	urls := make(chan string, 16)
	client, e := NewClient(n, mcpserver.New(n), base, key, func(u string) { urls <- u })
	if e != nil {
		t.Fatal(e)
	}
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan error, 1)
	go func() { done <- client.Run(ctx) }()
	t.Cleanup(func() {
		cancel()
		select {
		case <-done:
		case <-time.After(time.Second):
			t.Error("client did not stop")
		}
		shutdown, stop := context.WithTimeout(context.Background(), 5*time.Second)
		defer stop()
		if e := n.Stop(shutdown); e != nil {
			t.Error(e)
		}
	})
	return n, urls
}
func ready(t *testing.T, ch chan string) string {
	t.Helper()
	select {
	case u := <-ch:
		return u
	case <-time.After(8 * time.Second):
		t.Fatal("not ready")
		return ""
	}
}
func connect(t *testing.T, endpoint string, approval *bool) *mcp.ClientSession {
	t.Helper()
	opts := &mcp.ClientOptions{}
	if approval != nil {
		opts.ElicitationHandler = func(context.Context, *mcp.ElicitRequest) (*mcp.ElicitResult, error) {
			return &mcp.ElicitResult{Action: "accept", Content: map[string]any{"confirm": *approval}}, nil
		}
	}
	client := mcp.NewClient(&mcp.Implementation{Name: "sdk-test-not-real-user-client", Version: "1"}, opts)
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	session, e := client.Connect(ctx, &mcp.StreamableClientTransport{Endpoint: endpoint}, &mcp.ClientSessionOptions{ProtocolVersion: mcpserver.Protocol})
	if e != nil {
		t.Fatal(e)
	}
	t.Cleanup(func() { session.Close() })
	return session
}
func tool(t *testing.T, s *mcp.ClientSession, text string) executor.Result {
	t.Helper()
	result, e := s.CallTool(context.Background(), &mcp.CallToolParams{Name: "exec", Arguments: map[string]any{"shell_command": text}})
	if e != nil {
		t.Fatal(e)
	}
	data, _ := json.Marshal(result.StructuredContent)
	var r executor.Result
	if e = json.Unmarshal(data, &r); e != nil {
		t.Fatal(e)
	}
	return r
}
func TestMultipleTargetsReviewAndIsolation(t *testing.T) {
	r, _ := New(testKey)
	server := httptest.NewServer(r)
	defer server.Close()
	defer r.Close()
	n1, ch1 := startNode(t, server.URL, "review", testKey)
	n2, ch2 := startNode(t, server.URL, "allow", testKey)
	u1, u2 := ready(t, ch1), ready(t, ch2)
	if n1.InstanceID == n2.InstanceID || u1 == u2 {
		t.Fatal("identity collision")
	}
	yes := true
	s1 := connect(t, u1, &yes)
	s2 := connect(t, u2, nil)
	if result := tool(t, s1, "printf one"); result.Stdout != "one" || !result.Started {
		t.Fatal(result)
	}
	if result := tool(t, s2, "printf two"); result.Stdout != "two" {
		t.Fatal(result)
	}
	u, _ := url.Parse(u1)
	q := u.Query()
	q.Set("token", n2.Token)
	u.RawQuery = q.Encode()
	req, _ := http.NewRequest("POST", u.String(), strings.NewReader("{}"))
	res, e := http.DefaultClient.Do(req)
	if e != nil {
		t.Fatal(e)
	}
	res.Body.Close()
	if res.StatusCode != 401 {
		t.Fatal("cross-target token accepted")
	}
	no := false
	s3 := connect(t, u1, &no)
	file := filepath.Join(t.TempDir(), "forbidden")
	if result := tool(t, s3, "touch "+file); result.Started {
		t.Fatal(result)
	}
	if _, e = os.Stat(file); e == nil {
		t.Fatal("denied effect")
	}
}
func TestOfflineReconnectAndLostResultNoReplay(t *testing.T) {
	r, _ := New(testKey)
	server := httptest.NewServer(r)
	defer server.Close()
	defer r.Close()
	n, ch := startNode(t, server.URL, "allow", testKey)
	endpoint := ready(t, ch)
	s := connect(t, endpoint, nil)
	file := filepath.Join(t.TempDir(), "effect")
	done := make(chan error, 1)
	go func() {
		response, e := s.CallTool(context.Background(), &mcp.CallToolParams{Name: "exec", Arguments: map[string]any{"shell_command": "sleep .15; echo x >> " + file}})
		if e == nil && response.IsError {
			raw, _ := json.Marshal(response.StructuredContent)
			e = errors.New(string(raw))
		}
		done <- e
	}()
	for i := 0; i < 200; i++ {
		if n.Info()["active_commands"].(int) > 0 {
			break
		}
		time.Sleep(time.Millisecond)
	}
	r.mu.Lock()
	for _, rt := range r.routes {
		rt.peer.close()
	}
	r.mu.Unlock()
	select {
	case e := <-done:
		if e == nil || !strings.Contains(e.Error(), "EXECUTION_UNKNOWN") {
			t.Fatalf("missing unknown: %v", e)
		}
	case <-time.After(time.Second):
		t.Fatal("lost response hung")
	}
	offline := filepath.Join(t.TempDir(), "offline")
	req, _ := http.NewRequest("POST", endpoint, strings.NewReader(`{"jsonrpc":"2.0","id":99,"method":"tools/call","params":{"name":"exec","arguments":{"shell_command":"touch `+offline+`"}}}`))
	res, e := http.DefaultClient.Do(req)
	if e != nil {
		t.Fatal(e)
	}
	res.Body.Close()
	if res.StatusCode != 200 {
		t.Fatal(res.Status)
	}
	next := ready(t, ch)
	if next != endpoint {
		t.Fatal("same-instance address changed")
	}
	time.Sleep(200 * time.Millisecond)
	data, e := os.ReadFile(file)
	if e != nil || string(data) != "x\n" {
		t.Fatalf("execution count: %q %v", data, e)
	}
	if _, e = os.Stat(offline); e == nil {
		t.Fatal("offline request queued")
	}
}
func TestRelayRestartGetsNewURL(t *testing.T) {
	r1, _ := New(testKey)
	var current atomic.Pointer[Server]
	current.Store(r1)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { current.Load().ServeHTTP(w, r) }))
	defer server.Close()
	defer func() { current.Load().Close() }()
	n, ch := startNode(t, server.URL, "allow", testKey)
	original := ready(t, ch)
	expiry := n.Expires
	r2, _ := New(testKey)
	current.Store(r2)
	r1.Close()
	next := ready(t, ch)
	if original == next || !expiry.Equal(n.Expires) {
		t.Fatal("restart URL/expiry")
	}
	res, e := http.Post(original, "application/json", strings.NewReader("{}"))
	if e != nil {
		t.Fatal(e)
	}
	res.Body.Close()
	if res.StatusCode != 404 {
		t.Fatal("old URL not invalid")
	}
	if result := tool(t, connect(t, next, nil), "true"); !result.Started {
		t.Fatal(result)
	}
}
func TestAnonymousRegistrationRejected(t *testing.T) {
	r, _ := New(testKey)
	server := httptest.NewServer(r)
	defer server.Close()
	defer r.Close()
	n, ch := startNode(t, server.URL, "allow", "different-test-only-key")
	select {
	case <-ch:
		t.Fatal("wrong key registered")
	case <-time.After(100 * time.Millisecond):
	}
	if n.Info()["connection_state"] == "ready" {
		t.Fatal("ready with invalid key")
	}
}
