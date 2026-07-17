package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
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

func TestConfigFlagRequired(t *testing.T) {
	for _, key := range []string{"FQCRON_CONFIG", "FQCRON_NATS_URL", "FQCRON_KV_BUCKET"} {
		os.Unsetenv(key)
	}
	if _, err := configFromArgs(nil); err == nil || !strings.Contains(err.Error(), "required") {
		t.Fatalf("expected required error, got %v", err)
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
