//! The records are asked before anything else about an exhausted trigger
//! (issues 967, 1066), with its message readable: before the check for a
//! dead letter already written, as well as before the message is read.
//!
//! A verdict the records give decides, whether or not a dead letter for
//! the trigger is already on the log: a run that started is settled as
//! started (its claim marked and the trigger counted), and a hold names
//! the trigger's subject when its message can be read.

use super::{
    LOOKED, Run, SEQ, arena, claim, dead_letter, exhaust, fail, kept, publish,
    stamped_at_the_streams_creation, watch,
};
use crate::control_plane::advisory_watch::AdvisoryOutcome;
use crate::control_plane::advisory_watch::test_kit::PAST;
use crate::events::subjects;

/// **A start on record decides, though a dead letter for the trigger is
/// already on the log.** Past the limit, with a copy still handling the
/// claim, the watch wrote the trigger's dead letter marked unchecked; the
/// copy then started its run, and the advisory comes back (its ack lost).
/// The records are asked first: the run is settled as started, its claim
/// marked and the trigger counted, rather than the advisory being acked as
/// already surfaced. The dead letter stays on the log, marked unchecked.
#[tokio::test]
async fn a_start_on_record_decides_though_a_dead_letter_is_already_on_the_log() {
    let (arena, _server) = arena().await;
    publish(&arena).await;
    arena.worker.queue(Run::WaitsInSetup);
    let first = arena.spawn_delivery(SEQ, 1, false);
    arena.worker.until_waiting(1).await;
    let advisory = stamped_at_the_streams_creation(&arena, &exhaust(&arena).await);
    let watch = watch(&arena).with_held_verdict_limit(PAST);
    assert_eq!(
        watch.handle_advisory(&advisory, LOOKED).await.unwrap(),
        AdvisoryOutcome::Unchecked
    );
    assert!(dead_letter(&arena).await.is_some());
    arena.worker.setup_gate.add_permits(1);
    arena.finish(first).await;

    let again = watch.handle_advisory(&advisory, LOOKED + 1).await.unwrap();

    assert_eq!(again, AdvisoryOutcome::RunStarted);
    assert_eq!(kept(&arena), 1);
    assert!(
        claim(&arena).await.is_some_and(|(started, _)| started),
        "the claim is marked started"
    );
}

/// **A hold on the records names the trigger's subject when its message can
/// be read**, and the run its claim names.
#[tokio::test]
async fn a_hold_on_the_records_names_the_subject_of_a_readable_message() {
    let (arena, _server) = arena().await;
    publish(&arena).await;
    arena.worker.queue(Run::WaitsInSetup);
    let first = arena.spawn_delivery(SEQ, 1, false);
    arena.worker.until_waiting(1).await;
    let copy = arena.worker.starts()[0].0.to_string();
    let advisory = exhaust(&arena).await;

    let outcome = watch(&arena).handle_advisory(&advisory, 1).await.unwrap();

    match &outcome {
        AdvisoryOutcome::Held(hold) => {
            assert_eq!(
                hold.why,
                "a copy of the trigger is still handling its claim"
            );
            assert_eq!(hold.subject, subjects::trigger(&arena.agent));
            assert_eq!(hold.invocation_id.as_deref(), Some(copy.as_str()));
        }
        other => panic!("held: {other:?}"),
    }
    arena.worker.setup_gate.add_permits(1);
    arena.finish(first).await;
}

/// How many dead letters the arena's agent holds for the trigger at
/// [`SEQ`].
async fn dead_letters_at_seq(
    arena: &crate::control_plane::dispatcher::own_claim_tests::Arena,
) -> usize {
    use futures::StreamExt;
    let subject = subjects::agent_failed(&arena.agent);
    let tip = arena
        .bus
        .last_event_seq_matching(&subject)
        .await
        .expect("the subject's tip");
    if tip == 0 {
        return 0;
    }
    let mut events = arena.bus.events_from(&subject, 1).await.expect("a tail");
    let mut written = 0;
    while let Some(next) = events.next().await {
        let crate::event_tail::TailedMessage { seq, event } = next.expect("an event");
        if event
            .as_ref()
            .and_then(crate::dead_letter::from_event)
            .is_some_and(|dead| dead.trigger_stream_seq == Some(SEQ))
        {
            written += 1;
        }
        if seq >= tip {
            break;
        }
    }
    written
}

/// **A hold over a dead letter already written does not end in a second.**
/// The watch wrote the trigger's dead letter, its advisory's ack was lost,
/// the daemon started again, and the advisory comes back while the worker
/// cannot say what its WAL holds: the verdict is held. Another failure of
/// the agent lands during the hold. When the records answer nothing on
/// record, the dead letter already written is found on the log, though it
/// is no longer the agent's last failure. (A watch that did not start
/// again answers from what it remembers.)
#[tokio::test]
async fn a_hold_over_a_dead_letter_already_written_does_not_end_in_a_second() {
    let (arena, _server) = arena().await;
    publish(&arena).await;
    super::fail(&arena, 4).await;
    arena.worker.queue(Run::DiesBeforeStart);
    arena.deliver(5, false).await;
    let advisory = exhaust(&arena).await;
    let watch = watch(&arena);
    assert_eq!(
        watch.handle_advisory(&advisory, 1).await.unwrap(),
        AdvisoryOutcome::Emitted
    );
    arena
        .worker
        .fail_wal_reads
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let watch = super::watch(&arena);
    let held = watch.handle_advisory(&advisory, 2).await.unwrap();
    assert!(matches!(held, AdvisoryOutcome::Held(_)), "{held:?}");
    let another = crate::events::Event::new(
        crate::agent::AgentId::new(&arena.agent).unwrap(),
        uuid::Uuid::now_v7(),
        crate::events::EventPayload::Failed(crate::events::FailedPayload {
            error_kind: crate::events::FailureKind::LlmError,
            error_message: "another run of the agent failed".to_string(),
            phase: crate::events::FailurePhase::Setup,
            partial_totals: crate::events::InvocationTotals::default(),
            detail: None,
        }),
    );
    arena.bus.publish(&another).await.expect("another failure");
    arena
        .worker
        .fail_wal_reads
        .store(false, std::sync::atomic::Ordering::SeqCst);

    let after = watch.handle_advisory(&advisory, 3).await.unwrap();

    assert_eq!(after, AdvisoryOutcome::AlreadySurfaced);
    assert_eq!(dead_letters_at_seq(&arena).await, 1);
}

/// **The dispatcher's own dead letter, written before the advisory, is
/// found by the watch.** The real dispatcher dead-letters the trigger on
/// its last failing delivery; another failure of the agent lands; the
/// broker's advisory for the same message follows, as when the last
/// delivery's ack was lost; the watch, handed it, finds that dead letter
/// and writes none. The dead letter is older than the advisory, so the
/// read for it starts at the trigger's publish, not at the advisory.
#[tokio::test]
async fn the_dispatchers_dead_letter_from_before_the_advisory_is_found() {
    let (arena, _server) = arena().await;
    // The arena's deliveries carry its own trigger id, so the message is
    // published under that id: `publish` mints another.
    let published = arena
        .bus
        .publish_trigger_named(
            &crate::agent::AgentId::new(&arena.agent).unwrap(),
            arena.trigger_id,
            &serde_json::json!({ "input": "hi" }),
        )
        .await
        .expect("publish the trigger");
    assert_eq!(published.stream_seq, SEQ);
    fail(&arena, 4).await;
    arena.worker.queue(Run::FailsTransiently);
    arena.deliver(5, false).await;
    let inline = dead_letter(&arena)
        .await
        .expect("the dispatcher's own dead letter");
    assert_eq!(
        inline.annotations.0.get("dead_letter_source"),
        Some(&serde_json::json!("inline"))
    );
    let another = crate::events::Event::new(
        crate::agent::AgentId::new(&arena.agent).unwrap(),
        uuid::Uuid::now_v7(),
        crate::events::EventPayload::Failed(crate::events::FailedPayload {
            error_kind: crate::events::FailureKind::LlmError,
            error_message: "another run of the agent failed".to_string(),
            phase: crate::events::FailurePhase::Setup,
            partial_totals: crate::events::InvocationTotals::default(),
            detail: None,
        }),
    );
    arena.bus.publish(&another).await.expect("another failure");
    let advisory = exhaust(&arena).await;

    let outcome = watch(&arena).handle_advisory(&advisory, 1).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::AlreadySurfaced);
}
