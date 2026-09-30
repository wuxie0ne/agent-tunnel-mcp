package relayproto

import (
	"net/http"
	"testing"
)

func TestHeaderCanonicalizationAndWhitelist(t *testing.T) {
	h := http.Header{}
	h.Set("MCP-Protocol-Version", "2026-07-28")
	h.Set("Mcp-Method", "tools/call")
	h.Set("Connection", "keep-alive")
	f := FilterHeaders(h)
	if f.Get("MCP-Protocol-Version") != "2026-07-28" || f.Get("Mcp-Method") != "tools/call" {
		t.Fatal(f)
	}
	if f.Get("Connection") != "" {
		t.Fatal("hop-by-hop forwarded")
	}
}
