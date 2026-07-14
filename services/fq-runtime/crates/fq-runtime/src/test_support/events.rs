//! Event capture and assertion helpers for NATS-backed tests.
//!
//! The pattern this replaces (previously duplicated in
//! `executor::tests` and `reducer::runner::tests`):
//!
//! ```ignore
//! let mut sub = bus.subscribe(format!("fq.agent.{}.>", agent_id))
//!     .await.expect("subscribe");
//! tokio::time::sleep(Duration::from_millis(50)).await;
//! // ... action ...
//! let mut events = Vec::new();
//! for _ in 0..N {
//!     let event = tokio::time::timeout(...).await....
//!     events.push(event);
//! }
//! ```
//!
//! Plus a per-file `event_kind(&Event) -> &'static str` mapping
//! and ad-hoc kind-sequence assertions.

use std::time::Duration;

use futures::StreamExt;
use uuid::Uuid;

use crate::bus::EventBus;
use crate::events::{Event, EventPayload};

/// Stable, snake_case kind name for an event payload.
///
/// Used by [`assert_kinds_in_order`] and [`find_first`] to make
/// assertions readable. The names match those returned by the
/// existing `event_kind` helpers in the executor and reducer
/// runner tests; replacing those duplicates with this function
/// is the first refactor that uses this module.
pub fn event_kind(event: &Event) -> &'static str {
    event_kind_of(&event.payload)
}

/// Payload-level variant of [`event_kind`], for callers that hold a
/// payload without its envelope (e.g. the trace oracle's messages).
pub fn event_kind_of(payload: &EventPayload) -> &'static str {
    match payload {
        EventPayload::Triggered(_) => "triggered",
        EventPayload::LlmRequest(_) => "llm_request",
        EventPayload::LlmDispatched(_) => "llm_dispatched",
        EventPayload::LlmResponse(_) => "llm_response",
        EventPayload::ToolCall(_) => "tool_call",
        EventPayload::ToolDispatched(_) => "tool_dispatched",
        EventPayload::ToolResult(_) => "tool_result",
        EventPayload::Completed(_) => "completed",
        EventPayload::Failed(_) => "failed",
        EventPayload::HostNotice(_) => "host_notice",
        EventPayload::InvocationAmbiguous(_) => "invocation_ambiguous",
        EventPayload::InvocationArchived(_) => "invocation_archived",
        EventPayload::InvocationArchiveAcked(_) => "invocation_archive_acked",
        EventPayload::InvocationOperatorRecovered(_) => "invocation_operator_recovered",
        EventPayload::SystemStartup(_) => "system_startup",
        EventPayload::SystemShutdown(_) => "system_shutdown",
        EventPayload::SystemTaskFailed(_) => "system_task_failed",
        EventPayload::SystemRecovery(_) => "system_recovery",
        EventPayload::WorkerHeartbeat(_) => "worker_heartbeat",
        EventPayload::McpServerLog(_) => "mcp_server_log",
    }
}

/// Skip a NATS-backed test if `FQ_NATS_URL` isn't set. Returns
/// the URL when present; prints a `skipping:` line otherwise.
///
/// Use at the top of any test that needs a real NATS connection:
///
/// ```ignore
/// #[tokio::test]
/// async fn my_test() {
///     let Some(url) = require_nats() else { return };
///     // ... real test ...
/// }
/// ```
pub fn require_nats() -> Option<String> {
    match std::env::var("FQ_NATS_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("skipping: FQ_NATS_URL not set");
            None
        }
    }
}

/// Subscribe to an agent's event subject, run an async action,
/// then collect `expected_count` events emitted while it ran.
///
/// The 50ms sleep between subscribe and action is intentional —
/// it lets NATS register the subscription before any publish
/// happens. Without it, fast actions can publish before the
/// subscriber is ready and events get missed.
///
/// # Panics
///
/// - If subscription fails.
/// - If fewer than `expected_count` events arrive within
///   `per_event_timeout` (per-event timeout, not total).
/// - If the stream closes early or an event fails to
///   deserialise.
pub async fn capture_events<F, Fut>(
    bus: &EventBus,
    agent_id: &str,
    expected_count: usize,
    per_event_timeout: Duration,
    action: F,
) -> Vec<Event>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut sub = bus
        .subscribe(format!("fq.agent.{agent_id}.>"))
        .await
        .expect("subscribe");

    tokio::time::sleep(Duration::from_millis(50)).await;

    action().await;

    let mut events = Vec::with_capacity(expected_count);
    for i in 0..expected_count {
        let event = tokio::time::timeout(per_event_timeout, sub.next())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for event {i} of {expected_count}"))
            .unwrap_or_else(|| panic!("event stream closed at event {i} of {expected_count}"))
            .unwrap_or_else(|e| panic!("failed to deserialise event {i}: {e}"));
        events.push(event);
    }
    events
}

/// Assert that the kinds of `events` match `expected` exactly,
/// in order. Panics with a readable diff on mismatch.
pub fn assert_kinds_in_order(events: &[Event], expected: &[&str]) {
    let actual: Vec<&str> = events.iter().map(event_kind).collect();
    assert_eq!(
        actual, expected,
        "event kinds did not match expected sequence"
    );
}

/// Find the first event of the given kind. Returns `None` if
/// not present.
pub fn find_first<'a>(events: &'a [Event], kind: &str) -> Option<&'a Event> {
    events.iter().find(|e| event_kind(e) == kind)
}

/// Assert that the events appear in *relative* order: each kind
/// in `expected_order` is found in the events, and they appear
/// in the listed order. Other event kinds may interleave.
///
/// Useful for "between X and Y, Z must appear" assertions when
/// other events may also be present.
pub fn assert_kinds_appear_in_relative_order(events: &[Event], expected_order: &[&str]) {
    let mut search_from = 0;
    for kind in expected_order {
        let found_at = events[search_from..]
            .iter()
            .position(|e| event_kind(e) == *kind)
            .unwrap_or_else(|| {
                let actual: Vec<&str> = events.iter().map(event_kind).collect();
                panic!(
                    "kind {kind:?} not found in events at or after index {search_from}; \
                     full kind sequence: {actual:?}"
                )
            });
        search_from += found_at + 1;
    }
}

/// Assert the given events form a single `parent_event_id`-rooted
/// chain.
///
/// Per the resolved decisions in step 2 of the envelope-refactor
/// plan:
///
/// - Exactly one event must have `parent_event_id == None`, and it
///   must be a `Triggered` event (the chain root).
/// - Every other event's `parent_event_id` must refer to the
///   `event_id` of some other event in the input.
/// - No two events may share the same `parent_event_id` (no
///   branches) — the loop is linear.
///
/// On violation, panics with a message naming the violating event
/// and the kind.
pub fn assert_parent_chain(events: &[Event]) {
    use std::collections::{HashMap, HashSet};
    let ids: HashSet<Uuid> = events.iter().map(|e| e.envelope.event_id).collect();
    let mut roots: Vec<usize> = Vec::new();
    let mut parents_seen: HashMap<Uuid, usize> = HashMap::new();
    for (i, e) in events.iter().enumerate() {
        match e.envelope.parent_event_id {
            None => roots.push(i),
            Some(p) => {
                assert!(
                    ids.contains(&p),
                    "event {i} (kind={}) has orphan parent {p}",
                    event_kind(e)
                );
                if let Some(prior) = parents_seen.insert(p, i) {
                    panic!(
                        "events {prior} and {i} both claim parent {p} \
                         (kinds={} and {}); chain must be linear",
                        event_kind(&events[prior]),
                        event_kind(e)
                    );
                }
            }
        }
    }
    assert_eq!(
        roots.len(),
        1,
        "expected exactly one root (parent_event_id = None); found {}: indices {:?}",
        roots.len(),
        roots
    );
    let root_idx = roots[0];
    let root_kind = event_kind(&events[root_idx]);
    assert_eq!(
        root_kind, "triggered",
        "chain root must be a `triggered` event, was {root_kind}"
    );
}

/// Assert all events share a single `invocation_id`. Returns the
/// id for further assertions.
pub fn assert_single_invocation(events: &[Event]) -> Uuid {
    let first = events
        .first()
        .expect("expected at least one event")
        .envelope
        .invocation_id;
    for (i, e) in events.iter().enumerate() {
        assert_eq!(
            e.envelope.invocation_id,
            first,
            "event {i} (kind={}) has different invocation_id than event 0",
            event_kind(e)
        );
    }
    first
}

#[cfg(test)]
mod tests {
    //! Pure-Rust tests for the helpers. The NATS-backed
    //! `capture_events` helper is exercised indirectly by every
    //! test that uses it across the wider crate.

    use super::*;
    use crate::agent::AgentId;
    use crate::events::{
        ConfigSnapshot, Event, EventPayload, LlmResponsePayload, SandboxSnapshot, StopReason,
        TokenUsage, ToolCallPayload, ToolResultPayload, TriggerSource, TriggeredPayload,
    };

    fn aid(s: &str) -> AgentId {
        AgentId::new(s).expect("test agent id must be valid")
    }
    use serde_json::json;

    fn config_snapshot() -> ConfigSnapshot {
        ConfigSnapshot {
            name: "test-agent".to_string(),
            model: "claude-haiku".to_string(),
            system_prompt: "test".to_string(),
            tools: vec![],
            sandbox: SandboxSnapshot::default(),
            budget: None,
            ..Default::default()
        }
    }

    fn triggered(invocation_id: Uuid) -> Event {
        Event::new(
            aid("test-agent"),
            invocation_id,
            EventPayload::Triggered(TriggeredPayload {
                trigger_source: TriggerSource::Manual,
                trigger_subject: None,
                trigger_payload: json!({}),
                config_snapshot: config_snapshot(),
            }),
        )
    }

    fn tool_call(invocation_id: Uuid) -> Event {
        Event::new(
            aid("test-agent"),
            invocation_id,
            EventPayload::ToolCall(ToolCallPayload {
                tool_call_id: crate::events::ToolCallId::new("c1").unwrap(),
                tool_name: "test_tool".to_string(),
                parameters: json!({}),
            }),
        )
    }

    fn tool_result(invocation_id: Uuid) -> Event {
        Event::new(
            aid("test-agent"),
            invocation_id,
            EventPayload::ToolResult(ToolResultPayload {
                tool_call_id: crate::events::ToolCallId::new("c1").unwrap(),
                output: "ok".to_string(),
                is_error: false,
                error_kind: None,
                duration_ms: 1,
            }),
        )
    }

    fn llm_response(invocation_id: Uuid) -> Event {
        Event::new(
            aid("test-agent"),
            invocation_id,
            EventPayload::LlmResponse(LlmResponsePayload {
                origin: crate::events::LlmCallOrigin::AgentTurn,
                call_id: Uuid::now_v7(),
                content: Some("hi".to_string()),
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        )
    }

    #[test]
    fn event_kind_covers_sample_variants() {
        let inv = Uuid::now_v7();
        assert_eq!(event_kind(&triggered(inv)), "triggered");
        assert_eq!(event_kind(&tool_call(inv)), "tool_call");
        assert_eq!(event_kind(&llm_response(inv)), "llm_response");
    }

    #[test]
    fn assert_kinds_in_order_passes_on_match() {
        let inv = Uuid::now_v7();
        let events = vec![triggered(inv), tool_call(inv), tool_result(inv)];
        assert_kinds_in_order(&events, &["triggered", "tool_call", "tool_result"]);
    }

    #[test]
    #[should_panic(expected = "event kinds did not match")]
    fn assert_kinds_in_order_panics_on_mismatch() {
        let inv = Uuid::now_v7();
        let events = vec![triggered(inv), tool_call(inv)];
        assert_kinds_in_order(&events, &["triggered", "tool_result"]);
    }

    #[test]
    fn find_first_returns_match() {
        let inv = Uuid::now_v7();
        let events = vec![triggered(inv), tool_call(inv)];
        let found = find_first(&events, "tool_call").expect("present");
        assert!(matches!(found.payload, EventPayload::ToolCall(_)));
    }

    #[test]
    fn find_first_returns_none_when_absent() {
        let inv = Uuid::now_v7();
        let events = vec![triggered(inv)];
        assert!(find_first(&events, "tool_call").is_none());
    }

    #[test]
    fn assert_kinds_appear_in_relative_order_passes_with_interleaving() {
        let inv = Uuid::now_v7();
        let events = vec![
            triggered(inv),
            tool_call(inv),
            llm_response(inv),
            tool_result(inv),
        ];
        assert_kinds_appear_in_relative_order(&events, &["tool_call", "tool_result"]);
    }

    #[test]
    #[should_panic(expected = "not found in events at or after")]
    fn assert_kinds_appear_in_relative_order_panics_when_out_of_order() {
        let inv = Uuid::now_v7();
        let events = vec![tool_result(inv), tool_call(inv)];
        assert_kinds_appear_in_relative_order(&events, &["tool_call", "tool_result"]);
    }

    #[test]
    fn assert_single_invocation_returns_id_when_uniform() {
        let inv = Uuid::now_v7();
        let events = vec![triggered(inv), tool_call(inv)];
        assert_eq!(assert_single_invocation(&events), inv);
    }

    #[test]
    #[should_panic(expected = "different invocation_id")]
    fn assert_single_invocation_panics_when_mixed() {
        let events = vec![triggered(Uuid::now_v7()), tool_call(Uuid::now_v7())];
        assert_single_invocation(&events);
    }

    #[test]
    fn assert_parent_chain_passes_on_linear_chain() {
        let inv = Uuid::now_v7();
        let e1 = triggered(inv);
        let e2 = tool_call(inv).with_parent(e1.envelope.event_id);
        let e3 = tool_result(inv).with_parent(e2.envelope.event_id);
        assert_parent_chain(&[e1, e2, e3]);
    }

    #[test]
    #[should_panic(expected = "orphan parent")]
    fn parent_chain_helper_detects_orphan() {
        let inv = Uuid::now_v7();
        let e1 = triggered(inv);
        // tool_call points at a parent uuid that doesn't exist in
        // the captured set.
        let e2 = tool_call(inv).with_parent(Uuid::now_v7());
        assert_parent_chain(&[e1, e2]);
    }

    #[test]
    #[should_panic(expected = "expected exactly one root")]
    fn parent_chain_helper_detects_multiple_roots() {
        let inv = Uuid::now_v7();
        let e1 = triggered(inv);
        let e2 = triggered(inv); // second root — parent is None
        assert_parent_chain(&[e1, e2]);
    }

    #[test]
    #[should_panic(expected = "both claim parent")]
    fn parent_chain_helper_detects_branching() {
        let inv = Uuid::now_v7();
        let e1 = triggered(inv);
        let e2 = tool_call(inv).with_parent(e1.envelope.event_id);
        // Same parent as e2 — that's a branch.
        let e3 = tool_result(inv).with_parent(e1.envelope.event_id);
        assert_parent_chain(&[e1, e2, e3]);
    }

    #[test]
    #[should_panic(expected = "chain root must be a `triggered`")]
    fn parent_chain_helper_rejects_non_triggered_root() {
        let inv = Uuid::now_v7();
        let root = tool_call(inv); // not triggered
        assert_parent_chain(&[root]);
    }
}
