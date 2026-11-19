//! The answers that consume a trigger without a run (issue 1055), each
//! with its ack lost.
//!
//! Every answer that deals with a trigger and starts no run moves the
//! claim to `consumed`, with its reason, and only then acks the delivery.
//! So an ack that does not reach the broker, because the connection
//! dropped or the daemon stopped between the write and the ack, leaves a
//! consumed claim and an unacked message, and the copy the broker brings
//! back meets the claim and is acked and dropped: the trigger is not
//! handled a second time. Each test here makes one such answer through the
//! real dispatcher, on a connection that answers nothing, and then hands
//! the copy in.
//!
//! The answer on a trigger's last delivery, its dead letter published, is
//! in `dead_letter_watch`, beside the watch it hands the trigger to.

use std::sync::atomic::Ordering;
use std::time::Duration;

use super::{Answered, Arena, Run, SEQ, arena};
use crate::control_plane::dispatcher::CONSUMER_NAME;
use crate::control_plane::store::{ClaimState, ConsumedWhy};

/// The claim on the arena's trigger: its state and the run it names.
async fn claim_now(arena: &Arena) -> Option<(ClaimState, Option<String>)> {
    arena
        .claims
        .trigger_claim(arena.key())
        .await
        .unwrap()
        .map(|row| (row.state, row.invocation_id))
}

/// The claim row on the arena's trigger, whole.
async fn claim_row(arena: &Arena) -> Option<crate::control_plane::store::TriggerClaimRow> {
    arena.claims.trigger_claim(arena.key()).await.unwrap()
}

/// How many copies of a consumed trigger the dispatcher has dropped.
fn copies_dropped(arena: &Arena) -> u64 {
    arena
        .bus
        .consumer_ledger()
        .record(CONSUMER_NAME)
        .consumed_dropped
}

/// After `first`, an answer whose ack was lost: the claim is consumed as
/// `why`, and the copy the broker brings back, delivery `copy_attempt`
/// made by `copy`, is acked and dropped, starts nothing, is counted once as
/// a consumed copy dropped, and leaves the claim as it was.
async fn the_copy_is_dropped<F>(arena: &Arena, why: ConsumedWhy, first: Answered, copy: F)
where
    F: std::future::Future<Output = Answered>,
{
    assert_eq!(
        (first.acks, first.naks),
        (0, 0),
        "the first delivery's answer is lost: {first:?}"
    );
    let consumed = claim_now(arena).await.expect("the claim is kept, consumed");
    assert_eq!(consumed.0, ClaimState::Consumed { why: why.clone() });
    let runs = arena.worker.starts().len();
    let dropped = copies_dropped(arena);

    let copy = copy.await;

    assert_eq!((copy.acks, copy.naks), (1, 0), "{copy:?}");
    assert_eq!(arena.worker.starts().len(), runs, "nothing runs again");
    assert_eq!(copies_dropped(arena), dropped + 1, "counted once");
    assert_eq!(
        claim_now(arena).await,
        Some(consumed),
        "the claim is left as it was"
    );
}

/// The subject the arena's agent is triggered on.
fn subject(arena: &Arena) -> String {
    crate::events::subjects::trigger(&arena.agent)
}

/// A payload that parses.
fn payload() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "input": "hi" })).unwrap()
}

/// **A trigger on a subject the dispatcher cannot address is consumed as a
/// refused subject**, and its copy is dropped.
#[tokio::test]
async fn a_refused_subject_is_consumed_and_its_copy_dropped() {
    let (arena, _server) = arena().await;
    let first = arena
        .deliver_shaped(1, true, "fq.not-a-trigger", payload())
        .await;
    let copy = arena.deliver_shaped(2, false, "fq.not-a-trigger", payload());
    the_copy_is_dropped(&arena, ConsumedWhy::RefusedSubject, first, copy).await;
}

/// **A trigger naming an agent id that is not a valid one is consumed as
/// such**, and its copy is dropped.
#[tokio::test]
async fn an_invalid_agent_id_is_consumed_and_its_copy_dropped() {
    let (arena, _server) = arena().await;
    let first = arena
        .deliver_shaped(1, true, "fq.trigger.*", payload())
        .await;
    let copy = arena.deliver_shaped(2, false, "fq.trigger.*", payload());
    the_copy_is_dropped(&arena, ConsumedWhy::InvalidAgentId, first, copy).await;
}

/// **A trigger for an agent that is not loaded is consumed as an unknown
/// agent**, and its copy is dropped.
#[tokio::test]
async fn an_unknown_agent_is_consumed_and_its_copy_dropped() {
    let (arena, _server) = arena().await;
    let unknown = crate::events::subjects::trigger("nobody-loaded-by-this-arena");
    let first = arena.deliver_shaped(1, true, &unknown, payload()).await;
    let copy = arena.deliver_shaped(2, false, &unknown, payload());
    the_copy_is_dropped(&arena, ConsumedWhy::UnknownAgent, first, copy).await;
}

/// **A trigger whose payload does not parse is consumed as an invalid
/// payload**, and its copy is dropped.
#[tokio::test]
async fn an_invalid_payload_is_consumed_and_its_copy_dropped() {
    let (arena, _server) = arena().await;
    let subject = subject(&arena);
    let first = arena
        .deliver_shaped(1, true, &subject, b"not json".to_vec())
        .await;
    let copy = arena.deliver_shaped(2, false, &subject, b"not json".to_vec());
    the_copy_is_dropped(&arena, ConsumedWhy::InvalidPayload, first, copy).await;
}

/// **A run that fails for good before its first WAL write consumes its
/// trigger as failed for good**, and the copy is dropped rather than run
/// to fail again.
#[tokio::test]
async fn a_run_that_fails_for_good_before_its_start_is_consumed_and_its_copy_dropped() {
    let (arena, _server) = arena().await;
    arena.worker.queue(Run::FailsForGood);
    let first = arena.deliver(1, true).await;
    assert_eq!(arena.worker.starts().len(), 1);
    the_copy_is_dropped(
        &arena,
        ConsumedWhy::FailedForGood,
        first,
        arena.deliver(2, false),
    )
    .await;
}

/// **A run an operator halted in setup consumes its trigger as halted in
/// setup**, and the copy is dropped rather than started.
#[tokio::test]
async fn a_run_halted_in_setup_is_consumed_and_its_copy_dropped() {
    let (arena, _server) = arena().await;
    arena.worker.queue(Run::HaltedInSetup);
    let first = arena.deliver(1, true).await;
    the_copy_is_dropped(
        &arena,
        ConsumedWhy::HaltedInSetup,
        first,
        arena.deliver(2, false),
    )
    .await;
}

/// **A run that returns before its first WAL write without failing
/// consumes its trigger as returned unstarted**, and the copy is dropped.
#[tokio::test]
async fn a_run_that_returns_before_its_start_is_consumed_and_its_copy_dropped() {
    let (arena, _server) = arena().await;
    arena.worker.queue(Run::ReturnsUnstarted);
    let first = arena.deliver(1, true).await;
    the_copy_is_dropped(
        &arena,
        ConsumedWhy::ReturnedUnstarted,
        first,
        arena.deliver(2, false),
    )
    .await;
}

/// Another trigger of the arena's agent, at sequence 2, set up and held
/// there: it holds the agent's one slot and the one worker permit, so the
/// arena's trigger, handed in next, waits in a hold. Release it with the
/// worker's setup gate.
async fn an_occupant(arena: &Arena) -> super::Handling {
    arena.worker.queue(Run::WaitsInSetup);
    let occupant = arena.spawn_delivery(2, 1, false);
    arena.worker.until_waiting(1).await;
    occupant
}

/// The arena's trigger handed in on a connection that answers nothing, and
/// left waiting in its hold: its claim is taken, and long enough has passed
/// for its delivery to be past every check before the hold.
async fn held_with_its_ack_lost(arena: &Arena) -> super::Handling {
    let held = arena.spawn_delivery(SEQ, 1, true);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while claim_now(arena).await.is_none() {
        assert!(tokio::time::Instant::now() < deadline, "no claim taken");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    held
}

/// How many messages the trigger stream holds.
async fn triggers_published(arena: &Arena) -> u64 {
    arena
        .bus
        .jetstream()
        .get_stream(crate::bus::TRIGGER_STREAM_NAME)
        .await
        .expect("the trigger stream")
        .info()
        .await
        .expect("its info")
        .state
        .messages
}

/// **A trigger requeued from a hold is consumed as requeued, and a crash
/// between that write and the ack leaves a copy that is dropped (issue
/// 1055).** A drain lets go of the trigger's hold, and the dispatcher
/// publishes its replacement under the same trigger id, records the old
/// delivery's claim consumed, and acks it. The ack is lost here, as a
/// daemon stopping between the write and the ack would lose it: the copy
/// the broker brings back meets the consumed claim and is dropped, so the
/// work runs once, as the replacement, and no second replacement is
/// published.
#[tokio::test]
async fn a_trigger_requeued_from_a_hold_is_consumed_and_its_copy_dropped() {
    let (arena, _server) = arena().await;
    let occupant = an_occupant(&arena).await;
    let held = held_with_its_ack_lost(&arena).await;
    let published = triggers_published(&arena).await;

    arena.worker.draining.store(true, Ordering::SeqCst);
    let first = arena.finish(held).await;
    arena.worker.draining.store(false, Ordering::SeqCst);
    assert_eq!(
        triggers_published(&arena).await,
        published + 1,
        "the replacement"
    );

    the_copy_is_dropped(
        &arena,
        ConsumedWhy::RequeuedFromHold,
        first,
        arena.deliver(2, false),
    )
    .await;
    assert_eq!(
        triggers_published(&arena).await,
        published + 1,
        "no second replacement"
    );
    arena.worker.setup_gate.add_permits(1);
    arena.finish(occupant).await;
}

/// The arena's agent defined again with one slot (`max_concurrent: 1`), as
/// a reload would, so that a second trigger of it waits in the agent's cap
/// hold rather than for a worker permit. The directory holds the
/// definition for as long as the test needs it.
fn with_one_slot(arena: &Arena) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(format!("{}.md", arena.agent));
    std::fs::write(
        &path,
        format!(
            "---\nname: {}\nmodel: claude-haiku\nbudget: 1.0\nmax_concurrent: 1\n---\n\nTest agent.",
            arena.agent
        ),
    )
    .unwrap();
    let mut registry = crate::agent::AgentRegistry::new();
    registry.load_file(&path);
    assert!(registry.errors().is_empty(), "{:?}", registry.errors());
    arena.registry.swap(registry);
    dir
}

/// **A trigger whose agent a reload removed while it waited in a hold is
/// consumed as agent removed**, and its copy is dropped. Two worker
/// permits and one slot for the agent, so the trigger waits in the agent's
/// cap hold, which reads the registry again on every tick.
#[tokio::test]
async fn a_trigger_whose_agent_was_removed_during_a_hold_is_consumed_and_its_copy_dropped() {
    let server = crate::test_support::nats::test_nats();
    let arena = Arena::new(server.url(), 2).await;
    let _definition = with_one_slot(&arena);
    let occupant = an_occupant(&arena).await;
    let held = held_with_its_ack_lost(&arena).await;

    arena.registry.swap(crate::agent::AgentRegistry::new());
    let first = arena.finish(held).await;

    the_copy_is_dropped(
        &arena,
        ConsumedWhy::AgentRemoved,
        first,
        arena.deliver(2, false),
    )
    .await;
    arena.worker.setup_gate.add_permits(1);
    arena.finish(occupant).await;
}

/// **Startup recovery and `fq doctor` leave a claim the dispatcher
/// consumed (issue 1055).** A run fails for good before its first WAL
/// write and its ack is lost; the daemon restarts. Recovery marks no
/// consumed claim and resumes nothing, the stranded-claims read behind
/// `fq doctor` does not list it, and the copy that comes back to the new
/// process is dropped.
#[tokio::test]
async fn recovery_and_doctor_leave_a_claim_the_dispatcher_consumed() {
    let (mut arena, _server) = arena().await;
    arena.worker.queue(Run::FailsForGood);
    let first = arena.deliver(1, true).await;
    assert_eq!((first.acks, first.naks), (0, 0), "{first:?}");
    let consumed = claim_now(&arena).await;
    assert!(
        matches!(
            consumed,
            Some((
                ClaimState::Consumed {
                    why: ConsumedWhy::FailedForGood
                },
                _
            ))
        ),
        "{consumed:?}"
    );

    arena.restart().await;

    assert_eq!(claim_now(&arena).await, consumed, "recovery leaves it");
    assert!(
        arena
            .claims
            .unsettled_trigger_claims()
            .await
            .unwrap()
            .is_empty(),
        "nothing is stranded"
    );
    let copy = arena.deliver(2, false).await;
    assert_eq!((copy.acks, copy.naks), (1, 0), "{copy:?}");
    assert_eq!(arena.worker.starts().len(), 1, "nothing runs again");
    assert_eq!(claim_now(&arena).await, consumed);
}

/// **An operator's drop that meets another consuming answer is recorded
/// beside it, whatever the answer (issue 1055).** The run is in setup when
/// the operator drops it; the drop writes the run's ownership row, and the
/// run, rather than stopping at its halt, fails for good, or returns, or is
/// halted. The consume reads the drop's row in the statement that writes
/// it, so the claim keeps the run's own reason and records the drop beside
/// it, and the last look, taking only the WAL as a start on a claim that
/// records a drop, reads it as consumed: not as a start, with an error line
/// and the claim rewritten as started, for a run that never ran.
#[tokio::test]
async fn a_drop_that_meets_another_consuming_answer_is_recorded_beside_it() {
    use crate::control_plane::OwnerStatus;
    use crate::control_plane::dispatcher::{ConsumedClaim, Exhausted};
    for (script, why) in [
        (
            Run::WaitsInSetupThenFailsForGood,
            ConsumedWhy::FailedForGood,
        ),
        (
            Run::WaitsInSetupThenReturnsUnstarted,
            ConsumedWhy::ReturnedUnstarted,
        ),
        (Run::WaitsInSetupThenHalts, ConsumedWhy::HaltedInSetup),
    ] {
        let (arena, _server) = arena().await;
        arena.worker.queue(script);
        let first = arena.spawn_delivery(SEQ, 1, true);
        arena.worker.until_waiting(1).await;
        let (_, run) = claim_now(&arena).await.expect("the claim");
        let run = run.expect("it names its run");
        arena
            .claims
            .upsert_invocation_ownership(&run, &arena.claimant, 1, OwnerStatus::Failed)
            .await
            .unwrap();
        arena.worker.setup_gate.add_permits(1);
        arena.finish(first).await;

        let row = claim_row(&arena).await.expect("the claim");
        assert_eq!(
            (row.state, row.operator_dropped_at.is_some()),
            (ClaimState::Consumed { why: why.clone() }, true),
            "{why}: the run's own reason, and the drop beside it"
        );
        let check = arena.dispatcher.exhaustion_check().expect("a claim store");
        assert_eq!(
            check.last_look(SEQ).await,
            Exhausted::Consumed(ConsumedClaim::Checked {
                invocation_id: Some(run),
                why: why.clone(),
                operator_dropped: true,
            }),
            "{why}: not a start"
        );
    }
}

/// **A run halted in setup has its drop recorded, whichever of the halt's
/// consume and the drop's handler lands first (issue 1055).** An operator
/// halts a run in setup only by dropping it, and the drop writes the run's
/// ownership row. Here the halt's consume lands first, with no row yet, and
/// the drop's handler then records the drop on the consumed claim; the
/// other order, the row first and the consume reading it, is the halt case
/// of the test above. Either way the claim says halted in setup, with the
/// drop beside it.
#[tokio::test]
async fn a_run_halted_in_setup_has_its_drop_recorded_whichever_lands_first() {
    use crate::control_plane::CoordinationConsumer;
    use crate::events::{Event, EventPayload, InvocationOperatorRecoveredPayload};
    let (arena, _server) = arena().await;
    arena.worker.queue(Run::HaltedInSetup);
    let halted = arena.deliver(1, false).await;
    assert_eq!((halted.acks, halted.naks), (1, 0), "{halted:?}");
    let run = arena.worker.starts()[0].0;
    let row = claim_row(&arena).await.expect("the claim");
    assert_eq!(
        (row.state, row.operator_dropped_at),
        (
            ClaimState::Consumed {
                why: ConsumedWhy::HaltedInSetup
            },
            None
        ),
        "no drop on record yet"
    );

    let consumer = CoordinationConsumer::new(arena.bus.clone(), arena.claims.clone());
    let payload = InvocationOperatorRecoveredPayload {
        action: "drop".to_string(),
        final_phase: "failed".to_string(),
        reason: None,
        requested_by: None,
        archive: None,
    };
    let event = Event::new(
        crate::agent::AgentId::new(&arena.agent).unwrap(),
        run,
        EventPayload::InvocationOperatorRecovered(payload.clone()),
    );
    consumer
        .handle_invocation_operator_recovered(&event, &payload)
        .await
        .expect("the drop's handler");

    let row = claim_row(&arena).await.expect("the claim");
    assert_eq!(
        (row.state, row.operator_dropped_at.is_some()),
        (
            ClaimState::Consumed {
                why: ConsumedWhy::HaltedInSetup
            },
            true
        ),
        "halted in setup, with the drop beside it"
    );
}
