package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"log/slog"
	"sort"
	"strings"
	"time"
)

// AreasPath and PolicyPath are the two declarations the verdict is
// computed from. Both are read from the PR's BASE ref, never its head:
// reading them from the head would let a PR ship a policy that blesses
// itself, which is the one thing this sweep must not allow.
const (
	AreasPath  = ".github/areas.yml"
	PolicyPath = ".github/merge-policy.yml"
)

// mergeVerdictMarker makes the verdict comment idempotent, the same way
// provenanceMarker makes the PR-body stamp idempotent: the sweep finds
// its own comment by this marker and edits it, so a PR carries exactly
// one verdict comment however many times it is swept.
const mergeVerdictMarker = "<!-- fq-merge-verdict -->"

// maxVerdictFileRows caps the rendered file table. A fleet PR is normally
// a handful of files; the cap keeps a wide one from burying the rule that
// decided it, and the deciding file is named in the rule regardless.
const maxVerdictFileRows = 40

// PullRequest is the cheap projection of an open PR from the listing: it
// is everything needed to decide whether the PR is in scope (a fleet PR)
// and whether its verdict is already current, before spending a request
// on the full facts.
type PullRequest struct {
	Number  int
	HeadSHA string
	BaseRef string
	Body    string
}

// PRComment is one issue comment on a PR — the sweep's own, found by its
// marker.
type PRComment struct {
	ID   int64
	Body string
}

// MergeVerdictSource is the seam over GitHub for the merge-verdict sweep.
// It is separate from IssueSource and ReviewSource so the label state
// machine keeps its minimal contract: a watcher without one simply never
// sweeps. Everything here is PR-shaped; nothing here decides anything.
type MergeVerdictSource interface {
	// ListOpenPullRequests returns the open PRs, cheaply.
	ListOpenPullRequests(ctx context.Context) ([]PullRequest, error)
	// PRFactsFor gathers the full per-PR facts a verdict needs.
	PRFactsFor(ctx context.Context, pr PullRequest) (PRFacts, error)
	// RepoFileAtRef reads a tracked file's contents at a git ref.
	RepoFileAtRef(ctx context.Context, path, ref string) ([]byte, error)
	// RepoLabels returns the labels that exist in the repository.
	RepoLabels(ctx context.Context) ([]string, error)
	// SetPRLabels adds one label and removes the others, tolerating a
	// label that was not there.
	SetPRLabels(ctx context.Context, pr int, add string, remove []string) error
	// RemovePRLabel removes one label, tolerating its absence.
	RemovePRLabel(ctx context.Context, pr int, label string) error
	// FindPRComment returns the PR's first comment containing marker.
	FindPRComment(ctx context.Context, pr int, marker string) (PRComment, bool, error)
	// CreatePRComment posts a new comment on the PR.
	CreatePRComment(ctx context.Context, pr int, body string) error
	// UpdatePRComment replaces an existing comment's body.
	UpdatePRComment(ctx context.Context, id int64, body string) error
}

// MergeVerdictSweeper gives every open fleet PR one advisory merge
// verdict per poll: a label from TierLabels and one comment, rewritten in
// place when the head SHA or the policy changes and left alone otherwise.
//
// It never writes anything but that label and that comment, never merges,
// and never fails a poll cycle: every error is logged and the next PR is
// tried. The verdict it renders is Verdict's, unchanged — the sweep's own
// job is only to gather facts and be idempotent about the two writes.
type MergeVerdictSweeper struct {
	Source MergeVerdictSource
	Config Config
	Log    *slog.Logger
	// Now supplies the observation time for the age check; nil means
	// time.Now. A seam for tests only.
	Now func() time.Time

	// Rubric, when set, adds the second (non-deterministic) verdict to
	// the same comment: a TypeSafe System One scorer over
	// .github/merge-rubric.yml. nil means the token is absent, which is
	// reported once at startup and then in every comment — never as an
	// error, and never with any effect on the tier.
	Rubric *RubricScorer

	// scored remembers the cache key (head SHA + policy digest) each PR
	// was last scored at, so a PR nobody pushed to costs one listing
	// entry per poll and no requests at all. In memory on purpose: a
	// restart re-scores everything once, and the comment upsert compares
	// bodies, so a re-score that changes nothing writes nothing.
	scored map[int]string
}

// NewMergeVerdictSweeper constructs a sweeper over a MergeVerdictSource.
func NewMergeVerdictSweeper(src MergeVerdictSource, cfg Config, log *slog.Logger) *MergeVerdictSweeper {
	return &MergeVerdictSweeper{Source: src, Config: cfg, Log: log, scored: make(map[int]string)}
}

// Sweep runs one pass over the open PRs. It returns nothing: this is the
// advisory step of a poll cycle and must never be able to fail one.
func (s *MergeVerdictSweeper) Sweep(ctx context.Context) {
	prs, err := s.Source.ListOpenPullRequests(ctx)
	if err != nil {
		s.Log.Error("list open PRs failed; skipping merge-verdict sweep this poll", "err", err)
		return
	}
	labels, err := s.Source.RepoLabels(ctx)
	if err != nil {
		s.Log.Error("list repository labels failed; skipping merge-verdict sweep this poll", "err", err)
		return
	}
	if missing := missingLabels(labels, s.Rubric != nil); len(missing) > 0 {
		s.Log.Error("merge verdict labels do not exist in the repository; skipping the sweep — create them (see the adapter README)",
			"missing", strings.Join(missing, ", "))
		return
	}
	policies := make(map[string]*loadedPolicy)
	for _, pr := range prs {
		if !strings.Contains(pr.Body, provenanceMarker) {
			continue
		}
		loaded := s.policyFor(ctx, pr.BaseRef, policies)
		if loaded.err != nil {
			continue
		}
		key := pr.HeadSHA + "/" + loaded.digest
		if s.scored[pr.Number] == key {
			continue
		}
		if err := s.scorePR(ctx, pr, loaded); err != nil {
			s.Log.Error("merge verdict failed; PR left as it was", "pr", pr.Number, "err", err)
			continue
		}
		s.scored[pr.Number] = key
	}
}

// loadedPolicy is one base ref's declarations, loaded once per sweep. The
// digest covers both files, so an edit to either re-scores every PR based
// on that ref without anyone having to push to them.
type loadedPolicy struct {
	policy Policy
	areas  Areas
	rubric Rubric
	// rubricErr is kept apart from err: a rubric that will not load must
	// degrade the rubric half of the comment, never withhold verdict 1.
	rubricErr error
	digest    string
	ref       string
	err       error
}

func (s *MergeVerdictSweeper) policyFor(ctx context.Context, ref string, cache map[string]*loadedPolicy) *loadedPolicy {
	if hit, ok := cache[ref]; ok {
		return hit
	}
	loaded := s.loadPolicy(ctx, ref)
	if loaded.err != nil {
		// Once per ref per poll, not once per PR: a malformed policy
		// would otherwise print the same refusal a dozen times.
		s.Log.Error("merge policy unusable at this ref; no verdict for PRs based on it",
			"ref", ref, "err", loaded.err)
	}
	cache[ref] = loaded
	return loaded
}

func (s *MergeVerdictSweeper) loadPolicy(ctx context.Context, ref string) *loadedPolicy {
	areasRaw, err := s.Source.RepoFileAtRef(ctx, AreasPath, ref)
	if err != nil {
		return &loadedPolicy{ref: ref, err: fmt.Errorf("read %s@%s: %w", AreasPath, ref, err)}
	}
	policyRaw, err := s.Source.RepoFileAtRef(ctx, PolicyPath, ref)
	if err != nil {
		return &loadedPolicy{ref: ref, err: fmt.Errorf("read %s@%s: %w", PolicyPath, ref, err)}
	}
	areas, err := ParseAreas(areasRaw)
	if err != nil {
		return &loadedPolicy{ref: ref, err: err}
	}
	policy, err := ParsePolicy(policyRaw, areas)
	if err != nil {
		return &loadedPolicy{ref: ref, err: err}
	}
	policy.InReviewLabel = s.Config.InReviewLabel
	policy.HoldLabel = s.Config.HoldLabel
	rubric, rubricRaw, rubricErr := s.loadRubric(ctx, ref)
	// The digest covers all three files, so editing any of them
	// re-scores every PR based on this ref without anyone pushing to them.
	sum := sha256.Sum256(bytes.Join([][]byte{areasRaw, policyRaw, rubricRaw}, nil))
	return &loadedPolicy{
		policy: policy, areas: areas, rubric: rubric, rubricErr: rubricErr,
		digest: hex.EncodeToString(sum[:8]), ref: ref,
	}
}

// loadRubric reads the rubric declaration, returning any failure rather
// than raising it: verdict 1 must land whatever state the rubric file is in.
func (s *MergeVerdictSweeper) loadRubric(ctx context.Context, ref string) (Rubric, []byte, error) {
	if s.Rubric == nil {
		return Rubric{}, nil, nil
	}
	raw, err := s.Source.RepoFileAtRef(ctx, RubricPath, ref)
	if err != nil {
		return Rubric{}, nil, fmt.Errorf("read %s@%s: %w", RubricPath, ref, err)
	}
	rubric, err := ParseRubric(raw)
	if err != nil {
		return Rubric{}, raw, err
	}
	return rubric, raw, nil
}

func (s *MergeVerdictSweeper) scorePR(ctx context.Context, pr PullRequest, loaded *loadedPolicy) error {
	facts, err := s.Source.PRFactsFor(ctx, pr)
	if err != nil {
		return fmt.Errorf("gather facts: %w", err)
	}
	facts.ObservedAt = s.now()
	decision := Verdict(loaded.policy, loaded.areas, facts)
	// Verdict 1 is labelled before the rubric is asked anything, so a
	// slow or failing scorer cannot delay or withhold it.
	if err := s.Source.SetPRLabels(ctx, pr.Number, decision.Tier.Label(), otherTierLabels(decision.Tier)); err != nil {
		return fmt.Errorf("apply %s: %w", decision.Tier.Label(), err)
	}
	rubric := ScoreRubric(ctx, s.Rubric, loaded.rubric, loaded.rubricErr, RubricState(facts, decision.Files))
	if s.Rubric != nil {
		if err := s.setRubricLabel(ctx, pr.Number, rubric.Flagged); err != nil {
			s.Log.Error("applying the rubric label failed; the verdict comment still lands", "pr", pr.Number, "err", err)
		}
	}
	body := renderVerdictComment(decision, facts, loaded, rubric)
	if err := s.upsertComment(ctx, pr.Number, body); err != nil {
		return fmt.Errorf("write verdict comment: %w", err)
	}
	s.Log.Info("merge verdict", "pr", pr.Number, "tier", decision.Tier.String(),
		"head", shortSHA(facts.HeadSHA), "rule", decision.Rule,
		"rubric", rubricLogValue(rubric))
	return nil
}

// setRubricLabel adds or removes RubricFlagLabel. Removing matters as
// much as adding: a push that answers the concern must clear the flag, or
// the label outlives the state it described.
func (s *MergeVerdictSweeper) setRubricLabel(ctx context.Context, pr int, flagged bool) error {
	if flagged {
		return s.Source.SetPRLabels(ctx, pr, RubricFlagLabel, nil)
	}
	return s.Source.RemovePRLabel(ctx, pr, RubricFlagLabel)
}

func rubricLogValue(r RubricResult) string {
	if r.Reason != "" {
		return r.Reason
	}
	if r.Flagged {
		return "flagged"
	}
	return "clear"
}

// upsertComment writes the verdict comment exactly once per PR, and
// rewrites it only when its text actually changed. Comparing the rendered
// body rather than trusting the cache is what keeps a watcher restart —
// which forgets every cache entry — from editing every open PR's comment
// to the identical text and notifying everyone watching it.
func (s *MergeVerdictSweeper) upsertComment(ctx context.Context, pr int, body string) error {
	existing, found, err := s.Source.FindPRComment(ctx, pr, mergeVerdictMarker)
	if err != nil {
		return err
	}
	if !found {
		return s.Source.CreatePRComment(ctx, pr, body)
	}
	if strings.TrimSpace(existing.Body) == strings.TrimSpace(body) {
		return nil
	}
	return s.Source.UpdatePRComment(ctx, existing.ID, body)
}

func (s *MergeVerdictSweeper) now() time.Time {
	if s.Now != nil {
		return s.Now()
	}
	return time.Now()
}

// missingLabels names the verdict labels the repository does not have.
// The sweep refuses to run without all three rather than applying the one
// that happens to exist, which would leave a PR carrying a stale verdict
// the sweep could not remove.
func missingLabels(existing []string, withRubric bool) []string {
	have := make(map[string]bool, len(existing))
	for _, l := range existing {
		have[l] = true
	}
	want := TierLabels
	if withRubric {
		want = append(append([]string{}, TierLabels...), RubricFlagLabel)
	}
	var missing []string
	for _, l := range want {
		if !have[l] {
			missing = append(missing, l)
		}
	}
	sort.Strings(missing)
	return missing
}

func otherTierLabels(tier Tier) []string {
	var others []string
	for _, l := range TierLabels {
		if l != tier.Label() {
			others = append(others, l)
		}
	}
	return others
}

// renderVerdictComment is the whole comment: what was decided, from what,
// and what a human would look at anyway. It is a pure function of the
// decision and the facts so the body can be compared against what is
// already on the PR.
func renderVerdictComment(d Decision, f PRFacts, loaded *loadedPolicy, rubric RubricResult) string {
	var b strings.Builder
	fmt.Fprintf(&b, "%s\n### 🤖 factor-q merge verdict — `%s`\n\n", mergeVerdictMarker, d.Tier.Label())
	fmt.Fprintf(&b, "Head `%s` · base `%s` · policy `%s`@`%s` (`%s`)\n\n",
		shortSHA(f.HeadSHA), f.BaseRef, PolicyPath, loaded.ref, loaded.digest)
	fmt.Fprintf(&b, "**Rule.** %s\n\n", d.Rule)
	writeFileTable(&b, d)
	writeCheckTable(&b, d)
	writeRubric(&b, rubric)
	b.WriteString("\n**Advisory.** Nothing merges automatically. The tier says how much " +
		"human supervision a change to these areas needs; the checks are reported, " +
		"not enforced. Calibration is the point — see " +
		"<https://github.com/bricef/factor-q/issues/879>. This comment is rewritten " +
		"in place when the head SHA or the policy changes.\n")
	return b.String()
}

func writeFileTable(b *strings.Builder, d Decision) {
	b.WriteString("| changed file | areas | tier |\n|---|---|---|\n")
	shown := d.Files
	if len(shown) > maxVerdictFileRows {
		shown = shown[:maxVerdictFileRows]
	}
	for _, row := range shown {
		areas := "*(none — default tier)*"
		if len(row.Areas) > 0 {
			areas = "`" + strings.Join(row.Areas, "`, `") + "`"
		}
		fmt.Fprintf(b, "| `%s` | %s | `%s` |\n", row.Path, areas, row.Tier)
	}
	if len(d.Files) > len(shown) {
		fmt.Fprintf(b, "\n*… and %d more changed file(s); the rule above names the one that decided.*\n", len(d.Files)-len(shown))
	}
	b.WriteString("\n")
}

func writeCheckTable(b *strings.Builder, d Decision) {
	b.WriteString("| structural check | result | detail |\n|---|---|---|\n")
	for _, c := range d.Checks {
		mark := "❌"
		if c.Pass {
			mark = "✅"
		}
		fmt.Fprintf(b, "| `%s` | %s | %s |\n", c.Name, mark, c.Reason)
	}
}

// writeRubric renders the second verdict, or the one line saying why
// there is none. The two verdicts share one comment on purpose: a
// reviewer reads one thing per PR, and the rubric's probabilities mean
// something only next to the areas the change touched.
func writeRubric(b *strings.Builder, r RubricResult) {
	b.WriteString("\n**Rubric (Jev).** ")
	if len(r.Answers) == 0 {
		fmt.Fprintf(b, "%s\n", r.Reason)
		return
	}
	verdict := "nothing reaches the threshold"
	if r.Flagged {
		verdict = "**flagged** — at least one question reaches the threshold"
	}
	fmt.Fprintf(b, "%s (≥ %.2f). Probabilities are calibrated and carry no rationale; they restrict, never construct.\n\n", verdict, r.Threshold)
	b.WriteString("| question | P(yes) | |\n|---|---|---|\n")
	for _, a := range r.Answers {
		mark := ""
		if a.Probability >= r.Threshold {
			mark = "⚑"
		}
		fmt.Fprintf(b, "| `%s` | %.2f | %s |\n", a.ID, a.Probability, mark)
	}
}

func shortSHA(sha string) string {
	if len(sha) > 12 {
		return sha[:12]
	}
	if sha == "" {
		return "unknown"
	}
	return sha
}
