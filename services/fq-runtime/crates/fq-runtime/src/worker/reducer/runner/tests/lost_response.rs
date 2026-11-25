//! A successful model call whose `llm.response` publish is lost (issue
//! 1168).
//!
//! A successful call's `llm.response` is the only event that carries what
//! the call cost: its usage on the payload, its cost on the envelope. The
//! runner closes the call's row in the worker's store, with the call's
//! cost, and then publishes the event. A bus fault at that publish ends
//! the run's drive with the error and leaves the run in flight, and so
//! does a stop; the next start resumes the run from its rows. The event
//! must reach the stream by then, and count once, or the call's spend is
//! missing from every cost report.
//!
//! A call's publish is awaited before the run does anything else, so only
//! the run's last call can have lost its `llm.response`. The calls before
//! it went out, and a resume must not publish them again.
//!
//! The runs here make three billed calls: the first two ask for the tool,
//! the third reports the outcome. They use the harness of the lost failure
//! (`lost_failure.rs`). The recording sink keeps every copy it is given,
//! as the stream does once the broker's duplicate window has passed, so a
//! copy published twice is checked where the window no longer helps: in
//! the projection the cost reports read.

use super::lost_failure::{TOOL, World, a_world, assert_same_event, cost_of, usage};
use super::*;
use crate::control_plane::projection::store::ProjectionStore;
use crate::test_support::oracle::check_resume_trace;
use crate::worker::reducer::rounds::totals_of_wal_rows;

/// What each of the run's three calls is billed for, tokens in and out.
const CALLS: [(u32, u32); 3] = [(10_000, 2_000), (3_000, 500), (1_000, 100)];

/// The publishes of the run: `triggered`, then the first call's
/// `llm.request`, `llm.dispatched` and `llm.response` (3), the tool's
/// three, the second call's three (its `llm.response` is 9), the tool's
/// three (its `tool.call` is 10), the third call's three, `completed` and
/// `invocation.archived`.
const FIRST_RESPONSE: usize = 3;
const SECOND_RESPONSE: usize = 9;
const SECOND_TOOL_CALL: usize = 10;

/// The model's answers, in the order the run asks for them.
fn answers() -> Vec<ChatResponse> {
    vec![
        tool_use(TOOL, "tc-0", json!({}), CALLS[0]),
        tool_use(TOOL, "tc-1", json!({}), CALLS[1]),
        canned("done", CALLS[2].0, CALLS[2].1),
    ]
}

/// A model that gives `answers`, in order.
fn model(answers: &[ChatResponse]) -> FixtureClient {
    let llm = FixtureClient::new();
    for answer in answers {
        llm.push_response(answer.clone());
    }
    llm
}

/// What the run was billed: its three calls, summed in the order the run
/// summed them.
fn billed() -> f64 {
    CALLS.iter().fold(0.0, |sum, &(input, output)| {
        sum + cost_of(usage(input, output))
    })
}

/// Run until the publish numbered `index`, which fails and must be the
/// publish of `kind`. Returns the model, which says what the run asked.
async fn stopped_at(world: &World, index: usize, kind: &str) -> FixtureClient {
    let llm = model(&answers());
    world.sink.fail_publish_at(index);
    let outcome = world.run_with(&llm).await;
    assert!(
        matches!(outcome, Err(ExecutorError::Bus(_))),
        "the stop: {outcome:?}"
    );
    let refused = world.sink.refused();
    assert_eq!(
        refused
            .first()
            .map(|event| event.payload.event_type())
            .unwrap_or("nothing"),
        kind,
        "the publish the stop falls at"
    );
    llm
}

/// The next start: the fault is over, and the run is resumed, with a
/// model that gives the answers the stopped run had not asked for.
async fn resumed(world: &World, asked: &FixtureClient) -> Result<InvocationOutcome, ExecutorError> {
    world.sink.clear_fault();
    let in_flight = world.store.find_in_flight_invocations().await.unwrap();
    assert_eq!(in_flight.len(), 1, "the stop left the run in flight");
    let rest = model(&answers()[asked.requests().len()..]);
    world
        .runner
        .resume(&world.agent, &rest, world.invocation_id())
        .await
}

/// The run's calls, by `call_id`, in the order the run made them.
fn call_ids(events: &[Event]) -> Vec<Uuid> {
    let mut ids = Vec::new();
    for event in events {
        if let EventPayload::LlmRequest(request) = &event.payload
            && !ids.contains(&request.call_id)
        {
            ids.push(request.call_id);
        }
    }
    ids
}

/// Every copy on the stream of call `n`'s `llm.response`: each is the one
/// event, with the call's usage and cost. Fails when there is none.
fn copies_of_response(events: &[Event], n: usize) -> Vec<Event> {
    let call_id = call_ids(events)[n];
    let copies: Vec<Event> = events
        .iter()
        .filter(|event| {
            matches!(&event.payload, EventPayload::LlmResponse(response) if response.call_id == call_id)
        })
        .cloned()
        .collect();
    let Some(first) = copies.first() else {
        panic!(
            "call {}'s llm.response, with its usage and cost, is on the stream: {:?}",
            n + 1,
            events
                .iter()
                .map(|event| event.payload.event_type())
                .collect::<Vec<_>>()
        );
    };
    for copy in &copies[1..] {
        assert_same_event(copy, first, "every copy on the stream is the one event");
    }
    let EventPayload::LlmResponse(payload) = &first.payload else {
        unreachable!()
    };
    assert_eq!(
        payload.usage.input_tokens,
        CALLS[n].0,
        "call {}'s usage",
        n + 1
    );
    let cost = first.envelope.cost.as_ref().expect("the call is priced");
    assert!(
        (cost.total_cost - cost_of(usage(CALLS[n].0, CALLS[n].1))).abs() < 1e-12,
        "call {}'s cost: {cost:?}",
        n + 1
    );
    copies
}

/// Each of the run's calls has its `llm.response` on the stream, once.
fn assert_each_response_once(events: &[Event]) {
    for n in 0..CALLS.len() {
        assert_eq!(
            copies_of_response(events, n).len(),
            1,
            "call {}'s llm.response, one copy",
            n + 1
        );
    }
}

/// What the stopped and resumed run left, checked: it completed, its
/// `completed` carries the totals its WAL rows record, the projection
/// folded from every event on the stream, copies included, gives it the
/// cost it was billed, once, the resume's own publishes are a resume
/// trace, and nothing is left in flight.
async fn assert_the_run_ended_with_its_cost_counted_once(
    world: &World,
    resumed: &Result<InvocationOutcome, ExecutorError>,
    from: usize,
) {
    assert!(
        matches!(resumed, Ok(InvocationOutcome::Completed { .. })),
        "the resume completes the run: {resumed:?}"
    );
    let events = world.sink.events();
    let invocation_id = world.invocation_id().to_string();

    let completed: Vec<&Event> = events
        .iter()
        .filter(|event| matches!(event.payload, EventPayload::Completed(_)))
        .collect();
    let [completed] = &completed[..] else {
        panic!("one completed: {completed:?}");
    };
    let EventPayload::Completed(payload) = &completed.payload else {
        unreachable!()
    };
    let llms = world
        .store
        .list_llm_dispatches_for_invocation(&invocation_id)
        .await
        .unwrap();
    let tools = world
        .store
        .list_tool_dispatches_for_invocation(&invocation_id)
        .await
        .unwrap();
    let recorded = totals_of_wal_rows(&llms, &tools);
    assert!(
        (payload.total_cost - billed()).abs() < 1e-12
            && (recorded.total_cost - billed()).abs() < 1e-12,
        "the run's cost, {}, is on its completed ({}) and in its rows ({})",
        billed(),
        payload.total_cost,
        recorded.total_cost
    );
    assert_eq!(
        (payload.total_llm_calls, payload.total_tool_calls),
        (3, 2),
        "the run's call counts on its completed"
    );
    assert_eq!(
        (recorded.total_llm_calls, recorded.total_tool_calls),
        (3, 2),
        "the run's call counts in its rows"
    );

    let projection = ProjectionStore::open(&world.dir.path().join("projection.db"))
        .await
        .unwrap();
    for (seq, event) in events.iter().enumerate() {
        projection
            .insert_event(event, Some(seq as u64))
            .await
            .unwrap();
    }
    let run = projection
        .cost_of_invocation(&invocation_id)
        .await
        .unwrap()
        .expect("the run's cost is on record");
    assert!(
        (run.total_cost - billed()).abs() < 1e-12,
        "the run's cost in the projection is {}, billed {}",
        run.total_cost,
        billed()
    );
    let input: u32 = CALLS.iter().map(|&(input, _)| input).sum();
    assert_eq!(
        (run.event_count, run.total_input_tokens),
        (3, i64::from(input)),
        "one row for each call"
    );
    let agents = projection
        .cost_summary(Some(world.agent.id().as_str()), None)
        .await
        .unwrap();
    let [agent] = &agents[..] else {
        panic!("one agent: {agents:?}");
    };
    assert!(
        (agent.total_cost - billed()).abs() < 1e-12,
        "fq costs reads {} for the agent, billed {}",
        agent.total_cost,
        billed()
    );

    if let Err(violations) = check_resume_trace(&events[from..]) {
        let lines: Vec<String> = violations.iter().map(|v| format!("  - {v}")).collect();
        panic!("the resume's trace:\n{}", lines.join("\n"));
    }
    assert!(
        world
            .store
            .find_in_flight_invocations()
            .await
            .unwrap()
            .is_empty(),
        "nothing for the next start to resume"
    );
}

/// **Red before issue 1168.** A bus fault at the first call's
/// `llm.response`, after the call was billed and its row closed. The fault
/// ends the run's drive and leaves the run in flight, with the call's
/// cost on no event. The next start publishes that very event, chained to
/// the call's `llm.dispatched`, before the run goes on; each call's cost
/// is on the stream once, and the run's totals are what its rows record.
#[tokio::test]
async fn a_lost_response_publish_keeps_the_calls_cost_on_the_stream() {
    let world = a_world().await;
    let asked = stopped_at(&world, FIRST_RESPONSE, "llm_response").await;
    assert!(
        !world
            .sink
            .events()
            .iter()
            .any(|event| matches!(event.payload, EventPayload::LlmResponse(_))),
        "until the next start, the call's cost is on no event"
    );
    let from = world.sink.events().len();
    let resumed = resumed(&world, &asked).await;
    let events = world.sink.events();
    let copies = copies_of_response(&events, 0);
    let [first] = &copies[..] else {
        panic!("the first call's llm.response, one copy: {copies:?}");
    };
    assert_same_event(
        first,
        &world.sink.refused()[0],
        "the resume publishes the llm.response the fault lost, itself",
    );
    let dispatched = events
        .iter()
        .find(|event| matches!(event.payload, EventPayload::LlmDispatched(_)))
        .expect("the call's llm.dispatched went out");
    assert_eq!(
        first.envelope.parent_event_id,
        Some(dispatched.envelope.event_id),
        "chained to the call's llm.dispatched"
    );
    assert_each_response_once(&events);
    assert_the_run_ended_with_its_cost_counted_once(&world, &resumed, from).await;
}

/// The first call's `llm.response` reached the stream, and the run never
/// learned it: the broker stored it and the acknowledgement was lost, or
/// the daemon stopped before it acted on it. The resume cannot tell this
/// from a publish that was lost. Whatever it publishes, the stream holds
/// the one event, and the cost reports count the call once.
#[tokio::test]
async fn a_response_out_before_the_stop_is_counted_once() {
    let world = a_world().await;
    let asked = model(&answers());
    world.sink.lose_ack_at(FIRST_RESPONSE);
    let outcome = world.run_with(&asked).await;
    assert!(
        matches!(outcome, Err(ExecutorError::Bus(_))),
        "the stop: {outcome:?}"
    );
    assert_eq!(
        world
            .sink
            .events()
            .last()
            .map(|event| event.payload.event_type()),
        Some("llm_response"),
        "the call's llm.response is out, and nothing after it"
    );
    let from = world.sink.events().len();
    let resumed = resumed(&world, &asked).await;
    let events = world.sink.events();
    copies_of_response(&events, 0);
    for n in 1..CALLS.len() {
        assert_eq!(copies_of_response(&events, n).len(), 1, "call {}", n + 1);
    }
    assert_the_run_ended_with_its_cost_counted_once(&world, &resumed, from).await;
}

/// **Red before issue 1168.** A run with two calls done, whose second
/// call's `llm.response` is lost. The resume publishes that one, and not
/// the first call's, which went out before the run went on.
#[tokio::test]
async fn a_resume_publishes_the_last_calls_lost_response_alone() {
    let world = a_world().await;
    let asked = stopped_at(&world, SECOND_RESPONSE, "llm_response").await;
    let from = world.sink.events().len();
    let resumed = resumed(&world, &asked).await;
    let events = world.sink.events();
    assert_same_event(
        &copies_of_response(&events, 1)[0],
        &world.sink.refused()[0],
        "the resume publishes the second call's lost llm.response, itself",
    );
    assert_each_response_once(&events);
    assert_the_run_ended_with_its_cost_counted_once(&world, &resumed, from).await;
}

/// A run with two calls done, both out, stopped at the next tool's
/// `tool.call`. The resume publishes no `llm.response` again: every call's
/// is on the stream once.
#[tokio::test]
async fn a_resume_publishes_no_response_that_went_out() {
    let world = a_world().await;
    let asked = stopped_at(&world, SECOND_TOOL_CALL, "tool_call").await;
    let from = world.sink.events().len();
    let resumed = resumed(&world, &asked).await;
    let events = world.sink.events();
    assert!(
        !events[from..]
            .iter()
            .take_while(|event| !matches!(event.payload, EventPayload::LlmRequest(_)))
            .any(|event| matches!(event.payload, EventPayload::LlmResponse(_))),
        "the resume publishes no llm.response before its own call: {:?}",
        events[from..]
            .iter()
            .map(|event| event.payload.event_type())
            .collect::<Vec<_>>()
    );
    assert_each_response_once(&events);
    assert_the_run_ended_with_its_cost_counted_once(&world, &resumed, from).await;
}
