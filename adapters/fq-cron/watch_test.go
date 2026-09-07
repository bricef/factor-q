package main

import (
	"context"
	"log"
	"os"
	"path/filepath"
	"sort"
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
			initial := mustLoad(t, path)

			logs := &syncBuffer{}
			w := NewConfigWatcher(path, initial, ConfigWatcherOptions{
				PollInterval:    20 * time.Millisecond,
				Settle:          20 * time.Millisecond,
				DisableFSNotify: pollOnly,
				Logger:          log.New(logs, "", 0),
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

			// Each rejection is asserted by the reason it carries, not by a
			// count of the shared "rejected" prefix: a count cannot tell one
			// cause from another, and the reasons are what the operator reads.
			if err := os.Remove(path); err != nil {
				t.Fatal(err)
			}
			waitForLog(t, logs, "config reload rejected: open", 2*time.Second)
			writeConfig(t, path, "not = [valid")
			waitForLog(t, logs, "config reload rejected: parse TOML", 2*time.Second)
			writeConfig(t, path, configText("third", "0 * * * *"))
			e = waitReload(t, events)
			if e.Diff.Removed[0] != "second" || e.Diff.Added[0] != "third" {
				t.Fatalf("old config was not retained: %+v", e.Diff)
			}
		})
	}
}

// The watcher starts from the configuration that is actually running, not
// from a read of its own. fq-cron loads the file, then waits for the broker —
// minutes, during an outage — and only then builds the watcher. A second read
// there seeded lastSeen with the *new* bytes while the scheduler kept running
// the old ones, so a write made inside that window was invisible: no diff, no
// log line, nothing until the file was written again
// (https://github.com/bricef/factor-q/issues/634).
func TestWatcherIsSeededFromTheConfigThatIsRunning(t *testing.T) {
	// startupWindow is the gap the defect lives in: load the file, then let
	// `written` be whatever the operator leaves on disk while fq-cron is
	// still connecting to the broker. It returns the watcher fq-cron would
	// build, seeded with what it is actually running, and its log.
	startupWindow := func(t *testing.T, written string) (*ConfigWatcher, *syncBuffer) {
		t.Helper()
		path := filepath.Join(t.TempDir(), "fq-cron.toml")
		writeConfig(t, path, configText("first", "0 * * * *"))
		running := mustLoad(t, path)
		writeConfig(t, path, written)
		logs := &syncBuffer{}
		return NewConfigWatcher(path, running, ConfigWatcherOptions{
			Settle: time.Millisecond,
			Logger: log.New(logs, "", 0),
		}), logs
	}

	t.Run("a write inside the window lands on the first check", func(t *testing.T) {
		w, _ := startupWindow(t, configText("first", "0 * * * *")+configText("second", "0 * * * *"))
		event, ok := w.Check()
		if !ok {
			t.Fatal("the first check missed a config written while fq-cron was starting")
		}
		if len(event.Diff.Added) != 1 || event.Diff.Added[0] != "second" || len(event.Diff.Removed) != 0 || len(event.Diff.Changed) != 0 {
			t.Fatalf("first-check diff = %+v, want second added and nothing else", event.Diff)
		}
		if names := jobNames(w.current); names != "first,second" {
			t.Fatalf("running config = %q, want both jobs", names)
		}
	})

	t.Run("an untouched file is not a reload", func(t *testing.T) {
		w, logs := startupWindow(t, configText("first", "0 * * * *"))
		if event, ok := w.Check(); ok {
			t.Fatalf("a file nobody touched was reloaded: %+v", event.Diff)
		}
		if strings.Contains(logs.String(), "config reload") {
			t.Fatalf("an unchanged file must say nothing; log was:\n%s", logs.String())
		}
	})

	// The seed composes with the reload rule rather than bypassing it: a first
	// check that lands in a writer's truncate gap is refused like any other
	// (#623), and the configuration loaded at startup keeps running.
	t.Run("a torn write inside the window is refused, not applied", func(t *testing.T) {
		w, logs := startupWindow(t, "")
		if event, ok := w.Check(); ok {
			t.Fatalf("a torn write was accepted: %+v", event.Diff)
		}
		if names := jobNames(w.current); names != "first" {
			t.Fatalf("running config = %q, want the one loaded at startup (%q)", names, "first")
		}
		if !strings.Contains(logs.String(), "declaring no jobs") {
			t.Fatalf("the refusal must say why; log was:\n%s", logs.String())
		}
	})
}

// A read landing between a writer's truncate and its write sees zero bytes,
// which TOML parses as a perfectly valid config with no jobs at all. The
// watcher used to accept that as "every job deleted"; it must refuse every
// shape of it and leave the running config alone
// (https://github.com/bricef/factor-q/issues/623).
func TestReloadRefusesAConfigThatDeclaresNoJobs(t *testing.T) {
	for name, text := range map[string]string{
		"truncated to zero bytes": "",
		"whitespace only":         "  \n\t\n",
		"comments only":           "# every job commented out\n",
		"header but no jobs":      "[limits]\nmax_fires_per_hour = 30\n",
	} {
		t.Run(name, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "fq-cron.toml")
			writeConfig(t, path, configText("first", "0 * * * *"))
			logs := &syncBuffer{}
			w := NewConfigWatcher(path, mustLoad(t, path), ConfigWatcherOptions{
				Settle: time.Millisecond,
				Logger: log.New(logs, "", 0),
			})

			writeConfig(t, path, text)
			if event, ok := w.Check(); ok {
				t.Fatalf("a config declaring no jobs was accepted: %+v", event.Diff)
			}
			if names := jobNames(w.current); names != "first" {
				t.Fatalf("running config = %q, want the previous one (%q)", names, "first")
			}
			if !strings.Contains(logs.String(), "declaring no jobs") {
				t.Fatalf("the refusal must say why; log was:\n%s", logs.String())
			}

			// And the watcher keeps watching: the completed write lands.
			writeConfig(t, path, configText("first", "0 * * * *")+configText("second", "0 * * * *"))
			event, ok := w.Check()
			if !ok || len(event.Diff.Added) != 1 || event.Diff.Added[0] != "second" || len(event.Diff.Removed) != 0 {
				t.Fatalf("the write after the refusal = %+v (accepted=%v)", event.Diff, ok)
			}
		})
	}
}

// `job = []` is the one way a file says "no jobs" out loud, and it is
// honoured in full: the jobs stop and their fire ledgers go with them.
func TestReloadAcceptsAnExplicitlyEmptyJobList(t *testing.T) {
	s := startScheduler(t, configText("alpha", "0 4 1 1 *")+configText("beta", "0 4 1 1 *"))
	defer s.stop(t)

	writeConfig(t, s.path, "job = []\n")
	waitForLog(t, s.logs, "config reload accepted: added=[] removed=[alpha beta]", 2*time.Second)
	waitForStateKeys(t, s.store, "", 2*time.Second)
}

// The same two jobs, the same store, and the intermediate state of a save
// written on purpose: nothing may be removed, and nothing accepted.
func TestATruncatedReadKeepsEveryJobsState(t *testing.T) {
	s := startScheduler(t, configText("alpha", "0 4 1 1 *")+configText("beta", "0 4 1 1 *"))
	defer s.stop(t)

	writeConfig(t, s.path, "")
	waitForLog(t, s.logs, "config reload rejected: 0 bytes declaring no jobs", 2*time.Second)
	if keys := stateKeys(s.store); keys != "alpha,beta" {
		t.Fatalf("state after a truncated read = %q, want both ledgers intact", keys)
	}
	if strings.Contains(s.logs.String(), "config reload accepted") {
		t.Fatalf("nothing may be accepted; log was:\n%s", s.logs.String())
	}

	// The completed save that follows is accepted, and still removes nothing.
	writeConfig(t, s.path, configText("alpha", "30 4 1 1 *")+configText("beta", "0 4 1 1 *"))
	waitForLog(t, s.logs, "config reload accepted: added=[] removed=[] changed=[alpha]", 2*time.Second)
	if keys := stateKeys(s.store); keys != "alpha,beta" {
		t.Fatalf("state after the completed save = %q, want both ledgers intact", keys)
	}
}

// A multi-step save is one reload of the finished file, not one per step —
// on the poll path as much as on the fsnotify one.
func TestWritesInsideTheSettleWindowProduceOneReload(t *testing.T) {
	for _, pollOnly := range []bool{false, true} {
		t.Run(map[bool]string{false: "fsnotify", true: "poll-only"}[pollOnly], func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "fq-cron.toml")
			writeConfig(t, path, configText("first", "0 * * * *"))
			w := NewConfigWatcher(path, mustLoad(t, path), ConfigWatcherOptions{
				PollInterval:    20 * time.Millisecond,
				Settle:          200 * time.Millisecond,
				DisableFSNotify: pollOnly,
				Logger:          log.New(&syncBuffer{}, "", 0),
			})
			ctx, cancel := context.WithCancel(context.Background())
			defer cancel()
			events := w.Run(ctx)

			writeConfig(t, path, configText("second", "0 * * * *"))
			time.Sleep(20 * time.Millisecond)
			writeConfig(t, path, configText("third", "0 * * * *"))

			if got := jobNames(waitReload(t, events).Config); got != "third" {
				t.Fatalf("the settled reload carried %q, want the last write (%q)", got, "third")
			}
			select {
			case extra := <-events:
				t.Fatalf("a second reload followed the same save: %+v", extra.Diff)
			case <-time.After(500 * time.Millisecond):
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

// runningScheduler is the production path a reload takes to the fire ledgers
// it can delete: a real ConfigWatcher over a real file, feeding runScheduler
// over an in-memory store seeded with one ledger per job.
type runningScheduler struct {
	path   string
	store  *MemoryStateStore
	logs   *syncBuffer
	cancel context.CancelFunc
	done   chan error
}

func startScheduler(t *testing.T, text string) *runningScheduler {
	t.Helper()
	path := filepath.Join(t.TempDir(), "fq-cron.toml")
	writeConfig(t, path, text)
	running := mustLoad(t, path)
	config := running.Config
	store := NewMemoryStateStore()
	for _, job := range config.Jobs {
		// A ledger with a recent slot: nothing to catch up, nothing to fire
		// for a year, so only a reload can move this scheduler.
		store.States[job.Name] = FireState{LastScheduled: time.Now(), PublishedAt: time.Now()}
	}
	logs := &syncBuffer{}
	logger := log.New(logs, "", 0)
	watcher := NewConfigWatcher(path, running, ConfigWatcherOptions{
		PollInterval: 20 * time.Millisecond,
		Settle:       20 * time.Millisecond,
		Logger:       logger,
	})
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan error, 1)
	go func() {
		done <- runScheduler(ctx, config, watcher.Run(ctx), &MemoryPublisher{}, store, logger)
	}()
	return &runningScheduler{path: path, store: store, logs: logs, cancel: cancel, done: done}
}

func (s *runningScheduler) stop(t *testing.T) {
	t.Helper()
	s.cancel()
	select {
	case err := <-s.done:
		if err != nil {
			t.Fatalf("runScheduler = %v, want a clean stop", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatalf("runScheduler did not stop on cancellation; log was:\n%s", s.logs.String())
	}
}

func stateKeys(store *MemoryStateStore) string {
	store.mu.RLock()
	defer store.mu.RUnlock()
	names := make([]string, 0, len(store.States))
	for name := range store.States {
		names = append(names, name)
	}
	sort.Strings(names)
	return strings.Join(names, ",")
}

func waitForStateKeys(t *testing.T, store *MemoryStateStore, want string, within time.Duration) {
	t.Helper()
	deadline := time.Now().Add(within)
	for {
		got := stateKeys(store)
		if got == want {
			return
		}
		if time.Now().After(deadline) {
			t.Fatalf("state keys = %q after %s, want %q", got, within, want)
		}
		time.Sleep(10 * time.Millisecond)
	}
}

func jobNames(config *Config) string {
	names := make([]string, 0, len(config.Jobs))
	for _, job := range config.Jobs {
		names = append(names, job.Name)
	}
	sort.Strings(names)
	return strings.Join(names, ",")
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

func mustLoad(t *testing.T, path string) *LoadedConfig {
	t.Helper()
	loaded, err := LoadConfig(path)
	if err != nil {
		t.Fatal(err)
	}
	return loaded
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
