package main

import (
	"context"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/nats-io/nats.go"
)

func newTestHealth(status nats.Status, maxAge time.Duration) (*Health, *time.Time) {
	now := time.Date(2026, 9, 5, 12, 0, 0, 0, time.UTC)
	h := &Health{maxAge: maxAge, now: func() time.Time { return now }}
	h.connStatus = func() nats.Status { return status }
	return h, &now
}

func get(t *testing.T, h http.Handler, method, path string) *httptest.ResponseRecorder {
	t.Helper()
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(method, path, nil))
	return rec
}

func TestHealthzOKWhenConnectedAndTicking(t *testing.T) {
	h, _ := newTestHealth(nats.CONNECTED, time.Minute)
	h.Tick()
	rec := get(t, h, http.MethodGet, "/healthz")
	if rec.Code != http.StatusOK {
		t.Fatalf("status %d, body %s", rec.Code, rec.Body)
	}
	body := rec.Body.String()
	for _, want := range []string{`"status":"ok"`, `"nats":"CONNECTED"`, `"last_cycle":"0s ago"`} {
		if !strings.Contains(body, want) {
			t.Errorf("body %s lacks %s", body, want)
		}
	}
	if ct := rec.Header().Get("Content-Type"); ct != "application/json" {
		t.Errorf("content type %q", ct)
	}
}

func TestHealthzUnhealthyBeforeFirstCycle(t *testing.T) {
	h, _ := newTestHealth(nats.CONNECTED, time.Minute)
	rec := get(t, h, http.MethodGet, "/healthz")
	if rec.Code != http.StatusServiceUnavailable {
		t.Fatalf("status %d, want 503", rec.Code)
	}
	if !strings.Contains(rec.Body.String(), `"last_cycle":"none yet"`) {
		t.Errorf("body %s", rec.Body)
	}
}

func TestHealthzUnhealthyWhenLoopStalls(t *testing.T) {
	h, now := newTestHealth(nats.CONNECTED, time.Minute)
	h.Tick()
	*now = now.Add(61 * time.Second)
	rec := get(t, h, http.MethodGet, "/healthz")
	if rec.Code != http.StatusServiceUnavailable {
		t.Fatalf("status %d, want 503; body %s", rec.Code, rec.Body)
	}
	if !strings.Contains(rec.Body.String(), `"last_cycle":"1m1s ago"`) {
		t.Errorf("body %s", rec.Body)
	}
	// A fresh tick heals it.
	h.Tick()
	if rec := get(t, h, http.MethodGet, "/healthz"); rec.Code != http.StatusOK {
		t.Fatalf("after tick: status %d", rec.Code)
	}
}

func TestHealthzUnhealthyWhenBrokerDown(t *testing.T) {
	h, _ := newTestHealth(nats.RECONNECTING, time.Minute)
	h.Tick()
	rec := get(t, h, http.MethodGet, "/healthz")
	if rec.Code != http.StatusServiceUnavailable {
		t.Fatalf("status %d, want 503", rec.Code)
	}
	if !strings.Contains(rec.Body.String(), `"nats":"RECONNECTING"`) {
		t.Errorf("body %s", rec.Body)
	}
}

func TestHealthzZeroValueChecksNothing(t *testing.T) {
	h := &Health{now: time.Now}
	if rec := get(t, h, http.MethodGet, "/healthz"); rec.Code != http.StatusOK {
		t.Fatalf("status %d", rec.Code)
	}
}

func TestHealthzIsAProbeNotAnAPI(t *testing.T) {
	h, _ := newTestHealth(nats.CONNECTED, 0)
	if rec := get(t, h, http.MethodGet, "/"); rec.Code != http.StatusNotFound {
		t.Errorf("GET /: %d", rec.Code)
	}
	if rec := get(t, h, http.MethodPost, "/healthz"); rec.Code != http.StatusMethodNotAllowed {
		t.Errorf("POST /healthz: %d", rec.Code)
	}
	if rec := get(t, h, http.MethodHead, "/healthz"); rec.Code != http.StatusOK {
		t.Errorf("HEAD /healthz: %d", rec.Code)
	}
}

func TestProbeFollowsTheEndpoint(t *testing.T) {
	h, now := newTestHealth(nats.CONNECTED, time.Minute)
	h.Tick()
	srv := httptest.NewServer(h)
	defer srv.Close()
	bind := strings.TrimPrefix(srv.URL, "http://")
	if err := probe(bind); err != nil {
		t.Fatalf("healthy: %v", err)
	}
	*now = now.Add(time.Hour)
	err := probe(bind)
	if err == nil {
		t.Fatal("stale: expected an error")
	}
	if !strings.Contains(err.Error(), "503") || !strings.Contains(err.Error(), "last_cycle") {
		t.Errorf("error should carry the status and the body: %v", err)
	}
}

func TestProbeFailsWhenNothingListens(t *testing.T) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	addr := ln.Addr().String()
	ln.Close()
	if err := probe(addr); err == nil {
		t.Fatal("expected a connection error")
	}
}

func TestListenHealthRefusesNonLoopback(t *testing.T) {
	for _, bind := range []string{"0.0.0.0:0", "[::]:0", "192.0.2.1:0", "nonsense"} {
		if ln, err := listenHealth(bind); err == nil {
			ln.Close()
			t.Errorf("%s: accepted", bind)
		}
	}
}

func TestServeHealthStopsWithContext(t *testing.T) {
	ln, err := listenHealth("127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	h, _ := newTestHealth(nats.CONNECTED, 0)
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan error, 1)
	go func() { done <- serveHealth(ctx, ln, h, discardLogger()) }()
	if err := probe(ln.Addr().String()); err != nil {
		t.Fatalf("serving: %v", err)
	}
	cancel()
	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("stop: %v", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("serveHealth did not return after cancel")
	}
}
