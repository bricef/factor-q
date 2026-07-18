package main

import (
	"context"
	"errors"
	"io"
	"log"
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
