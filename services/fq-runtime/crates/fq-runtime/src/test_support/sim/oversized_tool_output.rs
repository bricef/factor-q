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
//!   the event refused, that sink's limit has to sit below the cap: the
//!   cap is left at its default and the limit set under it, which is a
//!   broker configured below the cap, the case the containment is for.
//!
//! Beside that case, the other places a size refusal can land (#828):
//! on a resume whose replay rebuilds the refused event, on `triggered`
//! before anything durable exists, on the terminal event, and on the
//! archive after the run has already ended.

use super::tests::{end_turn, sim_tool_call};
use super::*;
use crate::events::{EventKind, FailedPayload, FailureDetail, FailureKind, FailurePhase};
use crate::test_support::events::event_kind;
use crate::worker::ResumeError;

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

/// The bytes `event` takes on the wire, as the bus sends it: its JSON
/// and the message-id header with it, which the broker counts too.
fn wire_size(event: &Event) -> usize {
    let body = serde_json::to_vec(event).expect("the event serialises");
    crate::bus::size::wire_len(event, body.len())
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
/// scope (see #981); the archive is left out of the check below rather
/// than the check being weakened for every event.
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

/// The kinds of `events`, in order.
fn kinds(events: &[Event]) -> Vec<&'static str> {
    events.iter().map(event_kind).collect()
}

/// Every `failed` payload in `events`.
fn failures(events: &[Event]) -> Vec<&FailedPayload> {
    events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::Failed(p) => Some(p),
            _ => None,
        })
        .collect()
}

/// A world whose invocations each take a workspace of their own under
/// `root`, so a test can see one reclaimed.
async fn world_with_workspaces(seed: u64, root: &std::path::Path) -> SimWorld {
    let provider = Arc::new(crate::worker::workspace::PerInvocationWorkspace::new(
        root.to_path_buf(),
    ));
    SimWorld::with_workspace(seed, 5.0, provider).await
}

/// `events` hold exactly one `failed`, and it is the one a refusal of
/// `refused` under `limit` ends a run with: the new kind, the phase of
/// the refused event, and the detail naming that event with its size on
/// the wire and the sink's limit.
fn assert_one_failed_for(events: &[Event], refused: &Event, limit: usize, phase: FailurePhase) {
    let failed = failures(events);
    assert_eq!(failed.len(), 1, "exactly one failed: {:?}", kinds(events));
    let failed = failed[0];
    let size = wire_size(refused);
    assert!(
        matches!(failed.error_kind, FailureKind::PayloadTooLarge),
        "{:?}",
        failed.error_kind
    );
    assert_eq!(format!("{:?}", failed.phase), format!("{phase:?}"));
    assert_eq!(
        failed.detail,
        Some(FailureDetail::PayloadTooLarge {
            event_type: EventKind::from(&refused.payload),
            size_bytes: size as u64,
            limit_bytes: limit as u64,
        })
    );
    assert_eq!(
        failed.error_message,
        format!("event of {size} bytes exceeds the broker max_payload of {limit} bytes")
    );
}

/// The row is terminal and failed, and its workspace is gone.
async fn assert_ended_and_reclaimed(world: &SimWorld, id: uuid::Uuid) {
    let row = world
        .store
        .get_invocation_state(&id.to_string())
        .await
        .unwrap()
        .expect("state row");
    assert!(row.terminal_at.is_some(), "the row is terminal");
    assert_eq!(row.phase, "failed");
    let workspace = row.workspace_ref.expect("workspace_ref persisted");
    assert!(
        !std::path::Path::new(&workspace).exists(),
        "a terminal run's workspace is reclaimed"
    );
}

/// Nothing is left for recovery: a resume refuses the row as terminal.
async fn assert_resume_refused(world: &SimWorld, id: uuid::Uuid) {
    let resumed = world
        .runner
        .resume(&world.agent, &FixtureClient::new(), id)
        .await;
    assert!(
        matches!(
            resumed,
            Err(ExecutorError::Resume(ResumeError::AlreadyTerminal { .. }))
        ),
        "{resumed:?}"
    );
}

/// The Phase 2 exit criterion, **oversized tool output**, as a crash DST
/// case (#828). The broker's limit sits below the tool-result cap, so the
/// capped answer is still too large: the `tool.result` is refused, and
/// the run ends with exactly one `failed` of the new kind naming it. The
/// row is terminal, the workspace reclaimed, and recovery has nothing to
/// resume. Before #828 the refusal surfaced as a transient bus error
/// with no terminal event: the row stayed in flight for every restart to
/// resume into the same refusal.
#[tokio::test]
async fn a_tool_result_too_large_for_the_broker_ends_the_run_with_one_failed() {
    let root = tempfile::tempdir().expect("workspace root");
    let world = world_with_workspaces(828, root.path()).await;
    let cap = crate::tools::ToolCallLimits::default().max_result_bytes;
    let limit = cap / 2;
    world.sink.set_max_payload(limit);
    let llm = FixtureClient::new();
    load_oversized_tool_output(&world, &llm, 2 * STOCK_MAX_PAYLOAD);

    let err = world.run(&llm).await.expect_err("the refusal ends the run");
    assert!(
        matches!(
            err,
            ExecutorError::InvocationFailed {
                kind: FailureKind::PayloadTooLarge,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(!err.is_transient(), "nothing retries it");

    let refused = world.sink.refused();
    assert_eq!(kinds(&refused), ["tool_result"]);
    let events = world.sink.events();
    assert_eq!(
        kinds(&events),
        [
            "triggered",
            "llm_request",
            "llm_dispatched",
            "llm_response",
            "tool_call",
            "tool_dispatched",
            "failed",
            "invocation_archived",
        ]
    );
    assert_one_failed_for(&events, &refused[0], limit, FailurePhase::ToolResult);

    let id = world.invocation_id();
    assert_ended_and_reclaimed(&world, id).await;
    assert_resume_refused(&world, id).await;
}

/// The zombie #828 was filed for. A crash at the `tool.result` publish
/// leaves the answer in the WAL; the broker then comes back with a limit
/// the next `llm.request`, which carries that answer, exceeds. The resume
/// replays the turn, rebuilds the request, and ends the run with one
/// `failed` naming it, without asking the model. The row is terminal, so
/// the next resume finds nothing to do, where it used to meet the same
/// refusal on every start.
///
/// The `invocation.archived` behind the `failed` carries the conversation,
/// answer included, and is refused too. The run has ended by then, so it
/// returns its own failure rather than the archive's refusal; the row is
/// left terminal and unarchived (see #981).
#[tokio::test]
async fn a_resume_whose_replay_rebuilds_an_oversized_request_ends_the_run_once() {
    let root = tempfile::tempdir().expect("workspace root");
    let world = world_with_workspaces(829, root.path()).await;
    let llm = FixtureClient::new();
    load_oversized_tool_output(&world, &llm, 64 * 1024);
    // Publishes: 0 triggered, 1-3 the model turn, 4 tool.call,
    // 5 tool.dispatched, 6 tool.result ← the crash.
    world.sink.fail_publish_at(6);
    let err = world.run(&llm).await.expect_err("the crash");
    assert!(
        matches!(err, ExecutorError::Bus(BusError::Publish(_))),
        "{err:?}"
    );
    let id = world.invocation_id();
    let before = world.sink.events().len();

    world.sink.clear_fault();
    let limit = 32 * 1024;
    world.sink.set_max_payload(limit);
    let resume_llm = FixtureClient::new();
    resume_llm.push_response(end_turn("never asked"));
    let err = world
        .runner
        .resume(&world.agent, &resume_llm, id)
        .await
        .expect_err("the replay rebuilds the refused request");
    assert!(
        matches!(
            err,
            ExecutorError::InvocationFailed {
                kind: FailureKind::PayloadTooLarge,
                ..
            }
        ),
        "the run's own failure, not the archive's refusal: {err:?}"
    );

    let refused = world.sink.refused();
    assert_eq!(
        kinds(&refused),
        ["tool_result", "llm_request", "invocation_archived"]
    );
    let resumed = &world.sink.events()[before..];
    assert_eq!(kinds(resumed), ["failed"]);
    assert_one_failed_for(resumed, &refused[1], limit, FailurePhase::LlmRequest);
    assert!(resume_llm.requests().is_empty(), "the model is never asked");

    assert_ended_and_reclaimed(&world, id).await;
    let row = world
        .store
        .get_invocation_state(&id.to_string())
        .await
        .unwrap()
        .expect("the row stays");
    assert_eq!(row.archive_status, None, "its archive was never handed off");
    assert_resume_refused(&world, id).await;
}

/// Before the first WAL write (#828): the `triggered` event itself is too
/// large, here for its trigger payload. Nothing durable exists, so the
/// `failed` is the whole record, unchained since nothing went before it;
/// no row is written and the workspace is not kept. The error is not
/// transient, so the dispatcher consumes the trigger rather than
/// redelivering it into the same refusal until it is dead-lettered as
/// `trigger_exhausted`.
#[tokio::test]
async fn a_triggered_too_large_for_the_broker_is_recorded_by_its_failed_alone() {
    let root = tempfile::tempdir().expect("workspace root");
    let world = world_with_workspaces(830, root.path()).await;
    let limit = 16 * 1024;
    world.sink.set_max_payload(limit);
    let llm = FixtureClient::new();
    llm.push_response(end_turn("never asked"));

    let err = world
        .runner
        .run(
            &world.agent,
            &llm,
            TriggerSource::Manual,
            None,
            json!({ "brief": "b".repeat(4 * limit) }),
        )
        .await
        .expect_err("the refusal ends the run");
    assert!(
        matches!(
            err,
            ExecutorError::InvocationFailed {
                kind: FailureKind::PayloadTooLarge,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(!err.is_transient(), "the dispatcher consumes the trigger");

    let refused = world.sink.refused();
    assert_eq!(kinds(&refused), ["triggered"]);
    let events = world.sink.events();
    assert_eq!(kinds(&events), ["failed"]);
    assert_one_failed_for(&events, &refused[0], limit, FailurePhase::Setup);
    let id = refused[0].envelope.invocation_id;
    assert_eq!(events[0].envelope.invocation_id, id);
    assert_eq!(events[0].envelope.parent_event_id, None);

    assert!(
        world
            .store
            .get_invocation_state(&id.to_string())
            .await
            .unwrap()
            .is_none(),
        "nothing durable is written"
    );
    let left: Vec<_> = std::fs::read_dir(root.path())
        .expect("workspace root readable")
        .flatten()
        .collect();
    assert!(left.is_empty(), "no workspace is kept: {left:?}");
    assert!(llm.requests().is_empty(), "the model is never asked");
}

/// After the run has ended (#828, see #981): the archive behind
/// `completed` carries the conversation at about four times its size, so
/// here it is the only event too large. The row is already terminal, so
/// this is not a failure of the invocation: no `failed` follows the
/// `completed`, and the refusal comes back exactly as it did before
/// #828, with the row left for the archive sweeper.
#[tokio::test]
async fn an_archive_too_large_after_completed_is_not_an_invocation_failure() {
    let root = tempfile::tempdir().expect("workspace root");
    let world = world_with_workspaces(831, root.path()).await;
    let limit = 400 * 1024;
    world.sink.set_max_payload(limit);
    let llm = FixtureClient::new();
    load_oversized_tool_output(&world, &llm, 150 * 1024);

    let err = world.run(&llm).await.expect_err("the archive is refused");
    assert!(
        matches!(
            err,
            ExecutorError::Bus(BusError::PayloadTooLarge {
                event_type: EventKind::InvocationArchived,
                ..
            })
        ),
        "{err:?}"
    );

    assert_eq!(kinds(&world.sink.refused()), ["invocation_archived"]);
    let events = world.sink.events();
    assert_eq!(kinds(&events).last(), Some(&"completed"));
    assert!(failures(&events).is_empty(), "no second terminal event");

    let row = world
        .store
        .get_invocation_state(&world.invocation_id().to_string())
        .await
        .unwrap()
        .expect("the row stays for the sweeper");
    assert!(row.terminal_at.is_some());
    assert_eq!(row.phase, "completed");
    assert_eq!(row.archive_status, None, "the archive is the sweeper's");
}

/// The terminal event itself (#828). A `completed` the broker would
/// refuse must not be what the row stores: nothing could ever publish it,
/// and the row would be terminal with its outcome nowhere. The run loop
/// measures it before the row goes terminal and ends the run on a
/// `failed` naming it instead, stored and published in its place.
///
/// A `completed` carries the summary the model wrote, and the
/// `llm.response` before it carries the same text, so live it is that
/// response that is refused first. On a resume it is not: a crash at the
/// response's publish leaves the turn in the WAL, and the replay goes
/// straight to the `completed`.
#[tokio::test]
async fn a_completed_too_large_for_the_broker_ends_the_run_failed_in_its_place() {
    let root = tempfile::tempdir().expect("workspace root");
    let world = world_with_workspaces(832, root.path()).await;
    let llm = FixtureClient::new();
    llm.push_response(end_turn(&"s".repeat(64 * 1024)));
    // Publishes: 0 triggered, 1 llm.request, 2 llm.dispatched,
    // 3 llm.response ← the crash.
    world.sink.fail_publish_at(3);
    world.run(&llm).await.expect_err("the crash");
    let id = world.invocation_id();
    let before = world.sink.events().len();

    world.sink.clear_fault();
    let limit = 32 * 1024;
    world.sink.set_max_payload(limit);
    let err = world
        .runner
        .resume(&world.agent, &FixtureClient::new(), id)
        .await
        .expect_err("the run ends failed");
    assert!(
        matches!(
            err,
            ExecutorError::InvocationFailed {
                kind: FailureKind::PayloadTooLarge,
                ..
            }
        ),
        "{err:?}"
    );

    // Measured, never published: the only event the sink refused is the
    // crash's.
    assert_eq!(kinds(&world.sink.refused()), ["llm_response"]);
    let resumed = &world.sink.events()[before..];
    assert_eq!(kinds(resumed), ["failed", "invocation_archived"]);
    let failed = failures(resumed)[0];
    assert!(matches!(failed.error_kind, FailureKind::PayloadTooLarge));
    assert!(matches!(failed.phase, FailurePhase::Reducer));
    let Some(FailureDetail::PayloadTooLarge {
        event_type,
        size_bytes,
        limit_bytes,
    }) = failed.detail.clone()
    else {
        panic!("a payload_too_large detail: {:?}", failed.detail);
    };
    assert_eq!(event_type, EventKind::Completed);
    assert!(size_bytes > limit as u64, "{size_bytes}");
    assert_eq!(limit_bytes, limit as u64);

    // The row stores the `failed` it published, under the phase the
    // sweeper archives it with.
    let row = world
        .store
        .get_invocation_state(&id.to_string())
        .await
        .unwrap()
        .expect("state row");
    let stored: Event =
        serde_json::from_str(row.terminal_outcome.as_deref().expect("a stored outcome"))
            .expect("the stored outcome reads");
    assert_eq!(stored.envelope.event_id, resumed[0].envelope.event_id);
    assert!(row.terminal_published_at.is_some(), "it went out");
    assert_ended_and_reclaimed(&world, id).await;
    assert_resume_refused(&world, id).await;
}
