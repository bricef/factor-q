package main

import (
	"log"
	"sort"
	"time"
)

// DefaultRemovalConfirm is how long a job dropped by a reload keeps its fire
// state before the deletion is carried out.
//
// Two config poll intervals. The settle (`--reload-settle`) defends against a
// read that lands *inside* a writer's truncate gap, by taking a second read
// and trusting only bytes that did not move. What it cannot see is a writer
// that emits the first of two `[[job]]` blocks and then stalls for longer than
// the settle: both reads return the same complete, valid prefix, so the reload
// is accepted with the second job "removed" and its fire ledger — the #612
// valve history included — deleted. The next complete write re-adds the job
// with an empty ledger, so the loss is silent
// (https://github.com/bricef/factor-q/issues/635).
//
// A stall long enough to survive the settle is bounded by how long the writer
// takes, not by anything fq-cron controls, so the answer is not a longer
// settle but a slower *removal*: additions and changes apply at once, and a
// deletion waits long enough for the rest of the file to arrive and be picked
// up by the ordinary poll. Two intervals means at least one whole poll fits
// inside the window whatever the phase, and normally two.
const DefaultRemovalConfirm = 2 * DefaultConfigPollInterval

// removalPolicy is how the loop treats a job that a reload dropped.
//
// Additions and changes apply immediately, and so does a removal — a dropped
// job stops firing the moment the new configuration is in force. What waits is
// the *deletion of its fire state*, because a configuration file that is
// missing a job is not always a configuration that means to lose it.
type removalPolicy struct {
	// Confirm is how long a dropped job's fire state is kept before it is
	// deleted. A job that returns inside the window keeps the ledger it left
	// with; a job still absent at the deadline loses it.
	Confirm time.Duration
	// Recheck, when set, is one fresh look at the configuration file taken at
	// the deadline, so a completed write that has not yet produced a reload
	// event is seen before anything is deleted. An accepted event it returns
	// is applied exactly as one arriving on the reload channel is — which it
	// must be, because the watcher offers each accepted config once, to
	// whichever caller sees it first. ConfigWatcher.Check is the production
	// implementation; nil means "trust the configuration already in force".
	Recheck func() (ReloadEvent, bool)
}

// pendingRemovals is the loop's set of parked deletions: for each job that has
// left the running configuration, the instant its fire state may be deleted.
//
// It is loop-local and touched only from the scheduler goroutine, so it needs
// no lock of its own.
type pendingRemovals struct {
	window   time.Duration
	logger   *log.Logger
	deadline map[string]time.Time
}

func newPendingRemovals(window time.Duration, logger *log.Logger) *pendingRemovals {
	return &pendingRemovals{window: window, logger: logger, deadline: make(map[string]time.Time)}
}

// park records that these jobs have left the configuration. They stop firing
// now — they are no longer in the config the loop plans from — but their state
// survives until the deadline.
func (p *pendingRemovals) park(names []string, now time.Time) {
	for _, name := range names {
		if _, already := p.deadline[name]; already {
			// Removed, briefly restored, removed again without the loop
			// seeing the restoration: the first deadline stands, so a file
			// that flaps cannot postpone the deletion indefinitely.
			continue
		}
		p.deadline[name] = now.Add(p.window)
		p.logger.Printf("job=%s removed from the configuration: it stops firing now, and its fire state is deleted in %s unless it returns", name, p.window)
	}
}

// retain cancels the parked removal of every job the given configuration
// declares. A job that comes back before its deadline keeps the ledger it left
// with, which is the whole point of parking the deletion rather than doing it.
func (p *pendingRemovals) retain(config *Config) {
	if len(p.deadline) == 0 {
		return
	}
	for name := range jobsByName(config) {
		if _, parked := p.deadline[name]; parked {
			delete(p.deadline, name)
			p.logger.Printf("job=%s removal cancelled: back in the configuration before its deadline, fire state kept", name)
		}
	}
}

// next is when the earliest parked removal falls due, or the zero time when
// nothing is parked — which is what timerUntil reads as "never".
func (p *pendingRemovals) next() time.Time {
	var earliest time.Time
	for _, at := range p.deadline {
		if earliest.IsZero() || at.Before(earliest) {
			earliest = at
		}
	}
	return earliest
}

// due is every parked removal whose deadline has passed, in name order. It
// leaves them parked: a deletion interrupted by a shutdown has not happened,
// and must still be counted as unconfirmed. forget is what drops them, once
// their fate is settled.
func (p *pendingRemovals) due(now time.Time) []string {
	var due []string
	for name, at := range p.deadline {
		if !at.After(now) {
			due = append(due, name)
		}
	}
	sort.Strings(due)
	return due
}

// forget drops these parked removals — carried out, or found to be moot.
// Dropping them is also what stops a deadline that has come and gone from
// waking the loop again.
func (p *pendingRemovals) forget(names ...string) {
	for _, name := range names {
		delete(p.deadline, name)
	}
}

// names is every job still parked, in name order.
func (p *pendingRemovals) names() []string {
	parked := make([]string, 0, len(p.deadline))
	for name := range p.deadline {
		parked = append(parked, name)
	}
	sort.Strings(parked)
	return parked
}

// logUnconfirmed says what a shutdown left parked. Nothing is deleted on the
// way out: a scheduler stopped while a save is in flight would otherwise carry
// out exactly the deletion the window exists to prevent, and it has no way to
// tell that shutdown from an orderly one. The key lingers in KV, a job of the
// same name reclaims it, and a name that never returns leaves one stale row.
func (p *pendingRemovals) logUnconfirmed() {
	if len(p.deadline) == 0 {
		return
	}
	p.logger.Printf("stopping with %d unconfirmed removal(s) %v: fire state kept, and reclaimed if a job of the same name returns", len(p.deadline), p.names())
}
