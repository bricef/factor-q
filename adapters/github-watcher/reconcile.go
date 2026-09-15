package main

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"time"
)

// InvocationHistory is the durable read side used by reconciliation. Unlike
// the live core-NATS subscription, Events returns retained lifecycle events,
// so a watcher restart does not erase the facts needed to finish a label
// transition.
type InvocationHistory interface {
	Events(ctx context.Context, agentID string) ([]OutcomeEvent, error)
}

// ReconcileSource is the GitHub ground truth needed by the in-progress sweep.
type ReconcileSource interface {
	ListByLabel(context.Context, string) ([]Issue, error)
	Relabel(context.Context, int, string, string) error
	OpenPRsClosingIssue(context.Context, int) ([]int, error)
}

// InProgressReconciler derives label state from GitHub and retained runtime
// events. It deliberately owns no correlation state: each pass starts from
// durable facts and is therefore safe after a restart.
type InProgressReconciler struct {
	Source  ReconcileSource
	History InvocationHistory
	Config  Config
	Log     *slog.Logger
	Now     func() time.Time
}

type issueRun struct {
	invocation string
	terminal   OutcomeEvent
	attempts   int
}

// issueRuns reconstructs the latest invocation and retry count for every
// issue. Events are delivered in stream order, so replacing terminal keeps
// the outcome for the latest trigger only.
func issueRuns(events []OutcomeEvent) map[int]issueRun {
	bindings := make(map[string]int)
	runs := make(map[int]issueRun)
	for _, ev := range events {
		switch ev.Kind {
		case OutcomeTriggered:
			if ev.Issue == 0 || ev.InvocationID == "" {
				continue
			}
			bindings[ev.InvocationID] = ev.Issue
			run := runs[ev.Issue]
			run.invocation = ev.InvocationID
			run.terminal = OutcomeEvent{}
			run.attempts++
			runs[ev.Issue] = run
		case OutcomeCompleted, OutcomeFailed, OutcomeAmbiguous:
			issue := bindings[ev.InvocationID]
			run := runs[issue]
			if issue != 0 && run.invocation == ev.InvocationID {
				run.terminal = ev
				runs[issue] = run
			}
		}
	}
	return runs
}

// Reconcile performs one idempotent in-progress sweep. An open closing PR is
// authoritative. Otherwise a retained terminal event drives the same
// completed/failed policy as the live reactor. A run with no terminal is left
// alone until ReconcileAfter, then re-queued (or failed if its historical
// trigger count has already spent the retry budget).
func (r *InProgressReconciler) Reconcile(ctx context.Context) {
	issues, err := r.Source.ListByLabel(ctx, r.Config.InProgressLabel)
	if err != nil {
		r.Log.Error("list in-progress issues failed; skipping reconcile", "err", err)
		return
	}
	if len(issues) == 0 {
		return
	}
	events, historyErr := r.History.Events(ctx, r.Config.TargetAgent)
	if historyErr != nil {
		r.Log.Error("query retained invocation events failed; PR reconciliation only this pass", "err", historyErr)
	}
	runs := issueRuns(events)
	for _, issue := range issues {
		r.reconcileIssue(ctx, issue, runs[issue.Number], historyErr == nil)
	}
}

func (r *InProgressReconciler) reconcileIssue(ctx context.Context, issue Issue, run issueRun, historyAvailable bool) {
	prs, err := r.Source.OpenPRsClosingIssue(ctx, issue.Number)
	if err != nil {
		r.Log.Error("checking open PR failed; leaving issue in progress", "issue", issue.Number, "err", err)
		return
	}
	if len(prs) > 0 {
		r.move(ctx, issue.Number, r.Config.InReviewLabel, "open PR closes issue")
		return
	}

	if !historyAvailable {
		return
	}

	if run.terminal.Kind != 0 {
		switch run.terminal.Kind {
		case OutcomeAmbiguous:
			r.move(ctx, issue.Number, r.Config.FailedLabel, "invocation recovery is ambiguous")
		case OutcomeCompleted:
			kind := "no_pr"
			if run.terminal.TaskStatus == "failed" || run.terminal.TaskStatus == "blocked" {
				kind = "task_" + run.terminal.TaskStatus
			}
			r.retryOrFail(ctx, issue.Number, kind, run.attempts)
		case OutcomeFailed:
			r.retryOrFail(ctx, issue.Number, run.terminal.ErrorKind, run.attempts)
		}
		return
	}

	now := time.Now
	if r.Now != nil {
		now = r.Now
	}
	if issue.UpdatedAt.IsZero() || now().Sub(issue.UpdatedAt) < r.Config.ReconcileAfter {
		return
	}
	r.retryOrFail(ctx, issue.Number, "reconcile_timeout", run.attempts)
}

func (r *InProgressReconciler) retryOrFail(ctx context.Context, issue int, kind string, attempts int) {
	terminal := terminalErrorKinds[kind]
	// One initial trigger plus MaxRetries is the complete attempt budget.
	if !terminal && attempts <= r.Config.MaxRetries {
		r.move(ctx, issue, r.Config.ReadyLabel,
			fmt.Sprintf("reconcile found %s; re-queueing within attempt budget", kind))
		return
	}
	r.move(ctx, issue, r.Config.FailedLabel,
		fmt.Sprintf("reconcile found %s; terminal or retries exhausted", kind))
}

func (r *InProgressReconciler) move(ctx context.Context, issue int, add, why string) {
	if err := r.Source.Relabel(ctx, issue, r.Config.InProgressLabel, add); err != nil {
		if errors.Is(err, ErrClaimLost) {
			return
		}
		r.Log.Error("in-progress reconcile relabel failed", "issue", issue, "to", add, "err", err)
		return
	}
	r.Log.Info("reconciled in-progress issue", "issue", issue, "to", add, "why", why)
}
