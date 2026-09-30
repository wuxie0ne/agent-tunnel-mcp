package target

import (
	"agent-tunnel/internal/config"
	"agent-tunnel/internal/executor"
	"agent-tunnel/internal/history"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"sync"
	"testing"
	"time"
)

func node(t *testing.T, mode string) *Node {
	t.Helper()
	c := config.DefaultTarget()
	c.Mode = mode
	c.Transport = "quick"
	c.LogDir = t.TempDir()
	n, e := New(c)
	if e != nil {
		t.Fatal(e)
	}
	n.SetConnection("ready", "http://localhost/mcp")
	t.Cleanup(func() {
		ctx, cancel := context.WithTimeout(context.Background(), time.Second*5)
		defer cancel()
		n.Stop(ctx)
	})
	return n
}
func cmd(t *testing.T, n *Node, text string) executor.Spec {
	s, e := n.Validate(executor.Input{ShellCommand: &text})
	if e != nil {
		t.Fatal(e)
	}
	return s
}
func TestModesAndApprovalBinding(t *testing.T) {
	n := node(t, "review")
	file := filepath.Join(t.TempDir(), "effect")
	s := cmd(t, n, "touch "+file)
	id, fail := n.Prepare(s)
	if fail.Code != "" {
		t.Fatal(fail)
	}
	other := cmd(t, n, "echo changed")
	if _, r := n.Consume(id, other, true); r.Code != "REQUEST_CONFLICT" {
		t.Fatal(r)
	}
	if _, e := os.Stat(file); e == nil {
		t.Fatal("effect without approve")
	}
	id, _ = n.Prepare(s)
	rid, r := n.Consume(id, s, true)
	if r.Code != "" {
		t.Fatal(r)
	}
	r = n.Execute(context.Background(), rid, s, true)
	if !r.Started {
		t.Fatal(r)
	}
	if n.Execute(context.Background(), rid, s, true).Code != "REQUEST_DUPLICATE" {
		t.Fatal("replay")
	}
	if _, r = n.Consume(id, s, true); r.Code == "" {
		t.Fatal("confirmation replay")
	}
	strict := node(t, "strict")
	if strict.Execute(context.Background(), "", cmd(t, strict, "true"), true).Started {
		t.Fatal("strict")
	}
}
func TestCancelExpireBusyAndReconnect(t *testing.T) {
	n := node(t, "allow")
	n.Config.Concurrency = 1
	s := cmd(t, n, "sleep .1")
	done := make(chan executor.Result, 1)
	go func() { done <- n.Execute(context.Background(), "a", s, false) }()
	for i := 0; i < 100; i++ {
		if n.Info()["active_commands"].(int) == 1 {
			break
		}
		time.Sleep(time.Millisecond)
	}
	if r := n.Execute(context.Background(), "b", s, false); r.Code != "BUSY" {
		t.Fatal(r)
	}
	<-done
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if r := n.Execute(ctx, "c", s, false); r.Started {
		t.Fatal(r)
	}
	review := node(t, "review")
	id, _ := review.Prepare(cmd(t, review, "true"))
	review.SetConnection("reconnecting", "")
	review.SetConnection("ready", "http://localhost/mcp")
	if _, r := review.Consume(id, cmd(t, review, "true"), true); r.Code == "" {
		t.Fatal("old approval restored")
	}
}

type faultySink struct {
	mu     sync.Mutex
	writes int
	failAt int
	err    error
}

func (f *faultySink) Write(history.Event) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.writes++
	if f.writes >= f.failAt {
		f.err = errors.New("injected disk failure")
	}
	return f.err
}
func (f *faultySink) Err() error   { f.mu.Lock(); defer f.mu.Unlock(); return f.err }
func (f *faultySink) Path() string { return "test" }
func (f *faultySink) Close() error { return nil }
func TestHistoryFailureBeforeAndAfterExecution(t *testing.T) {
	for _, at := range []int{1, 2} {
		t.Run(string(rune('0'+at)), func(t *testing.T) {
			c := config.DefaultTarget()
			c.Mode = "allow"
			c.Transport = "quick"
			sink := &faultySink{failAt: at}
			n, _ := NewWithHistory(c, ID(), sink)
			n.SetConnection("ready", "http://localhost/mcp")
			defer n.Stop(context.Background())
			file := filepath.Join(t.TempDir(), "effect")
			r := n.Execute(context.Background(), "a", cmd(t, n, "touch "+file), false)
			_, e := os.Stat(file)
			if at == 1 && (r.Started || e == nil) {
				t.Fatal(r)
			}
			if at == 2 && (!r.Started || r.HistoryStatus != "failed" || e != nil) {
				t.Fatal(r)
			}
			if r = n.Execute(context.Background(), "b", cmd(t, n, "true"), false); r.Started {
				t.Fatal("executed after history failed")
			}
		})
	}
}
func TestHistoryExcludesOutputAndToken(t *testing.T) {
	n := node(t, "allow")
	n.Execute(context.Background(), "test", cmd(t, n, "echo b3V0cHV0LW5vdC1mb3ItaGlzdG9yeQ== | base64 -d"), false)
	raw, e := os.ReadFile(n.History.Path())
	if e != nil {
		t.Fatal(e)
	}
	var ev history.Event
	lines := []byte(raw)
	_ = json.Unmarshal(lines, &ev)
	if len(raw) == 0 {
		t.Fatal("empty")
	}
	if bytes.Contains(raw, []byte(n.Token)) || bytes.Contains(raw, []byte("output-not-for-history")) {
		t.Fatal("token")
	}
}
