package mcpserver

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"

	"agent-tunnel/internal/config"
	"agent-tunnel/internal/executor"
	"agent-tunnel/internal/target"
	"github.com/modelcontextprotocol/go-sdk/mcp"
)

const Protocol = "2026-07-28"

var Version = "0.1.0-dev"

type forwardingKey struct{}

func WithForwardID(ctx context.Context, id string) context.Context {
	return context.WithValue(ctx, forwardingKey{}, id)
}
func toolResult(r executor.Result) *mcp.CallToolResult {
	return &mcp.CallToolResult{StructuredContent: r, IsError: r.Code != "" || (r.ExitCode != nil && *r.ExitCode != 0), Content: []mcp.Content{&mcp.TextContent{Text: fmt.Sprintf("%s %s %s; request=%s history=%s. Missing response never proves non-execution; do not automatically retry.", r.Status, r.Code, r.Message, r.RequestID, r.HistoryStatus)}}}
}
func New(n *target.Node) http.Handler {
	handler := mcp.NewStreamableHTTPHandler(func(r *http.Request) *mcp.Server {
		s := mcp.NewServer(&mcp.Implementation{Name: "agent-tunnel", Version: Version}, &mcp.ServerOptions{SupportedProtocolVersions: []string{Protocol}, Capabilities: &mcp.ServerCapabilities{Tools: &mcp.ToolCapabilities{}}, Instructions: "Commands run as the target OS user. Unknown/lost results must never be automatically retried. No background task or history retrieval tools."})
		s.AddTool(&mcp.Tool{Name: "target_info", Description: "Target identity, connection state, permissions and resource limits; no system diagnostics.", InputSchema: map[string]any{"type": "object", "additionalProperties": false}}, func(ctx context.Context, req *mcp.CallToolRequest) (*mcp.CallToolResult, error) {
			return &mcp.CallToolResult{StructuredContent: n.Info(), Content: []mcp.Content{&mcp.TextContent{Text: "Target identity and limits"}}}, nil
		})
		s.AddTool(&mcp.Tool{Name: "exec", Description: "Execute one synchronous noninteractive command. Exactly one shell_command or program. No stdin, background tasks, output retrieval or cancellation tool. Lost responses may mean command executed: never automatically retry.", InputSchema: map[string]any{"type": "object", "properties": map[string]any{"shell_command": map[string]any{"type": "string"}, "program": map[string]any{"type": "string"}, "args": map[string]any{"type": "array", "items": map[string]any{"type": "string"}, "maxItems": 1024}, "cwd": map[string]any{"type": "string"}, "max_output_bytes": map[string]any{"type": "integer", "minimum": 1, "maximum": config.MaxOutput}}, "additionalProperties": false, "oneOf": []any{map[string]any{"required": []string{"shell_command"}, "not": map[string]any{"anyOf": []any{map[string]any{"required": []string{"program"}}, map[string]any{"required": []string{"args"}}}}}, map[string]any{"required": []string{"program"}, "not": map[string]any{"required": []string{"shell_command"}}}}}}, func(ctx context.Context, req *mcp.CallToolRequest) (*mcp.CallToolResult, error) {
			var in executor.Input
			dec := json.NewDecoder(bytes.NewReader(req.Params.Arguments))
			dec.DisallowUnknownFields()
			if err := dec.Decode(&in); err != nil {
				return toolResult(executor.Failure("INVALID_ARGUMENT", "invalid command arguments")), nil
			}
			spec, err := n.Validate(in)
			if err != nil {
				return toolResult(executor.Failure("INVALID_ARGUMENT", err.Error())), nil
			}
			id, _ := r.Context().Value(forwardingKey{}).(string)
			approved := false
			if n.Config.Mode == "strict" {
				return toolResult(n.Reject(spec, "MODE_DENIED", "strict mode rejects every command")), nil
			}
			if n.Config.Mode == "review" {
				if req.Params.RequestState == "" {
					if len(req.Params.InputResponses) > 0 {
						return toolResult(n.Reject(spec, "APPROVAL_UNAVAILABLE", "input response without issued confirmation state")), nil
					}
					caps := req.ClientCapabilities()
					if caps == nil || caps.Elicitation == nil || caps.Elicitation.Form == nil {
						return toolResult(n.Reject(spec, "APPROVAL_UNAVAILABLE", "client does not support form elicitation")), nil
					}
					state, fail := n.Prepare(spec)
					if fail.Code != "" {
						return toolResult(fail), nil
					}
					data, _ := json.MarshalIndent(spec, "", "  ")
					return &mcp.CallToolResult{RequestState: state, InputRequests: mcp.InputRequestMap{"confirm": &mcp.ElicitParams{Mode: "form", Message: fmt.Sprintf("Approve command on %s (instance %s), mode review, timeout %s, UID %d/GID %d? Full request:\n%s", n.Config.Name, n.InstanceID, spec.Timeout, spec.UID, spec.GID, data), RequestedSchema: map[string]any{"type": "object", "properties": map[string]any{"confirm": map[string]any{"type": "boolean", "default": false}}, "required": []string{"confirm"}, "additionalProperties": false}}}}, nil
				}
				response, ok := req.Params.InputResponses["confirm"].(*mcp.ElicitResult)
				if ok && response.Action == "accept" {
					confirmed, _ := response.Content["confirm"].(bool)
					approved = confirmed
				}
				var fail executor.Result
				id, fail = n.Consume(req.Params.RequestState, spec, approved)
				if fail.Code != "" {
					return toolResult(fail), nil
				}
			} else if req.Params.RequestState != "" || len(req.Params.InputResponses) > 0 {
				return toolResult(executor.Failure("INVALID_ARGUMENT", "continuation not valid in allow mode")), nil
			}
			// HTTP cancellation is checked before starting; after admission Node owns execution.
			return toolResult(n.Execute(r.Context(), id, spec, approved)), nil
		})
		return s
	}, &mcp.StreamableHTTPOptions{Stateless: true, JSONResponse: true, MaxRequestBodyBytes: config.MaxHTTPBody, PropagateRequestCancellation: true, DisableLocalhostProtection: true})
	semaphore := make(chan struct{}, 16)
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/mcp" {
			http.NotFound(w, r)
			return
		}
		if r.Method != http.MethodPost {
			http.Error(w, "only JSON MCP POST supported", http.StatusMethodNotAllowed)
			return
		}
		token, err := Token(r)
		if err != nil || !n.Authenticate(token) {
			http.Error(w, "UNAUTHORIZED", http.StatusUnauthorized)
			return
		}
		if n.Expired() {
			http.Error(w, "TOKEN_EXPIRED", http.StatusUnauthorized)
			return
		}
		if origin := r.Header.Get("Origin"); origin != "" {
			o, e := url.Parse(origin)
			if e != nil || !n.AllowOrigin(o.Scheme+"://"+o.Host) && !((o.Hostname() == "127.0.0.1" || o.Hostname() == "::1" || o.Hostname() == "localhost") && o.Host == r.Host && (o.Scheme == "http" || o.Scheme == "https")) {
				http.Error(w, "invalid origin", http.StatusForbidden)
				return
			}
		}
		select {
		case semaphore <- struct{}{}:
			defer func() { <-semaphore }()
		default:
			http.Error(w, "BUSY", http.StatusServiceUnavailable)
			return
		}
		if v := r.Header.Get("MCP-Protocol-Version"); v != Protocol {
			http.Error(w, "unsupported protocol: use 2026-07-28 JSON MRTR", http.StatusBadRequest)
			return
		}
		if !n.HTTPBegin() {
			http.Error(w, "TARGET_STOPPED or TOKEN_EXPIRED", 503)
			return
		}
		defer n.HTTPEnd()
		b := NewBuffer()
		handler.ServeHTTP(b, r)
		if b.overflow {
			http.Error(w, "EXECUTION_UNKNOWN: response exceeds limit; do not retry", http.StatusInternalServerError)
			return
		}
		if strings.Contains(b.Header().Get("Content-Type"), "text/event-stream") {
			http.Error(w, "SSE unsupported; use JSON MRTR", http.StatusBadRequest)
			return
		}
		for k, values := range b.Header() {
			w.Header()[k] = values
		}
		w.WriteHeader(b.Status())
		w.Write(b.Body.Bytes())
	})
}
func Token(r *http.Request) (string, error) {
	if len(r.Header.Values("Authorization")) > 1 {
		return "", fmt.Errorf("multiple authorization headers")
	}
	query := r.URL.Query()
	if len(query["token"]) > 1 {
		return "", fmt.Errorf("duplicate token")
	}
	q := query.Get("token")
	a := r.Header.Get("Authorization")
	if a != "" {
		if !strings.HasPrefix(a, "Bearer ") {
			return "", fmt.Errorf("invalid authorization")
		}
		a = strings.TrimPrefix(a, "Bearer ")
	}
	if q != "" && a != "" && q != a {
		return "", fmt.Errorf("conflicting tokens")
	}
	if a != "" {
		return a, nil
	}
	return q, nil
}

type Buffer struct {
	header   http.Header
	Body     bytes.Buffer
	status   int
	overflow bool
}

func NewBuffer() *Buffer              { return &Buffer{header: make(http.Header)} }
func (b *Buffer) Header() http.Header { return b.header }
func (b *Buffer) Status() int {
	if b.status == 0 {
		return http.StatusOK
	}
	return b.status
}
func (b *Buffer) WriteHeader(status int) {
	if b.status == 0 {
		b.status = status
	}
}
func (b *Buffer) Write(p []byte) (int, error) {
	if b.status == 0 {
		b.status = 200
	}
	if b.Body.Len()+len(p) > config.MaxHTTPResponse {
		b.overflow = true
		return 0, io.ErrShortBuffer
	}
	return b.Body.Write(p)
}
func (b *Buffer) Flush() {}
