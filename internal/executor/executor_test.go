package executor

import (
	"agent-tunnel/internal/config"
	"context"
	"strings"
	"testing"
	"time"
)

func spec(t *testing.T, command string) Spec {
	t.Helper()
	s, e := Normalize(Input{ShellCommand: &command}, t.TempDir(), "/bin/sh", 2*time.Second)
	if e != nil {
		t.Fatal(e)
	}
	return s
}
func TestExecutionAndStdin(t *testing.T) {
	r := DefaultRunner().Run(context.Background(), spec(t, "printf hello; printf error >&2; read x; exit 7"))
	if !r.Started || r.ExitCode == nil || *r.ExitCode != 7 || r.Stdout != "hello" || r.Stderr != "error" {
		t.Fatalf("%+v", r)
	}
}
func TestDirectAndStartFailure(t *testing.T) {
	s, e := Normalize(Input{Program: "/bin/echo", Args: []string{"a; echo not-shell"}}, t.TempDir(), "/missing", time.Second)
	if e != nil {
		t.Fatal(e)
	}
	r := DefaultRunner().Run(context.Background(), s)
	if r.Stdout != "a; echo not-shell\n" {
		t.Fatal(r)
	}
	s.Program = "/no-program"
	r = DefaultRunner().Run(context.Background(), s)
	if r.Started || r.Code != "START_FAILED" {
		t.Fatal(r)
	}
}
func TestCaptureAndBudget(t *testing.T) {
	s := spec(t, "head -c 1500000 /dev/zero | tr '\\000' x; printf important >&2")
	s.MaxOutputBytes = 100
	r := DefaultRunner().Run(context.Background(), s)
	if !r.StdoutCaptureTruncated || !r.StdoutReturnTruncated || r.Stderr != "important" || len(r.Stdout)+len(r.Stderr) != 100 {
		t.Fatalf("%+v", r.Metadata())
	}
}
func TestTimeoutAndDetachedPipe(t *testing.T) {
	runner := Runner{TermGrace: 20 * time.Millisecond, DrainGrace: 50 * time.Millisecond}
	s := spec(t, "sleep 5")
	s.Timeout = 20 * time.Millisecond
	r := runner.Run(context.Background(), s)
	if !r.TimedOut || !r.Started {
		t.Fatal(r)
	}
	start := time.Now()
	r = runner.Run(context.Background(), spec(t, "sleep 0.2 & printf done"))
	if r.Stdout != "done" || time.Since(start) > time.Second {
		t.Fatal(r)
	}
}
func TestInvalidInput(t *testing.T) {
	s := "echo test"
	cases := []Input{{}, {ShellCommand: &s, Program: "echo"}, {ShellCommand: &s, Args: []string{"x"}}, {Program: "x\x00"}, {Program: "echo", Cwd: "relative"}, {Program: "echo", MaxOutputBytes: config.MaxOutput + 1}, {Program: strings.Repeat("a", config.MaxCommand+1)}}
	for _, in := range cases {
		if _, e := Normalize(in, t.TempDir(), "/bin/sh", time.Second); e == nil {
			t.Fatalf("accepted %+v", in)
		}
	}
}
func TestUTF8AndSplit(t *testing.T) {
	var a, b capture
	a.Write([]byte{0xff, 'x'})
	b.Write([]byte("中文错误"))
	r := Result{}
	output(&r, &a, &b, 8)
	if !r.StdoutEncodingReplaced || len(r.Stdout)+len(r.Stderr) > 8 {
		t.Fatal(r)
	}
}
