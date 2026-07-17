package main

import (
	"encoding/json"
	"testing"
	"time"
)

func TestRenderPayload(t *testing.T) {
	at := time.Date(2026, 7, 17, 12, 30, 0, 0, time.FixedZone("X", 3600))
	job := Job{Name: "nightly", Payload: map[string]any{"message": "run {{job}} at {{scheduled_time}}", "nested": map[string]any{"name": "{{job}}"}}}
	got, err := RenderPayload(job, at)
	if err != nil {
		t.Fatal(err)
	}
	var decoded map[string]any
	if err := json.Unmarshal(got, &decoded); err != nil {
		t.Fatal(err)
	}
	if decoded["message"] != "run nightly at 2026-07-17T12:30:00+01:00" {
		t.Fatalf("got %s", got)
	}
}

func TestRenderPayloadJSON(t *testing.T) {
	raw := `{"job":"{{job}}"}`
	got, err := RenderPayload(Job{Name: "heartbeat", PayloadJSON: &raw}, time.Time{})
	if err != nil || string(got) != `{"job":"heartbeat"}` {
		t.Fatalf("got %s, %v", got, err)
	}
}

func TestRenderNoPayload(t *testing.T) {
	got, err := RenderPayload(Job{}, time.Time{})
	if err != nil || got != nil {
		t.Fatalf("got %q, %v", got, err)
	}
}
