package main

import (
	"context"
	"errors"
	"io"
	"log"
	"strings"
	"sync"
	"testing"
	"time"
)

type orderedStore struct {
	state map[string]FireState
	order *[]string
}

func (s *orderedStore) Get(context.Context, string) (FireState, bool, error) {
	return FireState{}, false, nil
}
func (s *orderedStore) Put(_ context.Context, job string, state FireState) error {
	*s.order = append(*s.order, "record")
	if s.state == nil {
		s.state = make(map[string]FireState)
	}
	s.state[job] = state
	return nil
}
func (s *orderedStore) Delete(context.Context, string) error { return nil }

type orderedPublisher struct {
	order *[]string
	err   error
}

func (p *orderedPublisher) Publish(context.Context, string, string, []byte, time.Time, bool) error {
	*p.order = append(*p.order, "publish")
	return p.err
}

func TestPublishThenRecordOrdering(t *testing.T) {
	order := []string{}
	store := &orderedStore{order: &order}
	publisher := &orderedPublisher{order: &order}
	fire := Fire{Job: "job", Subject: "cron.test", ScheduledAt: time.Now()}
	job := Job{Name: "job", Schedule: "@every 1m", TZ: "UTC", Durable: boolPtr(true)}
	if err := publishWithBackoff(context.Background(), publisher, fire, job, log.New(io.Discard, "", 0)); err != nil {
		t.Fatal(err)
	}
	if err := store.Put(context.Background(), fire.Job, FireState{LastScheduled: fire.ScheduledAt}); err != nil {
		t.Fatal(err)
	}
	if len(order) != 2 || order[0] != "publish" || order[1] != "record" {
		t.Fatalf("side-effect order = %v, want [publish record]", order)
	}
}

// Once the valve trips, the scheduler used to wait only on a config
// reload — a burst silenced it until someone edited the file. It must now
// re-plan by itself when the window slides.
//
// The window is a real hour, so the test starts with the job's only fire
// published an hour ago all but 300 ms: the valve is shut when the loop
// starts and opens 300 ms later, with no reload and nothing else to wake
// the loop.
func TestSchedulerRefiresWhenTheValveWindowSlides(t *testing.T) {
	now := time.Now()
	store := NewMemoryStateStore()
	store.States["catch-up"] = FireState{
		LastScheduled: now.Add(-3 * time.Hour),
		PublishedAt:   now.Add(-time.Hour + 300*time.Millisecond),
	}
	config := &Config{
		Limits: Limits{MaxFiresPerHour: 1},
		Jobs: []Job{{
			Name: "catch-up", Schedule: "0 * * * *", Subject: "cron.valve",
			TZ: "UTC", CatchUp: "once", Enabled: boolPtr(true), Durable: boolPtr(false),
		}},
	}

	published := make(chan time.Time, 1)
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	logs := &syncBuffer{}
	done := make(chan error, 1)
	go func() {
		done <- runScheduler(ctx, config, nil, &signallingPublisher{at: published}, store, removalPolicy{}, log.New(logs, "", 0))
	}()

	select {
	case at := <-published:
		if waited := at.Sub(now); waited < 250*time.Millisecond {
			t.Fatalf("fired %s in, before the window slid: the valve was never shut", waited)
		}
	case <-time.After(10 * time.Second):
		t.Fatalf("the valve never reopened; log was:\n%s", logs.String())
	}
	if !strings.Contains(logs.String(), "valve=open") {
		t.Errorf("the reopening should be logged; log was:\n%s", logs.String())
	}

	cancel()
	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("runScheduler = %v, want a clean stop", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("runScheduler did not stop on cancellation")
	}
}

// The valve can only count fires if the loop records them, so a published
// fire must reach the job's ledger and not just its `published_at`.
func TestSchedulerRecordsEachFireInTheLedger(t *testing.T) {
	now := time.Now()
	store := NewMemoryStateStore()
	store.States["catch-up"] = FireState{LastScheduled: now.Add(-3 * time.Hour)}
	config := &Config{
		Limits: Limits{MaxFiresPerHour: 5},
		Jobs: []Job{{
			Name: "catch-up", Schedule: "0 * * * *", Subject: "cron.valve",
			TZ: "UTC", CatchUp: "once", Enabled: boolPtr(true), Durable: boolPtr(false),
		}},
	}

	published := make(chan time.Time, 1)
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	done := make(chan error, 1)
	go func() {
		done <- runScheduler(ctx, config, nil, &signallingPublisher{at: published}, store, removalPolicy{}, log.New(io.Discard, "", 0))
	}()

	select {
	case <-published:
	case <-time.After(10 * time.Second):
		t.Fatal("the catch-up fire never happened")
	}
	// The state write follows the publish with no cancellation point
	// between them, so a clean stop means the record has landed.
	cancel()
	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("runScheduler = %v, want a clean stop", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("runScheduler did not stop on cancellation")
	}

	state, ok, err := store.Get(context.Background(), "catch-up")
	if err != nil || !ok {
		t.Fatalf("state after the fire: ok=%v err=%v", ok, err)
	}
	if len(state.RecentFires) != 1 || !state.RecentFires[0].Equal(state.PublishedAt) {
		t.Fatalf("ledger = %v with published_at %s; want the one fire recorded", state.RecentFires, state.PublishedAt)
	}
}

// signallingPublisher reports the time of its first publish.
type signallingPublisher struct {
	at   chan time.Time
	once sync.Once
}

func (p *signallingPublisher) Publish(context.Context, string, string, []byte, time.Time, bool) error {
	p.once.Do(func() { p.at <- time.Now() })
	return nil
}

// removalHarness runs runScheduler over a hand-fed reload channel: no file and
// no watcher, so a reload lands exactly when the test says it does and the
// only clock that matters is the confirmation window.
type removalHarness struct {
	reloads chan ReloadEvent
	store   *countingStore
	logs    *syncBuffer
	window  time.Duration
	cancel  context.CancelFunc
	done    chan error
}

// startRemovalHarness seeds one ledger per named job. The schedule fires next
// New Year, so nothing but a reload or a removal deadline moves the loop.
func startRemovalHarness(t *testing.T, window time.Duration, jobs ...string) *removalHarness {
	t.Helper()
	var text string
	for _, name := range jobs {
		text += configText(name, "0 4 1 1 *")
	}
	config := mustParse(t, text)
	store := &countingStore{MemoryStateStore: NewMemoryStateStore()}
	for _, name := range jobs {
		store.States[name] = FireState{LastScheduled: time.Now(), PublishedAt: time.Now()}
	}
	logs := &syncBuffer{}
	reloads := make(chan ReloadEvent)
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan error, 1)
	go func() {
		done <- runScheduler(ctx, config, reloads, &MemoryPublisher{}, store,
			removalPolicy{Confirm: window}, log.New(logs, "", 0))
	}()
	return &removalHarness{reloads: reloads, store: store, logs: logs, window: window, cancel: cancel, done: done}
}

// reload delivers one accepted reload, diffed against what came before it
// exactly as the watcher would have diffed it.
func (h *removalHarness) reload(t *testing.T, previous, next *Config) {
	t.Helper()
	select {
	case h.reloads <- ReloadEvent{Config: next, Diff: diffConfigs(previous, next)}:
	case <-time.After(5 * time.Second):
		t.Fatalf("the scheduler never took the reload; log was:\n%s", h.logs.String())
	}
}

func (h *removalHarness) stop(t *testing.T) {
	t.Helper()
	h.cancel()
	select {
	case err := <-h.done:
		if err != nil {
			t.Fatalf("runScheduler = %v, want a clean stop", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatalf("runScheduler did not stop on cancellation; log was:\n%s", h.logs.String())
	}
}

// A job that leaves the configuration and returns inside the window keeps the
// ledger it left with. Nothing is deleted, then or later — the parked removal
// is cancelled outright rather than merely postponed.
func TestARemovedJobThatReturnsInsideTheWindowKeepsItsState(t *testing.T) {
	both := mustParse(t, configText("alpha", "0 4 1 1 *")+configText("beta", "0 4 1 1 *"))
	alphaOnly := mustParse(t, configText("alpha", "0 4 1 1 *"))
	window := 500 * time.Millisecond
	h := startRemovalHarness(t, window, "alpha", "beta")
	defer h.stop(t)
	before, _, _ := h.store.Get(context.Background(), "beta")

	h.reload(t, both, alphaOnly)
	waitForLog(t, h.logs, "job=beta removed from the configuration", 2*time.Second)
	h.reload(t, alphaOnly, both)
	waitForLog(t, h.logs, "job=beta removal cancelled", 2*time.Second)

	// Well past the deadline the removal would have had.
	time.Sleep(window + 300*time.Millisecond)
	if got := h.store.deletions(); len(got) != 0 {
		t.Fatalf("Delete was called for %v after the removal was cancelled", got)
	}
	after, ok, err := h.store.Get(context.Background(), "beta")
	if err != nil || !ok {
		t.Fatalf("beta's state: ok=%v err=%v", ok, err)
	}
	if !after.LastScheduled.Equal(before.LastScheduled) || !after.PublishedAt.Equal(before.PublishedAt) {
		t.Fatalf("beta's state = %+v, want the one it left with (%+v)", after, before)
	}
}

// A job that stays out is deleted — once, and not before the window closes.
func TestARemovalThatStandsDeletesTheStateAfterTheWindow(t *testing.T) {
	both := mustParse(t, configText("alpha", "0 4 1 1 *")+configText("beta", "0 4 1 1 *"))
	alphaOnly := mustParse(t, configText("alpha", "0 4 1 1 *"))
	window := 600 * time.Millisecond
	h := startRemovalHarness(t, window, "alpha", "beta")
	defer h.stop(t)

	removedAt := time.Now()
	h.reload(t, both, alphaOnly)
	waitForLog(t, h.logs, "job=beta removed from the configuration", 2*time.Second)

	// Half way through the window, the ledger is still there.
	time.Sleep(window / 2)
	if got := h.store.deletions(); len(got) != 0 {
		t.Fatalf("Delete was called for %v with the window still open", got)
	}
	waitForStateKeys(t, h.store, "alpha", 5*time.Second)
	if elapsed := time.Since(removedAt); elapsed < window {
		t.Fatalf("beta was deleted %s after the reload, before the %s window closed", elapsed, window)
	}
	if got := strings.Join(h.store.deletions(), ","); got != "beta" {
		t.Fatalf("deletions = %q, want exactly one Delete of beta", got)
	}
	waitForLog(t, h.logs, "job=beta removal confirmed after "+window.String(), 2*time.Second)
}

// A scheduler stopped while a removal is parked deletes nothing. It cannot
// tell an orderly shutdown from one that lands mid-save, and a deletion is the
// one thing it cannot take back.
func TestShutdownWithARemovalPendingDeletesNothing(t *testing.T) {
	both := mustParse(t, configText("alpha", "0 4 1 1 *")+configText("beta", "0 4 1 1 *"))
	alphaOnly := mustParse(t, configText("alpha", "0 4 1 1 *"))
	// Long enough that the window cannot close by itself during the test.
	h := startRemovalHarness(t, time.Minute, "alpha", "beta")

	h.reload(t, both, alphaOnly)
	waitForLog(t, h.logs, "job=beta removed from the configuration", 2*time.Second)
	h.stop(t)

	if got := h.store.deletions(); len(got) != 0 {
		t.Fatalf("Delete was called for %v on the way out", got)
	}
	if keys := stateKeys(h.store); keys != "alpha,beta" {
		t.Fatalf("state keys after shutdown = %q, want both kept", keys)
	}
	if !strings.Contains(h.logs.String(), "stopping with 1 unconfirmed removal(s) [beta]") {
		t.Fatalf("the shutdown must say what it left behind; log was:\n%s", h.logs.String())
	}
}

// The deadline's last look at the file is what closes the gap between the poll
// that dropped a job and the deadline itself: a write that completed in
// between has produced no reload event yet, and without the recheck the loop
// would delete a job the file on disk still declares.
func TestTheDeadlineRechecksTheFileBeforeDeleting(t *testing.T) {
	both := mustParse(t, configText("alpha", "0 4 1 1 *")+configText("beta", "0 4 1 1 *"))
	alphaOnly := mustParse(t, configText("alpha", "0 4 1 1 *"))
	store := &countingStore{MemoryStateStore: NewMemoryStateStore()}
	for _, name := range []string{"alpha", "beta"} {
		store.States[name] = FireState{LastScheduled: time.Now(), PublishedAt: time.Now()}
	}
	logs := &syncBuffer{}
	reloads := make(chan ReloadEvent)
	// The recheck answers once, with the complete file the watcher has not
	// delivered yet — exactly what ConfigWatcher.Check returns for a config it
	// has just accepted.
	var once sync.Once
	recheck := func() (ReloadEvent, bool) {
		event := ReloadEvent{}
		answered := false
		once.Do(func() {
			event = ReloadEvent{Config: both, Diff: diffConfigs(alphaOnly, both)}
			answered = true
		})
		return event, answered
	}
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan error, 1)
	go func() {
		done <- runScheduler(ctx, alphaOnly, reloads, &MemoryPublisher{}, store,
			removalPolicy{Confirm: 300 * time.Millisecond, Recheck: recheck}, log.New(logs, "", 0))
	}()
	defer func() {
		cancel()
		<-done
	}()

	select {
	case reloads <- ReloadEvent{Config: alphaOnly, Diff: diffConfigs(both, alphaOnly)}:
	case <-time.After(5 * time.Second):
		t.Fatalf("the scheduler never took the reload; log was:\n%s", logs.String())
	}
	waitForLog(t, logs, "job=beta removal cancelled", 5*time.Second)
	if got := store.deletions(); len(got) != 0 {
		t.Fatalf("Delete was called for %v; the recheck found beta in the file", got)
	}
}

func TestPermanentPublishIsNotRetried(t *testing.T) {
	order := []string{}
	publisher := &orderedPublisher{order: &order, err: PermanentPublishFailure(errors.New("no stream"))}
	fire := Fire{Job: "job", Subject: "missing", ScheduledAt: time.Now()}
	job := Job{Name: "job", Schedule: "@every 1m", TZ: "UTC", Durable: boolPtr(true)}
	err := publishWithBackoff(context.Background(), publisher, fire, job, log.New(io.Discard, "", 0))
	if !IsPermanentPublishError(err) || len(order) != 1 {
		t.Fatalf("err=%v attempts=%d, want one permanent failure", err, len(order))
	}
}
