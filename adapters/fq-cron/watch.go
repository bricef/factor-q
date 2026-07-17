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
	PollInterval    time.Duration
	Debounce        time.Duration
	DisableFSNotify bool
	Logger          *log.Logger
}

// ConfigWatcher watches one configuration file. Polling is the correctness
// mechanism; fsnotify only reduces latency.
type ConfigWatcher struct {
	path     string
	opts     ConfigWatcherOptions
	mu       sync.Mutex
	current  *Config
	lastSeen fileSignature
}

type fileSignature struct {
	hash    [sha256.Size]byte
	exists  bool
	readErr string
}

func NewConfigWatcher(path string, current *Config, opts ConfigWatcherOptions) *ConfigWatcher {
	if opts.PollInterval <= 0 {
		opts.PollInterval = DefaultConfigPollInterval
	}
	if opts.Debounce <= 0 {
		opts.Debounce = 100 * time.Millisecond
	}
	if opts.Logger == nil {
		opts.Logger = log.Default()
	}
	w := &ConfigWatcher{path: path, current: current, opts: opts}
	if data, err := os.ReadFile(path); err == nil {
		w.lastSeen = signature(data, nil)
	} else {
		w.lastSeen = signature(nil, err)
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

	var events <-chan fsnotify.Event
	var errors <-chan error
	var watcher *fsnotify.Watcher
	if !w.opts.DisableFSNotify {
		var err error
		watcher, err = fsnotify.NewWatcher()
		if err != nil {
			w.opts.Logger.Printf("config watch accelerator unavailable: %v", err)
		} else if err = watcher.Add(filepath.Dir(w.path)); err != nil {
			w.opts.Logger.Printf("config watch accelerator unavailable: %v", err)
			_ = watcher.Close()
			watcher = nil
		} else {
			events, errors = watcher.Events, watcher.Errors
			defer watcher.Close()
		}
	}

	var debounce <-chan time.Time
	var timer *time.Timer
	for {
		select {
		case <-ctx.Done():
			if timer != nil {
				timer.Stop()
			}
			return
		case <-poll.C:
			w.emitIfChanged(out)
		case <-hup:
			w.emitIfChanged(out)
		case event, ok := <-events:
			if !ok {
				events = nil
				continue
			}
			if filepath.Clean(event.Name) != filepath.Clean(w.path) {
				continue
			}
			if timer == nil {
				timer = time.NewTimer(w.opts.Debounce)
			} else {
				if !timer.Stop() {
					select {
					case <-timer.C:
					default:
					}
				}
				timer.Reset(w.opts.Debounce)
			}
			debounce = timer.C
		case <-debounce:
			debounce = nil
			w.emitIfChanged(out)
		case err, ok := <-errors:
			if ok {
				w.opts.Logger.Printf("config watch accelerator error: %v", err)
			} else {
				errors = nil
			}
		}
	}
}

func (w *ConfigWatcher) emitIfChanged(out chan<- ReloadEvent) {
	if event, ok := w.Check(); ok {
		out <- event
	}
}

// Check immediately examines the file. It returns an event only when a new,
// valid configuration differs in content from the last observed file.
func (w *ConfigWatcher) Check() (ReloadEvent, bool) {
	w.mu.Lock()
	defer w.mu.Unlock()

	data, err := os.ReadFile(w.path)
	sig := signature(data, err)
	if sig == w.lastSeen {
		return ReloadEvent{}, false
	}
	w.lastSeen = sig
	if err != nil {
		w.opts.Logger.Printf("config reload rejected: %v", err)
		return ReloadEvent{}, false
	}

	next, err := ParseConfig(data)
	if err != nil {
		w.opts.Logger.Printf("config reload rejected: %v", err)
		return ReloadEvent{}, false
	}
	diff := diffConfigs(w.current, next)
	w.current = next
	w.opts.Logger.Printf("config reload accepted: added=%v removed=%v changed=%v", diff.Added, diff.Removed, diff.Changed)
	return ReloadEvent{Config: next, Diff: diff}, true
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
