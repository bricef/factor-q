package main

import (
	"bufio"
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"os"
)

// The `verdict` subcommand: the one seam through which anything other
// than the poll loop can compute a merge verdict.
//
// It exists so the replay over already-merged PRs
// (scripts/merge-verdicts-replay.py) runs *this* verdict function rather
// than a second implementation of the rules in another language — the
// whole value of a replay is that it measures what the sweep would have
// said, and a reimplementation measures something else that happens to
// agree today.
//
// It makes no GitHub calls and reads no network: facts in, verdict out.
// Whoever has the facts — the sweep, a script, a test — brings them.
//
// Protocol: JSON Lines both ways. One PRFacts object per line of stdin,
// one verdict object per line of stdout, in the same order. A single
// object on a single line is the degenerate case, so a caller may batch
// or not without changing anything. A line that cannot be decoded is
// reported on that line's output as an `error` field rather than killing
// the batch, because one malformed PR in a replay of two hundred should
// cost one row.

// verdictResult is one line of the subcommand's output.
type verdictResult struct {
	Number         int           `json:"number"`
	Tier           string        `json:"tier,omitempty"`
	Rule           string        `json:"rule,omitempty"`
	Files          []FileTier    `json:"files,omitempty"`
	Checks         []Check       `json:"checks,omitempty"`
	ProvenanceForm string        `json:"provenance_form,omitempty"`
	AgentID        string        `json:"agent_id,omitempty"`
	Rubric         *RubricResult `json:"rubric,omitempty"`
	Error          string        `json:"error,omitempty"`
}

// runVerdictCmd implements `github-watcher verdict`.
func runVerdictCmd(args []string) error {
	fs := flag.NewFlagSet("github-watcher verdict", flag.ContinueOnError)
	areasPath := fs.String("areas", AreasPath, "path to the areas declaration")
	policyPath := fs.String("policy", PolicyPath, "path to the merge policy")
	inReview := fs.String("in-review-label", defaultInReviewLabel, "the label meaning an issue is in review")
	hold := fs.String("hold-label", defaultHoldLabel, "the label meaning a human is withholding a PR")
	rubricPath := fs.String("rubric", "", "path to the Jev rubric; empty skips the second verdict entirely")
	if err := fs.Parse(args); err != nil {
		return err
	}
	areasRaw, err := os.ReadFile(*areasPath)
	if err != nil {
		return fmt.Errorf("read %s: %w", *areasPath, err)
	}
	policyRaw, err := os.ReadFile(*policyPath)
	if err != nil {
		return fmt.Errorf("read %s: %w", *policyPath, err)
	}
	areas, err := ParseAreas(areasRaw)
	if err != nil {
		return err
	}
	policy, err := ParsePolicy(policyRaw, areas)
	if err != nil {
		return err
	}
	policy.InReviewLabel, policy.HoldLabel = *inReview, *hold
	rubric, scorer, err := rubricFor(*rubricPath)
	if err != nil {
		return err
	}
	return streamVerdicts(os.Stdin, os.Stdout, policy, areas, rubric, scorer)
}

// rubricFor loads the rubric only when one is asked for, and refuses
// loudly rather than degrading: a replay that silently produced no
// probabilities would be read as "the model was unsure", which is the one
// thing it must never be mistaken for. The live sweep degrades instead,
// because there a missing rubric must not withhold verdict 1.
func rubricFor(path string) (Rubric, *RubricScorer, error) {
	if path == "" {
		return Rubric{}, nil, nil
	}
	raw, err := os.ReadFile(path)
	if err != nil {
		return Rubric{}, nil, fmt.Errorf("read %s: %w", path, err)
	}
	rubric, err := ParseRubric(raw)
	if err != nil {
		return Rubric{}, nil, err
	}
	scorer, ok := NewRubricScorer()
	if !ok {
		return Rubric{}, nil, fmt.Errorf("--rubric needs %s in the environment", TypeSafeKeyEnv)
	}
	return rubric, scorer, nil
}

// streamVerdicts is the whole loop, taking its streams as arguments so a
// test drives it without touching the process's stdio.
func streamVerdicts(in io.Reader, out io.Writer, policy Policy, areas Areas, rubric Rubric, scorer *RubricScorer) error {
	scanner := bufio.NewScanner(in)
	// A PR's changed-file list is the long field; a megabyte of paths is
	// far beyond the largest PR this repo has seen (66 files) and far
	// under anything that would strain a poll.
	scanner.Buffer(make([]byte, 0, 64*1024), 4*1024*1024)
	enc := json.NewEncoder(out)
	for scanner.Scan() {
		line := scanner.Bytes()
		if len(trimSpaceBytes(line)) == 0 {
			continue
		}
		if err := enc.Encode(verdictFor(line, policy, areas, rubric, scorer)); err != nil {
			return err
		}
	}
	return scanner.Err()
}

func verdictFor(line []byte, policy Policy, areas Areas, rubric Rubric, scorer *RubricScorer) verdictResult {
	var facts PRFacts
	if err := json.Unmarshal(line, &facts); err != nil {
		return verdictResult{Error: fmt.Sprintf("decode facts: %v", err)}
	}
	if facts.ObservedAt.IsZero() {
		// A replay asks "what would the sweep have said?", and the only
		// honest observation moment for a PR that is already closed is
		// when it was opened plus however long it lived. A caller that
		// cares sets observed_at; one that does not gets the age check
		// measured from the PR's own creation, which reads as zero age
		// rather than as an accidental pass.
		facts.ObservedAt = facts.CreatedAt
	}
	d := Verdict(policy, areas, facts)
	result := verdictResult{
		Number: facts.Number,
		Tier:   d.Tier.String(), Rule: d.Rule, Files: d.Files, Checks: d.Checks,
		ProvenanceForm: d.ProvenanceForm, AgentID: d.AgentID,
	}
	if scorer != nil {
		scored := ScoreRubric(context.Background(), scorer, rubric, nil, RubricState(facts, d.Files))
		result.Rubric = &scored
	}
	return result
}

func trimSpaceBytes(b []byte) []byte {
	start, end := 0, len(b)
	for start < end && (b[start] == ' ' || b[start] == '\t' || b[start] == '\r' || b[start] == '\n') {
		start++
	}
	for end > start && (b[end-1] == ' ' || b[end-1] == '\t' || b[end-1] == '\r' || b[end-1] == '\n') {
		end--
	}
	return b[start:end]
}
