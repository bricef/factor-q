package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"sync"
	"time"

	"github.com/nats-io/nats.go/jetstream"
)

const DefaultStateBucket = "fq-cron-state"

type FireState struct {
	LastScheduled time.Time `json:"last_scheduled"`
	PublishedAt   time.Time `json:"published_at"`
}

// StateStore persists the last acknowledged fire for each job.
type StateStore interface {
	Get(context.Context, string) (FireState, bool, error)
	Put(context.Context, string, FireState) error
	Delete(context.Context, string) error
}

type KVStateStore struct{ kv jetstream.KeyValue }

// NewKVStateStore idempotently ensures bucket and binds a store to it.
func NewKVStateStore(ctx context.Context, js jetstream.JetStream, bucket string) (*KVStateStore, error) {
	if bucket == "" {
		bucket = DefaultStateBucket
	}
	kv, err := js.KeyValue(ctx, bucket)
	if errors.Is(err, jetstream.ErrBucketNotFound) {
		kv, err = js.CreateKeyValue(ctx, jetstream.KeyValueConfig{Bucket: bucket})
	}
	if err != nil {
		return nil, fmt.Errorf("ensure state bucket %q: %w", bucket, err)
	}
	return &KVStateStore{kv: kv}, nil
}

func (s *KVStateStore) Get(ctx context.Context, job string) (FireState, bool, error) {
	entry, err := s.kv.Get(ctx, job)
	if errors.Is(err, jetstream.ErrKeyNotFound) {
		return FireState{}, false, nil
	}
	if err != nil {
		return FireState{}, false, fmt.Errorf("get state for %q: %w", job, err)
	}
	var state FireState
	if err := json.Unmarshal(entry.Value(), &state); err != nil {
		return FireState{}, false, fmt.Errorf("decode state for %q: %w", job, err)
	}
	return state, true, nil
}

func (s *KVStateStore) Put(ctx context.Context, job string, state FireState) error {
	value, err := json.Marshal(state)
	if err != nil {
		return fmt.Errorf("encode state for %q: %w", job, err)
	}
	if _, err := s.kv.Put(ctx, job, value); err != nil {
		return fmt.Errorf("put state for %q: %w", job, err)
	}
	return nil
}

func (s *KVStateStore) Delete(ctx context.Context, job string) error {
	if err := s.kv.Delete(ctx, job); err != nil && !errors.Is(err, jetstream.ErrKeyNotFound) {
		return fmt.Errorf("delete state for %q: %w", job, err)
	}
	return nil
}

// MemoryStateStore is a concurrency-safe, map-backed test fake.
type MemoryStateStore struct {
	mu     sync.RWMutex
	States map[string]FireState
}

func NewMemoryStateStore() *MemoryStateStore {
	return &MemoryStateStore{States: make(map[string]FireState)}
}

func (s *MemoryStateStore) Get(_ context.Context, job string) (FireState, bool, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	state, ok := s.States[job]
	return state, ok, nil
}

func (s *MemoryStateStore) Put(_ context.Context, job string, state FireState) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.States == nil {
		s.States = make(map[string]FireState)
	}
	s.States[job] = state
	return nil
}

func (s *MemoryStateStore) Delete(_ context.Context, job string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	delete(s.States, job)
	return nil
}
