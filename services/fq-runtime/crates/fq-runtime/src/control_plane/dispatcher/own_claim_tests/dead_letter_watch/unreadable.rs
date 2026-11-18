//! The order of the watch's questions when the trigger's message cannot be
//! read (issue 1066), with the dispatcher's records to consult.
//!
//! A message the watch cannot read, for a reason other than its absence,
//! holds the verdict until the watch's limit. Two things come before that
//! and must not be undone by it: an advisory from an earlier incarnation of
//! the trigger stream is not read at all, so a read that would fail cannot
//! hold it; and past the limit the watch still takes its last look at the
//! records, so a run that started is not dead-lettered. The trigger stream
//! is deleted to make every read fail: the broker answers that there is no
//! stream (code 10059), not that there is no message.

use super::incarnation::{stamped_before_the_streams_creation, system_dead_letter};
use super::{
    LOOKED, Run, SEQ, arena, claim, dead_letter, exhaust, fail, kept, publish,
    stamped_at_the_streams_creation, watch,
};
use crate::bus::TRIGGER_STREAM_NAME;
use crate::control_plane::advisory_watch::AdvisoryOutcome;
use crate::control_plane::advisory_watch::test_kit::PAST;
use crate::control_plane::dispatcher::own_claim_tests::Arena;
use crate::dead_letter::from_event;

/// Delete the trigger stream, so every read of a message on it fails.
async fn delete_the_trigger_stream(arena: &Arena) {
    arena
        .bus
        .jetstream()
        .delete_stream(TRIGGER_STREAM_NAME)
        .await
        .expect("delete the trigger stream");
}

/// **An earlier incarnation's advisory is decided at once, though its
/// stream cannot be read.** The watch decides the incarnation before it
/// reads anything, and does not read for an earlier one, so a read that
/// would fail does not hold it: on its first look it is dead-lettered,
/// naming no trigger and saying it came from an earlier incarnation.
/// Reading first would hold it for an hour over a message it never needed.
#[tokio::test]
async fn an_earlier_incarnations_advisory_is_not_held_when_the_stream_cannot_be_read() {
    let (arena, _server) = arena().await;
    publish(&arena).await;
    let earlier = stamped_before_the_streams_creation(&arena, &exhaust(&arena).await);
    delete_the_trigger_stream(&arena).await;

    let outcome = watch(&arena).handle_advisory(&earlier, 1).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::Emitted, "not held");
    let dead = from_event(
        &system_dead_letter(&arena)
            .await
            .expect("dead-lettered under the system sentinel"),
    )
    .expect("a dead letter");
    let why = dead.unattributed.expect("it says why it names no trigger");
    assert!(
        why.starts_with("its advisory came from an earlier incarnation"),
        "{why}"
    );
}

/// **A run that started, whose trigger's message cannot be read, is not
/// dead-lettered.** Its last delivery started the run and its task died, so
/// a start is on record; the trigger stream is then gone. Inside the limit
/// the verdict is held on the unreadable message. Past it, the watch still
/// takes its last look at the records, finds the start, marks the claim and
/// writes no dead letter: written naming no trigger, the run would be
/// recorded as exhausted though it ran.
#[tokio::test]
async fn a_run_that_started_is_not_dead_lettered_when_its_message_cannot_be_read() {
    let (arena, _server) = arena().await;
    publish(&arena).await;
    fail(&arena, 4).await;
    arena.worker.queue(Run::DiesAfterStart);
    arena.deliver(5, false).await;
    let run = arena.worker.starts()[4].0.to_string();
    let advisory = stamped_at_the_streams_creation(&arena, &exhaust(&arena).await);
    assert_eq!(claim(&arena).await, Some((false, Some(run.clone()))));
    delete_the_trigger_stream(&arena).await;

    let inside = watch(&arena)
        .handle_advisory(&advisory, LOOKED)
        .await
        .unwrap();
    let past = watch(&arena)
        .with_held_verdict_limit(PAST)
        .handle_advisory(&advisory, LOOKED)
        .await
        .unwrap();

    assert!(
        matches!(&inside, AdvisoryOutcome::Held(hold) if hold.stream_seq == SEQ),
        "{inside:?}"
    );
    assert_eq!(past, AdvisoryOutcome::RunStarted);
    assert_eq!(claim(&arena).await, Some((true, Some(run))));
    assert_eq!(kept(&arena), 1);
    assert!(dead_letter(&arena).await.is_none());
    assert!(system_dead_letter(&arena).await.is_none());
}
