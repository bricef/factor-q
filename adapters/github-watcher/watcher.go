// Command github-watcher is a standalone external trigger adapter for
// factor-q. It polls a GitHub repository for issues labelled `ready` and,
// for each, drives a two-step label state machine before triggering a
// factor-q agent:
//
//  1. Relabel the issue `ready` -> `in-progress`.
//  2. Publish a trigger on `fq.trigger.<agent>` per the trigger wire
//     contract (docs/design/committed/trigger-wire-contract.md).
//
// Relabelling out of `ready` *before* triggering is the idempotency
// mechanism: a re-seen issue is no longer `ready`, so edits, re-polls, and
// watcher restarts cannot double-trigger.
//
// The adapter also *observes the outcome* of what it triggered, closing the
// gap that stranded issue #9: a failed invocation is relabelled off
// `in-progress` (retried, bounded, or escalated to `failed`) rather than
// left claimed forever, a completed one moves to `in-review`, and a merged
// PR moves the issue to `done`. Outcome observation lives in OutcomeReactor
// (event stream) and the review sweep below (merged PRs).
//
// The adapter depends on factor-q only through the documented wire
// contracts — the trigger wire contract and the event schema — never on
// fq-runtime's code. That is the point: the boundary is a construction, not
// a convention.
package main

import (
	"context"
	"fmt"
	"log/slog"
	"sort"
	"time"
)

// MinPollInterval is the floor on the poll cadence, to stay well within
// GitHub's API rate limits.
const MinPollInterval = 60 * time.Second

// Issue is the minimal GitHub issue shape the watcher needs.
type Issue struct {
	Number int
	Labels []string
}

// HasLabel reports whether the issue carries label.
func (i Issue) HasLabel(label string) bool {
	for _, l := range i.Labels {
		if l == label {
			return true
		}
	}
	return false
}

// IssueSource is the seam over GitHub: the poll logic reads and relabels
// issues through this interface, so it is testable against an in-memory
// fake without touching the network.
type IssueSource interface {
	// ListReady returns open issues that carry readyLabel.
	ListReady(ctx context.Context, readyLabel string) ([]Issue, error)
	// Relabel removes `remove` and adds `add` on the issue. This is the
	// idempotency step: it must complete before the trigger is published,
	// so a re-seen issue is no longer `ready`.
	Relabel(ctx context.Context, number int, remove, add string) error
}

// ReviewSource is the seam over GitHub for the review sweep: listing issues
// in review and asking whether an issue's linked PR has merged. It is
// separate from IssueSource so the trigger path keeps its minimal contract;
// a Watcher without one simply skips the merged-PR → done transition.
type ReviewSource interface {
	// ListByLabel returns open issues carrying label.
	ListByLabel(ctx context.Context, label string) ([]Issue, error)
	// HasMergedPR reports whether the issue has a merged PR linked to it
	// (i.e. the proposed fix landed).
	HasMergedPR(ctx context.Context, number int) (bool, error)
}

// TriggerPublisher publishes a trigger to a factor-q agent per the trigger
// wire contract. payload is the opaque string handed to the agent; the
// implementation JSON-encodes it as the message body.
type TriggerPublisher interface {
	Publish(ctx context.Context, agentID, payload string) error
}

// Config is the watcher's configuration.
type Config struct {
	Repo               string        // "owner/name", e.g. "bricef/factor-q"
	TargetAgent        string        // the agent to trigger, e.g. "m0-issue-fix"
	ReadyLabel         string        // the label that means "go", e.g. "ready"
	InProgressLabel    string        // the label applied on trigger, e.g. "in-progress"
	InReviewLabel      string        // applied when the agent completes (PR open), e.g. "in-review"
	FailedLabel        string        // applied when retries are exhausted / a terminal failure, e.g. "failed"
	DoneLabel          string        // applied when the proposed PR merges, e.g. "done"
	PollInterval       time.Duration // >= MinPollInterval
	MaxTriggersPerPoll int           // concurrency guard: at most N triggers per poll (0 = unbounded)
	MaxRetries         int           // bounded auto-retry budget for a transiently-failed issue
	TaskTemplate       string        // payload template; a single %d is the issue number
}

// PlannedTrigger is a decision to trigger the agent for one issue.
type PlannedTrigger struct {
	Issue   int
	Payload string
}

// planTriggers is pure: given the issues currently labelled ready and the
// config, it decides which triggers to fire. An issue is picked iff it
// carries ReadyLabel AND does not already carry InProgressLabel
// (belt-and-suspenders dedup), capped at MaxTriggersPerPoll, in ascending
// issue-number order for determinism.
func planTriggers(issues []Issue, cfg Config) []PlannedTrigger {
	eligible := make([]Issue, 0, len(issues))
	for _, iss := range issues {
		if iss.HasLabel(cfg.ReadyLabel) && !iss.HasLabel(cfg.InProgressLabel) {
			eligible = append(eligible, iss)
		}
	}
	sort.Slice(eligible, func(a, b int) bool { return eligible[a].Number < eligible[b].Number })
	if cfg.MaxTriggersPerPoll > 0 && len(eligible) > cfg.MaxTriggersPerPoll {
		eligible = eligible[:cfg.MaxTriggersPerPoll]
	}
	planned := make([]PlannedTrigger, 0, len(eligible))
	for _, iss := range eligible {
		planned = append(planned, PlannedTrigger{
			Issue:   iss.Number,
			Payload: fmt.Sprintf(cfg.TaskTemplate, iss.Number),
		})
	}
	return planned
}

// Watcher polls an IssueSource and triggers via a TriggerPublisher. If
// Reviewer is set, each poll also sweeps `in-review` issues and moves those
// whose PR has merged to `done`.
type Watcher struct {
	Source    IssueSource
	Publisher TriggerPublisher
	Reviewer  ReviewSource // optional; nil disables the merged-PR → done sweep
	Config    Config
	Log       *slog.Logger
}

// pollOnce runs one poll cycle: list ready issues, plan, and for each
// planned trigger relabel (ready -> in-progress) THEN publish. The
// relabel-before-publish order is the dedup: a re-seen issue is no longer
// ready. If the publish fails after the relabel, the claim is reverted
// (in-progress -> ready) so the next poll retries, rather than stranding
// the issue. Per-issue errors are logged and do not stop the others.
//
// After the trigger pass it runs the review sweep (merged PR → done).
func (w *Watcher) pollOnce(ctx context.Context) error {
	issues, err := w.Source.ListReady(ctx, w.Config.ReadyLabel)
	if err != nil {
		return fmt.Errorf("list ready issues: %w", err)
	}
	for _, pt := range planTriggers(issues, w.Config) {
		// Step 1: claim the issue by relabelling out of `ready`. This
		// must happen before the trigger so a re-seen issue cannot
		// double-trigger.
		if err := w.Source.Relabel(ctx, pt.Issue, w.Config.ReadyLabel, w.Config.InProgressLabel); err != nil {
			w.Log.Error("relabel failed; skipping trigger (will retry next poll)",
				"issue", pt.Issue, "err", err)
			continue
		}
		// Step 2: publish the trigger.
		if err := w.Publisher.Publish(ctx, w.Config.TargetAgent, pt.Payload); err != nil {
			w.Log.Error("trigger publish failed after relabel; reverting to ready so it retries",
				"issue", pt.Issue, "agent", w.Config.TargetAgent, "err", err)
			if rerr := w.Source.Relabel(ctx, pt.Issue, w.Config.InProgressLabel, w.Config.ReadyLabel); rerr != nil {
				w.Log.Error("failed to revert label after publish failure; issue stranded in in-progress",
					"issue", pt.Issue, "err", rerr)
			}
			continue
		}
		w.Log.Info("triggered agent for issue", "issue", pt.Issue, "agent", w.Config.TargetAgent)
	}
	w.sweepReview(ctx)
	return nil
}

// sweepReview moves `in-review` issues whose proposed PR has merged to
// `done`. It is best-effort: per-issue errors are logged and do not stop
// the others, and a missing Reviewer disables the sweep entirely.
func (w *Watcher) sweepReview(ctx context.Context) {
	if w.Reviewer == nil {
		return
	}
	inReview, err := w.Reviewer.ListByLabel(ctx, w.Config.InReviewLabel)
	if err != nil {
		w.Log.Error("list in-review issues failed; skipping review sweep this poll", "err", err)
		return
	}
	for _, iss := range inReview {
		merged, err := w.Reviewer.HasMergedPR(ctx, iss.Number)
		if err != nil {
			w.Log.Error("checking merged PR failed; leaving issue in review", "issue", iss.Number, "err", err)
			continue
		}
		if !merged {
			continue
		}
		if err := w.Source.Relabel(ctx, iss.Number, w.Config.InReviewLabel, w.Config.DoneLabel); err != nil {
			w.Log.Error("relabel to done failed; issue left in review", "issue", iss.Number, "err", err)
			continue
		}
		w.Log.Info("proposed PR merged; issue done", "issue", iss.Number)
	}
}

// Run polls immediately, then on Config.PollInterval, until ctx is
// cancelled.
func (w *Watcher) Run(ctx context.Context) error {
	w.Log.Info("github-watcher starting",
		"version", buildVersion(),
		"repo", w.Config.Repo, "agent", w.Config.TargetAgent,
		"poll", w.Config.PollInterval.String(), "ready", w.Config.ReadyLabel)
	ticker := time.NewTicker(w.Config.PollInterval)
	defer ticker.Stop()
	for {
		if err := w.pollOnce(ctx); err != nil {
			w.Log.Error("poll cycle failed", "err", err)
		}
		select {
		case <-ctx.Done():
			w.Log.Info("github-watcher stopping")
			return ctx.Err()
		case <-ticker.C:
		}
	}
}
