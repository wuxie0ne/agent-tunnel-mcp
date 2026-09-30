//go:build linux

package executor

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"time"
	"unicode/utf8"

	"agent-tunnel/internal/config"
	"golang.org/x/sys/unix"
)

type Input struct {
	ShellCommand   *string  `json:"shell_command,omitempty"`
	Program        string   `json:"program,omitempty"`
	Args           []string `json:"args,omitempty"`
	Cwd            string   `json:"cwd,omitempty"`
	MaxOutputBytes int      `json:"max_output_bytes,omitempty"`
}
type Spec struct {
	Input
	Shell   string        `json:"shell"`
	Timeout time.Duration `json:"timeout_ns"`
}

func Normalize(in Input, cwd, shell string, timeout time.Duration) (Spec, error) {
	if (in.ShellCommand != nil) == (in.Program != "") {
		return Spec{}, errors.New("exactly one of shell_command or program is required")
	}
	if in.ShellCommand != nil && len(in.Args) > 0 {
		return Spec{}, errors.New("args cannot accompany shell_command")
	}
	n := len(in.Program)
	if in.ShellCommand != nil {
		n += len(*in.ShellCommand)
	}
	if len(in.Args) > 1024 {
		return Spec{}, errors.New("too many arguments")
	}
	values := append([]string{in.Program}, in.Args...)
	if in.ShellCommand != nil {
		values = append(values, *in.ShellCommand)
	}
	for _, v := range values {
		if strings.ContainsRune(v, 0) {
			return Spec{}, errors.New("NUL in command")
		}
	}
	for _, v := range in.Args {
		n += len(v)
	}
	if n > config.MaxCommand {
		return Spec{}, errors.New("command exceeds 64 KiB")
	}
	if in.Cwd == "" {
		in.Cwd = cwd
	}
	if !filepath.IsAbs(in.Cwd) || strings.ContainsRune(in.Cwd, 0) {
		return Spec{}, errors.New("cwd must be absolute")
	}
	info, err := os.Stat(in.Cwd)
	if err != nil || !info.IsDir() {
		return Spec{}, errors.New("cwd is not an accessible directory")
	}
	in.Cwd = filepath.Clean(in.Cwd)
	if in.MaxOutputBytes == 0 {
		in.MaxOutputBytes = config.DefaultOutput
	}
	if in.MaxOutputBytes < 1 || in.MaxOutputBytes > config.MaxOutput {
		return Spec{}, errors.New("max_output_bytes must be 1..262144")
	}
	in.Args = append([]string(nil), in.Args...)
	if in.ShellCommand != nil {
		v := *in.ShellCommand
		in.ShellCommand = &v
	}
	return Spec{Input: in, Shell: shell, Timeout: timeout}, nil
}

type Result struct {
	RequestID              string    `json:"request_id,omitempty"`
	Status                 string    `json:"status"`
	Code                   string    `json:"code,omitempty"`
	Message                string    `json:"message,omitempty"`
	Started                bool      `json:"started"`
	ExitCode               *int      `json:"exit_code,omitempty"`
	Signal                 string    `json:"signal,omitempty"`
	TimedOut               bool      `json:"timed_out"`
	Stdout                 string    `json:"stdout"`
	Stderr                 string    `json:"stderr"`
	StdoutCaptureTruncated bool      `json:"stdout_capture_truncated"`
	StderrCaptureTruncated bool      `json:"stderr_capture_truncated"`
	StdoutReturnTruncated  bool      `json:"stdout_return_truncated"`
	StderrReturnTruncated  bool      `json:"stderr_return_truncated"`
	StdoutEncodingReplaced bool      `json:"stdout_encoding_replaced"`
	StderrEncodingReplaced bool      `json:"stderr_encoding_replaced"`
	StdoutCaptured         int       `json:"stdout_captured_bytes"`
	StderrCaptured         int       `json:"stderr_captured_bytes"`
	CleanupStatus          string    `json:"cleanup_status"`
	HistoryStatus          string    `json:"history_status"`
	StartTime              time.Time `json:"start_time,omitempty"`
	EndTime                time.Time `json:"end_time"`
	DurationMS             int64     `json:"duration_ms"`
}

func Failure(code, msg string) Result {
	return Result{Status: "rejected", Code: code, Message: msg, CleanupStatus: "not_needed", HistoryStatus: "not_applicable"}
}

// Metadata omits output bodies, including when encoding/return truncation occurs.
func (r Result) Metadata() map[string]any {
	return map[string]any{"status": r.Status, "code": r.Code, "message": r.Message, "started": r.Started, "exit_code": r.ExitCode, "signal": r.Signal, "timed_out": r.TimedOut, "start_time": r.StartTime, "end_time": r.EndTime, "duration_ms": r.DurationMS, "cleanup_status": r.CleanupStatus, "stdout_captured_bytes": r.StdoutCaptured, "stderr_captured_bytes": r.StderrCaptured, "stdout_capture_truncated": r.StdoutCaptureTruncated, "stderr_capture_truncated": r.StderrCaptureTruncated, "stdout_return_truncated": r.StdoutReturnTruncated, "stderr_return_truncated": r.StderrReturnTruncated}
}

type capture struct {
	mu        sync.Mutex
	data      []byte
	truncated bool
}

func (c *capture) Write(p []byte) (int, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	n := len(p)
	remaining := config.CaptureLimit - len(c.data)
	if n > remaining {
		c.truncated = true
	}
	if remaining > n {
		remaining = n
	}
	c.data = append(c.data, p[:remaining]...)
	return n, nil
}
func (c *capture) snapshot() ([]byte, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	return append([]byte(nil), c.data...), c.truncated
}
func cut(s string, n int) string {
	if len(s) <= n {
		return s
	}
	s = s[:n]
	for !utf8.ValidString(s) && len(s) > 0 {
		s = s[:len(s)-1]
	}
	return s
}
func output(r *Result, a, b *capture, budget int) {
	x, xt := a.snapshot()
	y, yt := b.snapshot()
	r.StdoutCaptured = len(x)
	r.StderrCaptured = len(y)
	r.StdoutCaptureTruncated = xt
	r.StderrCaptureTruncated = yt
	r.StdoutEncodingReplaced = !utf8.Valid(x)
	r.StderrEncodingReplaced = !utf8.Valid(y)
	xs := strings.ToValidUTF8(string(x), "�")
	ys := strings.ToValidUTF8(string(y), "�")
	na := budget / 2
	nb := budget - na
	if len(xs) < na {
		nb += na - len(xs)
		na = len(xs)
	}
	if len(ys) < nb {
		na += nb - len(ys)
		nb = len(ys)
	}
	r.Stdout = cut(xs, na)
	r.Stderr = cut(ys, nb)
	r.StdoutReturnTruncated = len(r.Stdout) < len(xs)
	r.StderrReturnTruncated = len(r.Stderr) < len(ys)
}

type Runner struct{ TermGrace, DrainGrace time.Duration }

func DefaultRunner() Runner { return Runner{TermGrace: 2 * time.Second, DrainGrace: time.Second} }
func (runner Runner) Run(owner context.Context, s Spec) Result {
	r := Result{Status: "start_failed", Code: "START_FAILED", CleanupStatus: "not_needed", HistoryStatus: "pending"}
	var cmd *exec.Cmd
	if s.ShellCommand != nil {
		cmd = exec.Command(s.Shell, "-c", *s.ShellCommand)
	} else {
		cmd = exec.Command(s.Program, s.Args...)
	}
	cmd.Dir = s.Cwd
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	outR, outW, err := os.Pipe()
	if err != nil {
		r.Message = err.Error()
		return r
	}
	defer outR.Close()
	errR, errW, err := os.Pipe()
	if err != nil {
		outW.Close()
		r.Message = err.Error()
		return r
	}
	defer errR.Close()
	cmd.Stdout = outW
	cmd.Stderr = errW
	if err = cmd.Start(); err != nil {
		outW.Close()
		errW.Close()
		r.Message = err.Error()
		r.EndTime = time.Now().UTC()
		return r
	}
	outW.Close()
	errW.Close()
	r.Started = true
	start := time.Now()
	r.StartTime = start.UTC()
	r.Status = "exited"
	r.Code = ""
	var a, b capture
	drains := make(chan struct{})
	go func() {
		var wg sync.WaitGroup
		wg.Add(2)
		go func() { defer wg.Done(); io.Copy(&a, outR) }()
		go func() { defer wg.Done(); io.Copy(&b, errR) }()
		wg.Wait()
		close(drains)
	}()
	// Observe without reaping: the reserved leader PID prevents group-ID reuse
	// while timeout/shutdown signals are still possible.
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
	timer := time.NewTimer(s.Timeout)
	defer timer.Stop()
	terminated := false
	var observeErr error
	select {
	case observeErr = <-observed:
	case <-timer.C:
		terminated = true
		r.TimedOut = true
		r.Status = "timed_out"
		r.Code = "COMMAND_TIMEOUT"
	case <-owner.Done():
		terminated = true
		r.Status = "terminated"
		r.Code = "TARGET_STOPPED"
	}
	if terminated {
		r.CleanupStatus = "unconfirmed"
		unix.Kill(-cmd.Process.Pid, unix.SIGTERM)
		grace := time.NewTimer(runner.TermGrace)
		<-grace.C
		unix.Kill(-cmd.Process.Pid, unix.SIGKILL)
		select {
		case observeErr = <-observed:
		case <-time.After(runner.DrainGrace):
			r.Status = "unknown"
			r.Code = "EXECUTION_UNKNOWN"
			r.Message = "unable to confirm process exit"
			go func() { <-observed; cmd.Wait() }()
			output(&r, &a, &b, s.MaxOutputBytes)
			r.EndTime = time.Now().UTC()
			r.DurationMS = time.Since(start).Milliseconds()
			return r
		}
		r.CleanupStatus = "leader_exited_descendants_not_guaranteed"
	}
	if observeErr != nil {
		r.CleanupStatus = "unconfirmed"
		r.Code = "EXECUTION_UNKNOWN"
		r.Status = "unknown"
		r.Message = fmt.Sprintf("observe process: %v", observeErr)
	}
	waitErr := cmd.Wait()
	if cmd.ProcessState != nil {
		code := cmd.ProcessState.ExitCode()
		if code >= 0 {
			r.ExitCode = &code
		}
		if ws, ok := cmd.ProcessState.Sys().(syscall.WaitStatus); ok && ws.Signaled() {
			r.Signal = ws.Signal().String()
		}
	} else if waitErr != nil {
		r.Message = waitErr.Error()
	}
	select {
	case <-drains:
	case <-time.After(runner.DrainGrace):
		outR.Close()
		errR.Close()
		<-drains
		r.CleanupStatus = "output_pipe_closed_descendants_not_guaranteed"
	}
	output(&r, &a, &b, s.MaxOutputBytes)
	r.EndTime = time.Now().UTC()
	r.DurationMS = time.Since(start).Milliseconds()
	return r
}
