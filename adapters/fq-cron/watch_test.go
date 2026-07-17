package main

import (
	"bytes"
	"context"
	"log"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestConfigWatcherReloadPaths(t *testing.T) {
	for _, pollOnly := range []bool{false, true} {
		t.Run(map[bool]string{false: "fsnotify", true: "poll-only"}[pollOnly], func(t *testing.T) {
			dir := t.TempDir()
			path := filepath.Join(dir, "fq-cron.toml")
			writeConfig(t, path, configText("first", "0 * * * *"))
			initial, err := LoadConfig(path)
			if err != nil {
				t.Fatal(err)
			}

			var logs bytes.Buffer
			w := NewConfigWatcher(path, initial, ConfigWatcherOptions{
				PollInterval:    20 * time.Millisecond,
				Debounce:        5 * time.Millisecond,
				DisableFSNotify: pollOnly,
				Logger:          log.New(&logs, "", 0),
			})
			ctx, cancel := context.WithCancel(context.Background())
			defer cancel()
			events := w.Run(ctx)

			writeConfig(t, path, configText("second", "0 * * * *"))
			e := waitReload(t, events)
			if len(e.Diff.Added) != 1 || e.Diff.Added[0] != "second" || len(e.Diff.Removed) != 1 || e.Diff.Removed[0] != "first" {
				t.Fatalf("plain-write diff = %+v", e.Diff)
			}

			tmp := filepath.Join(dir, "replacement")
			writeConfig(t, tmp, configText("second", "15 * * * *"))
			if err := os.Rename(tmp, path); err != nil {
				t.Fatal(err)
			}
			e = waitReload(t, events)
			if len(e.Diff.Changed) != 1 || e.Diff.Changed[0] != "second" {
				t.Fatalf("atomic-rename diff = %+v", e.Diff)
			}

			if err := os.Remove(path); err != nil {
				t.Fatal(err)
			}
			waitLog(t, &logs, "config reload rejected")
			writeConfig(t, path, "not = [valid")
			waitLogCount(t, &logs, "config reload rejected", 2)
			writeConfig(t, path, configText("third", "0 * * * *"))
			e = waitReload(t, events)
			if e.Diff.Removed[0] != "second" || e.Diff.Added[0] != "third" {
				t.Fatalf("old config was not retained: %+v", e.Diff)
			}
		})
	}
}

func TestDiffConfigsSortedByName(t *testing.T) {
	old := mustParse(t, configText("z", "0 * * * *")+configText("same", "0 * * * *"))
	next := mustParse(t, configText("a", "0 * * * *")+configText("same", "15 * * * *"))
	d := diffConfigs(old, next)
	if strings.Join(d.Added, ",") != "a" || strings.Join(d.Removed, ",") != "z" || strings.Join(d.Changed, ",") != "same" {
		t.Fatalf("diff = %+v", d)
	}
}

func configText(name, schedule string) string {
	return "[[job]]\nname = \"" + name + "\"\nschedule = \"" + schedule + "\"\nsubject = \"fq.trigger.test\"\n"
}

func mustParse(t *testing.T, text string) *Config {
	t.Helper()
	c, err := ParseConfig([]byte(text))
	if err != nil {
		t.Fatal(err)
	}
	return c
}

func writeConfig(t *testing.T, path, text string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(text), 0o600); err != nil {
		t.Fatal(err)
	}
}

func waitReload(t *testing.T, events <-chan ReloadEvent) ReloadEvent {
	t.Helper()
	select {
	case event := <-events:
		return event
	case <-time.After(2 * time.Second):
		t.Fatal("timed out waiting for reload")
		return ReloadEvent{}
	}
}

func waitLog(t *testing.T, logs *bytes.Buffer, want string) {
	t.Helper()
	waitLogCount(t, logs, want, 1)
}

func waitLogCount(t *testing.T, logs *bytes.Buffer, want string, count int) {
	t.Helper()
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if strings.Count(logs.String(), want) >= count {
			return
		}
		time.Sleep(5 * time.Millisecond)
	}
	t.Fatalf("logs did not contain %q %d times: %s", want, count, logs.String())
}
