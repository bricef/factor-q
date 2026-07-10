//! The R1/R6 trace oracle (reducer verification plan, slice 1).
//!
//! [`check_invocation_trace`] states the canonical-sequence claim as
//! a pure predicate over one invocation's captured events: the trace
//! is `triggered`, then complete LLM triples
//! (`llm.request → llm.dispatched → llm.response`) and tool spans
//! (`tool.call → [nested LLM triples]* → tool.dispatched →
//! tool.result`, where the nested triples are server-initiated
//! sampling/evaluator calls made while the tool executes), ending in
//! **exactly one** terminal (`completed` / `failed`) followed by at
//! least one `invocation.archived` (sweeper republishes and
//! control-plane acks are tolerated after it). Synthetic tool errors
//! are a degenerate span: a lone error `tool.result` with no
//! call/dispatched pair, exactly as the runner emits them.
//!
//! Envelope invariants are checked alongside the grammar: every
//! event carries the same invocation id, and every
//! `parent_event_id` refers to an earlier event in the trace (the
//! runner's publish chain).
//!
//! The oracle describes a *normal, completed* run — recovery-era
//! events (`invocation.ambiguous`, operator recoveries) are
//! violations here; recovery scenarios get their own predicates when
//! the fault DST lands (slices 4–5). All violations are collected
//! rather than failing fast, so a broken trace reports everything
//! wrong with it at once.

use std::fmt;

use uuid::Uuid;

use crate::events::{Event, EventPayload};

/// One way a trace failed the canonical-sequence claim.
#[derive(Debug)]
pub struct TraceViolation {
    /// Index into the checked slice (usize::MAX for end-of-trace
    /// violations such as a missing terminal).
    pub index: usize,
    pub message: String,
}

impl fmt::Display for TraceViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.index == usize::MAX {
            write!(f, "at end of trace: {}", self.message)
        } else {
            write!(f, "at event {}: {}", self.index, self.message)
        }
    }
}

/// Where the grammar automaton is within one invocation's trace.
enum State {
    /// Nothing seen yet; only `triggered` is legal.
    Start,
    /// Between actions.
    Idle,
    /// Inside an agent-turn LLM triple.
    Llm { dispatched: bool },
    /// Inside a tool span; `llm` tracks a nested sampling/evaluator
    /// triple issued while the tool executes.
    Tool { dispatched: bool, llm: Option<bool> },
    /// Terminal seen; `archived` counts `invocation.archived`.
    Terminal { archived: usize },
}

/// Check one invocation's captured events against the canonical
/// sequence (claim R1) and the exactly-one-terminal /
/// archived-at-end shape (R6's trace footprint). Pure; NATS-free.
pub fn check_invocation_trace(events: &[Event]) -> Result<(), Vec<TraceViolation>> {
    check_with(events, State::Start, true)
}

/// Prefix mode (R1-under-faults, slice 5): a crashed run's captured
/// events must be a *prefix* of a canonical trace — grammar breaks
/// mid-trace are still violations, but the trace may end anywhere
/// (mid-span, after a terminal with the archived publish lost, or
/// even empty when the crash hit the `triggered` publish itself).
pub fn check_invocation_trace_prefix(events: &[Event]) -> Result<(), Vec<TraceViolation>> {
    if events.is_empty() {
        return Ok(());
    }
    check_with(events, State::Start, false)
}

/// Resume mode (slice 5): a resumed run starts a fresh chain with no
/// `triggered` re-publish — the trace begins mid-invocation (between
/// actions) and must still run to exactly one terminal followed by
/// `invocation.archived`.
pub fn check_resume_trace(events: &[Event]) -> Result<(), Vec<TraceViolation>> {
    check_with(events, State::Idle, true)
}

/// Resume-prefix mode (slice 7): a resumed run that itself crashed —
/// headless like a resume trace, truncatable like a prefix.
pub fn check_resume_trace_prefix(events: &[Event]) -> Result<(), Vec<TraceViolation>> {
    if events.is_empty() {
        return Ok(());
    }
    check_with(events, State::Idle, false)
}

fn check_with(
    events: &[Event],
    start_state: State,
    require_complete: bool,
) -> Result<(), Vec<TraceViolation>> {
    let mut violations: Vec<TraceViolation> = Vec::new();
    let mut push =
        |index: usize, message: String| violations.push(TraceViolation { index, message });

    if events.is_empty() {
        push(usize::MAX, "empty trace".to_string());
        return Err(violations);
    }

    let invocation_id = events[0].envelope.invocation_id;
    let mut seen_ids: Vec<Uuid> = Vec::with_capacity(events.len());
    let mut state = start_state;

    for (i, event) in events.iter().enumerate() {
        // -- Envelope invariants.
        if event.envelope.invocation_id != invocation_id {
            push(
                i,
                format!(
                    "invocation id {} differs from the trace's {}",
                    event.envelope.invocation_id, invocation_id
                ),
            );
        }
        if let Some(parent) = event.envelope.parent_event_id
            && !seen_ids.contains(&parent)
        {
            push(
                i,
                format!("parent_event_id {parent} does not refer to an earlier event"),
            );
        }
        seen_ids.push(event.envelope.event_id);

        // -- Grammar.
        let payload = &event.payload;
        state = match state {
            // MCP server logs are ambient fire-and-forget events —
            // legal at any point while the invocation is live.
            State::Start
                if !matches!(payload, EventPayload::Triggered(_))
                    && matches!(payload, EventPayload::McpServerLog(_)) =>
            {
                push(i, "mcp_server_log before triggered".to_string());
                State::Start
            }
            state @ (State::Idle | State::Llm { .. } | State::Tool { .. })
                if matches!(payload, EventPayload::McpServerLog(_)) =>
            {
                state
            }
            State::Start => match payload {
                EventPayload::Triggered(_) => State::Idle,
                other => {
                    push(
                        i,
                        format!("trace must start with triggered, got {}", kind(other)),
                    );
                    State::Idle
                }
            },
            State::Idle => match payload {
                EventPayload::LlmRequest(_) => State::Llm { dispatched: false },
                EventPayload::ToolCall(_) => State::Tool {
                    dispatched: false,
                    llm: None,
                },
                // Synthetic tool errors are published as a lone error
                // result with no call/dispatched pair.
                EventPayload::ToolResult(r) if r.is_error => State::Idle,
                EventPayload::Completed(_) | EventPayload::Failed(_) => {
                    State::Terminal { archived: 0 }
                }
                other => {
                    push(i, format!("unexpected {} between actions", kind(other)));
                    State::Idle
                }
            },
            State::Llm { dispatched } => match (payload, dispatched) {
                (EventPayload::LlmDispatched(_), false) => State::Llm { dispatched: true },
                (EventPayload::LlmResponse(_), true) => State::Idle,
                // A terminal may truncate an in-flight span (the
                // failure that ended the invocation).
                (EventPayload::Completed(_) | EventPayload::Failed(_), _) => {
                    State::Terminal { archived: 0 }
                }
                (other, _) => {
                    push(
                        i,
                        format!(
                            "unexpected {} inside an LLM triple (dispatched={dispatched})",
                            kind(other)
                        ),
                    );
                    State::Llm { dispatched }
                }
            },
            State::Tool { dispatched, llm } => match (payload, dispatched, llm) {
                (EventPayload::LlmRequest(_), false, None) => State::Tool {
                    dispatched,
                    llm: Some(false),
                },
                (EventPayload::LlmDispatched(_), false, Some(false)) => State::Tool {
                    dispatched,
                    llm: Some(true),
                },
                (EventPayload::LlmResponse(_), false, Some(true)) => State::Tool {
                    dispatched,
                    llm: None,
                },
                (EventPayload::ToolDispatched(_), false, None) => State::Tool {
                    dispatched: true,
                    llm: None,
                },
                (EventPayload::ToolResult(_), true, None) => State::Idle,
                (EventPayload::Completed(_) | EventPayload::Failed(_), _, _) => {
                    State::Terminal { archived: 0 }
                }
                (other, _, _) => {
                    push(
                        i,
                        format!(
                            "unexpected {} inside a tool span (dispatched={dispatched}, \
                             nested llm={llm:?})",
                            kind(other)
                        ),
                    );
                    State::Tool { dispatched, llm }
                }
            },
            State::Terminal { archived } => match payload {
                EventPayload::InvocationArchived(_) => State::Terminal {
                    archived: archived + 1,
                },
                // Control-plane ack; may interleave with sweeper
                // republishes after the terminal.
                EventPayload::InvocationArchiveAcked(_) => State::Terminal { archived },
                EventPayload::Completed(_) | EventPayload::Failed(_) => {
                    push(i, "second terminal event in one invocation".to_string());
                    State::Terminal { archived }
                }
                other => {
                    push(
                        i,
                        format!("unexpected {} after the terminal event", kind(other)),
                    );
                    State::Terminal { archived }
                }
            },
        };
    }

    if require_complete {
        match state {
            State::Terminal { archived: 0 } => push(
                usize::MAX,
                "terminal event was never followed by invocation.archived".to_string(),
            ),
            State::Terminal { .. } => {}
            _ => push(
                usize::MAX,
                "trace ended without a terminal (completed/failed) event".to_string(),
            ),
        }
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

/// Panic with every violation formatted — the test-friendly wrapper.
pub fn assert_valid_trace(events: &[Event]) {
    if let Err(violations) = check_invocation_trace(events) {
        let lines: Vec<String> = violations.iter().map(|v| format!("  - {v}")).collect();
        panic!(
            "trace violates the canonical sequence ({} violation(s)):\n{}",
            lines.len(),
            lines.join("\n")
        );
    }
}

/// Concurrent-trace oracle (parallel-workers Phase 2): partition the
/// interleaved sink by invocation id and validate each arc with the
/// **unchanged** single-invocation grammar, plus the cross-invocation
/// invariants:
///
/// - every arc has a `Triggered` root (no orphan events — E3);
/// - at no point are more than `bound` invocations simultaneously in
///   flight (D1). The sim clock is a global monotonic counter, so sink
///   order *is* the interleaving record: an invocation is "in flight"
///   between its first and last event, and the gauge is the maximum
///   number of open intervals at any event index.
///
/// `check_arc` is the per-arc grammar: [`check_invocation_trace`] when
/// every invocation ran to a terminal, [`check_invocation_trace_prefix`]
/// when some are suspended/crashed mid-trace.
fn check_concurrent_with(
    events: &[Event],
    bound: usize,
    check_arc: fn(&[Event]) -> Result<(), Vec<TraceViolation>>,
) -> Result<(), Vec<TraceViolation>> {
    let mut violations: Vec<TraceViolation> = Vec::new();
    if events.is_empty() {
        violations.push(TraceViolation {
            index: usize::MAX,
            message: "empty concurrent trace".to_string(),
        });
        return Err(violations);
    }

    // Partition by invocation id, preserving global order within each
    // arc and remembering each arc's first/last global index.
    let mut arcs: Vec<(Uuid, Vec<Event>, usize, usize)> = Vec::new();
    for (i, event) in events.iter().enumerate() {
        let id = event.envelope.invocation_id;
        match arcs.iter_mut().find(|(arc_id, ..)| *arc_id == id) {
            Some((_, arc, _, last)) => {
                arc.push(event.clone());
                *last = i;
            }
            None => arcs.push((id, vec![event.clone()], i, i)),
        }
    }

    for (id, arc, ..) in &arcs {
        if !matches!(arc[0].payload, EventPayload::Triggered(_)) {
            violations.push(TraceViolation {
                index: usize::MAX,
                message: format!("invocation {id}: arc does not start with Triggered"),
            });
        }
        if let Err(arc_violations) = check_arc(arc) {
            violations.extend(arc_violations.into_iter().map(|v| TraceViolation {
                index: v.index,
                message: format!("invocation {id}: {}", v.message),
            }));
        }
    }

    // D1: the overlap gauge, computed from the trace itself.
    let mut max_overlap = 0usize;
    for i in 0..events.len() {
        let open = arcs
            .iter()
            .filter(|(_, _, first, last)| *first <= i && i <= *last)
            .count();
        max_overlap = max_overlap.max(open);
    }
    if max_overlap > bound {
        violations.push(TraceViolation {
            index: usize::MAX,
            message: format!(
                "{max_overlap} invocations were in flight at once; the bound is {bound}"
            ),
        });
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

/// Every arc ran to a terminal — the happy-path concurrent oracle.
pub fn check_concurrent_trace(events: &[Event], bound: usize) -> Result<(), Vec<TraceViolation>> {
    check_concurrent_with(events, bound, check_invocation_trace)
}

/// Arcs may be prefixes — for traces captured mid-drain or mid-crash.
pub fn check_concurrent_trace_prefix(
    events: &[Event],
    bound: usize,
) -> Result<(), Vec<TraceViolation>> {
    check_concurrent_with(events, bound, check_invocation_trace_prefix)
}

/// Panic-with-details wrapper for [`check_concurrent_trace`].
pub fn assert_valid_concurrent_trace(events: &[Event], bound: usize) {
    if let Err(violations) = check_concurrent_trace(events, bound) {
        let lines: Vec<String> = violations.iter().map(|v| format!("  - {v}")).collect();
        panic!(
            "concurrent trace violates the canonical sequence ({} violation(s)):\n{}",
            lines.len(),
            lines.join("\n")
        );
    }
}

/// Panic-with-details wrapper for [`check_concurrent_trace_prefix`].
pub fn assert_valid_concurrent_trace_prefix(events: &[Event], bound: usize) {
    if let Err(violations) = check_concurrent_trace_prefix(events, bound) {
        let lines: Vec<String> = violations.iter().map(|v| format!("  - {v}")).collect();
        panic!(
            "concurrent trace (prefix mode) violates the canonical sequence ({} violation(s)):\n{}",
            lines.len(),
            lines.join("\n")
        );
    }
}

fn kind(payload: &EventPayload) -> &'static str {
    super::events::event_kind_of(payload)
}

/// The observational projection of a trace (reducer verification
/// plan, R4): per event, its kind plus the payload with volatile
/// fields masked — per-call UUIDs, measured durations, and clock
/// stamps whose values depend on *when* work ran rather than *what*
/// ran. Everything else (messages, tool parameters and outputs,
/// stop reasons, token usage, totals, the final state blob) is load-
/// bearing and kept. Two runs are observationally equivalent iff
/// their projections are equal.
pub fn observational_trace(events: &[Event]) -> Vec<serde_json::Value> {
    const VOLATILE_KEYS: &[&str] = &[
        "call_id",
        "duration_ms",
        "total_duration_ms",
        "started_at_ms",
        "terminal_at_ms",
    ];

    fn strip(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                for key in VOLATILE_KEYS {
                    map.remove(*key);
                }
                for child in map.values_mut() {
                    strip(child);
                }
            }
            serde_json::Value::Array(items) => {
                for child in items.iter_mut() {
                    strip(child);
                }
            }
            _ => {}
        }
    }

    events
        .iter()
        .map(|event| {
            let mut value = serde_json::to_value(&event.payload).expect("event payload serialises");
            strip(&mut value);
            value
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::agent::AgentId;
    use crate::events::{
        CompletedPayload, EventPayload, FailedPayload, FailureKind, FailurePhase, InvocationTotals,
        LlmDispatchedPayload, LlmRequestPayload, LlmResponsePayload, StopReason, TokenUsage,
        ToolCallId, ToolCallPayload, ToolDispatchedPayload, ToolResultPayload,
    };

    /// Builds chained events the way `publish_chained` does.
    struct Trace {
        agent_id: AgentId,
        invocation_id: Uuid,
        cursor: Option<Uuid>,
        events: Vec<Event>,
    }

    impl Trace {
        fn new() -> Self {
            Self {
                agent_id: AgentId::new("oracle-test").unwrap(),
                invocation_id: Uuid::now_v7(),
                cursor: None,
                events: Vec::new(),
            }
        }

        fn push(&mut self, payload: EventPayload) -> &mut Self {
            let mut event = Event::new(self.agent_id.clone(), self.invocation_id, payload);
            event.envelope.parent_event_id = self.cursor;
            self.cursor = Some(event.envelope.event_id);
            self.events.push(event);
            self
        }
    }

    fn triggered() -> EventPayload {
        EventPayload::Triggered(
            serde_json::from_value(json!({
                "trigger_source": "manual",
                "trigger_subject": null,
                "trigger_payload": null,
                "config_snapshot": {
                    "name": "oracle-test",
                    "model": "claude-test",
                    "system_prompt": "s",
                    "tools": [],
                    "sandbox": {},
                    "budget": null
                }
            }))
            .expect("triggered payload"),
        )
    }

    fn llm_request() -> EventPayload {
        EventPayload::LlmRequest(LlmRequestPayload {
            call_id: Uuid::now_v7(),
            model: "claude-test".to_string(),
            messages: vec![],
            tools_available: vec![],
            request_params: crate::events::RequestParams {
                temperature: None,
                max_tokens: None,
            },
            origin: Default::default(),
        })
    }

    fn llm_dispatched() -> EventPayload {
        EventPayload::LlmDispatched(LlmDispatchedPayload {
            call_id: Uuid::now_v7(),
            model: "claude-test".to_string(),
        })
    }

    fn llm_response() -> EventPayload {
        EventPayload::LlmResponse(LlmResponsePayload {
            call_id: Uuid::now_v7(),
            content: Some("ok".to_string()),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
            origin: Default::default(),
        })
    }

    fn tool_call() -> EventPayload {
        EventPayload::ToolCall(ToolCallPayload {
            tool_call_id: ToolCallId::new("t1").unwrap(),
            tool_name: "echo".to_string(),
            parameters: json!({}),
        })
    }

    fn tool_dispatched() -> EventPayload {
        EventPayload::ToolDispatched(ToolDispatchedPayload {
            tool_call_id: ToolCallId::new("t1").unwrap(),
            tool_name: "echo".to_string(),
        })
    }

    fn tool_result(is_error: bool) -> EventPayload {
        EventPayload::ToolResult(ToolResultPayload {
            tool_call_id: ToolCallId::new("t1").unwrap(),
            output: "out".to_string(),
            is_error,
            error_kind: None,
            duration_ms: 1,
        })
    }

    fn completed() -> EventPayload {
        EventPayload::Completed(CompletedPayload {
            result_summary: None,
            total_llm_calls: 1,
            total_tool_calls: 0,
            total_cost: 0.0,
            total_duration_ms: 1,
        })
    }

    fn failed() -> EventPayload {
        EventPayload::Failed(FailedPayload {
            error_kind: FailureKind::RuntimeError,
            error_message: "boom".to_string(),
            phase: FailurePhase::LlmResponse,
            partial_totals: InvocationTotals::default(),
        })
    }

    fn archived() -> EventPayload {
        EventPayload::InvocationArchived(
            serde_json::from_value(json!({
                "worker_id": Uuid::now_v7(),
                "final_phase": "completed",
                "final_state_blob": [],
                "started_at_ms": 1,
                "terminal_at_ms": 2
            }))
            .expect("archived payload"),
        )
    }

    #[test]
    fn canonical_tool_loop_passes() {
        let mut t = Trace::new();
        t.push(triggered())
            .push(llm_request())
            .push(llm_dispatched())
            .push(llm_response())
            .push(tool_call())
            .push(tool_dispatched())
            .push(tool_result(false))
            .push(llm_request())
            .push(llm_dispatched())
            .push(llm_response())
            .push(completed())
            .push(archived());
        assert_valid_trace(&t.events);
    }

    #[test]
    fn synthetic_tool_error_and_sampling_pass() {
        let mut t = Trace::new();
        t.push(triggered())
            .push(llm_request())
            .push(llm_dispatched())
            .push(llm_response())
            // Synthetic error: a lone error result, no call/dispatched.
            .push(tool_result(true))
            .push(llm_request())
            .push(llm_dispatched())
            .push(llm_response())
            // Tool span with a nested sampling triple mid-execution.
            .push(tool_call())
            .push(llm_request())
            .push(llm_dispatched())
            .push(llm_response())
            .push(tool_dispatched())
            .push(tool_result(false))
            .push(llm_request())
            .push(llm_dispatched())
            .push(llm_response())
            .push(failed())
            .push(archived())
            .push(archived()); // sweeper republish
        assert_valid_trace(&t.events);
    }

    #[test]
    fn missing_dispatched_is_a_violation() {
        let mut t = Trace::new();
        t.push(triggered())
            .push(llm_request())
            .push(llm_response()) // skipped llm_dispatched
            .push(completed())
            .push(archived());
        let violations = check_invocation_trace(&t.events).unwrap_err();
        assert!(
            violations
                .iter()
                .any(|v| v.message.contains("inside an LLM triple")),
            "{violations:?}"
        );
    }

    #[test]
    fn two_terminals_are_a_violation() {
        let mut t = Trace::new();
        t.push(triggered())
            .push(completed())
            .push(failed())
            .push(archived());
        let violations = check_invocation_trace(&t.events).unwrap_err();
        assert!(
            violations
                .iter()
                .any(|v| v.message.contains("second terminal")),
            "{violations:?}"
        );
    }

    #[test]
    fn work_after_terminal_is_a_violation() {
        let mut t = Trace::new();
        t.push(triggered())
            .push(completed())
            .push(archived())
            .push(llm_request());
        let violations = check_invocation_trace(&t.events).unwrap_err();
        assert!(
            violations
                .iter()
                .any(|v| v.message.contains("after the terminal")),
            "{violations:?}"
        );
    }

    #[test]
    fn missing_archived_is_a_violation() {
        let mut t = Trace::new();
        t.push(triggered()).push(completed());
        let violations = check_invocation_trace(&t.events).unwrap_err();
        assert!(
            violations
                .iter()
                .any(|v| v.message.contains("never followed by invocation.archived")),
            "{violations:?}"
        );
    }

    #[test]
    fn missing_terminal_is_a_violation() {
        let mut t = Trace::new();
        t.push(triggered())
            .push(llm_request())
            .push(llm_dispatched());
        let violations = check_invocation_trace(&t.events).unwrap_err();
        assert!(
            violations
                .iter()
                .any(|v| v.message.contains("without a terminal")),
            "{violations:?}"
        );
    }

    #[test]
    fn foreign_invocation_id_is_a_violation() {
        let mut t = Trace::new();
        t.push(triggered()).push(completed()).push(archived());
        let mut foreign = Event::new(
            t.agent_id.clone(),
            Uuid::now_v7(), // different invocation
            archived(),
        );
        foreign.envelope.parent_event_id = t.cursor;
        t.events.push(foreign);
        let violations = check_invocation_trace(&t.events).unwrap_err();
        assert!(
            violations
                .iter()
                .any(|v| v.message.contains("differs from")),
            "{violations:?}"
        );
    }

    #[test]
    fn broken_parent_chain_is_a_violation() {
        let mut t = Trace::new();
        t.push(triggered()).push(completed()).push(archived());
        t.events[2].envelope.parent_event_id = Some(Uuid::now_v7()); // dangling
        let violations = check_invocation_trace(&t.events).unwrap_err();
        assert!(
            violations
                .iter()
                .any(|v| v.message.contains("does not refer to an earlier event")),
            "{violations:?}"
        );
    }
}
