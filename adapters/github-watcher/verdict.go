package main

import (
	"fmt"
	"sort"
	"strings"
	"time"
)

// ClosingIssue is an issue a PR closes, with the labels it carries —
// enough to answer "does this PR finish exactly one issue, and was that
// issue in review?" without a second lookup.
type ClosingIssue struct {
	Number int      `json:"number"`
	Labels []string `json:"labels,omitempty"`
}

// PRFacts is everything the verdict is computed from: the facts about one
// pull request as observed at ObservedAt. It carries no GitHub client and
// no policy — Verdict is pure over this struct, which is what lets the
// live sweep and the replay over already-merged PRs run the same code.
//
// The JSON tags are the `verdict` subcommand's wire format (verdictcmd.go):
// the one seam through which anything but the poll loop computes a
// verdict, so the replay measures this code rather than a second
// implementation of the rules.
type PRFacts struct {
	Number         int            `json:"number"`
	HeadSHA        string         `json:"head_sha,omitempty"`
	BaseRef        string         `json:"base_ref,omitempty"`
	Body           string         `json:"body,omitempty"`
	Files          []string       `json:"files,omitempty"`
	CommitCount    int            `json:"commit_count"`
	Labels         []string       `json:"labels,omitempty"`
	MergeableState string         `json:"mergeable_state,omitempty"`
	ChangesRequest bool           `json:"changes_requested"` // a review in the CHANGES_REQUESTED state
	CreatedAt      time.Time      `json:"created_at,omitempty"`
	ClosingIssues  []ClosingIssue `json:"closing_issues,omitempty"`
	ObservedAt     time.Time      `json:"observed_at,omitempty"`
}

// FileTier is one row of a verdict's file table: a changed file, the
// policy areas it belongs to, and the tier those areas give it.
type FileTier struct {
	Path  string   `json:"path"`
	Areas []string `json:"areas,omitempty"` // the areas the policy maps; empty means the default tier applied
	Tier  Tier     `json:"tier"`
}

// Check is one structural check's result. Checks are reported, never
// enforced: the sweep is advisory, and a failing check is a reason a
// human looks, not a verdict of its own.
type Check struct {
	Name   string `json:"name"`
	Pass   bool   `json:"pass"`
	Reason string `json:"reason"`
}

// Decision is a full verdict: the tier, the file table it came from, the
// one rule that decided it, and every structural check.
type Decision struct {
	Tier   Tier
	Files  []FileTier
	Rule   string
	Checks []Check
}

// Structural check names. They are a stable vocabulary: the replay CSV
// has one column per name, so renaming one invalidates the comparison
// against earlier runs.
const (
	CheckProvenance      = "provenance"
	CheckClosingIssue    = "closing-issue"
	CheckMergeable       = "mergeable"
	CheckSingleCommit    = "single-commit"
	CheckNoChangesReq    = "no-changes-requested"
	CheckNoHoldLabel     = "no-hold-label"
	CheckMinAge          = "min-age"
	defaultInReviewLabel = "status:in-review"
	defaultHoldLabel     = "hold"
)

// CheckNames is the order every verdict reports its checks in.
var CheckNames = []string{
	CheckProvenance, CheckClosingIssue, CheckMergeable,
	CheckSingleCommit, CheckNoChangesReq, CheckNoHoldLabel, CheckMinAge,
}

// Verdict is the whole deterministic verdict, pure over its inputs.
//
// The tier is the most restrictive tier of any changed file; a file that
// matches no area the policy maps takes the policy's default tier. That
// direction is the safety property: adding a file to a PR can only ever
// make its verdict stricter, so a PR cannot dilute itself, and the areas
// covering the policy and the sweep's own sources are `never` so it
// cannot widen its own merge rights either.
//
// The structural checks are computed alongside and reported in full. None
// of them changes the tier. They are what a human would look at before
// merging something the policy already allows, and while the sweep is a
// measurement rather than a gate they exist to be calibrated, not obeyed.
func Verdict(policy Policy, areas Areas, facts PRFacts) Decision {
	files := fileTiers(policy, areas, facts.Files)
	tier, rule := decide(policy, files)
	return Decision{Tier: tier, Files: files, Rule: rule, Checks: structuralChecks(policy, facts)}
}

// fileTiers maps each changed file to the areas the policy knows about
// and the tier they imply, sorted by path so two runs over the same facts
// render byte-identically.
func fileTiers(policy Policy, areas Areas, paths []string) []FileTier {
	out := make([]FileTier, 0, len(paths))
	for _, path := range paths {
		row := FileTier{Path: path, Tier: policy.DefaultTier}
		for _, area := range areas.Match(path) {
			tier, mapped := policy.Areas[area]
			if !mapped {
				continue
			}
			if len(row.Areas) == 0 {
				row.Tier = tier
			} else if tier > row.Tier {
				row.Tier = tier
			}
			row.Areas = append(row.Areas, area)
		}
		out = append(out, row)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Path < out[j].Path })
	return out
}

// decide picks the most restrictive file tier and names the file that set
// it, so the comment can say which one line of the diff decided.
func decide(policy Policy, files []FileTier) (Tier, string) {
	if len(files) == 0 {
		return policy.DefaultTier, fmt.Sprintf(
			"no changed files, so the default tier `%s` applies", policy.DefaultTier)
	}
	worst := files[0]
	for _, f := range files[1:] {
		if f.Tier > worst.Tier {
			worst = f
		}
	}
	if len(worst.Areas) == 0 {
		return worst.Tier, fmt.Sprintf(
			"`%s` decides: it matches no area the policy maps, so the default tier `%s` applies — the most restrictive of %d changed file(s)",
			worst.Path, worst.Tier, len(files))
	}
	return worst.Tier, fmt.Sprintf(
		"`%s` decides: area `%s` is `%s` — the most restrictive of %d changed file(s)",
		worst.Path, strictestArea(policy, worst), worst.Tier, len(files))
}

// strictestArea names the area that gave the deciding file its tier. A
// file in several mapped areas is decided by the strictest of them, and
// the comment has to say which, or the explanation names an area that
// does not imply the tier it claims.
func strictestArea(policy Policy, f FileTier) string {
	for _, area := range f.Areas {
		if policy.Areas[area] == f.Tier {
			return area
		}
	}
	return strings.Join(f.Areas, ", ")
}

// structuralChecks computes every check, in CheckNames order. Each says
// why, pass or fail, because "single-commit: fail" without "4 commits"
// makes a reader open the PR to learn what the sweep already knew.
func structuralChecks(policy Policy, f PRFacts) []Check {
	age := f.ObservedAt.Sub(f.CreatedAt)
	return []Check{
		provenanceCheck(f),
		closingIssueCheck(policy, f),
		{CheckMergeable, f.MergeableState == "clean",
			fmt.Sprintf("mergeable_state is %s", orUnknown(f.MergeableState))},
		{CheckSingleCommit, f.CommitCount == 1,
			fmt.Sprintf("%d commit(s)", f.CommitCount)},
		{CheckNoChangesReq, !f.ChangesRequest, changesRequestedReason(f)},
		{CheckNoHoldLabel, !hasLabel(f.Labels, holdLabel(policy)),
			fmt.Sprintf("%q label is %s", holdLabel(policy), presence(hasLabel(f.Labels, holdLabel(policy))))},
		{CheckMinAge, age >= policy.MinAge,
			fmt.Sprintf("opened %s ago (minimum %s)", age.Round(time.Minute), policy.MinAge)},
	}
}

func provenanceCheck(f PRFacts) Check {
	if strings.Contains(f.Body, provenanceMarker) {
		return Check{CheckProvenance, true, "body carries the factor-q provenance footer"}
	}
	return Check{CheckProvenance, false, "no provenance footer in the body"}
}

// closingIssueCheck folds "closes exactly one issue" and "that issue is in
// review" into one check, because either half alone is not a fact anyone
// acts on: a PR closing two issues has no single issue whose state to read.
func closingIssueCheck(policy Policy, f PRFacts) Check {
	label := inReviewLabel(policy)
	switch len(f.ClosingIssues) {
	case 0:
		return Check{CheckClosingIssue, false, "closes no issue"}
	case 1:
		issue := f.ClosingIssues[0]
		if hasLabel(issue.Labels, label) {
			return Check{CheckClosingIssue, true, fmt.Sprintf("closes #%d, which carries %q", issue.Number, label)}
		}
		return Check{CheckClosingIssue, false,
			fmt.Sprintf("closes #%d, which does not carry %q (labels: %s)", issue.Number, label, orNone(issue.Labels))}
	default:
		nums := make([]string, 0, len(f.ClosingIssues))
		for _, issue := range f.ClosingIssues {
			nums = append(nums, fmt.Sprintf("#%d", issue.Number))
		}
		return Check{CheckClosingIssue, false,
			fmt.Sprintf("closes %d issues (%s)", len(nums), strings.Join(nums, ", "))}
	}
}

func changesRequestedReason(f PRFacts) string {
	if f.ChangesRequest {
		return "a review requests changes"
	}
	return "no review requests changes"
}

// inReviewLabel and holdLabel fall back to the repository's own defaults
// when the caller left them unset, so a hand-built Policy in a test or in
// the `verdict` seam still reports the checks the sweep would.
func inReviewLabel(p Policy) string {
	if p.InReviewLabel == "" {
		return defaultInReviewLabel
	}
	return p.InReviewLabel
}

func holdLabel(p Policy) string {
	if p.HoldLabel == "" {
		return defaultHoldLabel
	}
	return p.HoldLabel
}

func hasLabel(labels []string, want string) bool {
	for _, l := range labels {
		if l == want {
			return true
		}
	}
	return false
}

func presence(present bool) string {
	if present {
		return "present"
	}
	return "absent"
}

func orUnknown(s string) string {
	if s == "" {
		return "unknown"
	}
	return s
}

func orNone(labels []string) string {
	if len(labels) == 0 {
		return "none"
	}
	return strings.Join(labels, ", ")
}
