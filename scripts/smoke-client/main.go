// An automated protocol test driver, NOT a local Agent or product gateway.
package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"github.com/modelcontextprotocol/go-sdk/mcp"
	"net"
	"net/http"
	"net/url"
	"os"
	"strings"
	"time"
)

func main() {
	url := flag.String("url-file", "", "file containing a ready JSON line or MCP URL; avoids credentials in argv")
	resolver := flag.String("resolver", "", "test-only DNS-over-TCP server, e.g. 1.1.1.1:53")
	action := flag.String("confirm", "unavailable", "accept, decline, cancel, unavailable (SIMULATED confirmation)")
	command := flag.String("command", "printf agent-tunnel-smoke", "shell command for test")
	wait := flag.Duration("wait", 6*time.Minute, "client waiting deadline")
	flag.Parse()
	if e := run(*url, *action, *command, *wait, *resolver); e != nil {
		fmt.Fprintln(os.Stderr, e)
		os.Exit(1)
	}
}
func run(path, action, command string, wait time.Duration, resolver string) error {
	raw, e := os.ReadFile(path)
	if e != nil {
		return e
	}
	endpoint := strings.TrimSpace(string(raw))
	if strings.HasPrefix(endpoint, "{") {
		var ready struct {
			URL string `json:"mcp_url"`
		}
		if e = json.Unmarshal(raw, &ready); e != nil {
			return e
		}
		endpoint = ready.URL
	}
	if endpoint == "" {
		return errors.New("missing URL")
	}
	opts := &mcp.ClientOptions{}
	if action != "unavailable" {
		if action != "accept" && action != "decline" && action != "cancel" {
			return errors.New("invalid confirmation action")
		}
		opts.ElicitationHandler = func(context.Context, *mcp.ElicitRequest) (*mcp.ElicitResult, error) {
			return &mcp.ElicitResult{Action: action, Content: map[string]any{"confirm": action == "accept"}}, nil
		}
	}
	client := mcp.NewClient(&mcp.Implementation{Name: "agent-tunnel-automated-smoke-NOT-real-Agent-approval", Version: "1"}, opts)
	ctx, cancel := context.WithTimeout(context.Background(), wait)
	defer cancel()
	transport := http.DefaultTransport.(*http.Transport).Clone()
	if resolver != "" {
		dialer := &net.Dialer{Timeout: 30 * time.Second, KeepAlive: 30 * time.Second, Resolver: &net.Resolver{PreferGo: true, Dial: func(ctx context.Context, _, _ string) (net.Conn, error) {
			return (&net.Dialer{Timeout: 5 * time.Second}).DialContext(ctx, "tcp", resolver)
		}}}
		transport.DialContext = dialer.DialContext
	}
	var session *mcp.ClientSession
	// Discovery only: safe to retry before any command has been issued.
	for attempt := 0; attempt < 8; attempt++ {
		discovery, stop := context.WithTimeout(ctx, 5*time.Second)
		session, e = client.Connect(discovery, &mcp.StreamableClientTransport{Endpoint: endpoint, HTTPClient: &http.Client{Transport: transport}}, &mcp.ClientSessionOptions{ProtocolVersion: "2026-07-28"})
		stop()
		if e == nil {
			break
		}
		if ctx.Err() != nil {
			break
		}
		select {
		case <-ctx.Done():
		case <-time.After(2 * time.Second):
		}
	}

	if e != nil {
		return fmt.Errorf("MCP connection failed (URL/token redacted): %s", redact(e.Error(), endpoint))
	}
	defer session.Close()
	result, e := session.CallTool(ctx, &mcp.CallToolParams{Name: "exec", Arguments: map[string]any{"shell_command": command, "max_output_bytes": 262144}})
	if e != nil {
		return fmt.Errorf("MCP call failed; may have executed, do not retry: %s", redact(e.Error(), endpoint))
	}
	data, e := json.Marshal(result)
	if e != nil {
		return e
	}
	fmt.Println(string(data))
	if result.IsError {
		return errors.New("tool reported an error")
	}
	return nil
}
func redact(text, endpoint string) string {
	if text == "" {
		return text
	}
	text = strings.ReplaceAll(text, endpoint, "<MCP URL redacted>")
	if u, e := url.Parse(endpoint); e == nil {
		if token := u.Query().Get("token"); token != "" {
			text = strings.ReplaceAll(text, token, "<token redacted>")
		}
	}
	return text
}
