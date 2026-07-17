package main

import (
	"context"
	"encoding/json"
	"errors"
	"testing"
	"time"

	"github.com/nats-io/nats.go/jetstream"
)

func TestDedupMessageID(t *testing.T) {
	scheduled := time.Date(2026, 7, 17, 2, 0, 0, 0, time.FixedZone("offset", 3600))
	if got, want := DedupMessageID("nightly", scheduled), "fq-cron/nightly@2026-07-17T02:00:00+01:00"; got != want {
		t.Fatalf("DedupMessageID() = %q, want %q", got, want)
	}
}

func TestPublishErrorClassification(t *testing.T) {
	if !isNoStreamError(jetstream.ErrNoStreamResponse) {
		t.Fatal("no-stream error should be permanent")
	}
	permanent := &PublishError{Permanent: isNoStreamError(jetstream.ErrNoStreamResponse), Err: jetstream.ErrNoStreamResponse}
	if !IsPermanentPublishError(permanent) {
		t.Fatal("classified no-stream failure should be permanent")
	}
	if IsPermanentPublishError(TransientPublishFailure(context.DeadlineExceeded)) {
		t.Fatal("timeout should be transient")
	}
}

func TestMemoryPublisherScriptsFailures(t *testing.T) {
	p := &MemoryPublisher{Errors: []error{PermanentPublishFailure(errors.New("bad subject")), nil}}
	if err := p.Publish(context.Background(), "job", "subject", []byte("one"), time.Time{}, true); !IsPermanentPublishError(err) {
		t.Fatalf("first error = %v, want permanent", err)
	}
	if err := p.Publish(context.Background(), "job", "subject", []byte("two"), time.Time{}, false); err != nil {
		t.Fatalf("second error = %v", err)
	}
	if len(p.Publishes) != 2 || string(p.Publishes[1].Payload) != "two" {
		t.Fatalf("recorded publishes = %#v", p.Publishes)
	}
}

func TestFireStateJSONShapeAndMemoryRoundTrip(t *testing.T) {
	state := FireState{
		LastScheduled: time.Date(2026, 7, 17, 2, 0, 0, 0, time.UTC),
		PublishedAt:   time.Date(2026, 7, 17, 2, 0, 1, 0, time.UTC),
	}
	value, err := json.Marshal(state)
	if err != nil {
		t.Fatal(err)
	}
	want := `{"last_scheduled":"2026-07-17T02:00:00Z","published_at":"2026-07-17T02:00:01Z"}`
	if string(value) != want {
		t.Fatalf("JSON = %s, want %s", value, want)
	}

	store := NewMemoryStateStore()
	if err := store.Put(context.Background(), "nightly", state); err != nil {
		t.Fatal(err)
	}
	got, ok, err := store.Get(context.Background(), "nightly")
	if err != nil || !ok || !got.LastScheduled.Equal(state.LastScheduled) || !got.PublishedAt.Equal(state.PublishedAt) {
		t.Fatalf("Get() = %#v, %v, %v", got, ok, err)
	}
	if err := store.Delete(context.Background(), "nightly"); err != nil {
		t.Fatal(err)
	}
	if _, ok, _ := store.Get(context.Background(), "nightly"); ok {
		t.Fatal("deleted state still exists")
	}
}
