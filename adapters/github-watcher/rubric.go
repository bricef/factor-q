package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"sort"
	"time"

	"gopkg.in/yaml.v3"
)

// The second, non-deterministic half of the merge verdict: a small rubric
// of yes/no questions put to a TypeSafe System One model (Jev) over the
// PR's state, answered as calibrated probabilities with no rationale.
//
// It can only ever restrict, never construct, and it is never the safety
// boundary. Verdict 1 — areas and policy — is computed first and is
// unaffected by anything here, including this endpoint being down: a
// failure degrades to one line in the comment saying so.

// RubricPath is the rubric declaration, read from the PR's base ref like
// the policy and for the same reason.
const RubricPath = ".github/merge-rubric.yml"

// TypeSafeEndpoint and TypeSafeModel are the System One evaluation
// endpoint and the flagship model alias.
const (
	TypeSafeEndpoint = "https://api.typesafe.ai/v1/systemone"
	TypeSafeModel    = "jev-latest"
	// TypeSafeKeyEnv is the only place the token ever comes from: the
	// environment, delivered the way GH_TOKEN is. It is never in the repo.
	TypeSafeKeyEnv = "TYPESAFE_API_KEY"
	// rubricTimeout bounds the call. The sweep is advisory and runs every
	// poll; a slow scorer must not hold a cycle open.
	rubricTimeout = 20 * time.Second
	// RubricFlagLabel is set when any probability reaches the threshold.
	RubricFlagLabel = "merge:rubric-flagged"
)

// RubricQuestion is one noul question, exactly as `.github/merge-rubric.yml`
// declares it. No prompt text lives in Go: the client derives the request
// from this.
type RubricQuestion struct {
	ID           string `yaml:"id"`
	Instructions string `yaml:"instructions"`
	Criteria     struct {
		Yes string `yaml:"yes"`
		No  string `yaml:"no"`
	} `yaml:"criteria"`
}

// Rubric is the whole declaration: the questions and the threshold that
// turns a probability into a label.
type Rubric struct {
	FlagThreshold float64          `yaml:"flag_threshold"`
	Questions     []RubricQuestion `yaml:"questions"`
}

// ParseRubric decodes and validates the rubric. Like the policy loader,
// every failure is a refusal: a question that lost its instructions would
// otherwise be sent as an empty prompt and answered with a number nobody
// could interpret.
func ParseRubric(data []byte) (Rubric, error) {
	var r Rubric
	if err := yaml.Unmarshal(data, &r); err != nil {
		return Rubric{}, fmt.Errorf("parse merge-rubric.yml: %w", err)
	}
	if r.FlagThreshold <= 0 || r.FlagThreshold > 1 {
		return Rubric{}, fmt.Errorf("parse merge-rubric.yml: flag_threshold must be in (0,1], got %v", r.FlagThreshold)
	}
	if len(r.Questions) == 0 {
		return Rubric{}, fmt.Errorf("parse merge-rubric.yml: no questions declared")
	}
	seen := make(map[string]bool, len(r.Questions))
	for i, q := range r.Questions {
		if q.ID == "" {
			return Rubric{}, fmt.Errorf("parse merge-rubric.yml: question %d has no id", i)
		}
		if seen[q.ID] {
			return Rubric{}, fmt.Errorf("parse merge-rubric.yml: duplicate question id %q", q.ID)
		}
		seen[q.ID] = true
		if q.Instructions == "" {
			return Rubric{}, fmt.Errorf("parse merge-rubric.yml: question %q has no instructions", q.ID)
		}
	}
	return r, nil
}

// RubricAnswer is one question's probability of yes.
type RubricAnswer struct {
	ID          string  `json:"id"`
	Probability float64 `json:"probability"`
}

// RubricResult is what the comment renders: either four probabilities, or
// one honest line saying why there are none.
type RubricResult struct {
	Configured bool           `json:"configured"`
	Reason     string         `json:"reason,omitempty"` // why there are no answers
	Answers    []RubricAnswer `json:"answers,omitempty"`
	Flagged    bool           `json:"flagged"`
	Threshold  float64        `json:"threshold,omitempty"`
}

// RubricScorer puts a rubric to the System One endpoint.
type RubricScorer struct {
	Endpoint string
	Token    string
	Model    string
	HTTP     *http.Client
}

// NewRubricScorer builds a scorer from the environment, or reports that
// the token is absent so the caller can say "not configured" once at
// startup rather than failing every poll.
func NewRubricScorer() (*RubricScorer, bool) {
	token := os.Getenv(TypeSafeKeyEnv)
	if token == "" {
		return nil, false
	}
	return &RubricScorer{
		Endpoint: TypeSafeEndpoint,
		Token:    token,
		Model:    TypeSafeModel,
		HTTP:     &http.Client{Timeout: rubricTimeout},
	}, true
}

// rubricState is what the questions are asked about. Not the diff: the
// questions are about what the change claims and whether that matches its
// shape, and a diff would both dominate the state and cost tokens for
// nothing.
type rubricState struct {
	Issue struct {
		Title string `json:"title"`
		Body  string `json:"body"`
	} `json:"issue"`
	PR struct {
		Title       string `json:"title"`
		Body        string `json:"body"`
		HeadBranch  string `json:"head_branch"`
		CommitCount int    `json:"commit_count"`
	} `json:"pr"`
	Files    []rubricStateFile `json:"files"`
	DiffStat struct {
		Additions int `json:"additions"`
		Deletions int `json:"deletions"`
	} `json:"diff_stat"`
}

type rubricStateFile struct {
	Path  string   `json:"path"`
	Areas []string `json:"areas,omitempty"`
	Tier  Tier     `json:"tier"`
}

// RubricState builds the state from the facts and the verdict already
// computed, so the model sees the same file/area/tier table the human
// reads in the comment.
func RubricState(facts PRFacts, files []FileTier) any {
	var state rubricState
	if len(facts.ClosingIssues) > 0 {
		state.Issue.Title = facts.ClosingIssues[0].Title
		state.Issue.Body = facts.ClosingIssues[0].Body
	}
	state.PR.Title = facts.Title
	state.PR.Body = facts.Body
	state.PR.HeadBranch = facts.HeadBranch
	state.PR.CommitCount = facts.CommitCount
	state.DiffStat.Additions = facts.Additions
	state.DiffStat.Deletions = facts.Deletions
	state.Files = make([]rubricStateFile, 0, len(files))
	for _, f := range files {
		state.Files = append(state.Files, rubricStateFile{Path: f.Path, Areas: f.Areas, Tier: f.Tier})
	}
	return state
}

// systemOneRequest is the wire shape of POST /v1/systemone.
type systemOneRequest struct {
	State     any                      `json:"state"`
	Model     string                   `json:"model"`
	Questions map[string]systemOneNoul `json:"questions"`
}

type systemOneNoul struct {
	Type         string            `json:"type"`
	Instructions string            `json:"instructions"`
	Criteria     map[string]string `json:"criteria,omitempty"`
}

// systemOneResponse is the answer envelope. Only the noul value is read;
// a question answered with another type is a rubric/API mismatch and is
// reported as a missing answer rather than coerced.
type systemOneResponse struct {
	Model   string `json:"model"`
	Answers map[string]struct {
		Type string   `json:"type"`
		Noul *float64 `json:"noul"`
	} `json:"answers"`
}

// buildRubricRequest derives the request from the rubric file. The API's
// criteria keys are `true`/`false`; the file says `yes`/`no`, which is
// what the question actually reads as. The mapping lives here so the file
// stays in the vocabulary a person writes questions in.
func buildRubricRequest(r Rubric, model string, state any) systemOneRequest {
	questions := make(map[string]systemOneNoul, len(r.Questions))
	for _, q := range r.Questions {
		noul := systemOneNoul{Type: "noul", Instructions: q.Instructions}
		if q.Criteria.Yes != "" || q.Criteria.No != "" {
			noul.Criteria = map[string]string{}
			if q.Criteria.Yes != "" {
				noul.Criteria["true"] = q.Criteria.Yes
			}
			if q.Criteria.No != "" {
				noul.Criteria["false"] = q.Criteria.No
			}
		}
		questions[q.ID] = noul
	}
	return systemOneRequest{State: state, Model: model, Questions: questions}
}

// Score puts every question to the endpoint in one request and returns
// the probabilities in the rubric's declared order.
func (s *RubricScorer) Score(ctx context.Context, r Rubric, state any) ([]RubricAnswer, error) {
	body, err := json.Marshal(buildRubricRequest(r, s.Model, state))
	if err != nil {
		return nil, fmt.Errorf("encode request: %w", err)
	}
	ctx, cancel := context.WithTimeout(ctx, rubricTimeout)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, s.Endpoint, bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+s.Token)
	req.Header.Set("Content-Type", "application/json")
	resp, err := s.HTTP.Do(req)
	if err != nil {
		return nil, fmt.Errorf("system one: %w", err)
	}
	defer resp.Body.Close()
	raw, err := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if err != nil {
		return nil, fmt.Errorf("system one: read response: %w", err)
	}
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return nil, fmt.Errorf("system one: %s", resp.Status)
	}
	return parseRubricResponse(raw, r)
}

// parseRubricResponse pulls one probability per declared question, in the
// rubric's order. A question the response did not answer is an error
// rather than a silent zero: zero is a real probability, and "the model
// said no" must never be indistinguishable from "the model said nothing".
func parseRubricResponse(raw []byte, r Rubric) ([]RubricAnswer, error) {
	var resp systemOneResponse
	if err := json.Unmarshal(raw, &resp); err != nil {
		return nil, fmt.Errorf("system one: parse response: %w", err)
	}
	answers := make([]RubricAnswer, 0, len(r.Questions))
	var missing []string
	for _, q := range r.Questions {
		a, ok := resp.Answers[q.ID]
		if !ok || a.Noul == nil || a.Type != "noul" {
			missing = append(missing, q.ID)
			continue
		}
		answers = append(answers, RubricAnswer{ID: q.ID, Probability: *a.Noul})
	}
	if len(missing) > 0 {
		sort.Strings(missing)
		return nil, fmt.Errorf("system one: no noul answer for %v", missing)
	}
	return answers, nil
}

// ScoreRubric is the whole second verdict, failure paths included: it
// never returns an error, because the rubric must never affect verdict 1.
// Every way it can fail becomes a line a reader can act on.
func ScoreRubric(ctx context.Context, scorer *RubricScorer, r Rubric, rubricErr error, state any) RubricResult {
	if scorer == nil {
		return RubricResult{Configured: false, Reason: "not configured (" + TypeSafeKeyEnv + " is unset)"}
	}
	if rubricErr != nil {
		return RubricResult{Configured: true, Reason: "unavailable (" + rubricErr.Error() + ")"}
	}
	answers, err := scorer.Score(ctx, r, state)
	if err != nil {
		return RubricResult{Configured: true, Reason: "unavailable (" + err.Error() + ")"}
	}
	result := RubricResult{Configured: true, Answers: answers, Threshold: r.FlagThreshold}
	for _, a := range answers {
		if a.Probability >= r.FlagThreshold {
			result.Flagged = true
		}
	}
	return result
}
