// Package history stores local command events, never command output or credentials.
package history

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sync"
	"time"
)

type Sink interface {
	Write(Event) error
	Err() error
	Path() string
	Close() error
}
type Event struct {
	Version    int       `json:"version"`
	Event      string    `json:"event"`
	Time       time.Time `json:"time"`
	InstanceID string    `json:"instance_id"`
	RequestID  string    `json:"request_id"`
	Data       any       `json:"data"`
}

type Writer struct {
	mu            sync.Mutex
	dir, instance string
	file          *os.File
	size, limit   int64
	keep          int
	unhealthy     error
	closed        bool
}

func Open(root, instance string) (*Writer, error) { return open(root, instance, 10<<20, 5) }
func open(root, instance string, limit int64, keep int) (*Writer, error) {
	if instance == "" || filepath.Base(instance) != instance || instance == "." || instance == ".." {
		return nil, errors.New("invalid history instance")
	}
	if err := os.MkdirAll(root, 0700); err != nil {
		return nil, err
	}
	info, err := os.Lstat(root)
	if err != nil {
		return nil, err
	}
	if !info.IsDir() || info.Mode().Perm()&0022 != 0 {
		return nil, errors.New("history root must be a real directory not writable by group/others")
	}
	dir := filepath.Join(root, instance)
	if err := os.Mkdir(dir, 0700); err != nil {
		return nil, fmt.Errorf("create history instance: %w", err)
	}
	file, err := os.OpenFile(filepath.Join(dir, "commands.jsonl"), os.O_CREATE|os.O_EXCL|os.O_WRONLY|os.O_APPEND, 0600)
	if err != nil {
		return nil, err
	}
	w := &Writer{dir: dir, instance: instance, file: file, limit: limit, keep: keep}
	if err = file.Sync(); err != nil {
		file.Close()
		return nil, err
	}
	return w, nil
}
func (w *Writer) Path() string { return filepath.Join(w.dir, "commands.jsonl") }
func (w *Writer) Err() error   { w.mu.Lock(); defer w.mu.Unlock(); return w.unhealthy }
func (w *Writer) fail(err error) error {
	if w.unhealthy == nil {
		w.unhealthy = fmt.Errorf("command history unavailable: %w", err)
	}
	return w.unhealthy
}
func (w *Writer) Write(e Event) error {
	w.mu.Lock()
	defer w.mu.Unlock()
	if w.unhealthy != nil {
		return w.unhealthy
	}
	if w.closed {
		return w.fail(errors.New("closed"))
	}
	e.Version = 1
	e.Time = time.Now().UTC()
	e.InstanceID = w.instance
	data, err := json.Marshal(e)
	if err != nil {
		return w.fail(err)
	}
	if len(data) > 1<<20 {
		return w.fail(errors.New("history event exceeds 1 MiB"))
	}
	data = append(data, '\n')
	if w.size > 0 && w.size+int64(len(data)) > w.limit {
		if err = w.rotate(); err != nil {
			return w.fail(err)
		}
	}
	n, err := w.file.Write(data)
	if err == nil && n != len(data) {
		err = errors.New("short write")
	}
	if err != nil {
		return w.fail(err)
	}
	w.size += int64(n)
	if err = w.file.Sync(); err != nil {
		return w.fail(err)
	}
	return nil
}
func (w *Writer) rotate() error {
	if err := w.file.Close(); err != nil {
		return err
	}
	old := filepath.Join(w.dir, fmt.Sprintf("commands.%d.jsonl", w.keep-1))
	if err := os.Remove(old); err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}
	for i := w.keep - 2; i >= 1; i-- {
		src := filepath.Join(w.dir, fmt.Sprintf("commands.%d.jsonl", i))
		dst := filepath.Join(w.dir, fmt.Sprintf("commands.%d.jsonl", i+1))
		if err := os.Rename(src, dst); err != nil && !errors.Is(err, os.ErrNotExist) {
			return err
		}
	}
	if err := os.Rename(w.Path(), filepath.Join(w.dir, "commands.1.jsonl")); err != nil {
		return err
	}
	file, err := os.OpenFile(w.Path(), os.O_CREATE|os.O_EXCL|os.O_WRONLY|os.O_APPEND, 0600)
	if err != nil {
		return err
	}
	w.file = file
	w.size = 0
	dir, err := os.Open(w.dir)
	if err != nil {
		return err
	}
	defer dir.Close()
	return dir.Sync()
}
func (w *Writer) Close() error {
	w.mu.Lock()
	defer w.mu.Unlock()
	if w.closed {
		return w.unhealthy
	}
	w.closed = true
	if w.file != nil {
		if err := w.file.Close(); err != nil {
			return w.fail(err)
		}
	}
	return w.unhealthy
}
