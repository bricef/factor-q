package main

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

const testRubricYAML = `
flag_threshold: 0.7
questions:
  - id: admits_deferred_work
    instructions: Does the body admit deferred work?
    criteria:
      yes: It says something was left undone.
      no: It says the change is complete.
  - id: alters_runtime_behaviour
    instructions: Does this alter runtime behaviour?
`

func testRubric(t *testing.T) Rubric {
	t.Helper()
	r, err := ParseRubric([]byte(testRubricYAML))
	if err != nil {
		t.Fatalf("ParseRubric: %v", err)
	}
	return r
}

// `yes` and `no` are YAML 1.1 booleans. yaml.v3 is 1.2, but the rubric
// file's readability depends on those exact spellings, so the decode is
// gated rather than assumed.
func TestParseRubricReadsYesAndNoCriteria(t *testing.T) {
	r := testRubric(t)
	if r.FlagThreshold != 0.7 || len(r.Questions) != 2 {
		t.Fatalf("rubric = %+v", r)
	}
	if r.Questions[0].Criteria.Yes != "It says something was left undone." {
		t.Errorf("criteria.yes = %q", r.Questions[0].Criteria.Yes)
	}
	if r.Questions[0].Criteria.No != "It says the change is complete." {
		t.Errorf("criteria.no = %q", r.Questions[0].Criteria.No)
	}
}

func TestParseRubricRefuses(t *testing.T) {
	cases := map[string]string{
		"no threshold":     "questions:\n  - id: a\n    instructions: q\n",
		"threshold over 1": "flag_threshold: 1.5\nquestions:\n  - id: a\n    instructions: q\n",
		"no questions":     "flag_threshold: 0.7\nquestions: []\n",
		"no id":            "flag_threshold: 0.7\nquestions:\n  - instructions: q\n",
		"no instructions":  "flag_threshold: 0.7\nquestions:\n  - id: a\n",
		"duplicate id":     "flag_threshold: 0.7\nquestions:\n  - id: a\n    instructions: q\n  - id: a\n    instructions: r\n",
	}
	for name, yaml := range cases {
		t.Run(name, func(t *testing.T) {
			if _, err := ParseRubric([]byte(yaml)); err == nil {
				t.Fatal("ParseRubric = nil error, want a refusal")
			}
		})
	}
}

func TestShippedRubricLoads(t *testing.T) {
	raw, err := os.ReadFile(filepath.Join("..", "..", RubricPath))
	if err != nil {
		t.Fatalf("read %s: %v", RubricPath, err)
	}
	r, err := ParseRubric(raw)
	if err != nil {
		t.Fatalf("ParseRubric(%s): %v", RubricPath, err)
	}
	if len(r.Questions) != 4 {
		t.Errorf("the shipped rubric has %d questions, want the issue's four", len(r.Questions))
	}
}

// The request must be derived from the file, not carried in Go: change
// the file, and the wire changes with it.
func TestRequestIsDerivedFromTheRubricFile(t *testing.T) {
	var got systemOneRequest
	var auth, contentType string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		auth, contentType = r.Header.Get("Authorization"), r.Header.Get("Content-Type")
		if err := json.NewDecoder(r.Body).Decode(&got); err != nil {
			t.Errorf("decode request: %v", err)
		}
		writeNouls(w, map[string]float64{"admits_deferred_work": 0.1, "alters_runtime_behaviour": 0.4})
	}))
	defer server.Close()

	scorer := &RubricScorer{Endpoint: server.URL, Token: "secret", Model: TypeSafeModel, HTTP: server.Client()}
	facts := cleanFacts()
	facts.Title, facts.HeadBranch, facts.Additions, facts.Deletions = "the PR", "m0/issue-879", 12, 3
	facts.ClosingIssues[0].Title, facts.ClosingIssues[0].Body = "the issue", "do the thing"
	decision := Verdict(testPolicy(t), testAreas(t), facts)

	answers, err := scorer.Score(context.Background(), testRubric(t), RubricState(facts, decision.Files))
	if err != nil {
		t.Fatalf("Score: %v", err)
	}
	if auth != "Bearer secret" || contentType != "application/json" {
		t.Errorf("headers = %q / %q", auth, contentType)
	}
	if got.Model != TypeSafeModel {
		t.Errorf("model = %q, want %q", got.Model, TypeSafeModel)
	}
	q, ok := got.Questions["admits_deferred_work"]
	if !ok {
		t.Fatalf("questions = %v, want the rubric's ids as keys", got.Questions)
	}
	if q.Type != "noul" || q.Instructions != "Does the body admit deferred work?" {
		t.Errorf("question = %+v", q)
	}
	// The file says yes/no; the API says true/false.
	if q.Criteria["true"] != "It says something was left undone." || q.Criteria["false"] != "It says the change is complete." {
		t.Errorf("criteria = %v", q.Criteria)
	}
	// A question with no criteria sends none rather than two empty strings.
	if got.Questions["alters_runtime_behaviour"].Criteria != nil {
		t.Errorf("criteria = %v, want none", got.Questions["alters_runtime_behaviour"].Criteria)
	}
	// The state is the issue, the PR, the file table and the diff stat —
	// and never the diff.
	state, err := json.Marshal(got.State)
	if err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{`"title":"the issue"`, `"head_branch":"m0/issue-879"`, `"commit_count":1`, `"path":"docs/a.md"`, `"tier":"unsupervised"`, `"additions":12`} {
		if !strings.Contains(string(state), want) {
			t.Errorf("state is missing %s:\n%s", want, state)
		}
	}
	if len(answers) != 2 || answers[0].ID != "admits_deferred_work" || answers[0].Probability != 0.1 {
		t.Errorf("answers = %+v, want the rubric's order", answers)
	}
}

func writeNouls(w http.ResponseWriter, values map[string]float64) {
	answers := map[string]any{}
	for id, v := range values {
		answers[id] = map[string]any{"type": "noul", "noul": v}
	}
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(map[string]any{"model": "jev-1.13.0", "answers": answers})
}

// Zero is a real probability, so "the model said no" must never be
// indistinguishable from "the model said nothing".
func TestAMissingAnswerIsAnErrorNotAZero(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		writeNouls(w, map[string]float64{"admits_deferred_work": 0.2})
	}))
	defer server.Close()
	scorer := &RubricScorer{Endpoint: server.URL, Token: "t", Model: TypeSafeModel, HTTP: server.Client()}
	_, err := scorer.Score(context.Background(), testRubric(t), "state")
	if err == nil || !strings.Contains(err.Error(), "alters_runtime_behaviour") {
		t.Fatalf("err = %v, want it to name the unanswered question", err)
	}
}

func TestRubricDegradesAndNeverAffectsVerdictOne(t *testing.T) {
	failing := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		http.Error(w, "nope", http.StatusInternalServerError)
	}))
	defer failing.Close()
	garbage := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		_, _ = w.Write([]byte("{not json"))
	}))
	defer garbage.Close()

	cases := []struct {
		name    string
		scorer  *RubricScorer
		loadErr error
		want    string
	}{
		{"no token", nil, nil, "not configured (" + TypeSafeKeyEnv + " is unset)"},
		{"unloadable rubric", &RubricScorer{Endpoint: failing.URL, HTTP: failing.Client()}, errors.New("no such file"), "unavailable (no such file)"},
		{"api error", &RubricScorer{Endpoint: failing.URL, Model: TypeSafeModel, HTTP: failing.Client()}, nil, "unavailable (system one: 500"},
		{"malformed response", &RubricScorer{Endpoint: garbage.URL, Model: TypeSafeModel, HTTP: garbage.Client()}, nil, "unavailable (system one: parse response"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got := ScoreRubric(context.Background(), tc.scorer, testRubric(t), tc.loadErr, "state")
			if len(got.Answers) != 0 || got.Flagged {
				t.Errorf("a failure produced answers or a flag: %+v", got)
			}
			if !strings.HasPrefix(got.Reason, strings.SplitN(tc.want, "(", 2)[0]) || !strings.Contains(got.Reason, strings.Trim(strings.SplitN(tc.want, "(", 2)[1], ")")) {
				t.Errorf("reason = %q, want it to look like %q", got.Reason, tc.want)
			}
		})
	}
}

func TestFlaggedIffAProbabilityReachesTheThreshold(t *testing.T) {
	cases := []struct {
		name    string
		values  map[string]float64
		flagged bool
	}{
		{"below", map[string]float64{"admits_deferred_work": 0.69, "alters_runtime_behaviour": 0.1}, false},
		{"at the threshold", map[string]float64{"admits_deferred_work": 0.7, "alters_runtime_behaviour": 0.1}, true},
		{"above", map[string]float64{"admits_deferred_work": 0.1, "alters_runtime_behaviour": 0.95}, true},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
				writeNouls(w, tc.values)
			}))
			defer server.Close()
			scorer := &RubricScorer{Endpoint: server.URL, Model: TypeSafeModel, HTTP: server.Client()}
			got := ScoreRubric(context.Background(), scorer, testRubric(t), nil, "state")
			if got.Flagged != tc.flagged {
				t.Errorf("flagged = %v, want %v (%+v)", got.Flagged, tc.flagged, got.Answers)
			}
		})
	}
}

// The sweep half: the rubric reaches the same comment, sets and clears
// its own label, and a failing scorer still leaves verdict 1 on the PR.
func TestSweepPutsTheRubricInTheSameComment(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		writeNouls(w, map[string]float64{"admits_deferred_work": 0.91, "stays_within_the_issue": 0.2,
			"takes_a_maintainer_decision": 0.1, "alters_runtime_behaviour": 0.05})
	}))
	defer server.Close()

	src := newFakeVerdictSource()
	shipped, err := os.ReadFile(filepath.Join("..", "..", RubricPath))
	if err != nil {
		t.Fatal(err)
	}
	src.files[RubricPath+"@main"] = shipped
	src.labels = append(src.labels, RubricFlagLabel)
	src.addPR(900, "aaa", true, []string{"docs/a.md"})
	sweeper := newTestSweeper(src)
	sweeper.Rubric = &RubricScorer{Endpoint: server.URL, Model: TypeSafeModel, HTTP: server.Client()}
	sweeper.Sweep(context.Background())

	body := src.comments[900].Body
	if strings.Count(body, mergeVerdictMarker) != 1 {
		t.Errorf("want one comment carrying one marker:\n%s", body)
	}
	for _, want := range []string{"merge:unsupervised", "Rubric (Jev)", "| `admits_deferred_work` | 0.91 | ⚑ |", "**flagged**"} {
		if !strings.Contains(body, want) {
			t.Errorf("comment is missing %q:\n%s", want, body)
		}
	}
	if !contains(src.prLabels[900], RubricFlagLabel) {
		t.Errorf("labels = %v, want the rubric flag", src.prLabels[900])
	}
}

func TestSweepClearsTheRubricFlagWhenTheAnswerChanges(t *testing.T) {
	high := true
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		p := 0.05
		if high {
			p = 0.95
		}
		writeNouls(w, map[string]float64{"admits_deferred_work": p, "stays_within_the_issue": 0.1,
			"takes_a_maintainer_decision": 0.1, "alters_runtime_behaviour": 0.1})
	}))
	defer server.Close()

	src := newFakeVerdictSource()
	shipped, _ := os.ReadFile(filepath.Join("..", "..", RubricPath))
	src.files[RubricPath+"@main"] = shipped
	src.labels = append(src.labels, RubricFlagLabel)
	src.addPR(900, "aaa", true, []string{"docs/a.md"})
	sweeper := newTestSweeper(src)
	sweeper.Rubric = &RubricScorer{Endpoint: server.URL, Model: TypeSafeModel, HTTP: server.Client()}
	ctx := context.Background()
	sweeper.Sweep(ctx)
	if !contains(src.prLabels[900], RubricFlagLabel) {
		t.Fatalf("labels = %v, want the flag set first", src.prLabels[900])
	}

	high = false
	src.prs[0].HeadSHA = "bbb"
	facts := src.facts[900]
	facts.HeadSHA = "bbb"
	src.facts[900] = facts
	sweeper.Sweep(ctx)

	if contains(src.prLabels[900], RubricFlagLabel) {
		t.Errorf("labels = %v, want the flag cleared once the answer changed", src.prLabels[900])
	}
}

func TestSweepStillLabelsWhenTheRubricIsUnreachable(t *testing.T) {
	src := newFakeVerdictSource() // no rubric file at the ref at all
	src.addPR(900, "aaa", true, []string{"ops/x.yml"})
	src.labels = append(src.labels, RubricFlagLabel)
	sweeper := newTestSweeper(src)
	sweeper.Rubric = &RubricScorer{Endpoint: "http://127.0.0.1:1", Model: TypeSafeModel, HTTP: &http.Client{}}
	sweeper.Sweep(context.Background())

	if got := src.prLabels[900]; len(got) == 0 || got[0] != "merge:never" {
		t.Errorf("labels = %v, want merge:never despite the rubric being unreachable", got)
	}
	if body := src.comments[900].Body; !strings.Contains(body, "unavailable (") {
		t.Errorf("comment does not say why there is no rubric:\n%s", body)
	}
}

func contains(haystack []string, needle string) bool {
	for _, s := range haystack {
		if s == needle {
			return true
		}
	}
	return false
}
