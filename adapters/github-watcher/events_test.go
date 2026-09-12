package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"slices"
	"strings"
	"testing"

	"github.com/nats-io/nats.go"
)

// wireEventJSON builds a raw event in the EXACT shape the runtime puts on
// the wire: an envelope plus an adjacently-tagged payload (serde
// `tag = "event_type", content = "payload"`), so the concrete payload is
// nested under payload.payload. Building fixtures this way is the whole
// point — a flat fixture is what let the decode bug ship, so these tests
// deliberately mirror the runtime serialization.
//
// It stamps envelope version 2, the older of the versions the watcher
// reads; wireEventJSONAt stamps any version, and is how the current one is
// covered.
func wireEventJSON(t *testing.T, invID, eventType string, inner any) []byte {
	t.Helper()
	return wireEventJSONAt(t, 2, invID, eventType, inner)
}

// wireEventJSONAt is wireEventJSON with the envelope's schema_version
// chosen by the caller.
func wireEventJSONAt(t *testing.T, schemaVersion int, invID, eventType string, inner any) []byte {
	t.Helper()
	b, err := json.Marshal(map[string]any{
		"envelope": map[string]any{"schema_version": schemaVersion, "invocation_id": invID},
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

// Every version in supportedSchemaVersions must actually decode. The set is
// the watcher's half of the contract `just check-schema-versions` enforces
// against the runtime's SUPPORTED_SCHEMA_VERSIONS, and a version listed
// there but not decodable would satisfy the drift gate while skipping every
// event all the same.
func TestDecodeAcceptsEverySupportedSchemaVersion(t *testing.T) {
	for _, version := range supportedSchemaVersions {
		t.Run(fmt.Sprintf("v%d", version), func(t *testing.T) {
			s := &NatsOutcomeSource{taskTemplate: "issue #%d"}
			data := wireEventJSONAt(t, version, "inv-v", "completed", map[string]any{"task_status": "success"})
			ev, ok := s.decode(&nats.Msg{Subject: "fq.agent.a.completed", Data: data})
			if !ok || ev.Kind != OutcomeCompleted || ev.InvocationID != "inv-v" || ev.TaskStatus != "success" {
				t.Fatalf("v%d decode = %+v ok=%v, want completed/inv-v/success", version, ev, ok)
			}
			if n := s.schemaVersionMismatches.Load(); n != 0 {
				t.Fatalf("v%d counted %d schema mismatches, want 0", version, n)
			}
		})
	}
}

// A version the watcher does not read is still skipped and still counted —
// the counter is the only signal that a bump has outrun this adapter.
func TestDecodeRejectsUnsupportedSchemaVersion(t *testing.T) {
	for _, version := range []int{1, 99} {
		t.Run(fmt.Sprintf("v%d", version), func(t *testing.T) {
			if schemaVersionSupported(version) {
				t.Skipf("v%d is now supported; pick another out-of-set version", version)
			}
			var logs bytes.Buffer
			s := &NatsOutcomeSource{taskTemplate: "issue #%d", log: slog.New(slog.NewTextHandler(&logs, nil))}
			data := wireEventJSONAt(t, version, "inv-new", "completed", map[string]any{})
			if ev, ok := s.decode(&nats.Msg{Subject: "fq.agent.a.completed", Data: data}); ok {
				t.Fatalf("schema version %d decoded unexpectedly: %+v", version, ev)
			}
			if s.schemaVersionMismatches.Load() != 1 || !strings.Contains(logs.String(), "unsupported schema version") {
				t.Fatalf("mismatch count/log = %d/%q", s.schemaVersionMismatches.Load(), logs.String())
			}
		})
	}
}

// The regression guard for #694, end to end over the two halves that were
// broken apart: a version-3 `triggered` binds the invocation to its issue
// and a version-3 `completed` moves the issue on to in-review. Before the
// fix both events were skipped at the version check, the binding was never
// learned, no relabel was attempted, and the issue sat at in-progress —
// which is exactly what #692 did for eight days' worth of fleet runs.
func TestSchemaVersion3CompletionReachesInReview(t *testing.T) {
	src := &labelSource{}
	reactor := NewOutcomeReactor(src, outcomeConfig(), discardLogger())
	reactor.Stamper = &fakeStamper{prsByIssue: map[int][]int{692: {693}}, bodies: map[int]string{693: "body"}}
	source := &NatsOutcomeSource{taskTemplate: "Implement the fix described in GitHub issue #%d."}

	for _, msg := range []*nats.Msg{
		{Subject: "fq.agent.m0-issue-fix.triggered", Data: wireEventJSONAt(t, 3, "inv-692", "triggered", map[string]any{
			"trigger_payload": map[string]any{
				"task":   "Implement the fix described in GitHub issue #692.",
				"github": map[string]any{"repo": "bricef/factor-q", "issue": 692},
			},
		})},
		{Subject: "fq.agent.m0-issue-fix.completed", Data: wireEventJSONAt(t, 3, "inv-692", "completed", map[string]any{
			"task_status": "success",
		})},
	} {
		ev, ok := source.decode(msg)
		if !ok {
			t.Fatalf("v3 %s was skipped at decode; the watcher is deaf to what the runtime writes", msg.Subject)
		}
		reactor.React(context.Background(), ev)
	}

	if want := []string{"relabel #692 in-progress->in-review"}; !slices.Equal(src.ops, want) {
		t.Errorf("ops = %v, want %v", src.ops, want)
	}
	if n := source.schemaVersionMismatches.Load(); n != 0 {
		t.Errorf("schema mismatches = %d, want 0 for the version the runtime writes", n)
	}
}
