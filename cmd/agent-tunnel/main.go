package main

import (
	"agent-tunnel/internal/config"
	"agent-tunnel/internal/mcpserver"
	"agent-tunnel/internal/relay"
	"agent-tunnel/internal/target"
	"agent-tunnel/internal/tunnel"
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"log"
	"net"
	"net/http"
	"os"
	"os/signal"
	"sync"
	"syscall"
	"time"
)

var version = "0.1.0-dev"

func main() {
	log.SetFlags(log.LstdFlags | log.LUTC)
	if e := run(os.Args[1:]); e != nil {
		fmt.Fprintln(os.Stderr, "agent-tunnel:", e)
		os.Exit(1)
	}
}
func run(args []string) error {
	mcpserver.Version = version
	if len(args) == 0 {
		return errors.New("usage: agent-tunnel target|relay|version; use target -h or relay -h")
	}
	if args[0] == "version" || args[0] == "--version" {
		fmt.Println("agent-tunnel", version)
		return nil
	}
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()
	switch args[0] {
	case "target":
		return runTarget(ctx, args[1:])
	case "relay":
		return runRelay(ctx, args[1:])
	default:
		return errors.New("expected target, relay, or version")
	}
}
func runTarget(ctx context.Context, args []string) error {
	c := config.DefaultTarget()
	f := flag.NewFlagSet("target", flag.ContinueOnError)
	f.StringVar(&c.Mode, "mode", "", "required: allow, review, strict")
	f.StringVar(&c.Transport, "transport", "", "required: quick or relay")
	f.StringVar(&c.Name, "name", "", "display name (default hostname)")
	f.StringVar(&c.RelayURL, "relay-url", "", "relay http(s) origin")
	f.StringVar(&c.Shell, "shell", c.Shell, "shell absolute path")
	f.StringVar(&c.LogDir, "log-dir", c.LogDir, "command history root directory")
	f.StringVar(&c.Cloudflared, "cloudflared", "", "existing cloudflared executable path")
	f.DurationVar(&c.TTL, "ttl", c.TTL, "token validity, maximum 24h")
	f.DurationVar(&c.Timeout, "command-timeout", c.Timeout, "per-command timeout, maximum 30m")
	f.IntVar(&c.Concurrency, "concurrency", c.Concurrency, "simultaneous commands, 1..32")
	if e := f.Parse(args); e != nil {
		if errors.Is(e, flag.ErrHelp) {
			return nil
		}
		return e
	}
	if f.NArg() != 0 {
		return errors.New("unexpected positional arguments")
	}
	c.RegistrationKey = os.Getenv("AGENT_TUNNEL_REGISTRATION_KEY")
	if e := c.Validate(); e != nil {
		return e
	}
	if c.Transport == "quick" {
		path, e := tunnel.Find(c.Cloudflared)
		if e != nil {
			return e
		}
		c.Cloudflared = path
	}
	n, e := target.New(c)
	if e != nil {
		return e
	}
	owner, stop := context.WithCancel(context.Background())
	accessDone := make(chan error, 1)
	done := accessDone
	managerStarted := false
	var server *http.Server
	var shutdownOnce sync.Once
	var shutdownErr error
	shutdown := func() {
		shutdownOnce.Do(func() {
			budget, cancel := context.WithTimeout(context.Background(), 10*time.Second)
			defer cancel()
			shutdownErr = n.Stop(budget)
			stop()
			if server != nil {
				server.Close()
			}
			if managerStarted {
				select {
				case <-accessDone:
				case <-budget.Done():
					if shutdownErr == nil {
						shutdownErr = budget.Err()
					}
					log.Print("access shutdown not confirmed")
				}
			}
		})
	}
	defer shutdown()
	handler := mcpserver.New(n)
	var once sync.Once
	ready := make(chan struct{})
	notify := func(endpoint string) {
		info := n.Info()
		info["event"] = "ready"
		info["mcp_url"] = endpoint
		data, _ := json.Marshal(info)
		fmt.Println(string(data))
		once.Do(func() { close(ready) })
	}
	if c.Transport == "relay" {
		client, e := relay.NewClient(n, handler, c.RelayURL, c.RegistrationKey, notify)
		if e != nil {
			return e
		}
		managerStarted = true
		go func() { accessDone <- client.Run(owner); close(accessDone) }()
	} else {
		listener, e := net.Listen("tcp", "127.0.0.1:0")
		if e != nil {
			return e
		}
		server = &http.Server{Handler: handler, ReadHeaderTimeout: 5 * time.Second, MaxHeaderBytes: 16 << 10, ReadTimeout: 10 * time.Second, IdleTimeout: time.Minute}
		go server.Serve(listener)
		defer server.Close()
		manager := &tunnel.Manager{Node: n, Binary: c.Cloudflared, Origin: "http://" + listener.Addr().String(), Ready: notify}
		managerStarted = true
		go func() { accessDone <- manager.Run(owner); close(accessDone) }()
	}
	initial := time.NewTimer(30 * time.Second)
	defer initial.Stop()
	select {
	case <-ready:
	case e := <-done:
		stop()
		if e == nil {
			return errors.New("access ended before ready")
		}
		return e
	case <-initial.C:
		stop()
		return errors.New("initial access did not become ready within 30 seconds")
	case <-ctx.Done():
		stop()
		return nil
	}
	// Expiry can close the access manager, but must not terminate the target.
	for {
		select {
		case <-ctx.Done():
			shutdown()
			return shutdownErr
		case err := <-done:
			if err != nil && !errors.Is(err, context.Canceled) {
				stop()
				return err
			}
			done = nil
		}
	}
}
func runRelay(ctx context.Context, args []string) error {
	f := flag.NewFlagSet("relay", flag.ContinueOnError)
	address := f.String("listen", "0.0.0.0:8080", "public listen address (HTTP is unencrypted)")
	if e := f.Parse(args); e != nil {
		if errors.Is(e, flag.ErrHelp) {
			return nil
		}
		return e
	}
	if f.NArg() != 0 {
		return errors.New("unexpected positional arguments")
	}
	r, e := relay.New(os.Getenv("AGENT_TUNNEL_REGISTRATION_KEY"))
	if e != nil {
		return e
	}
	defer r.Close()
	listener, e := net.Listen("tcp", *address)
	if e != nil {
		return e
	}
	server := &http.Server{Handler: r, ReadHeaderTimeout: 5 * time.Second, ReadTimeout: 10 * time.Second, MaxHeaderBytes: 16 << 10, IdleTimeout: time.Minute}
	done := make(chan error, 1)
	go func() { done <- server.Serve(listener) }()
	fmt.Printf("relay listening on %s (HTTP/WS is unencrypted; registration key not displayed)\n", listener.Addr())
	select {
	case err := <-done:
		return err
	case <-ctx.Done():
		r.Close()
		shutdown, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		return server.Shutdown(shutdown)
	}
}
