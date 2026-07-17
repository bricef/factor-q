package main

import (
	"encoding/json"
	"testing"

	"github.com/nats-io/nats.go"
)

// wireEventJSON builds a raw event in the EXACT shape the runtime puts on
// the wire: an envelope plus an adjacently-tagged payload (serde
// `tag = "event_type", content = "payload"`), so the concrete payload is
// nested under payload.payload. Building fixtures this way is the whole
// point — a flat fixture is what let the decode bug ship, so these tests
// deliberately mirror the runtime serialization.
func wireEventJSON(t *testing.T, invID, eventType string, inner any) []byte {
	t.Helper()
	b, err := json.Marshal(map[string]any{
		"envelope": map[string]any{"invocation_id": invID},
		"payload": map[string]any{
			"event_type": eventType,
			"payload":    inner,
		},
	})
	if err != nil {
		t.Fatalf("marshal wire event: %v", err)
	}
	return b
}

// Regression guard for the outcome-observation stranding bug: because the
// runtime nests the concrete payload under payload.payload, decode must
// unwrap the adjacent tag. Reading it flat yields a null trigger_payload
// (issue 0), which silently breaks the invocation->issue binding and leaves
// every issue stranded in in-progress.
func TestDecodeUnwrapsAdjacentlyTaggedTriggered(t *testing.T) {
	s := &NatsOutcomeSource{taskTemplate: "Implement the fix described in GitHub issue #%d."}
	data := wireEventJSON(t, "inv-9", "triggered", map[string]any{
		"trigger_payload": "Implement the fix described in GitHub issue #50.",
	})
	ev, ok := s.decode(&nats.Msg{Subject: "fq.agent.m0-issue-fix.triggered", Data: data})
	if !ok {
		t.Fatal("triggered event should decode")
	}
	if ev.Kind != OutcomeTriggered {
		t.Errorf("kind = %v, want triggered", ev.Kind)
	}
	if ev.InvocationID != "inv-9" {
		t.Errorf("invocation id = %q, want inv-9", ev.InvocationID)
	}
	if ev.Issue != 50 {
		t.Errorf("issue = %d, want 50 (payload read one level too shallow?)", ev.Issue)
	}
}

func TestDecodeConventionPayloadUsesGitHubIssue(t *testing.T) {
	s := &NatsOutcomeSource{taskTemplate: "issue #%d"}
	data := wireEventJSON(t, "inv-10", "triggered", map[string]any{
		"trigger_payload": map[string]any{
			"task":   "issue #50",
			"github": map[string]any{"repo": "owner/repo", "issue": 50},
		},
	})
	ev, ok := s.decode(&nats.Msg{Subject: "fq.agent.m0-issue-fix.triggered", Data: data})
	if !ok || ev.Issue != 50 {
		t.Fatalf("convention payload decode = %+v ok=%v, want issue 50", ev, ok)
	}
}

func TestDecodeCompletedCarriesTaskStatus(t *testing.T) {
	s := &NatsOutcomeSource{taskTemplate: "issue #%d"}
	data := wireEventJSON(t, "inv-3", "completed", map[string]any{"task_status": "blocked"})
	ev, ok := s.decode(&nats.Msg{Subject: "fq.agent.a.completed", Data: data})
	if !ok || ev.Kind != OutcomeCompleted || ev.InvocationID != "inv-3" || ev.TaskStatus != "blocked" {
		t.Fatalf("completed decode = %+v ok=%v, want completed/inv-3/blocked", ev, ok)
	}
}

func TestDecodeCompletedWithoutTaskStatusIsCompatible(t *testing.T) {
	s := &NatsOutcomeSource{taskTemplate: "issue #%d"}
	data := wireEventJSON(t, "inv-old", "completed", map[string]any{})
	ev, ok := s.decode(&nats.Msg{Subject: "fq.agent.a.completed", Data: data})
	if !ok || ev.TaskStatus != "" {
		t.Fatalf("legacy completed decode = %+v ok=%v, want empty task status", ev, ok)
	}
}

func TestDecodeFailedUnwrapsErrorKind(t *testing.T) {
	s := &NatsOutcomeSource{taskTemplate: "issue #%d"}
	data := wireEventJSON(t, "inv-4", "failed", map[string]any{"error_kind": "llm_error"})
	ev, ok := s.decode(&nats.Msg{Subject: "fq.agent.a.failed", Data: data})
	if !ok || ev.Kind != OutcomeFailed || ev.ErrorKind != "llm_error" {
		t.Fatalf("failed decode = %+v ok=%v, want failed/llm_error (error_kind too shallow?)", ev, ok)
	}
}
