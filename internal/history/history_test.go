package history

import (
	"encoding/json"
	"os"
	"path/filepath"
	"sync"
	"testing"
)

func TestHistoryConcurrentAndPrivate(t *testing.T) {
	w, e := Open(t.TempDir(), "instance")
	if e != nil {
		t.Fatal(e)
	}
	var wg sync.WaitGroup
	for i := 0; i < 16; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			if e := w.Write(Event{Event: "execution_intent", RequestID: "r", Data: map[string]string{"command": "echo\nhello"}}); e != nil {
				t.Error(e)
			}
		}()
	}
	wg.Wait()
	w.Close()
	f, e := os.Open(w.Path())
	if e != nil {
		t.Fatal(e)
	}
	defer f.Close()
	d := json.NewDecoder(f)
	n := 0
	for d.More() {
		var ev Event
		if e = d.Decode(&ev); e != nil {
			t.Fatal(e)
		}
		if ev.Version != 1 || ev.InstanceID != "instance" {
			t.Fatal(ev)
		}
		n++
	}
	if n != 16 {
		t.Fatal(n)
	}
	info, _ := os.Stat(w.Path())
	if info.Mode().Perm() != 0600 {
		t.Fatal(info.Mode())
	}
}
func TestRotationAndStickyFailure(t *testing.T) {
	w, e := open(t.TempDir(), "i", 180, 3)
	if e != nil {
		t.Fatal(e)
	}
	for i := 0; i < 10; i++ {
		if e = w.Write(Event{Event: "result", Data: "payload"}); e != nil {
			t.Fatal(e)
		}
	}
	files, _ := os.ReadDir(w.dir)
	if len(files) != 3 {
		t.Fatalf("files=%d", len(files))
	}
	w.file.Close()
	if w.Write(Event{Event: "bad"}) == nil || w.Err() == nil {
		t.Fatal("failed closed file accepted")
	}
	if w.Write(Event{Event: "again"}) == nil {
		t.Fatal("not sticky")
	}
}
func TestUnsafeRootAndExistingInstance(t *testing.T) {
	root := t.TempDir()
	link := filepath.Join(root, "link")
	os.Symlink(root, link)
	if _, e := Open(link, "i"); e == nil {
		t.Fatal("symlink root accepted")
	}
	w, e := Open(root, "i")
	if e != nil {
		t.Fatal(e)
	}
	defer w.Close()
	if _, e = Open(root, "i"); e == nil {
		t.Fatal("reused instance")
	}
}
