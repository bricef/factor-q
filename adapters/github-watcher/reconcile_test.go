package main

import (
	"context"
	"errors"
	"fmt"
	"slices"
	"testing"
	"time"

	"github.com/nats-io/nats.go"
	"github.com/nats-io/nats.go/jetstream"
)

type staticHistory struct {
	events []OutcomeEvent
	err    error
}

func (h staticHistory) Events(context.Context, string) ([]OutcomeEvent, error) {
	return h.events, h.err
}

type reconcileSource struct {
	issues map[int]Issue
	prs    map[int][]int
	prErr  error
	ops    []string
}

func (s *reconcileSource) ListByLabel(_ context.Context, label string) ([]Issue, error) {
	var out []Issue
	for _, issue := range s.issues {
		if issue.HasLabel(label) {
			out = append(out, issue)
		}
	}
	return out, nil
}

func (s *reconcileSource) OpenPRsClosingIssue(_ context.Context, issue int) ([]int, error) {
	if s.prErr != nil {
		return nil, s.prErr
	}
	return s.prs[issue], nil
}

func (s *reconcileSource) Relabel(_ context.Context, number int, remove, add string) error {
	issue := s.issues[number]
	if !issue.HasLabel(remove) {
		return ErrClaimLost
	}
	labels := make([]string, 0, len(issue.Labels))
	for _, label := range issue.Labels {
		if label != remove && label != add {
			labels = append(labels, label)
		}
	}
	issue.Labels = append(labels, add)
	s.issues[number] = issue
	s.ops = append(s.ops, fmt.Sprintf("relabel #%d %s->%s", number, remove, add))
	return nil
}

func reconcileEvents(issue int, terminal OutcomeEvent, attempts int) []OutcomeEvent {
	var events []OutcomeEvent
	for n := 1; n <= attempts; n++ {
		inv := fmt.Sprintf("inv-%d", n)
		events = append(events, OutcomeEvent{Kind: OutcomeTriggered, InvocationID: inv, Issue: issue})
		if n == attempts && terminal.Kind != 0 {
			terminal.InvocationID = inv
			events = append(events, terminal)
		}
	}
	return events
}

func TestReconcileInProgressBranches(t *testing.T) {
	now := time.Date(2026, 9, 15, 0, 0, 0, 0, time.UTC)
	tests := []struct {
		name     string
		prs      []int
		terminal OutcomeEvent
		attempts int
		age      time.Duration
		want     string
	}{
		{"open PR", []int{41}, OutcomeEvent{}, 1, time.Minute, "in-review"},
		{"completed without PR", nil, OutcomeEvent{Kind: OutcomeCompleted}, 1, time.Minute, "ready"},
		{"failed retryable", nil, OutcomeEvent{Kind: OutcomeFailed, ErrorKind: "llm_error"}, 1, time.Minute, "ready"},
		{"failed terminal", nil, OutcomeEvent{Kind: OutcomeFailed, ErrorKind: "budget_exceeded"}, 1, time.Minute, "failed"},
		{"retry budget exhausted", nil, OutcomeEvent{Kind: OutcomeFailed, ErrorKind: "llm_error"}, 3, time.Minute, "failed"},
		{"in flight within bound", nil, OutcomeEvent{}, 1, time.Hour, ""},
		{"eventless past bound", nil, OutcomeEvent{}, 1, 5 * time.Hour, "ready"},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			src := &reconcileSource{
				issues: map[int]Issue{7: {Number: 7, Labels: []string{"in-progress"}, UpdatedAt: now.Add(-tt.age)}},
				prs:    map[int][]int{7: tt.prs},
			}
			cfg := outcomeConfig()
			cfg.ReconcileAfter = 4 * time.Hour
			r := &InProgressReconciler{Source: src, History: staticHistory{events: reconcileEvents(7, tt.terminal, tt.attempts)}, Config: cfg, Log: discardLogger(), Now: func() time.Time { return now }}
			r.Reconcile(context.Background())
			var want []string
			if tt.want != "" {
				want = []string{"relabel #7 in-progress->" + tt.want}
			}
			if !slices.Equal(src.ops, want) {
				t.Fatalf("ops = %v, want %v", src.ops, want)
			}
		})
	}
}

func TestRestartMidFlightReconcilesAndIsIdempotent(t *testing.T) {
	now := time.Now()
	src := &reconcileSource{issues: map[int]Issue{7: {Number: 7, Labels: []string{"in-progress"}, UpdatedAt: now}}, prs: map[int][]int{}}
	cfg := outcomeConfig()
	cfg.ReconcileAfter = 4 * time.Hour

	// Before restart the invocation is still running, so ground truth causes
	// no transition. A fresh reconciler (empty memory) then sees the retained
	// terminal event and recovers the label.
	first := &InProgressReconciler{Source: src, History: staticHistory{events: reconcileEvents(7, OutcomeEvent{}, 1)}, Config: cfg, Log: discardLogger(), Now: func() time.Time { return now }}
	first.Reconcile(context.Background())
	second := &InProgressReconciler{Source: src, History: staticHistory{events: reconcileEvents(7, OutcomeEvent{Kind: OutcomeFailed, ErrorKind: "llm_error"}, 1)}, Config: cfg, Log: discardLogger(), Now: func() time.Time { return now }}
	second.Reconcile(context.Background())
	second.Reconcile(context.Background())
	if want := []string{"relabel #7 in-progress->ready"}; !slices.Equal(src.ops, want) {
		t.Fatalf("two reconcile passes = %v, want one idempotent transition %v", src.ops, want)
	}
}

func TestReconcileLeavesStateWhenGroundTruthUnavailable(t *testing.T) {
	src := &reconcileSource{issues: map[int]Issue{7: {Number: 7, Labels: []string{"in-progress"}}}, prErr: errors.New("github down")}
	r := &InProgressReconciler{Source: src, History: staticHistory{}, Config: outcomeConfig(), Log: discardLogger()}
	r.Reconcile(context.Background())
	if len(src.ops) != 0 {
		t.Fatalf("ops = %v, want no transition without GitHub ground truth", src.ops)
	}
}

func TestNatsInvocationHistoryReadsRetainedEvents(t *testing.T) {
	binary := natsServerBinary(t)
	port := freePort(t)
	startBroker(t, binary, port)
	var nc *nats.Conn
	var err error
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		nc, err = nats.Connect(fmt.Sprintf("nats://127.0.0.1:%d", port), nats.Timeout(100*time.Millisecond))
		if err == nil {
			break
		}
		time.Sleep(20 * time.Millisecond)
	}
	if err != nil {
		t.Fatal(err)
	}
	defer nc.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	js, err := jetstream.New(nc)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := js.CreateStream(ctx, jetstream.StreamConfig{Name: eventStream, Subjects: []string{"fq.agent.>"}}); err != nil {
		t.Fatal(err)
	}
	for _, event := range []struct {
		subject string
		data    []byte
	}{
		{"fq.agent.m0-issue-fix.triggered", wireEventJSON(t, "inv-7", "triggered", map[string]any{"trigger_payload": "issue #7"})},
		{"fq.agent.m0-issue-fix.failed", wireEventJSON(t, "inv-7", "failed", map[string]any{"error_kind": "llm_error"})},
		// Server-side filtering must exclude another agent.
		{"fq.agent.other.completed", wireEventJSON(t, "other", "completed", map[string]any{"task_status": "success"})},
	} {
		if _, err := js.Publish(ctx, event.subject, event.data); err != nil {
			t.Fatal(err)
		}
	}
	decoder := NewNatsOutcomeSource(nc, "issue #%d", discardLogger())
	history := NewNatsInvocationHistory(nc, decoder)
	events, err := history.Events(ctx, "m0-issue-fix")
	if err != nil {
		t.Fatal(err)
	}
	if len(events) != 2 || events[0].Issue != 7 || events[1].Kind != OutcomeFailed {
		t.Fatalf("history events = %#v, want trigger #7 then failed", events)
	}
}

func TestReconcileOpenPRDoesNotDependOnEventHistory(t *testing.T) {
	src := &reconcileSource{issues: map[int]Issue{7: {Number: 7, Labels: []string{"in-progress"}}}, prs: map[int][]int{7: {41}}}
	r := &InProgressReconciler{Source: src, History: staticHistory{err: errors.New("stream down")}, Config: outcomeConfig(), Log: discardLogger()}
	r.Reconcile(context.Background())
	if want := []string{"relabel #7 in-progress->in-review"}; !slices.Equal(src.ops, want) {
		t.Fatalf("ops = %v, want PR-grounded transition %v", src.ops, want)
	}
}
