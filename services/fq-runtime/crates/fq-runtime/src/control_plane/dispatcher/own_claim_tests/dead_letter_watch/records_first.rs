//! The records are asked before anything else about an exhausted trigger
//! (issues 967, 1066), with its message readable: before the check for a
//! dead letter already written, as well as before the message is read.
//!
//! A verdict the records give decides, whether or not a dead letter for
//! the trigger is already on the log: a run that started is settled as
//! started (its claim marked and the trigger counted), and a hold names
//! the trigger's subject when its message can be read.

use super::{
    LOOKED, Run, SEQ, arena, claim, dead_letter, exhaust, kept, publish,
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
