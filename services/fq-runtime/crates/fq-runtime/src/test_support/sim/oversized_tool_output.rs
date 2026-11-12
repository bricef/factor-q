//! The oversized-tool-output script: a tool whose answer is larger than
//! a stock broker's `max_payload`, the case the 2026-09-03
//! production-readiness review named (finding C4).
//!
//! Two changes use it, so it lands with whichever comes first:
//!
//! - the tool-result cap (#827), here: the `tool.result` and every
//!   `llm.request` after it stay under a stock `max_payload`, because
//!   the answer is cut before anything records it to `[tools]
//!   max_result_bytes` — counted as the bytes it takes inside the event,
//!   so an answer of control characters is cut shorter than one of
//!   letters;
//! - the crash DST case for an event over `max_payload` (#828), which
//!   drives the script against a sink with a `max_payload` of its own.
//!   With the cap in force a tool's answer takes no more than `[tools]
//!   max_result_bytes` inside the event, so for the `tool.result` to be
//!   the event refused, that sink's limit has to sit below the cap.

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
    load_tool_answer(world, llm, "o".repeat(output_bytes));
}

/// Load the script with `answer` as the tool's output, whatever it is
/// made of.
pub(super) fn load_tool_answer(world: &SimWorld, llm: &FixtureClient, answer: String) {
    world.tool.push_output(ToolResult::ok(answer));
    llm.push_response(sim_tool_call("oversized"));
    llm.push_response(end_turn("read the large answer"));
}

/// The bytes `event` takes on the wire, as the bus serialises it.
fn wire_size(event: &Event) -> usize {
    serde_json::to_vec(event)
        .expect("the event serialises")
        .len()
}

/// A tool answering twice a stock `max_payload` used to put a 2 MiB
/// `tool.result` on the bus, and the same text into every later
/// `llm.request`. Capped, the run completes and every event the cap
/// governs would fit a stock broker.
///
/// `invocation.archived` is not among them. It carries the reducer's
/// state blob — the conversation, this answer included — encoded as a
/// JSON array of integers, roughly four bytes on the wire per byte of
/// state, so one answer at the 256 KiB cap makes the archive about
/// 1 MiB. State-blob sizing is outside #827, which names it out of
/// scope; the archive is left out of the check below rather than the
/// check being weakened for every event.
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
    assert!(
        answer.len() <= cap,
        "{} bytes over a {cap}-byte cap",
        answer.len()
    );
    assert!(
        answer.contains(&format!("of {} bytes", 2 * STOCK_MAX_PAYLOAD)),
        "the marker counts what the tool produced"
    );
    let governed = events
        .iter()
        .filter(|e| !matches!(e.payload, EventPayload::InvocationArchived(_)));
    for event in governed {
        let size = wire_size(event);
        assert!(
            size <= STOCK_MAX_PAYLOAD,
            "{} is {size} bytes, over a stock max_payload",
            event_kind(event)
        );
    }
}

/// The same answer made of NUL bytes — what most of an uncompressed tar
/// of small files is, and valid UTF-8, so `file_read` returns it. JSON
/// writes each NUL as six bytes (`\u0000`), so a cap that counted text
/// bytes let 256 KiB of it become about 1.5 MiB in the `tool.result`
/// and in every `llm.request` after it, and a stock broker refused the
/// publish. The cap counts the bytes the answer takes in the event.
#[tokio::test]
async fn an_answer_of_control_characters_is_capped_by_its_size_on_the_wire() {
    let world = SimWorld::new(827, 5.0).await;
    let llm = FixtureClient::new();
    load_tool_answer(&world, &llm, "\u{0}".repeat(2 * STOCK_MAX_PAYLOAD));

    let outcome = world.run(&llm).await.expect("the run completes");
    assert!(
        matches!(outcome, InvocationOutcome::Completed { .. }),
        "expected Completed, got {outcome:?}"
    );

    let events = world.sink.events();
    let result_at = events
        .iter()
        .position(|e| matches!(e.payload, EventPayload::ToolResult(_)))
        .expect("a tool.result event");
    let next_request = events[result_at..]
        .iter()
        .find(|e| matches!(e.payload, EventPayload::LlmRequest(_)))
        .expect("an llm.request after the tool answered");
    for event in [&events[result_at], next_request] {
        let size = wire_size(event);
        assert!(
            size <= STOCK_MAX_PAYLOAD,
            "{} is {size} bytes, over a stock max_payload",
            event_kind(event)
        );
    }
}
