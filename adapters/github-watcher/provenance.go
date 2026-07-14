package main

import (
	"context"
	"fmt"
	"strings"
	"time"
)

// ProvenanceStamper is the seam over GitHub for stamping invocation
// provenance on the PR an agent opened (issue #162). Separate from
// IssueSource/ReviewSource so the label state machine keeps its minimal
// contract; a reactor without one simply skips stamping.
type ProvenanceStamper interface {
	// OpenPRsClosingIssue returns the numbers of *open* PRs that close
	// the issue (a PR body carrying `Closes #N`).
	OpenPRsClosingIssue(ctx context.Context, issue int) ([]int, error)
	// PRBody returns the PR's current body.
	PRBody(ctx context.Context, pr int) (string, error)
	// SetPRBody replaces the PR's body.
	SetPRBody(ctx context.Context, pr int, body string) error
}

// provenanceMarker makes the stamp idempotent: a body already carrying
// it is never stamped again, however many times the completed event is
// observed (watcher restarts, retriggers of the same issue).
const provenanceMarker = "<!-- fq-provenance -->"

// provenanceFooter renders the machine-authored provenance block for a
// PR body. The invocation id is the operator's handle for `fq
// invocation show` / the dashboard; the agent id and trigger issue
// close the loop back to the label state machine that dispatched it.
func provenanceFooter(agentID, invocationID string, issue int, readyLabel string, completedAt time.Time) string {
	return fmt.Sprintf(
		"%s\n🤖 **factor-q provenance** — agent: `%s` · invocation: `%s` · trigger: #%d (%s) · completed: %s",
		provenanceMarker,
		agentID,
		invocationID,
		issue,
		readyLabel,
		completedAt.UTC().Format(time.RFC3339),
	)
}

// appendProvenance returns the body with the footer appended, and
// whether an append happened. A body already carrying the marker is
// returned unchanged (false) — the stamp is one-shot per PR.
func appendProvenance(body, footer string) (string, bool) {
	if strings.Contains(body, provenanceMarker) {
		return body, false
	}
	trimmed := strings.TrimRight(body, "\n ")
	if trimmed == "" {
		return "---\n" + footer + "\n", true
	}
	return trimmed + "\n\n---\n" + footer + "\n", true
}
