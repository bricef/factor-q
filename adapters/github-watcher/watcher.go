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
// The adapter depends on factor-q only through the trigger wire contract
// (a NATS subject + a JSON payload) — never on fq-runtime's code. That is
// the point: the boundary is a construction, not a convention.
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
	PollInterval       time.Duration // >= MinPollInterval
	MaxTriggersPerPoll int           // concurrency guard: at most N triggers per poll (0 = unbounded)
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

// Watcher polls an IssueSource and triggers via a TriggerPublisher.
type Watcher struct {
	Source    IssueSource
	Publisher TriggerPublisher
	Config    Config
	Log       *slog.Logger
}

// pollOnce runs one poll cycle: list ready issues, plan, and for each
// planned trigger relabel (ready -> in-progress) THEN publish. The
// relabel-before-publish order is the dedup: a re-seen issue is no longer
// ready. If the publish fails after the relabel, the claim is reverted
// (in-progress -> ready) so the next poll retries, rather than stranding
// the issue. Per-issue errors are logged and do not stop the others.
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
	return nil
}

// Run polls immediately, then on Config.PollInterval, until ctx is
// cancelled.
func (w *Watcher) Run(ctx context.Context) error {
	w.Log.Info("github-watcher starting",
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
