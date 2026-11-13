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
/// `llm.request`. Capped, the run completes and every event it
/// publishes would fit a stock broker.
///
/// `invocation.archived` included. The cap does not govern it: it
/// carries the reducer's state blob, the conversation with this answer
/// in it, about 263,000 bytes. As a JSON array of integers, about four
/// bytes on the wire for each byte of state, this archive was about
/// 1,052,000 bytes, over the limit; as base64 (#981), four bytes for
/// every three, it is about 351,000, with room for a second capped
/// answer (see the next test).
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
    for event in &events {
        let size = wire_size(event);
        assert!(
            size <= STOCK_MAX_PAYLOAD,
            "{} is {size} bytes, over a stock max_payload",
            event_kind(event)
        );
    }
}

/// What a broker counts against its `max_payload` for `event`: its JSON
/// and the message-id header the bus publishes with it.
///
/// Measured from the JSON, not from `wire_size`, which counts the
/// body alone today: adding the header to whatever `wire_size` returns
/// would count it twice once `wire_size` counts it too.
fn counted_by_the_broker(event: &Event) -> usize {
    let body = serde_json::to_vec(event).expect("the event serialises");
    let header = format!(
        "NATS/1.0\r\nNats-Msg-Id: {}\r\n\r\n",
        event.envelope.event_id
    );
    body.len() + header.len()
}

/// Where a stock broker stops taking the archive, counted in capped
/// answers (#981). The archive writes the run's state as base64, four
/// bytes for every three, and the default cap is a quarter of a stock
/// `max_payload`, so three capped answers are a whole 1 MiB of base64
/// before the rest of the conversation is counted. A run that read one
/// capped answer archives in about 351 KB, about 697 KB under the
/// limit, which is room for a second; a run that read three cannot
/// archive on a stock broker.
#[tokio::test]
async fn two_capped_answers_archive_on_a_stock_broker_and_three_do_not() {
    let mut archives = Vec::new();
    for answers in 1..=3 {
        let world = SimWorld::new(981, 5.0).await;
        let llm = FixtureClient::new();
        for i in 0..answers {
            world
                .tool
                .push_output(ToolResult::ok("o".repeat(2 * STOCK_MAX_PAYLOAD)));
            llm.push_response(sim_tool_call(&format!("read-{i}")));
        }
        llm.push_response(end_turn("read the large answers"));

        let outcome = world.run(&llm).await.expect("the run completes");
        assert!(
            matches!(outcome, InvocationOutcome::Completed { .. }),
            "{answers} answers: expected Completed, got {outcome:?}"
        );
        let archive = world
            .sink
            .events()
            .into_iter()
            .find(|e| matches!(e.payload, EventPayload::InvocationArchived(_)))
            .expect("the run publishes its archive");
        archives.push(counted_by_the_broker(&archive));
    }

    let [one, two, three] = archives[..] else {
        unreachable!("three runs");
    };
    assert!(
        one <= 360 * 1000,
        "one capped answer archives in {one} bytes, more than the about 351 KB this test's \
         documentation records"
    );
    assert!(
        two <= STOCK_MAX_PAYLOAD,
        "two capped answers archive in {two} bytes, over a stock max_payload"
    );
    assert!(
        three > STOCK_MAX_PAYLOAD,
        "three capped answers archive in {three} bytes, under a stock max_payload; the \
         operating guide says they do not fit"
    );
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
