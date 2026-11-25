//! What a resume decides about the copy a row keeps (`owed_response`):
//! published, or not, and for an unreadable copy the cost it carries,
//! which the error the resume logs names.

use uuid::Uuid;

use super::{Owed, owed_response};
use crate::agent::AgentId;
use crate::events::{
    Event, EventPayload, LlmCallOrigin, LlmResponsePayload, StopReason, TokenUsage,
};
use crate::worker::store::{DispatchStatus, LlmDispatchRow};

/// What the call's row records it cost.
const COST: f64 = 0.0215;

/// A run, a call of it, the call's `llm.response`, and the run's rows.
struct Kept {
    invocation_id: Uuid,
    call_id: Uuid,
    event: Event,
    rows: Vec<LlmDispatchRow>,
}

fn kept() -> Kept {
    let invocation_id = Uuid::now_v7();
    let call_id = Uuid::now_v7();
    let event = Event::new(
        AgentId::new("owed-response").unwrap(),
        invocation_id,
        EventPayload::LlmResponse(LlmResponsePayload {
            round: 1,
            call_id,
            parts: crate::events::assistant_parts(Some("done".to_string()), vec![]),
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 1_000,
                output_tokens: 100,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cache_write_5m_tokens: None,
                cache_write_1h_tokens: None,
                reasoning_tokens: None,
            },
            origin: LlmCallOrigin::AgentTurn,
        }),
    );
    let row = |request_id: String, cost_usd: f64| LlmDispatchRow {
        invocation_id: invocation_id.to_string(),
        request_id,
        model: "claude-haiku".to_string(),
        status: DispatchStatus::Completed,
        request_payload: "{}".to_string(),
        response: Some("{}".to_string()),
        cost_usd: Some(cost_usd),
        origin: LlmCallOrigin::AgentTurn,
        is_error: Some(false),
        intent_at: 1,
        dispatched_at: Some(2),
        completed_at: Some(3),
        seq: Some(1),
        deferred_at: None,
    };
    let rows = vec![
        row(Uuid::now_v7().to_string(), 0.5),
        row(call_id.to_string(), COST),
    ];
    Kept {
        invocation_id,
        call_id,
        event,
        rows,
    }
}

impl Kept {
    fn decide(&self, stored: &str) -> Owed {
        owed_response(
            self.invocation_id,
            &self.call_id.to_string(),
            stored,
            &self.rows,
        )
    }

    /// The event as the row keeps it, with `edit` applied to its JSON.
    fn stored(&self, edit: impl FnOnce(&mut serde_json::Value)) -> String {
        let mut value = serde_json::to_value(&self.event).unwrap();
        edit(&mut value);
        serde_json::to_string(&value).unwrap()
    }
}

/// The call's own `llm.response` is published, as it was kept.
#[test]
fn the_calls_own_response_is_published() {
    let kept = kept();
    let Owed::Publish(event) = kept.decide(&kept.stored(|_| {})) else {
        panic!("published");
    };
    assert_eq!(
        serde_json::to_value(&event).unwrap(),
        serde_json::to_value(&kept.event).unwrap()
    );
}

/// A copy written under an envelope version this build does not read, as
/// after a deploy across an event-schema change, is the call's history,
/// and its cost reaches no `llm.response`: it is unreadable, with the
/// cost the call's row records, not another row's, for the error to name.
#[test]
fn a_copy_of_an_envelope_version_this_build_does_not_read_is_unreadable_with_its_cost() {
    let kept = kept();
    let stored = kept.stored(|value| value["envelope"]["schema_version"] = 99.into());
    let Owed::Unreadable { error, cost_usd } = kept.decide(&stored) else {
        panic!("unreadable");
    };
    assert!(
        error.to_string().contains("schema_version 99"),
        "the error says why: {error}"
    );
    assert_eq!(cost_usd, Some(COST), "the cost the call's row records");
}

/// Text that is not an event is unreadable too, with the call's cost.
#[test]
fn text_that_is_not_an_event_is_unreadable_with_its_cost() {
    let kept = kept();
    let Owed::Unreadable { cost_usd, .. } = kept.decide("{\"envelope\": \"not an event\"") else {
        panic!("unreadable");
    };
    assert_eq!(cost_usd, Some(COST));
}

/// A readable event that is not this run's and this call's `llm.response`
/// is not published, and is not the unreadable case either.
#[test]
fn another_runs_or_another_calls_event_is_not_the_calls() {
    let kept = kept();
    let mut another_run = kept.event.clone();
    another_run.envelope.invocation_id = Uuid::now_v7();
    let mut another_call = kept.event.clone();
    let EventPayload::LlmResponse(response) = &mut another_call.payload else {
        unreachable!()
    };
    response.call_id = Uuid::now_v7();
    for event in [another_run, another_call] {
        let stored = serde_json::to_string(&event).unwrap();
        assert!(
            matches!(kept.decide(&stored), Owed::NotTheCalls),
            "{stored}"
        );
    }
}
