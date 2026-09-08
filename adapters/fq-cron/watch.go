package main

import (
	"context"
	"crypto/sha256"
	"fmt"
	"log"
	"os"
	"os/signal"
	"path/filepath"
	"reflect"
	"sort"
	"sync"
	"syscall"
	"time"

	"github.com/fsnotify/fsnotify"
)

const DefaultConfigPollInterval = 30 * time.Second

// DefaultReloadSettle is how long a changed config file must hold still
// before the watcher reads it as final. Low hundreds of milliseconds: long
// enough to span a save's truncate-then-write, an editor's temp-file rename,
// or the last chunks of an `scp`; short enough that an edit still applies
// while the operator is looking at the log.
const DefaultReloadSettle = 250 * time.Millisecond

// ConfigDiff describes a wholesale, validated configuration change.
type ConfigDiff struct {
	Added   []string
	Removed []string
	Changed []string
}

// ReloadEvent is emitted only for accepted reloads. Config remains owned by
// the watcher and must be treated as read-only.
type ReloadEvent struct {
	Config *Config
	Diff   ConfigDiff
}

type ConfigWatcherOptions struct {
	PollInterval time.Duration
	// Settle is the quiet period a changed file must hold before what was
	// read counts as the whole file. One number does two jobs: fsnotify
	// write bursts are coalesced for this long, and every changed read —
	// from any trigger, the poll included — is confirmed by a second read
	// taken Settle later, with only byte-identical reads accepted.
	Settle          time.Duration
	DisableFSNotify bool
	Logger          *log.Logger
}

// ConfigWatcher watches one configuration file. Polling is the correctness
// mechanism; fsnotify only reduces latency.
type ConfigWatcher struct {
	path    string
	opts    ConfigWatcherOptions
	mu      sync.Mutex
	current *Config
	// seeded reports whether lastSeen holds a reading at all. "Nothing has
	// been seen yet" is a state of its own rather than a particular
	// fileSignature value: making it one would rest on which signatures
	// happen to be unreachable, and a later change to fileSignature's fields
	// could quietly turn "unseeded" into "already saw an empty file".
	seeded   bool
	lastSeen fileSignature
	// unsettledLogged keeps a file that is being written continuously to
	// one log line per streak rather than one per check.
	unsettledLogged bool
}

// checkOutcome is what one examination of the file concluded.
type checkOutcome int

const (
	// checkUnchanged: the file reads the same as the last one seen.
	checkUnchanged checkOutcome = iota
	// checkUnsettled: the file moved between the two reads, so nothing was
	// concluded and nothing recorded — it must be looked at again.
	checkUnsettled
	// checkRejected: the file was read whole and refused; the running
	// config stands.
	checkRejected
	// checkAccepted: a new config is in force and an event was produced.
	checkAccepted
)

type fileSignature struct {
	hash    [sha256.Size]byte
	exists  bool
	readErr string
}

// NewConfigWatcher watches path, starting from the configuration that is
// already running and from the bytes it was loaded from. It does not read the
// file: seeding lastSeen from a read of its own would make the first Check a
// comparison against whatever the file holds *now* rather than against what
// the scheduler is running. fq-cron loads the config, then waits for the
// broker — minutes, during an outage — and only then gets here, so a file
// rewritten inside that window would otherwise be invisible until something
// wrote it again (https://github.com/bricef/factor-q/issues/634).
//
// running must be non-nil; a watcher with no running configuration has
// nothing to compare against, and the alternative to the panic is a first
// check that reports every job in the file as added — a wrong answer given
// quietly, where this one is loud and immediate at startup.
//
// running.Raw may be nil, for a caller that has no bytes to offer — a config
// built in memory by a test. Nothing has been seen then, and the first Check
// treats whatever the file holds as a change.
func NewConfigWatcher(path string, running *LoadedConfig, opts ConfigWatcherOptions) *ConfigWatcher {
	if opts.PollInterval <= 0 {
		opts.PollInterval = DefaultConfigPollInterval
	}
	if opts.Settle <= 0 {
		opts.Settle = DefaultReloadSettle
	}
	if opts.Logger == nil {
		opts.Logger = log.Default()
	}
	w := &ConfigWatcher{path: path, current: running.Config, opts: opts}
	if running.Raw != nil {
		w.lastSeen, w.seeded = signature(running.Raw, nil), true
	}
	return w
}

// Run starts the watcher and returns accepted reloads. The channel closes
// when ctx is cancelled.
func (w *ConfigWatcher) Run(ctx context.Context) <-chan ReloadEvent {
	out := make(chan ReloadEvent)
	go w.run(ctx, out)
	return out
}

func (w *ConfigWatcher) run(ctx context.Context, out chan<- ReloadEvent) {
	defer close(out)
	poll := time.NewTicker(w.opts.PollInterval)
	defer poll.Stop()

	hup := make(chan os.Signal, 1)
	signal.Notify(hup, syscall.SIGHUP)
	defer signal.Stop(hup)

	events, errors, closeAccelerator := w.accelerator()
	defer closeAccelerator()

	var settle <-chan time.Time
	var timer *time.Timer
	arm := func() {
		if timer == nil {
			timer = time.NewTimer(w.opts.Settle)
		} else {
			if !timer.Stop() {
				select {
				case <-timer.C:
				default:
				}
			}
			timer.Reset(w.opts.Settle)
		}
		settle = timer.C
	}
	// A file caught mid-write concluded nothing, so come back for it after
	// another settle rather than waiting out a whole poll interval.
	examine := func() {
		if w.emitIfChanged(ctx, out) == checkUnsettled {
			arm()
		}
	}
	for {
		select {
		case <-ctx.Done():
			if timer != nil {
				timer.Stop()
			}
			return
		case <-poll.C:
			examine()
		case <-hup:
			examine()
		case event, ok := <-events:
			if !ok {
				events = nil
				continue
			}
			if filepath.Clean(event.Name) != filepath.Clean(w.path) {
				continue
			}
			arm()
		case <-settle:
			settle = nil
			examine()
		case err, ok := <-errors:
			if ok {
				w.opts.Logger.Printf("config watch accelerator error: %v", err)
			} else {
				errors = nil
			}
		}
	}
}

// accelerator subscribes to the config file's *directory* — editors save by
// renaming over the file, which orphans a watch on its inode. A failure is a
// latency regression, not an outage: the poll is the guarantee.
func (w *ConfigWatcher) accelerator() (<-chan fsnotify.Event, <-chan error, func()) {
	if w.opts.DisableFSNotify {
		return nil, nil, func() {}
	}
	watcher, err := fsnotify.NewWatcher()
	if err != nil {
		w.opts.Logger.Printf("config watch accelerator unavailable: %v", err)
		return nil, nil, func() {}
	}
	if err := watcher.Add(filepath.Dir(w.path)); err != nil {
		w.opts.Logger.Printf("config watch accelerator unavailable: %v", err)
		_ = watcher.Close()
		return nil, nil, func() {}
	}
	return watcher.Events, watcher.Errors, func() { _ = watcher.Close() }
}

func (w *ConfigWatcher) emitIfChanged(ctx context.Context, out chan<- ReloadEvent) checkOutcome {
	event, outcome := w.check(ctx)
	if outcome != checkAccepted {
		return outcome
	}
	select {
	case out <- event:
	case <-ctx.Done():
	}
	return outcome
}

// Check immediately examines the file. It returns the configuration the
// watcher holds as current — the one in force from its point of view, whether
// or not the reload that put it there has been taken off the channel yet — and
// an event for a reload this look accepted, if it did: see check. ctx bounds
// the settle, so a caller shutting down is not held for it.
//
// The configuration is returned on every path deliberately. A caller that only
// acted on the event would, at exactly the wrong moment, decide on a copy one
// reload behind: the watcher's own poll may have accepted a config and be
// blocked handing it over while the caller asks, and to the watcher that file
// is then "unchanged" (#635).
func (w *ConfigWatcher) Check(ctx context.Context) (*Config, ReloadEvent, bool) {
	event, outcome := w.check(ctx)
	if outcome == checkAccepted {
		return event.Config, event, true
	}
	w.mu.Lock()
	defer w.mu.Unlock()
	return w.current, ReloadEvent{}, false
}

// check reads the file and decides. A reload is accepted only when the file
// reads, holds still across the settle, parses, validates, and declares at
// least one job — or declares `job = []`, the one way a config says "no jobs"
// out loud. Anything else leaves the running config in force.
func (w *ConfigWatcher) check(ctx context.Context) (ReloadEvent, checkOutcome) {
	// The settle wait is held under the lock deliberately: two concurrent
	// checks must not interleave their reads of the same file.
	w.mu.Lock()
	defer w.mu.Unlock()

	data, err := os.ReadFile(w.path)
	sig := signature(data, err)
	if w.seeded && sig == w.lastSeen {
		return ReloadEvent{}, checkUnchanged
	}
	// The file differs from the last one seen — but a writer may be part
	// way through it: os.WriteFile truncates before it writes, an editor
	// renames over a temp file, scp streams. Read it again after the settle
	// and trust only bytes that did not move.
	if !waitFor(ctx, w.opts.Settle) {
		return ReloadEvent{}, checkUnchanged // shutting down
	}
	confirm, confirmErr := os.ReadFile(w.path)
	if signature(confirm, confirmErr) != sig {
		if !w.unsettledLogged {
			w.opts.Logger.Printf("config changed while being read; reload deferred until it settles")
			w.unsettledLogged = true
		}
		return ReloadEvent{}, checkUnsettled
	}
	w.unsettledLogged = false
	w.lastSeen, w.seeded = sig, true
	if err != nil {
		w.opts.Logger.Printf("config reload rejected: %v", err)
		return ReloadEvent{}, checkRejected
	}

	next, declaresJobs, err := parseConfig(data)
	if err != nil {
		w.opts.Logger.Printf("config reload rejected: %v", err)
		return ReloadEvent{}, checkRejected
	}
	// Zero bytes are valid TOML declaring no jobs, so a read that lands in
	// a writer's truncate gap parses as "every job deleted" — and the
	// scheduler would delete every job's fire ledger to match. A config only
	// means that when it says so (#623).
	if len(next.Jobs) == 0 && !declaresJobs {
		w.opts.Logger.Printf("config reload rejected: %d bytes declaring no jobs, and no explicit `job = []`", len(data))
		return ReloadEvent{}, checkRejected
	}
	diff := diffConfigs(w.current, next)
	w.current = next
	w.opts.Logger.Printf("config reload accepted: added=%v removed=%v changed=%v", diff.Added, diff.Removed, diff.Changed)
	return ReloadEvent{Config: next, Diff: diff}, checkAccepted
}

// waitFor waits out d, reporting false if ctx ended first.
func waitFor(ctx context.Context, d time.Duration) bool {
	timer := time.NewTimer(d)
	defer timer.Stop()
	select {
	case <-ctx.Done():
		return false
	case <-timer.C:
		return true
	}
}

func signature(data []byte, err error) fileSignature {
	if err != nil {
		return fileSignature{readErr: fmt.Sprintf("%T: %v", err, err)}
	}
	return fileSignature{hash: sha256.Sum256(data), exists: true}
}

func diffConfigs(old, next *Config) ConfigDiff {
	oldJobs := jobsByName(old)
	newJobs := jobsByName(next)
	var d ConfigDiff
	for name, job := range newJobs {
		oldJob, exists := oldJobs[name]
		if !exists {
			d.Added = append(d.Added, name)
		} else if !reflect.DeepEqual(oldJob, job) {
			d.Changed = append(d.Changed, name)
		}
	}
	for name := range oldJobs {
		if _, exists := newJobs[name]; !exists {
			d.Removed = append(d.Removed, name)
		}
	}
	sort.Strings(d.Added)
	sort.Strings(d.Removed)
	sort.Strings(d.Changed)
	return d
}

func jobsByName(config *Config) map[string]Job {
	jobs := make(map[string]Job)
	if config != nil {
		for _, job := range config.Jobs {
			jobs[job.Name] = job
		}
	}
	return jobs
}
