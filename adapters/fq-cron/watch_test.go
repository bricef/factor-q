package main

import (
	"context"
	"log"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
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
		_, event, ok := w.Check(context.Background())
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
		if _, event, ok := w.Check(context.Background()); ok {
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
		if _, event, ok := w.Check(context.Background()); ok {
			t.Fatalf("a torn write was accepted: %+v", event.Diff)
		}
		if names := jobNames(w.current); names != "first" {
			t.Fatalf("running config = %q, want the one loaded at startup (%q)", names, "first")
		}
		if !strings.Contains(logs.String(), "declaring no jobs") {
			t.Fatalf("the refusal must say why; log was:\n%s", logs.String())
		}
	})

	// #634's "silently and indefinitely", in full: LoadConfig itself lands in
	// the truncate gap, so fq-cron starts with zero jobs. Startup does not
	// apply the reload rule, so nothing refuses that (#632) — the first check
	// is the only thing that can put the jobs back, and it can only do so by
	// comparing against what was actually loaded.
	t.Run("a startup that read a torn file recovers on the first check", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "fq-cron.toml")
		writeConfig(t, path, "") // the writer's truncate, caught by LoadConfig
		running := mustLoad(t, path)
		if len(running.Config.Jobs) != 0 {
			t.Fatalf("a torn read must load as no jobs, got %q", jobNames(running.Config))
		}
		// A zero-byte file still seeds: os.ReadFile returns empty but non-nil,
		// so this is a watcher that has seen zero bytes, not one that has seen
		// nothing. The distinction is the whole point of the seeded flag.
		if running.Raw == nil {
			t.Fatal("a zero-byte config must still seed the watcher")
		}
		writeConfig(t, path, configText("first", "0 * * * *")) // the writer finishes
		w := NewConfigWatcher(path, running, ConfigWatcherOptions{
			Settle: time.Millisecond,
			Logger: log.New(&syncBuffer{}, "", 0),
		})

		_, event, ok := w.Check(context.Background())
		if !ok || len(event.Diff.Added) != 1 || event.Diff.Added[0] != "first" {
			t.Fatalf("the completed write = %+v (accepted=%v), want first added; "+
				"a watcher that seeded itself would have swallowed it and scheduled nothing indefinitely", event.Diff, ok)
		}
	})

	// A caller with no bytes to offer — a config built in memory — has seen
	// nothing, and the first check treats whatever is on disk as a change.
	// Asserted rather than reasoned about: this is the documented meaning of a
	// nil Raw, and it must not rest on which fileSignature values happen to be
	// unreachable.
	t.Run("a watcher given no bytes treats its first read as a change", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "fq-cron.toml")
		writeConfig(t, path, configText("first", "0 * * * *"))
		// Deliberately the same content the config was parsed from: a seeded
		// watcher would call this unchanged and return nothing. An unseeded
		// one has no reading to call it against, so it must reload.
		unseeded := &LoadedConfig{Config: mustParse(t, configText("first", "0 * * * *"))}
		w := NewConfigWatcher(path, unseeded, ConfigWatcherOptions{
			Settle: time.Millisecond,
			Logger: log.New(&syncBuffer{}, "", 0),
		})

		_, event, ok := w.Check(context.Background())
		if !ok {
			t.Fatal("an unseeded watcher must treat its first read as a change")
		}
		if len(event.Diff.Added) != 0 || len(event.Diff.Removed) != 0 || len(event.Diff.Changed) != 0 {
			t.Fatalf("first-check diff = %+v, want an empty one: the file matches the config, it had simply never been seen", event.Diff)
		}
		// And it is seeded now, so the same file is not a reload twice.
		if _, event, ok := w.Check(context.Background()); ok {
			t.Fatalf("the second check reloaded an untouched file: %+v", event.Diff)
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
			if _, event, ok := w.Check(context.Background()); ok {
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
			_, event, ok := w.Check(context.Background())
			if !ok || len(event.Diff.Added) != 1 || event.Diff.Added[0] != "second" || len(event.Diff.Removed) != 0 {
				t.Fatalf("the write after the refusal = %+v (accepted=%v)", event.Diff, ok)
			}
		})
	}
}

// testRemovalConfirm is the `--removal-confirm` window these tests run with:
// long enough that "before the deadline" and "after it" are distinguishable on
// a loaded machine, short enough to wait out several times in one test.
const testRemovalConfirm = 400 * time.Millisecond

// `job = []` is the one way a file says "no jobs" out loud, and it is honoured
// in full — the jobs stop at once and their fire ledgers go with them. It
// takes the same confirmation window as any other removal, though: one rule,
// no fast path, because a file that says `job = []` is as capable of being
// half-written as any other.
func TestReloadAcceptsAnExplicitlyEmptyJobList(t *testing.T) {
	s := startScheduler(t, configText("alpha", "0 4 1 1 *")+configText("beta", "0 4 1 1 *"), testRemovalConfirm)
	defer s.stop(t)

	removedAt := time.Now()
	writeConfig(t, s.path, "job = []\n")
	waitForLog(t, s.logs, "config reload accepted: added=[] removed=[alpha beta]", 2*time.Second)
	// The jobs stop immediately; the ledgers do not go until the window does.
	if keys := stateKeys(s.store); keys != "alpha,beta" {
		t.Fatalf("state keys = %q the moment `job = []` landed, want both ledgers still parked", keys)
	}
	waitForStateKeys(t, s.store, "", 5*time.Second)
	if elapsed := time.Since(removedAt); elapsed < testRemovalConfirm {
		t.Fatalf("both ledgers were deleted %s after the reload, before the %s window closed", elapsed, testRemovalConfirm)
	}
	if got := strings.Join(s.store.deletions(), ","); got != "alpha,beta" {
		t.Fatalf("deletions = %q, want exactly one per job", got)
	}
}

// The same two jobs, the same store, and the intermediate state of a save
// written on purpose: nothing may be removed, and nothing accepted.
func TestATruncatedReadKeepsEveryJobsState(t *testing.T) {
	s := startScheduler(t, configText("alpha", "0 4 1 1 *")+configText("beta", "0 4 1 1 *"), testRemovalConfirm)
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

// The sibling of TestATruncatedReadKeepsEveryJobsState, one stall later. A
// writer that emits the *first* of two `[[job]]` blocks and then holds still
// for longer than the settle gives the watcher two byte-identical reads of a
// complete, valid, one-job file: the reload is accepted with the second job
// removed, and nothing about the file can say otherwise
// (https://github.com/bricef/factor-q/issues/635).
//
// Everything up to and including that acceptance still happens. What must not
// happen is the deletion: beta stops firing at once, its ledger is parked, the
// rest of the file arrives inside the confirmation window, and beta comes back
// to the history it left with. The proof that it did is that beta fires at
// all — a job whose state was deleted looks new to the planner, which gives it
// its next *future* slot and publishes nothing for an hour.
func TestAStalledWriteKeepsTheDroppedJobsLedger(t *testing.T) {
	alpha := configText("alpha", "0 4 1 1 *")
	beta := func(enabled string) string {
		return "[[job]]\nname = \"beta\"\nschedule = \"0 * * * *\"\nsubject = \"fq.trigger.test\"\n" +
			"catch_up = \"once\"\nenabled = " + enabled + "\n"
	}
	// A ledger with a slot three hours back (so beta has a fire to catch up
	// on the moment it is enabled) and one publication inside the valve's
	// sliding window (so a surviving ledger is visible in what is stored).
	lastSlot := time.Now().Add(-3 * time.Hour)
	inWindow := time.Now().Add(-30 * time.Minute)
	s := startSchedulerSeeded(t, alpha+beta("false"), 3*time.Second, func(store *countingStore, _ *Config) {
		store.States["alpha"] = FireState{LastScheduled: time.Now(), PublishedAt: time.Now()}
		store.States["beta"] = FireState{LastScheduled: lastSlot, PublishedAt: inWindow, RecentFires: []time.Time{inWindow}}
	})
	defer s.stop(t)

	// The stalled write: the first block, then nothing for well over a settle.
	writeConfig(t, s.path, alpha)
	waitForLog(t, s.logs, "config reload accepted: added=[] removed=[beta]", 2*time.Second)
	waitForLog(t, s.logs, "job=beta removed from the configuration", 2*time.Second)
	if keys := stateKeys(s.store); keys != "alpha,beta" {
		t.Fatalf("state keys = %q after the stalled write, want beta's ledger parked, not deleted", keys)
	}

	// The writer finishes, inside the window.
	writeConfig(t, s.path, alpha+beta("true"))
	waitForLog(t, s.logs, "job=beta removal cancelled", 2*time.Second)

	select {
	case job := <-s.published:
		if job != "beta" {
			t.Fatalf("published %q, want beta catching up", job)
		}
	case <-time.After(5 * time.Second):
		t.Fatalf("beta never fired after it came back, so it came back empty; log was:\n%s", s.logs.String())
	}

	state, ok, err := s.store.Get(context.Background(), "beta")
	if err != nil || !ok {
		t.Fatalf("beta's state after it came back: ok=%v err=%v", ok, err)
	}
	if len(state.RecentFires) < 2 || !state.RecentFires[0].Equal(inWindow) {
		t.Fatalf("beta's ledger = %v, want the seeded fire at %s still at its head with the catch-up appended", state.RecentFires, inWindow)
	}
	if got := s.store.deletions(); len(got) != 0 {
		t.Fatalf("Delete was called for %v; a removal cancelled inside the window deletes nothing", got)
	}
}

// The deadline can fire while the watcher is mid-settle on the very write that
// brings the job back. The watcher then accepts it, moves its own notion of
// the config on, and blocks handing the event to the loop — which is busy in
// its deadline recheck. That recheck sees a file identical to what the watcher
// last saw and reports "unchanged", and a loop that judged absence by its own
// copy of the config, one reload behind, would delete the ledger a moment
// before taking the very event that re-adds the job. Absence is judged against
// the watcher's config instead, so the ledger survives (#635, found in the
// review of #641).
//
// The timings put the deadline inside the watcher's settle: poll 20 ms, settle
// 400 ms, window 1 s, and the complete write 850 ms after the job was parked.
func TestADeadlineInsideTheWatchersSettleDoesNotDeleteAReturningJob(t *testing.T) {
	alpha := configText("alpha", "0 4 1 1 *")
	beta := configText("beta", "0 4 1 1 *")
	marker := time.Now().Add(-42 * time.Minute)
	s := startSchedulerTimed(t, alpha+beta, time.Second, 400*time.Millisecond, func(store *countingStore, _ *Config) {
		store.States["alpha"] = FireState{LastScheduled: time.Now(), PublishedAt: time.Now()}
		store.States["beta"] = FireState{LastScheduled: time.Now(), PublishedAt: marker}
	})
	defer s.stop(t)

	// The stalled write parks beta.
	writeConfig(t, s.path, alpha)
	waitForLog(t, s.logs, "job=beta removed from the configuration", 3*time.Second)
	parked := time.Now()

	// The writer finishes 150 ms before the deadline: the watcher picks it up
	// within a poll and is still in its 400 ms settle when the deadline fires.
	time.Sleep(850*time.Millisecond - time.Since(parked))
	writeConfig(t, s.path, alpha+beta)

	switch outcome := waitForEitherLog(t, s.logs, 5*time.Second, "job=beta removal cancelled", "job=beta removal confirmed"); outcome {
	case "job=beta removal cancelled":
	default:
		t.Fatalf("the deadline deleted a job the watcher had already taken back (%q); log was:\n%s", outcome, s.logs.String())
	}
	waitForLog(t, s.logs, "config reload accepted: added=[beta]", 5*time.Second)
	if got := s.store.deletions(); len(got) != 0 {
		t.Fatalf("Delete was called for %v; log was:\n%s", got, s.logs.String())
	}
	after, ok, err := s.store.Get(context.Background(), "beta")
	if err != nil || !ok {
		t.Fatalf("beta's state after it came back: ok=%v err=%v", ok, err)
	}
	if !after.PublishedAt.Equal(marker) {
		t.Fatalf("beta's ledger = %+v, want the one it left with (PublishedAt %s)", after, marker)
	}
}

// waitForEitherLog returns the first of the needles to appear in the log, or
// fails the test when none does within the deadline. For a test whose failure
// mode is a *different* log line — a deletion where a cancellation was due —
// it fails fast and says which one it saw.
func waitForEitherLog(t *testing.T, logs *syncBuffer, within time.Duration, needles ...string) string {
	t.Helper()
	deadline := time.Now().Add(within)
	for {
		text := logs.String()
		for _, needle := range needles {
			if strings.Contains(text, needle) {
				return needle
			}
		}
		if time.Now().After(deadline) {
			t.Fatalf("none of %q appeared within %s; log was:\n%s", needles, within, text)
		}
		time.Sleep(10 * time.Millisecond)
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
	path      string
	store     *countingStore
	published chan string
	logs      *syncBuffer
	cancel    context.CancelFunc
	done      chan error
}

// startScheduler seeds every job with a ledger whose slot is now: nothing to
// catch up, nothing to fire for a year, so only a reload can move it.
// removalConfirm is the real `--removal-confirm` window, shrunk to something a
// test can wait out.
func startScheduler(t *testing.T, text string, removalConfirm time.Duration) *runningScheduler {
	t.Helper()
	return startSchedulerSeeded(t, text, removalConfirm, func(store *countingStore, config *Config) {
		for _, job := range config.Jobs {
			store.States[job.Name] = FireState{LastScheduled: time.Now(), PublishedAt: time.Now()}
		}
	})
}

// startSchedulerSeeded is startScheduler with the store's contents chosen by
// the caller — a test that wants a job to resume from a specific ledger has to
// write that ledger before the loop's first read.
func startSchedulerSeeded(t *testing.T, text string, removalConfirm time.Duration, seed func(*countingStore, *Config)) *runningScheduler {
	t.Helper()
	return startSchedulerTimed(t, text, removalConfirm, 20*time.Millisecond, seed)
}

// startSchedulerTimed is startSchedulerSeeded with the settle chosen too — for
// a test whose point is where the removal deadline lands relative to the
// watcher's own settle.
func startSchedulerTimed(t *testing.T, text string, removalConfirm, settle time.Duration, seed func(*countingStore, *Config)) *runningScheduler {
	t.Helper()
	path := filepath.Join(t.TempDir(), "fq-cron.toml")
	writeConfig(t, path, text)
	running := mustLoad(t, path)
	config := running.Config
	store := &countingStore{MemoryStateStore: NewMemoryStateStore()}
	seed(store, config)
	logs := &syncBuffer{}
	logger := log.New(logs, "", 0)
	watcher := NewConfigWatcher(path, running, ConfigWatcherOptions{
		PollInterval: 20 * time.Millisecond,
		Settle:       settle,
		Logger:       logger,
	})
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan error, 1)
	published := make(chan string, 16)
	go func() {
		done <- runScheduler(ctx, config, watcher.Run(ctx), &namingPublisher{jobs: published}, store,
			removalPolicy{Confirm: removalConfirm, Recheck: watcher.Check}, logger)
	}()
	return &runningScheduler{path: path, store: store, published: published, logs: logs, cancel: cancel, done: done}
}

// namingPublisher reports the name of each job it publishes. MemoryPublisher
// appends to a slice with no lock, which a test goroutine cannot read while
// the loop runs; a channel can be read from anywhere.
type namingPublisher struct{ jobs chan string }

func (p *namingPublisher) Publish(_ context.Context, job, _ string, _ []byte, _ time.Time, _ bool) error {
	select {
	case p.jobs <- job:
	default:
	}
	return nil
}

// countingStore is a MemoryStateStore that records every deletion, so a test
// can assert not merely that a ledger survived but that nothing tried to
// delete it.
type countingStore struct {
	*MemoryStateStore
	// Named apart from the embedded store's own mutex, so `store.mu` in a
	// helper still means the one guarding States.
	deleteMu sync.Mutex
	deleted  []string
}

func (s *countingStore) Delete(ctx context.Context, job string) error {
	s.deleteMu.Lock()
	s.deleted = append(s.deleted, job)
	s.deleteMu.Unlock()
	return s.MemoryStateStore.Delete(ctx, job)
}

// deletions is every job Delete has been called for, in call order.
func (s *countingStore) deletions() []string {
	s.deleteMu.Lock()
	defer s.deleteMu.Unlock()
	return append([]string(nil), s.deleted...)
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

func stateKeys(store *countingStore) string {
	store.mu.RLock()
	defer store.mu.RUnlock()
	names := make([]string, 0, len(store.States))
	for name := range store.States {
		names = append(names, name)
	}
	sort.Strings(names)
	return strings.Join(names, ",")
}

func waitForStateKeys(t *testing.T, store *countingStore, want string, within time.Duration) {
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
