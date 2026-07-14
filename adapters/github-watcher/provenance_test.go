package main

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"testing"
	"time"
)

// fakeStamper is an in-memory ProvenanceStamper: PR bodies keyed by PR
// number, with injectable failures per method.
type fakeStamper struct {
	prsByIssue map[int][]int
	bodies     map[int]string
	sets       int
	listErr    error
	bodyErr    error
	setErr     error
}

func (s *fakeStamper) OpenPRsClosingIssue(_ context.Context, issue int) ([]int, error) {
	if s.listErr != nil {
		return nil, s.listErr
	}
	return s.prsByIssue[issue], nil
}

func (s *fakeStamper) PRBody(_ context.Context, pr int) (string, error) {
	if s.bodyErr != nil {
		return "", s.bodyErr
	}
	return s.bodies[pr], nil
}

func (s *fakeStamper) SetPRBody(_ context.Context, pr int, body string) error {
	if s.setErr != nil {
		return s.setErr
	}
	s.bodies[pr] = body
	s.sets++
	return nil
}

func fixedNow() time.Time {
	return time.Date(2026, 7, 14, 9, 12, 0, 0, time.UTC)
}

// --- pure rendering / append ---

func TestProvenanceFooterCarriesAllFields(t *testing.T) {
	footer := provenanceFooter("m0-issue-fix", "inv-0198", 143, "status:ready", fixedNow())
	for _, want := range []string{
		provenanceMarker,
		"`m0-issue-fix`",
		"`inv-0198`",
		"#143",
		"status:ready",
		"2026-07-14T09:12:00Z",
	} {
		if !strings.Contains(footer, want) {
			t.Errorf("footer missing %q: %s", want, footer)
		}
	}
}

func TestAppendProvenanceAppendsOnceThenIsIdempotent(t *testing.T) {
	footer := provenanceFooter("a", "inv", 1, "status:ready", fixedNow())

	stamped, changed := appendProvenance("## Summary\nSome PR body\n", footer)
	if !changed {
		t.Fatal("first append must report a change")
	}
	if !strings.Contains(stamped, "Some PR body") || !strings.Contains(stamped, provenanceMarker) {
		t.Fatalf("stamped body must keep the original and add the marker: %s", stamped)
	}

	again, changed := appendProvenance(stamped, footer)
	if changed {
		t.Fatal("a body already carrying the marker must not be stamped again")
	}
	if again != stamped {
		t.Fatal("idempotent append must return the body unchanged")
	}
}

func TestAppendProvenanceOnEmptyBody(t *testing.T) {
	stamped, changed := appendProvenance("", "footer-line")
	if !changed || !strings.Contains(stamped, "footer-line") {
		t.Fatalf("empty body must still gain the footer: %q", stamped)
	}
}

// --- reactor integration ---

func stampReactor(src IssueSource, st ProvenanceStamper) *OutcomeReactor {
	r := NewOutcomeReactor(src, outcomeConfig(), discardLogger())
	r.Stamper = st
	r.Now = fixedNow
	return r
}

func TestCompletedStampsProvenanceOnTheOpenPR(t *testing.T) {
	src := &labelSource{}
	st := &fakeStamper{
		prsByIssue: map[int][]int{7: {41}},
		bodies:     map[int]string{41: "## Summary\nfix"},
	}
	r := stampReactor(src, st)

	triggeredThen(r, "inv-1", 7, OutcomeEvent{Kind: OutcomeCompleted, InvocationID: "inv-1"})

	if len(src.ops) != 1 || !strings.Contains(src.ops[0], "in-progress->in-review") {
		t.Fatalf("completed must still relabel to in-review: %v", src.ops)
	}
	body := st.bodies[41]
	for _, want := range []string{provenanceMarker, "inv-1", "#7", "## Summary"} {
		if !strings.Contains(body, want) {
			t.Errorf("stamped PR body missing %q: %s", want, body)
		}
	}
}

func TestStampIsIdempotentAcrossReobservedCompletions(t *testing.T) {
	src := &labelSource{}
	st := &fakeStamper{
		prsByIssue: map[int][]int{7: {41}},
		bodies:     map[int]string{41: "body\n\n---\n" + provenanceMarker + "\nstamped"},
	}
	r := stampReactor(src, st)

	triggeredThen(r, "inv-1", 7, OutcomeEvent{Kind: OutcomeCompleted, InvocationID: "inv-1"})

	if st.sets != 0 {
		t.Fatalf("a PR already carrying the marker must not be rewritten; got %d writes", st.sets)
	}
}

func TestStampFailureDoesNotBlockTheLabelTransition(t *testing.T) {
	src := &labelSource{}
	st := &fakeStamper{listErr: errors.New("graphql down")}
	r := stampReactor(src, st)

	triggeredThen(r, "inv-1", 7, OutcomeEvent{Kind: OutcomeCompleted, InvocationID: "inv-1"})

	if len(src.ops) != 1 || !strings.Contains(src.ops[0], "in-progress->in-review") {
		t.Fatalf("stamp failure must leave the relabel intact: %v", src.ops)
	}
}

func TestNilStamperSkipsStamping(t *testing.T) {
	src := &labelSource{}
	r := NewOutcomeReactor(src, outcomeConfig(), discardLogger())

	// Must not panic and must still relabel.
	triggeredThen(r, "inv-1", 7, OutcomeEvent{Kind: OutcomeCompleted, InvocationID: "inv-1"})
	if len(src.ops) != 1 {
		t.Fatalf("nil stamper must not affect the label machine: %v", src.ops)
	}
}

func TestStampCoversEveryOpenClosingPR(t *testing.T) {
	src := &labelSource{}
	st := &fakeStamper{
		prsByIssue: map[int][]int{7: {41, 42}},
		bodies:     map[int]string{41: "a", 42: "b"},
	}
	r := stampReactor(src, st)

	triggeredThen(r, "inv-1", 7, OutcomeEvent{Kind: OutcomeCompleted, InvocationID: "inv-1"})

	if st.sets != 2 {
		t.Fatalf("both open PRs must be stamped, got %d writes", st.sets)
	}
}

// --- GraphQL response parsing ---

func TestParseOpenPRResponseFiltersToOpenState(t *testing.T) {
	stdout := []byte(`{"data":{"repository":{"issue":{"closedByPullRequestsReferences":{"nodes":[
		{"number":41,"state":"OPEN"},
		{"number":40,"state":"MERGED"},
		{"number":39,"state":"CLOSED"}
	]}}}}}`)
	prs, err := parseOpenPRResponse(stdout)
	if err != nil {
		t.Fatal(err)
	}
	if fmt.Sprint(prs) != "[41]" {
		t.Fatalf("want only the open PR, got %v", prs)
	}
}

func TestParseOpenPRResponseEmpty(t *testing.T) {
	prs, err := parseOpenPRResponse([]byte(`{"data":{"repository":{"issue":{"closedByPullRequestsReferences":{"nodes":[]}}}}}`))
	if err != nil {
		t.Fatal(err)
	}
	if len(prs) != 0 {
		t.Fatalf("want none, got %v", prs)
	}
}
