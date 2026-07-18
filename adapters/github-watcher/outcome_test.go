package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"slices"
	"strings"
	"testing"
)

// --- fakes for the outcome / review paths ---

// labelSource is an in-memory IssueSource+ReviewSource that records relabel
// ops and can answer merged-PR queries, so the outcome reactor and review
// sweep are testable without gh or NATS.
type labelSource struct {
	ops      []string
	relErr   error
	inReview []Issue
	merged   map[int]bool
	mergeErr error
}

func (s *labelSource) ListReady(context.Context, string) ([]Issue, error) { return nil, nil }

func (s *labelSource) Relabel(_ context.Context, number int, remove, add string) error {
	if s.relErr != nil {
		return s.relErr
	}
	s.ops = append(s.ops, fmt.Sprintf("relabel #%d %s->%s", number, remove, add))
	return nil
}

func (s *labelSource) ListByLabel(_ context.Context, _ string) ([]Issue, error) {
	return s.inReview, nil
}

func (s *labelSource) HasMergedPR(_ context.Context, number int) (bool, error) {
	if s.mergeErr != nil {
		return false, s.mergeErr
	}
	return s.merged[number], nil
}

func outcomeConfig() Config {
	c := testConfig()
	c.InReviewLabel = "in-review"
	c.FailedLabel = "failed"
	c.DoneLabel = "done"
	c.MaxRetries = 2
	return c
}

func triggeredThen(reactor *OutcomeReactor, inv string, issue int, terminal OutcomeEvent) {
	reactor.React(context.Background(), OutcomeEvent{Kind: OutcomeTriggered, InvocationID: inv, Issue: issue})
	reactor.React(context.Background(), terminal)
}

// --- issue-number recovery from the task template ---

func TestIssueNumberFromTemplate(t *testing.T) {
	tmpl := "Implement the fix described in GitHub issue #%d."
	if got := issueNumberFromTemplate(tmpl, "Implement the fix described in GitHub issue #15."); got != 15 {
		t.Errorf("recovered issue = %d, want 15", got)
	}
	if got := issueNumberFromTemplate(tmpl, "something unrelated"); got != 0 {
		t.Errorf("non-matching payload = %d, want 0", got)
	}
	// A payload from a different template must not match.
	if got := issueNumberFromTemplate(tmpl, "issue 15"); got != 0 {
		t.Errorf("wrong-shape payload = %d, want 0", got)
	}
}

func TestIssueFromTriggerPayload(t *testing.T) {
	tmpl := "issue #%d"
	raw, _ := json.Marshal("issue #42")
	if got := issueFromTriggerPayload(tmpl, raw); got != 42 {
		t.Errorf("issue = %d, want 42", got)
	}
	// A non-string trigger payload (e.g. an object) yields unknown.
	if got := issueFromTriggerPayload(tmpl, json.RawMessage(`{"k":1}`)); got != 0 {
		t.Errorf("non-string payload = %d, want 0", got)
	}
}

// --- outcome reactor: the core of the stranding fix ---

func TestReactCompletedMovesToInReview(t *testing.T) {
	src := &labelSource{}
	r := NewOutcomeReactor(src, outcomeConfig(), discardLogger())
	triggeredThen(r, "inv-1", 7, OutcomeEvent{Kind: OutcomeCompleted, InvocationID: "inv-1"})
	want := []string{"relabel #7 in-progress->in-review"}
	if !slices.Equal(src.ops, want) {
		t.Errorf("ops = %v, want %v", src.ops, want)
	}
}

func TestReactCompletedDeliverableGate(t *testing.T) {
	tests := []struct {
		name    string
		stamper ProvenanceStamper
		want    string
		wantLog string
	}{
		{"open PR", &fakeStamper{prsByIssue: map[int][]int{7: {41}}, bodies: map[int]string{41: "body"}}, "relabel #7 in-progress->in-review", "pr_count=1"},
		{"no PR", &fakeStamper{prsByIssue: map[int][]int{}}, "relabel #7 in-progress->ready", "pr_count=0"},
		{"lookup error fails open", &fakeStamper{listErr: errors.New("graphql down")}, "relabel #7 in-progress->in-review", "level=WARN"},
		{"nil stamper fails open", nil, "relabel #7 in-progress->in-review", "level=WARN"},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			src := &labelSource{}
			var logs bytes.Buffer
			log := slog.New(slog.NewTextHandler(&logs, nil))
			r := NewOutcomeReactor(src, outcomeConfig(), log)
			r.Stamper = tt.stamper
			triggeredThen(r, "inv", 7, OutcomeEvent{Kind: OutcomeCompleted, InvocationID: "inv"})
			if !slices.Equal(src.ops, []string{tt.want}) {
				t.Errorf("ops = %v, want %q", src.ops, tt.want)
			}
			if got := logs.String(); !strings.Contains(got, "issue=7") || !strings.Contains(got, "invocation=inv") || !strings.Contains(got, tt.wantLog) {
				t.Errorf("gate log = %q, want issue, invocation, and %q", got, tt.wantLog)
			}
		})
	}
}

func TestCompletedWithoutPRSharesRetryBudget(t *testing.T) {
	src := &labelSource{}
	r := NewOutcomeReactor(src, outcomeConfig(), discardLogger())
	r.Stamper = &fakeStamper{prsByIssue: map[int][]int{}}
	triggeredThen(r, "llm", 9, OutcomeEvent{Kind: OutcomeFailed, InvocationID: "llm", ErrorKind: "llm_error"})
	triggeredThen(r, "no-pr-1", 9, OutcomeEvent{Kind: OutcomeCompleted, InvocationID: "no-pr-1"})
	triggeredThen(r, "no-pr-2", 9, OutcomeEvent{Kind: OutcomeCompleted, InvocationID: "no-pr-2"})
	want := []string{
		"relabel #9 in-progress->ready",
		"relabel #9 in-progress->ready",
		"relabel #9 in-progress->failed",
	}
	if !slices.Equal(src.ops, want) {
		t.Errorf("ops = %v, want %v", src.ops, want)
	}
}

func TestReactCompletedTaskStatuses(t *testing.T) {
	for _, status := range []string{"", "success", "partial"} {
		t.Run("reviewable_"+status, func(t *testing.T) {
			src := &labelSource{}
			r := NewOutcomeReactor(src, outcomeConfig(), discardLogger())
			triggeredThen(r, "inv", 7, OutcomeEvent{Kind: OutcomeCompleted, InvocationID: "inv", TaskStatus: status})
			want := []string{"relabel #7 in-progress->in-review"}
			if !slices.Equal(src.ops, want) {
				t.Errorf("status %q ops = %v, want %v", status, src.ops, want)
			}
		})
	}

	for _, status := range []string{"failed", "blocked"} {
		t.Run("retryable_"+status, func(t *testing.T) {
			src := &labelSource{}
			r := NewOutcomeReactor(src, outcomeConfig(), discardLogger())
			triggeredThen(r, "inv", 8, OutcomeEvent{Kind: OutcomeCompleted, InvocationID: "inv", TaskStatus: status})
			want := []string{"relabel #8 in-progress->ready"}
			if !slices.Equal(src.ops, want) {
				t.Errorf("status %q ops = %v, want %v", status, src.ops, want)
			}
		})
	}
}

func TestReactCompletedBlockedExhaustsRetries(t *testing.T) {
	src := &labelSource{}
	cfg := outcomeConfig()
	cfg.MaxRetries = 0
	r := NewOutcomeReactor(src, cfg, discardLogger())
	triggeredThen(r, "inv", 8, OutcomeEvent{Kind: OutcomeCompleted, InvocationID: "inv", TaskStatus: "blocked"})
	want := []string{"relabel #8 in-progress->failed"}
	if !slices.Equal(src.ops, want) {
		t.Errorf("ops = %v, want %v", src.ops, want)
	}
}

func TestReactTransientFailureRetriesThenEscalates(t *testing.T) {
	src := &labelSource{}
	cfg := outcomeConfig() // MaxRetries = 2
	r := NewOutcomeReactor(src, cfg, discardLogger())
	// The same issue fails transiently three times. Each failure is a new
	// invocation (a fresh trigger from the poll loop after re-queuing).
	for i, inv := range []string{"inv-a", "inv-b", "inv-c"} {
		triggeredThen(r, inv, 9, OutcomeEvent{Kind: OutcomeFailed, InvocationID: inv, ErrorKind: "llm_error"})
		_ = i
	}
	// First two failures re-queue (bounded retry); the third exhausts the
	// budget and escalates to `failed` — never stranded in in-progress.
	want := []string{
		"relabel #9 in-progress->ready",
		"relabel #9 in-progress->ready",
		"relabel #9 in-progress->failed",
	}
	if !slices.Equal(src.ops, want) {
		t.Errorf("ops = %v, want %v", src.ops, want)
	}
}

func TestReactTerminalFailureEscalatesImmediately(t *testing.T) {
	src := &labelSource{}
	r := NewOutcomeReactor(src, outcomeConfig(), discardLogger())
	// A terminal error (budget exhausted) is not retried: re-triggering
	// would fail the same way.
	triggeredThen(r, "inv-x", 3, OutcomeEvent{Kind: OutcomeFailed, InvocationID: "inv-x", ErrorKind: "budget_exceeded"})
	want := []string{"relabel #3 in-progress->failed"}
	if !slices.Equal(src.ops, want) {
		t.Errorf("ops = %v, want %v", src.ops, want)
	}
}

func TestReactUnknownInvocationIsIgnored(t *testing.T) {
	src := &labelSource{}
	r := NewOutcomeReactor(src, outcomeConfig(), discardLogger())
	// No prior `triggered` bound this invocation to an issue; nothing to do.
	r.React(context.Background(), OutcomeEvent{Kind: OutcomeCompleted, InvocationID: "unbound"})
	if len(src.ops) != 0 {
		t.Errorf("ops = %v, want none for an unbound invocation", src.ops)
	}
}

func TestReactZeroRetriesEscalatesOnFirstFailure(t *testing.T) {
	src := &labelSource{}
	cfg := outcomeConfig()
	cfg.MaxRetries = 0
	r := NewOutcomeReactor(src, cfg, discardLogger())
	triggeredThen(r, "inv-1", 5, OutcomeEvent{Kind: OutcomeFailed, InvocationID: "inv-1", ErrorKind: "llm_error"})
	want := []string{"relabel #5 in-progress->failed"}
	if !slices.Equal(src.ops, want) {
		t.Errorf("ops = %v, want %v (bounded retry, 0 budget)", src.ops, want)
	}
}

func TestReactAmbiguousEscalatesToFailed(t *testing.T) {
	src := &labelSource{}
	r := NewOutcomeReactor(src, outcomeConfig(), discardLogger())
	triggeredThen(r, "inv-9", 12, OutcomeEvent{Kind: OutcomeAmbiguous, InvocationID: "inv-9"})
	// Straight to failed — recovery limbo needs an operator, not a
	// retry — and never a false in-review.
	want := []string{"relabel #12 in-progress->failed"}
	if !slices.Equal(src.ops, want) {
		t.Errorf("ops = %v, want %v", src.ops, want)
	}
	// The binding is forgotten on first reaction: a duplicate
	// ambiguous event (replayed stream, daemon re-emit) is a no-op.
	r.React(context.Background(), OutcomeEvent{Kind: OutcomeAmbiguous, InvocationID: "inv-9"})
	if !slices.Equal(src.ops, want) {
		t.Errorf("ops after duplicate = %v, want unchanged %v", src.ops, want)
	}
}

// --- review sweep: merged PR -> done ---

func TestSweepReviewMovesMergedToDone(t *testing.T) {
	src := &labelSource{
		inReview: []Issue{{10, []string{"in-review"}}, {11, []string{"in-review"}}},
		merged:   map[int]bool{10: true, 11: false},
	}
	w := &Watcher{Source: src, Reviewer: src, Config: outcomeConfig(), Log: discardLogger()}
	w.sweepReview(context.Background())
	want := []string{"relabel #10 in-review->done"}
	if !slices.Equal(src.ops, want) {
		t.Errorf("ops = %v, want %v (only the merged issue moves)", src.ops, want)
	}
}

func TestSweepReviewSkippedWithoutReviewer(t *testing.T) {
	src := &labelSource{}
	w := &Watcher{Source: src, Reviewer: nil, Config: outcomeConfig(), Log: discardLogger()}
	w.sweepReview(context.Background()) // must not panic; no ops
	if len(src.ops) != 0 {
		t.Errorf("ops = %v, want none when no Reviewer is configured", src.ops)
	}
}

func TestSweepReviewMergeCheckErrorLeavesIssue(t *testing.T) {
	src := &labelSource{
		inReview: []Issue{{10, []string{"in-review"}}},
		mergeErr: errors.New("gh boom"),
	}
	w := &Watcher{Source: src, Reviewer: src, Config: outcomeConfig(), Log: discardLogger()}
	w.sweepReview(context.Background())
	if len(src.ops) != 0 {
		t.Errorf("ops = %v, want none when the merge check errors", src.ops)
	}
}

// --- gh graphql merged-PR parsing ---

func TestParseMergedPRResponse(t *testing.T) {
	mergedJSON := []byte(`{"data":{"repository":{"issue":{"closedByPullRequestsReferences":{"nodes":[{"merged":false},{"merged":true}]}}}}}`)
	if ok, err := parseMergedPRResponse(mergedJSON); err != nil || !ok {
		t.Errorf("merged response: ok=%v err=%v, want ok=true", ok, err)
	}
	noneJSON := []byte(`{"data":{"repository":{"issue":{"closedByPullRequestsReferences":{"nodes":[{"merged":false}]}}}}}`)
	if ok, err := parseMergedPRResponse(noneJSON); err != nil || ok {
		t.Errorf("unmerged response: ok=%v err=%v, want ok=false", ok, err)
	}
	emptyJSON := []byte(`{"data":{"repository":{"issue":{"closedByPullRequestsReferences":{"nodes":[]}}}}}`)
	if ok, err := parseMergedPRResponse(emptyJSON); err != nil || ok {
		t.Errorf("empty response: ok=%v err=%v, want ok=false", ok, err)
	}
}

// --- subject routing ---

func TestOutcomeKindFromSubject(t *testing.T) {
	cases := map[string]struct {
		kind OutcomeKind
		ok   bool
	}{
		"fq.agent.m0-issue-fix.triggered":    {OutcomeTriggered, true},
		"fq.agent.m0-issue-fix.completed":    {OutcomeCompleted, true},
		"fq.agent.m0-issue-fix.failed":       {OutcomeFailed, true},
		"fq.agent.m0-issue-fix.llm.response": {0, false},
		"fq.agent.m0-issue-fix.tool.result":  {0, false},

		"fq.agent.m0-issue-fix.invocation.ambiguous": {OutcomeAmbiguous, true},
		// The full `.invocation.ambiguous` suffix is required — a
		// bare trailing token must not match.
		"fq.agent.m0-issue-fix.ambiguous": {0, false},
	}
	for subject, want := range cases {
		got, ok := outcomeKindFromSubject(subject)
		if ok != want.ok || (ok && got != want.kind) {
			t.Errorf("%s -> (%v,%v), want (%v,%v)", subject, got, ok, want.kind, want.ok)
		}
	}
}

func TestReactCompletionResetsRetryBudget(t *testing.T) {
	for _, tc := range []struct {
		name          string
		priorFailures int
	}{{"after one failure", 1}, {"after exhausted budget", 2}} {
		t.Run(tc.name, func(t *testing.T) {
			src := &labelSource{}
			r := NewOutcomeReactor(src, outcomeConfig(), discardLogger())
			for i := 0; i < tc.priorFailures; i++ {
				inv := fmt.Sprintf("failed-%d", i)
				triggeredThen(r, inv, 9, OutcomeEvent{Kind: OutcomeFailed, InvocationID: inv, ErrorKind: "llm_error"})
			}
			triggeredThen(r, "complete", 9, OutcomeEvent{Kind: OutcomeCompleted, InvocationID: "complete"})
			if _, ok := r.attempts[9]; ok {
				t.Fatal("attempts entry survived completion")
			}
			// A later re-queue starts with a fresh budget: its first transient
			// failure records attempt one rather than escalating immediately.
			triggeredThen(r, "fresh", 9, OutcomeEvent{Kind: OutcomeFailed, InvocationID: "fresh", ErrorKind: "llm_error"})
			if got := r.attempts[9]; got != 1 {
				t.Fatalf("fresh failure attempts = %d, want 1", got)
			}
		})
	}
}

func TestEscalationResetsRetryBudget(t *testing.T) {
	for _, tc := range []struct {
		name     string
		escalate func(r *OutcomeReactor)
	}{
		{"retries exhausted", func(r *OutcomeReactor) {
			for i := 0; i < 3; i++ {
				inv := fmt.Sprintf("transient-%d", i)
				triggeredThen(r, inv, 7, OutcomeEvent{Kind: OutcomeFailed, InvocationID: inv, ErrorKind: "llm_error"})
			}
		}},
		{"terminal error", func(r *OutcomeReactor) {
			triggeredThen(r, "prior", 7, OutcomeEvent{Kind: OutcomeFailed, InvocationID: "prior", ErrorKind: "llm_error"})
			triggeredThen(r, "term", 7, OutcomeEvent{Kind: OutcomeFailed, InvocationID: "term", ErrorKind: "budget_exceeded"})
		}},
		{"ambiguous recovery", func(r *OutcomeReactor) {
			triggeredThen(r, "prior", 7, OutcomeEvent{Kind: OutcomeFailed, InvocationID: "prior", ErrorKind: "llm_error"})
			triggeredThen(r, "amb", 7, OutcomeEvent{Kind: OutcomeAmbiguous, InvocationID: "amb"})
		}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			src := &labelSource{}
			r := NewOutcomeReactor(src, outcomeConfig(), discardLogger())
			tc.escalate(r)
			if _, ok := r.attempts[7]; ok {
				t.Fatal("attempts entry survived escalation to the failed label")
			}
			// An operator re-queue after escalation starts with a fresh
			// budget: the next transient failure re-queues for retry
			// instead of escalating straight back to the failed label.
			triggeredThen(r, "requeued", 7, OutcomeEvent{Kind: OutcomeFailed, InvocationID: "requeued", ErrorKind: "llm_error"})
			if got := r.attempts[7]; got != 1 {
				t.Fatalf("post-escalation failure attempts = %d, want 1", got)
			}
			if want := "relabel #7 in-progress->ready"; src.ops[len(src.ops)-1] != want {
				t.Fatalf("last op = %q, want %q", src.ops[len(src.ops)-1], want)
			}
		})
	}
}
