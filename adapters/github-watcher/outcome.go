package main

import (
	"context"
	"fmt"
	"log/slog"
	"regexp"
	"strconv"
	"strings"
	"sync"
)

// OutcomeKind is which lifecycle event the runtime emitted for an
// invocation. The watcher only cares about the terminal outcomes plus the
// triggered event (which is where the issue number ↔ invocation binding
// lives — see OutcomeEvent).
type OutcomeKind int

const (
	// OutcomeTriggered is the `triggered` event: an invocation started. It
	// carries the trigger payload, from which the issue number is
	// recovered.
	OutcomeTriggered OutcomeKind = iota
	// OutcomeCompleted is the `completed` event: the invocation finished
	// successfully (the agent opened its PR).
	OutcomeCompleted
	// OutcomeFailed is the `failed` event: the invocation terminated with
	// an error.
	OutcomeFailed
)

// OutcomeEvent is the minimal projection of a runtime lifecycle event the
// reactor needs. It is decoded from the event-schema wire format
// (docs/design/committed/event-schema.md) by an OutcomeSource, so the pure
// reaction logic never touches NATS or JSON.
//
// Issue is the GitHub issue number this invocation is working, recovered
// from the `triggered` event's trigger_payload; it is 0 (unknown) on
// completed/failed events, which carry only the invocation id. ErrorKind is
// set on OutcomeFailed and classifies transient vs terminal failures.
type OutcomeEvent struct {
	Kind         OutcomeKind
	InvocationID string
	Issue        int    // known on OutcomeTriggered; 0 otherwise
	ErrorKind    string // set on OutcomeFailed
}

// OutcomeSource is the seam over the runtime's event stream: it delivers
// decoded OutcomeEvents for one agent. Implemented over core NATS by
// NatsOutcomeSource; faked in tests.
type OutcomeSource interface {
	// Outcomes runs until ctx is cancelled, invoking handle for each
	// decoded lifecycle event of the target agent. Undecodable messages
	// are skipped, not surfaced.
	Outcomes(ctx context.Context, agentID string, handle func(OutcomeEvent)) error
}

// terminalErrorKinds are `failed` error_kinds that will not be retried: a
// re-trigger of the same issue would fail the same way. Everything else
// (notably `llm_error` — the transient class that stranded issue #9) is
// treated as retryable up to the bounded retry budget.
var terminalErrorKinds = map[string]bool{
	"budget_exceeded":   true,
	"max_iterations":    true,
	"sandbox_violation": true,
}

// issueNumberFromTemplate builds a matcher that recovers the issue number
// from a rendered task payload, given the task template. The template
// contains exactly one %d (validated at config time); everything else is
// matched literally. Returns 0 if the payload does not match.
func issueNumberFromTemplate(template, payload string) int {
	idx := strings.Index(template, "%d")
	if idx < 0 {
		return 0
	}
	prefix := regexp.QuoteMeta(template[:idx])
	suffix := regexp.QuoteMeta(template[idx+2:])
	re, err := regexp.Compile("^" + prefix + `(\d+)` + suffix + "$")
	if err != nil {
		return 0
	}
	m := re.FindStringSubmatch(payload)
	if m == nil {
		return 0
	}
	n, err := strconv.Atoi(m[1])
	if err != nil {
		return 0
	}
	return n
}

// OutcomeReactor observes invocation outcomes and drives the issue's label
// state machine past `in-progress`, closing the observability gap that
// stranded issue #9. It is the outcome-side counterpart to the poll loop's
// ready → in-progress claim.
//
// Bindings from invocation id to issue number are learned from `triggered`
// events; retries per issue are bounded by Config.MaxRetries.
type OutcomeReactor struct {
	Source IssueSource
	Config Config
	Log    *slog.Logger

	mu       sync.Mutex
	binding  map[string]int // invocation_id -> issue number
	attempts map[int]int    // issue number -> failures already reacted to
}

// NewOutcomeReactor constructs a reactor over an IssueSource.
func NewOutcomeReactor(src IssueSource, cfg Config, log *slog.Logger) *OutcomeReactor {
	return &OutcomeReactor{
		Source:   src,
		Config:   cfg,
		Log:      log,
		binding:  make(map[string]int),
		attempts: make(map[int]int),
	}
}

// React processes one outcome event, relabelling the bound issue when the
// invocation reaches a terminal state:
//
//   - triggered → record the invocation_id ↔ issue binding.
//   - completed → in-progress → in-review (the agent opened its PR).
//   - failed    → in-progress → ready to retry (bounded, transient errors)
//     or in-progress → failed (retries exhausted or a terminal error).
//
// Relabel errors are logged, not returned: one bad reaction must not stop
// the outcome stream.
func (r *OutcomeReactor) React(ctx context.Context, ev OutcomeEvent) {
	switch ev.Kind {
	case OutcomeTriggered:
		if ev.Issue > 0 && ev.InvocationID != "" {
			r.mu.Lock()
			r.binding[ev.InvocationID] = ev.Issue
			r.mu.Unlock()
		}
	case OutcomeCompleted:
		issue, ok := r.lookup(ev.InvocationID)
		if !ok {
			return
		}
		r.relabel(ctx, issue, r.Config.InProgressLabel, r.Config.InReviewLabel,
			"invocation completed; issue awaiting review")
		r.forget(ev.InvocationID)
	case OutcomeFailed:
		issue, ok := r.lookup(ev.InvocationID)
		if !ok {
			return
		}
		r.reactFailed(ctx, issue, ev.ErrorKind)
		r.forget(ev.InvocationID)
	}
}

func (r *OutcomeReactor) reactFailed(ctx context.Context, issue int, errorKind string) {
	r.mu.Lock()
	prior := r.attempts[issue]
	terminal := terminalErrorKinds[errorKind]
	retry := !terminal && prior < r.Config.MaxRetries
	if retry {
		r.attempts[issue] = prior + 1
	}
	r.mu.Unlock()

	if retry {
		r.relabel(ctx, issue, r.Config.InProgressLabel, r.Config.ReadyLabel,
			fmt.Sprintf("invocation failed (%s); re-queuing for retry %d/%d",
				errorKind, prior+1, r.Config.MaxRetries))
		return
	}
	reason := "retries exhausted"
	if terminal {
		reason = "terminal error"
	}
	r.relabel(ctx, issue, r.Config.InProgressLabel, r.Config.FailedLabel,
		fmt.Sprintf("invocation failed (%s); %s — needs operator attention", errorKind, reason))
}

func (r *OutcomeReactor) lookup(invocationID string) (int, bool) {
	r.mu.Lock()
	defer r.mu.Unlock()
	issue, ok := r.binding[invocationID]
	return issue, ok
}

func (r *OutcomeReactor) forget(invocationID string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	delete(r.binding, invocationID)
}

func (r *OutcomeReactor) relabel(ctx context.Context, issue int, remove, add, why string) {
	if err := r.Source.Relabel(ctx, issue, remove, add); err != nil {
		r.Log.Error("outcome relabel failed; issue may be stranded",
			"issue", issue, "from", remove, "to", add, "err", err)
		return
	}
	r.Log.Info("reacted to invocation outcome", "issue", issue, "from", remove, "to", add, "why", why)
}

// Run subscribes to the target agent's outcomes and reacts to each until
// ctx is cancelled.
func (r *OutcomeReactor) Run(ctx context.Context, source OutcomeSource) error {
	return source.Outcomes(ctx, r.Config.TargetAgent, func(ev OutcomeEvent) {
		r.React(ctx, ev)
	})
}
