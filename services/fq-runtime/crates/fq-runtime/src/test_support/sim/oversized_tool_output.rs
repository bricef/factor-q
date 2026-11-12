//! The oversized-tool-output script: a tool whose answer is larger than
//! a stock broker's `max_payload`, the case the 2026-09-03
//! production-readiness review named (finding C4).
//!
//! Two changes use it, so it lands with whichever comes first:
//!
//! - the tool-result cap (#827), here: every event the run publishes
//!   stays under a stock `max_payload`, because the answer is cut to
//!   `[tools] max_result_bytes` before anything records it;
//! - the crash DST case for an event over `max_payload` (#828), which
//!   drives the script against a sink with a `max_payload` of its own.
//!   With the cap in force a tool's answer reaches the sink at no more
//!   than `[tools] max_result_bytes`, so for the `tool.result` to be the
//!   event refused, that sink's limit has to sit below the cap.

use super::tests::{end_turn, sim_tool_call};
use super::*;
use crate::test_support::events::event_kind;

/// A stock NATS broker's `max_payload`: 1 MiB.
pub(super) const STOCK_MAX_PAYLOAD: usize = 1024 * 1024;

/// Load the script into `world` and `llm`: one [`SIM_TOOL`] call whose
/// answer is `output_bytes` bytes long, then an end turn.
pub(super) fn load_oversized_tool_output(
    world: &SimWorld,
    llm: &FixtureClient,
    output_bytes: usize,
) {
    world.tool.push_output(ToolResult::ok("o".repeat(output_bytes)));
    llm.push_response(sim_tool_call("oversized"));
    llm.push_response(end_turn("read the large answer"));
}

/// A tool answering twice a stock `max_payload` used to put a 2 MiB
/// `tool.result` on the bus, and the same text into every later
/// `llm.request`. Capped, the run completes and every event it
/// publishes would fit a stock broker.
#[tokio::test]
async fn a_tool_answer_past_a_stock_max_payload_publishes_capped() {
    let world = SimWorld::new(827, 5.0).await;
    let llm = FixtureClient::new();
    load_oversized_tool_output(&world, &llm, 2 * STOCK_MAX_PAYLOAD);

    let outcome = world.run(&llm).await.expect("the run completes");
    assert!(
        matches!(outcome, InvocationOutcome::Completed { .. }),
        "expected Completed, got {outcome:?}"
    );

    let events = world.sink.events();
    let cap = crate::tools::ToolCallLimits::default().max_result_bytes;
    let answer = events
        .iter()
        .find_map(|e| match &e.payload {
            EventPayload::ToolResult(p) => Some(&p.output),
            _ => None,
        })
        .expect("a tool.result event");
    assert!(answer.len() <= cap, "{} bytes over a {cap}-byte cap", answer.len());
    assert!(
        answer.contains(&format!("of {} bytes", 2 * STOCK_MAX_PAYLOAD)),
        "the marker counts what the tool produced"
    );
    for event in &events {
        let size = serde_json::to_vec(event).expect("the event serialises").len();
        assert!(
            size <= STOCK_MAX_PAYLOAD,
            "{} is {size} bytes, over a stock max_payload",
            event_kind(event)
        );
    }
}
