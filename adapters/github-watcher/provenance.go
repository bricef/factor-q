package main

import (
	"context"
	"fmt"
	"regexp"
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

var (
	agentProvenanceLine   = regexp.MustCompile(`(?m)^provenance: agent=(\S+) invocation=(\S+)`)
	footerProvenanceAgent = regexp.MustCompile(`(?m)^🤖 \*\*factor-q provenance\*\* — agent: ` + "`([^`]+)`")
)

// Provenance identifies which fleet provenance representation a PR body
// carries. AgentID comes from the watcher-stamped footer whenever the
// footer names an agent, and from the agent-written line otherwise: the
// footer is the only half the watcher wrote itself, so when the two
// disagree the footer is the trustworthy one. Form still reports both.
type Provenance struct {
	Form    string
	AgentID string
}

// parseProvenance is the single definition of a fleet PR. During the
// migration both the watcher footer and the line agents write themselves
// are authoritative.
//
// The footer is looked for in the raw body — the marker is itself an HTML
// comment — while the agent line is only read from prose: a line inside a
// code fence or an HTML comment is quoted, not claimed, so a human PR
// pasting a fleet body is not swept as fleet.
func parseProvenance(body string) Provenance {
	hasFooter := strings.Contains(body, provenanceMarker)
	line := agentProvenanceLine.FindStringSubmatch(provenanceProse(body))
	hasLine := len(line) > 0

	form := "none"
	switch {
	case hasFooter && hasLine:
		form = "both"
	case hasFooter:
		form = "footer"
	case hasLine:
		form = "line"
	}

	agentID := ""
	if footer := footerProvenanceAgent.FindStringSubmatch(body); len(footer) > 0 {
		agentID = footer[1]
	} else if hasLine {
		agentID = line[1]
	}
	return Provenance{Form: form, AgentID: agentID}
}

// provenanceProse blanks the spans of a body an agent line must not be
// read from — fenced code blocks (``` or ~~~) and HTML comments — keeping
// one output line per input line so the line regexp's anchors still land
// where they did. Line-based on purpose: a markdown parser would be a
// dependency and a second definition of what a fence is, and the only
// question here is whether a `provenance:` line is prose.
func provenanceProse(body string) string {
	lines := strings.Split(body, "\n")
	out := make([]string, len(lines))
	fence := ""
	inComment := false
	for i, line := range lines {
		if fence != "" {
			if run := fenceRun(line); run != "" && run[0] == fence[0] && len(run) >= len(fence) {
				fence = ""
			}
			continue
		}
		var visible string
		visible, inComment = stripHTMLComments(line, inComment)
		if run := fenceRun(line); run != "" {
			fence = run
			continue
		}
		out[i] = visible
	}
	return strings.Join(out, "\n")
}

// fenceRun returns the leading ``` or ~~~ run of a line, or "" when the
// line opens or closes no fence.
func fenceRun(line string) string {
	trimmed := strings.TrimLeft(line, " \t")
	for _, ch := range []byte{'`', '~'} {
		n := 0
		for n < len(trimmed) && trimmed[n] == ch {
			n++
		}
		if n >= 3 {
			return trimmed[:n]
		}
	}
	return ""
}

// stripHTMLComments removes the commented spans of one line, carrying the
// "still inside a comment" state to the next. The watcher's own marker is
// a comment too, but the footer is matched against the raw body, so
// blanking it here costs nothing.
func stripHTMLComments(line string, inComment bool) (string, bool) {
	var visible strings.Builder
	for line != "" {
		if inComment {
			end := strings.Index(line, "-->")
			if end < 0 {
				return visible.String(), true
			}
			line = line[end+len("-->"):]
			inComment = false
			continue
		}
		start := strings.Index(line, "<!--")
		if start < 0 {
			visible.WriteString(line)
			return visible.String(), false
		}
		visible.WriteString(line[:start])
		line = line[start+len("<!--"):]
		inComment = true
	}
	return visible.String(), inComment
}

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
