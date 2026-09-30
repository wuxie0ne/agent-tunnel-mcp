// Package tunnel owns the external cloudflared child, with no runtime downloads.
package tunnel

import (
	"agent-tunnel/internal/target"
	"bufio"
	"context"
	"errors"
	"fmt"
	"golang.org/x/sys/unix"
	"io"
	"log"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"syscall"
	"time"
)

type Manager struct {
	Node           *target.Node
	Binary, Origin string
	Ready          func(string)
}

func Find(path string) (string, error) {
	if path != "" {
		p, e := exec.LookPath(path)
		if e != nil {
			return "", fmt.Errorf("cloudflared unavailable: %w", e)
		}
		return p, nil
	}
	if executable, e := os.Executable(); e == nil {
		candidate := filepath.Join(filepath.Dir(executable), "cloudflared")
		if info, e := os.Stat(candidate); e == nil && !info.IsDir() && info.Mode()&0111 != 0 {
			return candidate, nil
		}
	}
	p, e := exec.LookPath("cloudflared")
	if e != nil {
		return "", errors.New("Quick Tunnel requires existing cloudflared; no runtime download")
	}
	return p, nil
}

var quickURL = regexp.MustCompile(`https://[a-z0-9-]+\.trycloudflare\.com`)

type diagnostics struct {
	mu    sync.Mutex
	lines []string
}

func (d *diagnostics) add(line string) {
	if len(line) > 1024 {
		line = line[:1024]
	}
	line = regexp.MustCompile(`(?i)(token|secret|password|authorization|key)(["\s]*[:=]["\s]*)[^\s",]+`).ReplaceAllString(line, "${1}${2}<redacted>")
	line = regexp.MustCompile(`([a-zA-Z][a-zA-Z0-9+.-]*://)[^\s/]*@`).ReplaceAllString(line, "${1}<userinfo-redacted>@")
	d.mu.Lock()
	defer d.mu.Unlock()
	d.lines = append(d.lines, line)
	if len(d.lines) > 8 {
		d.lines = d.lines[1:]
	}
}
func (d *diagnostics) tail() string {
	d.mu.Lock()
	defer d.mu.Unlock()
	return strings.Join(d.lines, " | ")
}

type event struct {
	url        string
	registered bool
}

func readOutput(r io.Reader, ch chan<- event, wg *sync.WaitGroup, diag *diagnostics) {
	defer wg.Done()
	scanner := bufio.NewScanner(r)
	scanner.Buffer(make([]byte, 4096), 64<<10)
	for scanner.Scan() {
		line := scanner.Text()
		diag.add(line)
		if strings.Contains(line, " ERR ") || strings.Contains(line, " WRN ") {
			if len(line) > 2048 {
				line = line[:2048]
			}
			line = regexp.MustCompile(`(?i)(token|secret|password|authorization|key)(["\s]*[:=]["\s]*)[^\s",]+`).ReplaceAllString(line, "${1}${2}<redacted>")
			log.Printf("cloudflared diagnostic: %s", line)
		}
		e := event{url: quickURL.FindString(line), registered: strings.Contains(line, "Registered tunnel connection")}
		if e.url != "" || e.registered {
			select {
			case ch <- e:
			default:
			}
		}
	}
	if scanner.Err() != nil {
		io.Copy(io.Discard, r)
	}
}
func (m *Manager) Run(ctx context.Context) error {
	path, e := Find(m.Binary)
	if e != nil {
		return e
	}
	delay := time.Second
	for {
		if m.Node.Expired() {
			return nil
		}
		ready, err := m.once(ctx, path)
		if ctx.Err() != nil {
			return ctx.Err()
		}
		if m.Node.Expired() {
			return nil
		}
		m.Node.SetConnection("reconnecting", "")
		if err != nil {
			log.Printf("cloudflared exited (%v); restarting without replaying commands", err)
		}
		if ready {
			delay = time.Second
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(delay):
		}
		delay *= 2
		if delay > 10*time.Second {
			delay = 10 * time.Second
		}
	}
}
func (m *Manager) once(ctx context.Context, path string) (bool, error) {
	empty, err := os.CreateTemp("", "agent-tunnel-cloudflared-*.yml")
	if err != nil {
		return false, err
	}
	file := empty.Name()
	defer os.Remove(file)
	empty.WriteString("{}\n")
	empty.Close()
	cmd := exec.Command(path, "tunnel", "--config", file, "--no-autoupdate", "--protocol", "http2", "--url", m.Origin)
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	out, e := cmd.StdoutPipe()
	if e != nil {
		return false, e
	}
	errout, e := cmd.StderrPipe()
	if e != nil {
		return false, e
	}
	if e = cmd.Start(); e != nil {
		return false, e
	}
	observed := make(chan error, 1)
	go func() {
		var info unix.Siginfo
		var e error
		for {
			e = unix.Waitid(unix.P_PID, cmd.Process.Pid, &info, unix.WEXITED|unix.WNOWAIT, nil)
			if e != unix.EINTR {
				break
			}
		}
		observed <- e
	}()
	events := make(chan event, 16)
	var diag diagnostics
	var readers sync.WaitGroup
	readers.Add(2)
	go readOutput(out, events, &readers, &diag)
	go readOutput(errout, events, &readers, &diag)
	registered := false
	lastReady := ""
	endpoint := ""
	ready := false
	tick := time.NewTicker(time.Second)
	defer tick.Stop()
	var exitErr error
loop:
	for {
		select {
		case ev := <-events:
			if ev.url != "" {
				u, e := url.Parse(ev.url)
				if e == nil && u.Scheme == "https" && strings.HasSuffix(u.Hostname(), ".trycloudflare.com") {
					endpoint = ev.url
				}
			}
			registered = registered || ev.registered
			if endpoint != "" && registered {
				u, _ := url.Parse(endpoint)
				u.Path = "/mcp"
				q := u.Query()
				q.Set("token", m.Node.Token)
				u.RawQuery = q.Encode()
				if !ready || lastReady != u.String() {
					lastReady = u.String()
					m.Node.SetConnection("ready", u.String())
					if m.Ready != nil {
						m.Ready(u.String())
					}
					ready = true
				}
			}
		case exitErr = <-observed:
			break loop
		case <-ctx.Done():
			unix.Kill(-cmd.Process.Pid, unix.SIGTERM)
			time.Sleep(time.Second)
			unix.Kill(-cmd.Process.Pid, unix.SIGKILL)
			exitErr = <-observed
			break loop
		case <-tick.C:
			if m.Node.Expired() && m.Node.Idle() {
				unix.Kill(-cmd.Process.Pid, unix.SIGTERM)
				time.Sleep(time.Second)
				unix.Kill(-cmd.Process.Pid, unix.SIGKILL)
				exitErr = <-observed
				break loop
			}
		}
	}
	waitErr := cmd.Wait()
	out.Close()
	errout.Close()
	readers.Wait()
	if exitErr != nil {
		return ready, exitErr
	}
	if waitErr != nil {
		return ready, fmt.Errorf("%w: %s", waitErr, diag.tail())
	}
	if !ready {
		return false, fmt.Errorf("exited before ready: %s", diag.tail())
	}
	return ready, nil
}
