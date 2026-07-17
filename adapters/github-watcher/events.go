package main

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"

	"github.com/nats-io/nats.go"
)

// NatsOutcomeSource observes a factor-q agent's invocation outcomes by
// subscribing to the runtime's event subjects on core NATS, per the event
// schema (docs/design/committed/event-schema.md):
//
//	fq.agent.<agent>.triggered
//	fq.agent.<agent>.completed
//	fq.agent.<agent>.failed
//
// It decodes each into an OutcomeEvent and recovers the issue number from
// the `triggered` event's trigger_payload using the task template — the
// same template the publisher rendered — so the reactor can bind an
// invocation to its issue.
//
// Core NATS (at-most-once) is deliberate: a missed outcome is not fatal
// because the poll loop is the backstop (a re-queued issue is picked up on
// the next poll), and observing outcomes must not compete with the
// runtime's own durable JetStream consumers on the events stream.
type NatsOutcomeSource struct {
	nc           *nats.Conn
	taskTemplate string
}

// NewNatsOutcomeSource opens a core-NATS subscription seam over an existing
// connection. It reuses the publisher's connection rather than opening its
// own.
func NewNatsOutcomeSource(nc *nats.Conn, taskTemplate string) *NatsOutcomeSource {
	return &NatsOutcomeSource{nc: nc, taskTemplate: taskTemplate}
}

// eventEnvelope is the subset of the event-schema envelope the watcher
// reads.
type eventEnvelope struct {
	InvocationID string `json:"invocation_id"`
}

// triggeredPayload is the subset of the `triggered` payload the watcher
// reads: the convention payload, which carries source-specific GitHub fields.
type triggeredPayload struct {
	TriggerPayload json.RawMessage `json:"trigger_payload"`
}

// failedPayload is the subset of the `failed` payload the watcher reads.
type failedPayload struct {
	ErrorKind string `json:"error_kind"`
}

// wireEvent is the on-the-wire event: an envelope plus an
// adjacently-tagged payload. The runtime serializes its EventPayload enum
// with serde `#[serde(tag = "event_type", content = "payload")]`, so the
// concrete payload (trigger_payload, error_kind, …) is nested one level
// deeper — under `payload.payload` — NOT at the top of the payload object.
// Decoding must go through the wrapper's Content, or every payload field
// reads as null (which silently breaks the invocation→issue binding).
type wireEvent struct {
	Envelope eventEnvelope `json:"envelope"`
	Payload  struct {
		EventType string          `json:"event_type"`
		Content   json.RawMessage `json:"payload"`
	} `json:"payload"`
}

// Outcomes subscribes to the agent's lifecycle subjects and invokes handle
// for each decoded event until ctx is cancelled. Undecodable messages are
// skipped.
func (s *NatsOutcomeSource) Outcomes(ctx context.Context, agentID string, handle func(OutcomeEvent)) error {
	msgs := make(chan *nats.Msg, 64)
	subs := make([]*nats.Subscription, 0, 4)
	for _, kind := range []string{"triggered", "completed", "failed", "invocation.ambiguous"} {
		subject := fmt.Sprintf("fq.agent.%s.%s", agentID, kind)
		sub, err := s.nc.ChanSubscribe(subject, msgs)
		if err != nil {
			for _, prev := range subs {
				_ = prev.Unsubscribe()
			}
			return fmt.Errorf("subscribe to %s: %w", subject, err)
		}
		subs = append(subs, sub)
	}
	defer func() {
		for _, sub := range subs {
			_ = sub.Unsubscribe()
		}
	}()

	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		case msg := <-msgs:
			if ev, ok := s.decode(msg); ok {
				handle(ev)
			}
		}
	}
}

// decode turns a NATS message into an OutcomeEvent, returning false if it
// is not a lifecycle event the watcher acts on or cannot be parsed.
func (s *NatsOutcomeSource) decode(msg *nats.Msg) (OutcomeEvent, bool) {
	kind, ok := outcomeKindFromSubject(msg.Subject)
	if !ok {
		return OutcomeEvent{}, false
	}
	var we wireEvent
	if err := json.Unmarshal(msg.Data, &we); err != nil {
		return OutcomeEvent{}, false
	}
	ev := OutcomeEvent{Kind: kind, InvocationID: we.Envelope.InvocationID}
	switch kind {
	case OutcomeTriggered:
		var tp triggeredPayload
		if err := json.Unmarshal(we.Payload.Content, &tp); err == nil {
			ev.Issue = issueFromTriggerPayload(s.taskTemplate, tp.TriggerPayload)
		}
	case OutcomeFailed:
		var fp failedPayload
		if err := json.Unmarshal(we.Payload.Content, &fp); err == nil {
			ev.ErrorKind = fp.ErrorKind
		}
	}
	return ev, true
}

// issueFromTriggerPayload recovers the issue number from a `triggered`
// event's convention payload. It accepts the watcher's former JSON-string
// shape while in-flight legacy invocations complete.
func issueFromTriggerPayload(template string, raw json.RawMessage) int {
	var payload TriggerPayload
	if err := json.Unmarshal(raw, &payload); err == nil && payload.GitHub.Issue > 0 {
		return payload.GitHub.Issue
	}
	var task string
	if err := json.Unmarshal(raw, &task); err != nil {
		return 0
	}
	return issueNumberFromTemplate(template, task)
}

// outcomeKindFromSubject maps a fq.agent.<agent>.<type> subject to the
// OutcomeKind the watcher acts on.
func outcomeKindFromSubject(subject string) (OutcomeKind, bool) {
	switch {
	case hasSuffixToken(subject, "triggered"):
		return OutcomeTriggered, true
	case hasSuffixToken(subject, "completed"):
		return OutcomeCompleted, true
	case hasSuffixToken(subject, "failed"):
		return OutcomeFailed, true
	case strings.HasSuffix(subject, ".invocation.ambiguous"):
		return OutcomeAmbiguous, true
	default:
		return 0, false
	}
}

// hasSuffixToken reports whether subject's final dot-separated token is tok.
func hasSuffixToken(subject, tok string) bool {
	n := len(subject) - len(tok)
	if n < 1 || subject[n-1] != '.' {
		return false
	}
	return subject[n:] == tok
}
