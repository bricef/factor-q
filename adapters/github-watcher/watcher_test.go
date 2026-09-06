package main

import (
	"context"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"slices"
	"strings"
	"testing"
	"time"
)

func testConfig() Config {
	return Config{
		Repo: "owner/repo", TargetAgent: "m0-issue-fix",
		ReadyLabel: "ready", InProgressLabel: "in-progress",
		PollInterval: MinPollInterval, MaxTriggersPerPoll: 3,
		TaskTemplate: "issue #%d",
	}
}

func discardLogger() *slog.Logger { return slog.New(slog.NewTextHandler(io.Discard, nil)) }

// --- pure planner ---

func TestPlanTriggers(t *testing.T) {
	cfg := testConfig()
	cases := []struct {
		name   string
		issues []Issue
		max    int
		want   []int // expected issue numbers, in order
	}{
		{"ready only", []Issue{{1, []string{"ready"}}, {2, []string{"bug"}}}, 3, []int{1}},
		{"skips in-progress", []Issue{{1, []string{"ready", "in-progress"}}, {2, []string{"ready"}}}, 3, []int{2}},
		{"sorted ascending", []Issue{{4, []string{"ready"}}, {1, []string{"ready"}}}, 3, []int{1, 4}},
		{"capped per poll", []Issue{{1, []string{"ready"}}, {2, []string{"ready"}}, {3, []string{"ready"}}}, 2, []int{1, 2}},
		{"none ready", []Issue{{1, []string{"bug"}}}, 3, nil},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			cfg.MaxTriggersPerPoll = tc.max
			got := planTriggers(tc.issues, cfg)
			nums := make([]int, len(got))
			for i, p := range got {
				nums[i] = p.Issue
				if want := fmt.Sprintf("issue #%d", p.Issue); p.Payload.Task != want {
					t.Errorf("payload task = %q, want %q", p.Payload.Task, want)
				}
				if p.Payload.GitHub.Issue != p.Issue {
					t.Errorf("payload issue = %d, want %d", p.Payload.GitHub.Issue, p.Issue)
				}
			}
			if !slices.Equal(nums, tc.want) {
				t.Errorf("planned issues = %v, want %v", nums, tc.want)
			}
		})
	}
}

// --- fakes sharing an ordered op log ---

type recorder struct{ ops []string }

type fakeSource struct {
	rec        *recorder
	issues     []Issue
	relabelErr map[int]error
	// relabelHook, if set, decides each call's outcome from the arguments
	// — for tests that need one transition of an issue to fail and
	// another to succeed.
	relabelHook func(number int, remove, add string) error
}

func (f *fakeSource) ListReady(context.Context, string) ([]Issue, error) { return f.issues, nil }
func (f *fakeSource) Relabel(_ context.Context, number int, remove, add string) error {
	if f.relabelHook != nil {
		if err := f.relabelHook(number, remove, add); err != nil {
			return err
		}
	}
	if f.relabelErr != nil {
		if err := f.relabelErr[number]; err != nil {
			return err
		}
	}
	f.rec.ops = append(f.rec.ops, fmt.Sprintf("relabel #%d %s->%s", number, remove, add))
	return nil
}

type fakePublisher struct {
	rec  *recorder
	fail bool
}

func (f *fakePublisher) Publish(_ context.Context, agentID string, payload TriggerPayload) error {
	if f.fail {
		return errors.New("publish boom")
	}
	f.rec.ops = append(f.rec.ops, fmt.Sprintf("publish %s %q", agentID, payload.Task))
	return nil
}

func newWatcher(src IssueSource, pub TriggerPublisher) *Watcher {
	return &Watcher{Source: src, Publisher: pub, Config: testConfig(), Log: discardLogger()}
}

// --- poll loop ---

func TestPollOnceRelabelsBeforePublishing(t *testing.T) {
	rec := &recorder{}
	w := newWatcher(
		&fakeSource{rec: rec, issues: []Issue{{1, []string{"ready"}}, {2, []string{"ready"}}}},
		&fakePublisher{rec: rec},
	)
	if err := w.pollOnce(context.Background()); err != nil {
		t.Fatalf("pollOnce: %v", err)
	}
	want := []string{
		"relabel #1 ready->in-progress",
		`publish m0-issue-fix "issue #1"`,
		"relabel #2 ready->in-progress",
		`publish m0-issue-fix "issue #2"`,
	}
	if !slices.Equal(rec.ops, want) {
		t.Errorf("ops =\n  %v\nwant\n  %v", rec.ops, want)
	}
}

func TestPollOnceSkipsWhenRelabelFails(t *testing.T) {
	rec := &recorder{}
	w := newWatcher(
		&fakeSource{
			rec:        rec,
			issues:     []Issue{{1, []string{"ready"}}, {2, []string{"ready"}}},
			relabelErr: map[int]error{1: errors.New("relabel boom")},
		},
		&fakePublisher{rec: rec},
	)
	if err := w.pollOnce(context.Background()); err != nil {
		t.Fatalf("pollOnce: %v", err)
	}
	// #1's relabel failed, so #1 must never be published; #2 proceeds.
	for _, op := range rec.ops {
		if op == `publish m0-issue-fix "issue #1"` {
			t.Errorf("issue #1 was published despite a failed relabel: %v", rec.ops)
		}
	}
	if !slices.Contains(rec.ops, `publish m0-issue-fix "issue #2"`) {
		t.Errorf("issue #2 should still have been published: %v", rec.ops)
	}
}

// Losing the claim is not the same as failing to claim: the issue is
// being worked by whoever won, so the trigger must not go out, and the
// log must say which issue and why.
func TestPollOnceDoesNotPublishALostClaim(t *testing.T) {
	rec := &recorder{}
	logs := &syncBuffer{}
	w := &Watcher{
		Source: &fakeSource{
			rec:        rec,
			issues:     []Issue{{1, []string{"ready"}}, {2, []string{"ready"}}},
			relabelErr: map[int]error{1: fmt.Errorf("remove %q from #1: %w", "ready", ErrClaimLost)},
		},
		Publisher: &fakePublisher{rec: rec},
		Config:    testConfig(),
		Log:       slog.New(slog.NewTextHandler(logs, nil)),
	}
	if err := w.pollOnce(context.Background()); err != nil {
		t.Fatalf("pollOnce: %v", err)
	}
	if slices.Contains(rec.ops, `publish m0-issue-fix "issue #1"`) {
		t.Errorf("issue #1 was published although the claim was lost: %v", rec.ops)
	}
	if !slices.Contains(rec.ops, `publish m0-issue-fix "issue #2"`) {
		t.Errorf("issue #2 should still have been published: %v", rec.ops)
	}
	if got := logs.String(); !strings.Contains(got, "claim lost") || !strings.Contains(got, "issue=1") {
		t.Errorf("log = %q, want a claim-lost line naming issue 1", got)
	}
}

// An issue carrying both labels is skipped by the planner for ever, so
// the one thing the log must not say is "will retry next poll".
func TestPollOnceRaisesAClaimStrandedWithBothLabels(t *testing.T) {
	rec := &recorder{}
	logs := &syncBuffer{}
	w := &Watcher{
		Source: &fakeSource{
			rec:        rec,
			issues:     []Issue{{7, []string{"ready"}}},
			relabelErr: map[int]error{7: fmt.Errorf("remove %q from #7 failed and %q could not be rolled back: %w", "ready", "in-progress", ErrBothLabels)},
		},
		Publisher: &fakePublisher{rec: rec},
		Config:    testConfig(),
		Log:       slog.New(slog.NewTextHandler(logs, nil)),
	}
	if err := w.pollOnce(context.Background()); err != nil {
		t.Fatal(err)
	}
	if len(rec.ops) != 0 {
		t.Errorf("ops = %v, want nothing published on a failed claim", rec.ops)
	}
	got := logs.String()
	if strings.Contains(got, "will retry next poll") {
		t.Errorf("a stranded issue is never retried; log = %q", got)
	}
	if !strings.Contains(got, "by hand") || !strings.Contains(got, "issue=7") {
		t.Errorf("log = %q, want a hand-repair line naming issue 7", got)
	}
}

// "Stranded" is what the log says when a transition leaves an issue
// nowhere. A lost race leaves it somewhere — wherever the winner put it —
// so it must not be reported the same way, at any of the relabel sites.
func TestLostRacesAreNotReportedAsStrandings(t *testing.T) {
	lost := fmt.Errorf("remove %q from #7: %w", "in-progress", ErrClaimLost)

	t.Run("revert after a failed publish", func(t *testing.T) {
		logs := &syncBuffer{}
		// The claim succeeds and the publish fails; the revert then finds
		// the label already gone.
		source := &fakeSource{rec: &recorder{}, issues: []Issue{{7, []string{"ready"}}}}
		source.relabelHook = func(_ int, remove, _ string) error {
			if remove == testConfig().InProgressLabel {
				return lost
			}
			return nil
		}
		w := &Watcher{
			Source:    source,
			Publisher: &fakePublisher{rec: &recorder{}, fail: true},
			Config:    testConfig(),
			Log:       slog.New(slog.NewTextHandler(logs, nil)),
		}
		if err := w.pollOnce(context.Background()); err != nil {
			t.Fatal(err)
		}
		if got := logs.String(); strings.Contains(got, "stranded") || !strings.Contains(got, "nothing to revert") {
			t.Errorf("log = %q, want the revert reported as a lost race", got)
		}
	})

	t.Run("outcome reaction", func(t *testing.T) {
		logs := &syncBuffer{}
		src := &labelSource{relErr: lost}
		r := NewOutcomeReactor(src, outcomeConfig(), slog.New(slog.NewTextHandler(logs, nil)))
		triggeredThen(r, "inv", 7, OutcomeEvent{Kind: OutcomeFailed, InvocationID: "inv", ErrorKind: "budget_exceeded"})
		if got := logs.String(); strings.Contains(got, "stranded") || !strings.Contains(got, "already moved on") {
			t.Errorf("log = %q, want the outcome relabel reported as a lost race", got)
		}
	})

	t.Run("review sweep", func(t *testing.T) {
		logs := &syncBuffer{}
		cfg := outcomeConfig()
		w := &Watcher{
			Source:    &labelSource{relErr: lost},
			Publisher: &fakePublisher{rec: &recorder{}},
			Reviewer:  &labelSource{inReview: []Issue{{7, []string{"in-review"}}}, merged: map[int]bool{7: true}},
			Config:    cfg,
			Log:       slog.New(slog.NewTextHandler(logs, nil)),
		}
		w.sweepReview(context.Background())
		if got := logs.String(); strings.Contains(got, "left in review") || !strings.Contains(got, "already left in-review") {
			t.Errorf("log = %q, want the sweep reported as a lost race", got)
		}
	})
}

func TestPollOnceRevertsOnPublishFailure(t *testing.T) {
	rec := &recorder{}
	w := newWatcher(
		&fakeSource{rec: rec, issues: []Issue{{7, []string{"ready"}}}},
		&fakePublisher{rec: rec, fail: true},
	)
	if err := w.pollOnce(context.Background()); err != nil {
		t.Fatalf("pollOnce: %v", err)
	}
	// Claim then, on publish failure, release the claim so it retries.
	want := []string{
		"relabel #7 ready->in-progress",
		"relabel #7 in-progress->ready",
	}
	if !slices.Equal(rec.ops, want) {
		t.Errorf("ops = %v, want %v (claim then revert)", rec.ops, want)
	}
}

// --- config validation ---

func TestConfigFromArgsValidation(t *testing.T) {
	if _, _, _, err := configFromArgs([]string{"--repo", "owner/repo", "--poll", "30s"}); err == nil {
		t.Error("poll below the 60s floor should be rejected")
	}
	if _, _, _, err := configFromArgs([]string{"--repo", "owner/repo", "--task-template", "no placeholder"}); err == nil {
		t.Error("a task template lacking the issue-number placeholder should be rejected")
	}
	if _, _, _, err := configFromArgs([]string{"--repo", "owner/repo", "--task-template", "fix #%d and %s"}); err == nil {
		t.Error("a task template with another format verb should be rejected")
	}
	if _, _, _, err := configFromArgs([]string{"--repo", "owner/repo", "--task-template", "fix 50%% of #%d"}); err == nil {
		t.Error("a task template with a literal %% should be rejected — issue-number recovery cannot match its rendering")
	}
	cfg, _, _, err := configFromArgs([]string{"--repo", "owner/repo", "--poll", "90s"})
	if err != nil {
		t.Fatalf("valid config rejected: %v", err)
	}
	if cfg.PollInterval != 90*time.Second || cfg.Repo != "owner/repo" {
		t.Errorf("unexpected config: %+v", cfg)
	}
}

func TestConfigFromArgsRejectsMalformedEnv(t *testing.T) {
	for _, tc := range []struct{ key, value string }{{"GHW_POLL", "soon"}, {"GHW_MAX_PER_POLL", "many"}, {"GHW_MAX_RETRIES", "several"}} {
		t.Run(tc.key, func(t *testing.T) {
			t.Setenv(tc.key, tc.value)
			if _, _, _, err := configFromArgs([]string{"--repo", "owner/repo"}); err == nil || !strings.Contains(err.Error(), tc.key) {
				t.Fatalf("error = %v, want clear %s error", err, tc.key)
			}
		})
	}
}

func TestHealthBindFlagAndEnv(t *testing.T) {
	_, _, bind, err := configFromArgs([]string{"--repo", "owner/repo"})
	if err != nil {
		t.Fatal(err)
	}
	if bind != defaultHealthBind {
		t.Errorf("default health bind %q, want %q", bind, defaultHealthBind)
	}
	_, _, bind, err = configFromArgs([]string{"--repo", "owner/repo", "--health-bind", ""})
	if err != nil {
		t.Fatal(err)
	}
	if bind != "" {
		t.Errorf("--health-bind '' should disable the endpoint, got %q", bind)
	}
	t.Setenv(healthBindEnv, "127.0.0.1:1")
	_, _, bind, err = configFromArgs([]string{"--repo", "owner/repo"})
	if err != nil {
		t.Fatal(err)
	}
	if bind != "127.0.0.1:1" {
		t.Errorf("env health bind %q", bind)
	}
}
