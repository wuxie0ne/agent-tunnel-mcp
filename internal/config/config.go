package config

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"time"
)

const (
	CaptureLimit    = 1 << 20
	DefaultOutput   = 8 << 10
	MaxOutput       = 256 << 10
	MaxCommand      = 64 << 10
	MaxHTTPBody     = 128 << 10
	MaxHTTPResponse = 2 << 20
	MaxWSMessage    = 4 << 20
	MaxPending      = 8
	RecentRequests  = 256
	ApprovalTTL     = 60 * time.Second
)

type Target struct {
	Mode, Name, Transport, Shell, LogDir, RelayURL, Cloudflared string
	TTL, Timeout                                                time.Duration
	Concurrency                                                 int
	RegistrationKey                                             string
}

func DefaultTarget() Target {
	return Target{TTL: 2 * time.Hour, Timeout: 2 * time.Minute, Concurrency: 2, Shell: "/bin/sh", LogDir: "agent-tunnel-logs"}
}

func (c *Target) Validate() error {
	if c.Mode != "allow" && c.Mode != "review" && c.Mode != "strict" {
		return errors.New("--mode is required: allow, review, or strict")
	}
	if c.Transport != "quick" && c.Transport != "relay" {
		return errors.New("--transport is required: quick or relay")
	}
	if c.TTL <= 0 || c.TTL > 24*time.Hour {
		return errors.New("--ttl must be positive and at most 24h")
	}
	if c.Timeout <= 0 || c.Timeout > 30*time.Minute {
		return errors.New("--command-timeout must be positive and at most 30m")
	}
	if c.Concurrency < 1 || c.Concurrency > 32 {
		return errors.New("--concurrency must be 1..32")
	}
	if c.Shell == "" || !filepath.IsAbs(c.Shell) {
		return errors.New("--shell must be an absolute path")
	}
	if c.LogDir == "" {
		return errors.New("--log-dir must not be empty")
	}
	dir, err := filepath.Abs(c.LogDir)
	if err != nil {
		return err
	}
	c.LogDir = dir
	if c.Transport == "relay" && (c.RelayURL == "" || len(c.RegistrationKey) < 16) {
		return errors.New("relay requires --relay-url and AGENT_TUNNEL_REGISTRATION_KEY (at least 16 characters)")
	}
	if c.Name == "" {
		c.Name, _ = os.Hostname()
	}
	if len(c.Name) > 256 {
		return fmt.Errorf("target name exceeds 256 bytes")
	}
	return nil
}
