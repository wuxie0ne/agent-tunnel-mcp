package mcpserver

import (
	"agent-tunnel/internal/config"
	"agent-tunnel/internal/executor"
	"agent-tunnel/internal/target"
	"context"
	"encoding/json"
	"github.com/modelcontextprotocol/go-sdk/mcp"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func fixture(t *testing.T, mode string) (*target.Node, *httptest.Server) {
	t.Helper()
	c := config.DefaultTarget()
	c.Transport = "quick"
	c.Mode = mode
	c.LogDir = t.TempDir()
	n, e := target.New(c)
	if e != nil {
		t.Fatal(e)
	}
	h := httptest.NewServer(New(n))
	n.SetConnection("ready", h.URL+"/mcp")
	t.Cleanup(func() {
		h.Close()
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		if e := n.Stop(ctx); e != nil {
			t.Error(e)
		}
	})
	return n, h
}
func session(t *testing.T, endpoint string, approve *bool) *mcp.ClientSession {
	t.Helper()
	opts := &mcp.ClientOptions{}
	if approve != nil {
		opts.ElicitationHandler = func(_ context.Context, r *mcp.ElicitRequest) (*mcp.ElicitResult, error) {
			return &mcp.ElicitResult{Action: "accept", Content: map[string]any{"confirm": *approve}}, nil
		}
	}
	client := mcp.NewClient(&mcp.Implementation{Name: "automated-test-not-real-agent", Version: "1"}, opts)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	s, e := client.Connect(ctx, &mcp.StreamableClientTransport{Endpoint: endpoint}, &mcp.ClientSessionOptions{ProtocolVersion: Protocol})
	if e != nil {
		t.Fatal(e)
	}
	t.Cleanup(func() { s.Close() })
	return s
}
func TestSDKJSONReviewAndModes(t *testing.T) {
	for _, mode := range []string{"allow", "review", "strict"} {
		for _, approved := range []bool{true, false} {
			t.Run(mode+"/"+map[bool]string{true: "approve", false: "reject"}[approved], func(t *testing.T) {
				n, h := fixture(t, mode)
				s := session(t, h.URL+"/mcp?token="+n.Token, &approved)
				file := filepath.Join(t.TempDir(), "effect")
				result, e := s.CallTool(context.Background(), &mcp.CallToolParams{Name: "exec", Arguments: map[string]any{"shell_command": "touch " + file}})
				if e != nil {
					t.Fatal(e)
				}
				raw, _ := json.Marshal(result.StructuredContent)
				var r executor.Result
				json.Unmarshal(raw, &r)
				want := mode == "allow" || mode == "review" && approved
				if r.Started != want {
					t.Fatalf("want=%v result=%s", want, raw)
				}
				_, e = os.Stat(file)
				if (e == nil) != want {
					t.Fatal("side effect mismatch")
				}
			})
		}
	}
}
func TestNoConfirmationAndAuthentication(t *testing.T) {
	n, h := fixture(t, "review")
	s := session(t, h.URL+"/mcp?token="+n.Token, nil)
	r, e := s.CallTool(context.Background(), &mcp.CallToolParams{Name: "exec", Arguments: map[string]any{"program": "/bin/true"}})
	if e != nil {
		t.Fatal(e)
	}
	if !r.IsError {
		t.Fatal("confirmation unavailable accepted")
	}
	res, e := http.Post(h.URL+"/mcp", "application/json", nil)
	if e != nil {
		t.Fatal(e)
	}
	res.Body.Close()
	if res.StatusCode != 401 {
		t.Fatal(res.Status)
	}
}
