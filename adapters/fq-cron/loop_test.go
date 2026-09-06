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
		done <- runScheduler(ctx, config, nil, &signallingPublisher{at: published}, store, log.New(logs, "", 0))
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

// signallingPublisher reports the time of its first publish.
type signallingPublisher struct {
	at   chan time.Time
	once sync.Once
}

func (p *signallingPublisher) Publish(context.Context, string, string, []byte, time.Time, bool) error {
	p.once.Do(func() { p.at <- time.Now() })
	return nil
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
