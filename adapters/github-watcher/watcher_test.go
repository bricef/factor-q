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
}

func (f *fakeSource) ListReady(context.Context, string) ([]Issue, error) { return f.issues, nil }
func (f *fakeSource) Relabel(_ context.Context, number int, remove, add string) error {
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
	if _, _, err := configFromArgs([]string{"--repo", "owner/repo", "--poll", "30s"}); err == nil {
		t.Error("poll below the 60s floor should be rejected")
	}
	if _, _, err := configFromArgs([]string{"--repo", "owner/repo", "--task-template", "no placeholder"}); err == nil {
		t.Error("a task template lacking the issue-number placeholder should be rejected")
	}
	if _, _, err := configFromArgs([]string{"--repo", "owner/repo", "--task-template", "fix #%d and %s"}); err == nil {
		t.Error("a task template with another format verb should be rejected")
	}
	cfg, _, err := configFromArgs([]string{"--repo", "owner/repo", "--poll", "90s"})
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
			if _, _, err := configFromArgs([]string{"--repo", "owner/repo"}); err == nil || !strings.Contains(err.Error(), tc.key) {
				t.Fatalf("error = %v, want clear %s error", err, tc.key)
			}
		})
	}
}
