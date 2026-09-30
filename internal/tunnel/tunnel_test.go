package tunnel

import (
	"agent-tunnel/internal/config"
	"agent-tunnel/internal/target"
	"context"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestMissingCloudflaredAndLifecycle(t *testing.T) {
	if _, e := Find("/this-cloudflared-is-missing"); e == nil {
		t.Fatal("missing accepted")
	}
	dir := t.TempDir()
	program := filepath.Join(dir, "cloudflared")
	os.WriteFile(program, []byte("#!/bin/sh\necho https://first-test.trycloudflare.com\necho 'Registered tunnel connection'\nsleep .05\necho https://changed-test.trycloudflare.com\nsleep 5\n"), 0700)
	c := config.DefaultTarget()
	c.Mode = "strict"
	c.Transport = "quick"
	c.LogDir = dir
	n, e := target.New(c)
	if e != nil {
		t.Fatal(e)
	}
	defer n.Stop(context.Background())
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	ready := make(chan string, 4)
	manager := Manager{Node: n, Binary: program, Origin: "http://127.0.0.1:1234", Ready: func(u string) { ready <- u }}
	done := make(chan error, 1)
	go func() { done <- manager.Run(ctx) }()
	for i := 0; i < 2; i++ {
		select {
		case <-ready:
		case <-time.After(time.Second):
			t.Fatal("new URL not delivered")
		}
	}
	cancel()
	select {
	case <-done:
	case <-time.After(3 * time.Second):
		t.Fatal("cloudflared not stopped")
	}
}
