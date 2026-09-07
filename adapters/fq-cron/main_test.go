package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestConfigFromArgsDefaultsAndEnvironment(t *testing.T) {
	t.Setenv("FQCRON_CONFIG", "jobs.toml")
	t.Setenv("FQCRON_NATS_URL", "nats://example:4222")
	t.Setenv("FQCRON_KV_BUCKET", "jobs-state")
	cfg, err := configFromArgs([]string{"--check"})
	if err != nil {
		t.Fatal(err)
	}
	if cfg.ConfigPath != "jobs.toml" || cfg.NATSURL != "nats://example:4222" || cfg.KVBucket != "jobs-state" || !cfg.Check {
		t.Fatalf("unexpected config: %+v", cfg)
	}
}

// The settle is configuration like every other number: a default, a flag,
// and an environment variable, with a mistyped value an error rather than a
// silent fallback to the default.
func TestReloadSettleIsConfigurable(t *testing.T) {
	t.Setenv("FQCRON_CONFIG", "jobs.toml")
	cfg, err := configFromArgs(nil)
	if err != nil || cfg.ReloadSettle != DefaultReloadSettle {
		t.Fatalf("default settle = %s (%v), want %s", cfg.ReloadSettle, err, DefaultReloadSettle)
	}
	t.Setenv(reloadSettleEnv, "750ms")
	if cfg, err = configFromArgs(nil); err != nil || cfg.ReloadSettle != 750*time.Millisecond {
		t.Fatalf("settle from the environment = %s (%v)", cfg.ReloadSettle, err)
	}
	if cfg, err = configFromArgs([]string{"--reload-settle", "2s"}); err != nil || cfg.ReloadSettle != 2*time.Second {
		t.Fatalf("settle from the flag = %s (%v), and the flag must win over the environment", cfg.ReloadSettle, err)
	}
	t.Setenv(reloadSettleEnv, "soon")
	if _, err = configFromArgs(nil); err == nil || !strings.Contains(err.Error(), reloadSettleEnv) {
		t.Fatalf("a malformed %s = %v, want an error naming it", reloadSettleEnv, err)
	}
	t.Setenv(reloadSettleEnv, "250ms")
	if _, err = configFromArgs([]string{"--reload-settle", "-1s"}); err == nil || !strings.Contains(err.Error(), "negative") {
		t.Fatalf("a negative settle = %v, want a rejection", err)
	}
}

func TestConfigFlagRequired(t *testing.T) {
	for _, key := range []string{"FQCRON_CONFIG", "FQCRON_NATS_URL", "FQCRON_KV_BUCKET"} {
		os.Unsetenv(key)
	}
	if _, err := configFromArgs(nil); err == nil || !strings.Contains(err.Error(), "required") {
		t.Fatalf("expected required error, got %v", err)
	}
}

// --version answers without a config and without touching the broker:
// the deploy script and the image check run it on a binary that has
// neither, and the flag set would otherwise reject it as undefined
// (which is exactly what `just docker-check` found on 2026-09-04).
func TestVersionFlagNeedsNoConfig(t *testing.T) {
	for _, key := range []string{"FQCRON_CONFIG", "FQCRON_NATS_URL", "FQCRON_KV_BUCKET"} {
		os.Unsetenv(key)
	}
	for _, args := range [][]string{{"--version"}, {"-version"}, {"--version", "--config", "missing.toml"}} {
		if err := run(args); err != nil {
			t.Fatalf("run(%v) = %v, want nil", args, err)
		}
	}
	if v := buildVersion(); v == "" {
		t.Fatal("buildVersion() must never be empty")
	}
}

func TestCheckMode(t *testing.T) {
	path := filepath.Join(t.TempDir(), "jobs.toml")
	if err := os.WriteFile(path, []byte(validConfig), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := run([]string{"--check", "--config", path}); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte("not TOML"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := run([]string{"--check", "--config", path}); err == nil {
		t.Fatal("expected invalid config to fail")
	}
}

func TestProbeFlagNeedsNoConfig(t *testing.T) {
	// Nothing listens on this port, so the probe fails — but with a
	// connection error, not "--config is required".
	t.Setenv(healthBindEnv, "127.0.0.1:1")
	err := run([]string{"--probe"})
	if err == nil {
		t.Fatal("expected the probe to fail against a closed port")
	}
	if strings.Contains(err.Error(), "--config") {
		t.Fatalf("--probe was parsed as a normal run: %v", err)
	}
}
