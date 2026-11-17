//! The dead-letter watch against the dispatcher's records (issue 967).
//!
//! The real watch ([`AdvisoryWatch::handle_advisory`]) is handed the
//! broker's own MAX_DELIVERIES advisory for a real trigger, and consults
//! what the arena's dispatcher left on record: its claim store, the stub
//! worker's WAL, and the copies it is handling. The trigger is published
//! for real and exhausted on a scratch durable of the trigger stream that
//! never acks, with a one-second ack window and two deliveries, so the
//! broker emits its advisory within seconds; the advisory names the
//! message, not the durable that ran out, which is why a scratch durable
//! can stand for the dispatcher's. The dispatcher's own copies are made by
//! the arena, as the tests beside this file make them, so each life's
//! faults are the test's to place: a copy's task dies, a mark is refused,
//! an answer is lost. Every assertion is on a decision: the outcome, the
//! dead-letter event or its absence, the claim and the counts.

use std::time::Duration;

use futures::StreamExt;
use serde_json::json;

use super::{Arena, Run, SEQ, arena, heal, refuse_updates};
use crate::agent::AgentId;
use crate::bus::{ADVISORY_STREAM_NAME, STREAM_NAME, TRIGGER_STREAM_NAME};
use crate::control_plane::advisory_watch::{
    AdvisoryOutcome, AdvisoryWatch, CONSUMER_NAME as WATCH_CONSUMER,
};
use crate::dead_letter::{
    DEAD_LETTER_SOURCE_KEY, DEAD_LETTER_STREAM_SEQ_KEY, DEAD_LETTER_TRIGGER_ID_KEY,
};
use crate::events::{Event, EventPayload, FailureKind, subjects};
use crate::trigger::PublishedTrigger;

/// Publish the arena's trigger for real: the message the advisory names.
async fn publish(arena: &Arena) -> PublishedTrigger {
    let published = arena
        .bus
        .publish_trigger(
            &AgentId::new(&arena.agent).unwrap(),
            &json!({ "input": "hi" }),
        )
        .await
        .expect("publish the trigger");
    assert_eq!(
        published.stream_seq, SEQ,
        "the arena's copies name the message at this sequence"
    );
    published
}

/// Exhaust the published trigger on a scratch durable that never acks,
/// and return the broker's MAX_DELIVERIES advisory as the watch's stream
/// captured it.
async fn exhaust(arena: &Arena) -> Vec<u8> {
    let name = format!("exhaust-{}", uuid::Uuid::now_v7().simple());
    let consumer = arena
        .bus
        .jetstream()
        .get_stream(TRIGGER_STREAM_NAME)
        .await
        .expect("the trigger stream")
        .create_consumer(async_nats::jetstream::consumer::pull::Config {
            durable_name: Some(name.clone()),
            ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
            filter_subject: subjects::trigger(&arena.agent),
            max_deliver: 2,
            ack_wait: Duration::from_secs(1),
            ..Default::default()
        })
        .await
        .expect("a scratch durable");
    let mut messages = consumer.messages().await.expect("its messages");
    for delivery in 1..=2 {
        let _unacked = tokio::time::timeout(Duration::from_secs(5), messages.next())
            .await
            .unwrap_or_else(|_| panic!("delivery {delivery} within 5s"))
            .expect("the durable is open")
            .expect("a delivery");
    }
    let subject =
        format!("$JS.EVENT.ADVISORY.CONSUMER.MAX_DELIVERIES.{TRIGGER_STREAM_NAME}.{name}");
    let advisories = arena
        .bus
        .jetstream()
        .get_stream(ADVISORY_STREAM_NAME)
        .await
        .expect("the advisory stream");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(raw) = advisories.get_last_raw_message_by_subject(&subject).await {
            return raw.payload.to_vec();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the broker's MAX_DELIVERIES advisory never came"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The real watch, consulting the arena's dispatcher.
fn watch(arena: &Arena) -> AdvisoryWatch {
    AdvisoryWatch::new(arena.bus.clone()).with_records(arena.dispatcher.exhaustion_check())
}

/// The trigger's dead letter, if one was written.
async fn dead_letter(arena: &Arena) -> Option<Event> {
    let raw = arena
        .bus
        .jetstream()
        .get_stream(STREAM_NAME)
        .await
        .expect("the event stream")
        .get_last_raw_message_by_subject(&subjects::agent_failed(&arena.agent))
        .await
        .ok()?;
    Some(serde_json::from_slice(&raw.payload).expect("the event parses"))
}

/// How many exhausted triggers the watch kept out of the dead letters.
fn kept(arena: &Arena) -> u64 {
    arena
        .bus
        .consumer_ledger()
        .record(WATCH_CONSUMER)
        .started_not_dead_lettered
}

/// Deliveries 1 to `n` fail transiently before any start, and are NAKed.
async fn fail(arena: &Arena, n: u32) {
    for delivered in 1..=n {
        arena.worker.queue(Run::FailsTransiently);
        let answered = arena.deliver(delivered, false).await;
        assert_eq!(answered.naks, 1, "delivery {delivered}: {answered:?}");
    }
}

/// The claim, as `(marked started, the invocation it names)`.
async fn claim(arena: &Arena) -> Option<(bool, Option<String>)> {
    arena
        .claim(SEQ)
        .await
        .map(|row| (row.started, row.invocation_id))
}

/// **A last delivery whose run started and whose task then died is not
/// dead-lettered.** Four deliveries fail transiently; the fifth and last
/// starts the run, whose task panics in the poll of its first WAL write,
/// so no answer reaches the broker and no copy follows. The WAL holds the
/// run in flight; the watch finds it, marks the claim, and writes no
/// `trigger_exhausted`.
#[tokio::test]
async fn a_last_delivery_whose_run_started_and_whose_task_died_is_not_dead_lettered() {
    let (arena, _server) = arena().await;
    publish(&arena).await;
    fail(&arena, 4).await;
    arena.worker.queue(Run::DiesAfterStart);
    let last = arena.deliver(5, false).await;
    assert!(last.panicked, "{last:?}");
    let run = arena.worker.starts()[4].0.to_string();
    let advisory = exhaust(&arena).await;

    let outcome = watch(&arena).handle_advisory(&advisory).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::RunStarted);
    assert!(
        dead_letter(&arena).await.is_none(),
        "a trigger whose run started is not dead-lettered"
    );
    assert_eq!(
        claim(&arena).await,
        Some((true, Some(run))),
        "the watch marked the claim, naming the run"
    );
    assert_eq!(kept(&arena), 1);
}

/// **A last delivery whose mark landed and whose ack was lost is not
/// dead-lettered.** The fifth delivery starts the run, marks the claim and
/// acks, and the ack is lost. The claim records the start.
#[tokio::test]
async fn a_last_delivery_whose_mark_landed_and_whose_ack_was_lost_is_not_dead_lettered() {
    let (arena, _server) = arena().await;
    publish(&arena).await;
    fail(&arena, 4).await;
    let last = arena.deliver(5, true).await;
    assert!(!last.panicked, "{last:?}");
    let run = arena.worker.starts()[4].0.to_string();
    assert_eq!(claim(&arena).await, Some((true, Some(run.clone()))));
    let advisory = exhaust(&arena).await;

    let outcome = watch(&arena).handle_advisory(&advisory).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::RunStarted);
    assert!(dead_letter(&arena).await.is_none());
    assert_eq!(claim(&arena).await, Some((true, Some(run))));
    assert_eq!(kept(&arena), 1);
}

/// **A last delivery that is a copy of a run started earlier, whose ack
/// was lost, is not dead-lettered.** The fourth delivery starts the run,
/// the claim store refuses its mark and its ack is lost; the fifth is a
/// copy of that run, settled by the record, whose mark is refused too and
/// whose ack is lost as well. The claim is still `claimed`, naming a run
/// the WAL holds finished: the watch marks it, and nothing starts twice.
#[tokio::test]
async fn a_last_delivery_that_copies_a_run_started_earlier_is_not_dead_lettered() {
    let (arena, _server) = arena().await;
    publish(&arena).await;
    fail(&arena, 3).await;
    refuse_updates(&arena.claim_db).await;
    let fourth = arena.deliver(4, true).await;
    assert!(!fourth.panicked, "{fourth:?}");
    let run = arena.worker.starts()[3].0.to_string();
    let fifth = arena.deliver(5, true).await;
    assert!(!fifth.panicked, "{fifth:?}");
    assert_eq!(claim(&arena).await, Some((false, Some(run.clone()))));
    heal(&arena.claim_db).await;
    let advisory = exhaust(&arena).await;

    let outcome = watch(&arena).handle_advisory(&advisory).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::RunStarted);
    assert!(dead_letter(&arena).await.is_none());
    assert_eq!(
        claim(&arena).await,
        Some((true, Some(run))),
        "the watch marked the claim, naming the run on record"
    );
    assert_eq!(arena.worker.starts().len(), 4, "nothing started twice");
    assert_eq!(kept(&arena), 1);
}

/// **A last delivery whose run was archived while the watch read its
/// records is not dead-lettered.** The fifth delivery's run finishes with
/// its mark refused and its ack lost, so the claim is `claimed` and the WAL
/// holds the run finished. When the watch asks the worker about it, the
/// worker hands the run off to the archive (the ownership row written, then
/// the WAL row deleted) and answers from a WAL that no longer holds it. The
/// ownership row, read second, does: the watch finds the start there,
/// marks the claim, and writes no dead letter. Ignoring the ownership row,
/// or reading it before the WAL, would dead-letter a run that finished.
#[tokio::test]
async fn a_last_delivery_whose_run_was_archived_meanwhile_is_not_dead_lettered() {
    let (arena, _server) = arena().await;
    publish(&arena).await;
    fail(&arena, 4).await;
    refuse_updates(&arena.claim_db).await;
    let last = arena.deliver(5, true).await;
    assert!(!last.panicked, "{last:?}");
    let run = arena.worker.starts()[4].0.to_string();
    heal(&arena.claim_db).await;
    assert_eq!(claim(&arena).await, Some((false, Some(run.clone()))));
    let advisory = exhaust(&arena).await;
    *arena.worker.hands_off_on_read.lock().unwrap() = Some(std::sync::Arc::clone(&arena.claims));

    let outcome = watch(&arena).handle_advisory(&advisory).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::RunStarted);
    assert!(
        arena.wal().await.is_empty(),
        "the run was handed off while the watch read the WAL"
    );
    assert!(dead_letter(&arena).await.is_none());
    assert_eq!(claim(&arena).await, Some((true, Some(run))));
    assert_eq!(kept(&arena), 1);
}

/// **A trigger with nothing on record is dead-lettered as before.** Four
/// deliveries fail transiently and the fifth's task dies before its first
/// WAL write: the claim is `claimed`, no copy handles it, and nothing of
/// its run is on record. The watch writes the same `trigger_exhausted` it
/// always has, with the same annotations, and leaves the claim alone.
#[tokio::test]
async fn a_trigger_with_nothing_on_record_is_dead_lettered_as_before() {
    let (arena, _server) = arena().await;
    let published = publish(&arena).await;
    fail(&arena, 4).await;
    arena.worker.queue(Run::DiesBeforeStart);
    let last = arena.deliver(5, false).await;
    assert!(last.panicked, "{last:?}");
    let advisory = exhaust(&arena).await;

    let outcome = watch(&arena).handle_advisory(&advisory).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::Emitted);
    let event = dead_letter(&arena)
        .await
        .expect("the trigger is dead-lettered");
    assert!(
        matches!(&event.payload, EventPayload::Failed(p) if matches!(p.error_kind, FailureKind::TriggerExhausted)),
        "{:?}",
        event.payload
    );
    let annotations = &event.annotations.0;
    assert_eq!(
        annotations.get(DEAD_LETTER_STREAM_SEQ_KEY),
        Some(&json!(SEQ))
    );
    assert_eq!(
        annotations.get(DEAD_LETTER_SOURCE_KEY),
        Some(&json!("advisory"))
    );
    assert_eq!(
        annotations.get(DEAD_LETTER_TRIGGER_ID_KEY),
        Some(&json!(published.id.to_string()))
    );
    assert_eq!(
        claim(&arena).await.map(|(started, _)| started),
        Some(false),
        "the claim is left as it was"
    );
    assert_eq!(kept(&arena), 0);
}

/// **A trigger whose records cannot be read is held, not dead-lettered,
/// until they can.** The fifth delivery's run started and its task died,
/// so a start is on record, but the worker cannot say what its WAL holds.
/// A dead letter written now could be false, so the verdict is held: no
/// event, the claim untouched, and the advisory comes back to be decided
/// again. Once the WAL can be read, the run is found.
#[tokio::test]
async fn a_trigger_whose_records_cannot_be_read_is_held_until_they_can() {
    let (arena, _server) = arena().await;
    publish(&arena).await;
    fail(&arena, 4).await;
    arena.worker.queue(Run::DiesAfterStart);
    arena.deliver(5, false).await;
    let advisory = exhaust(&arena).await;
    arena
        .worker
        .fail_wal_reads
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let watch = watch(&arena);

    let held = watch.handle_advisory(&advisory).await.unwrap();

    assert!(
        matches!(held, AdvisoryOutcome::Held { .. }),
        "the verdict is held while the records cannot be read: {held:?}"
    );
    assert!(dead_letter(&arena).await.is_none());
    assert_eq!(claim(&arena).await.map(|(started, _)| started), Some(false));
    arena
        .worker
        .fail_wal_reads
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        watch.handle_advisory(&advisory).await.unwrap(),
        AdvisoryOutcome::RunStarted
    );
    assert!(dead_letter(&arena).await.is_none());
    assert_eq!(claim(&arena).await.map(|(started, _)| started), Some(true));
    assert_eq!(kept(&arena), 1);
}

/// **A trigger whose claim a copy is still handling when its deliveries
/// run out is held until that copy ends, and then not dead-lettered if its
/// run started.** The first copy waits in setup, before its first WAL
/// write, with nothing on record, while the broker spends the trigger's
/// deliveries. Dead-lettering now would be false if the run then starts,
/// so the verdict is held. The run starts and the copy marks its claim;
/// asked again, the watch finds the start.
#[tokio::test]
async fn a_trigger_still_handled_is_held_and_not_dead_lettered_once_its_run_starts() {
    let (arena, _server) = arena().await;
    publish(&arena).await;
    arena.worker.queue(Run::WaitsInSetup);
    let first = arena.spawn_delivery(SEQ, 1, false);
    arena.worker.until_waiting(1).await;
    let advisory = exhaust(&arena).await;
    let watch = watch(&arena);

    let held = watch.handle_advisory(&advisory).await.unwrap();

    assert!(
        matches!(held, AdvisoryOutcome::Held { .. }),
        "the verdict is held while a copy handles the claim: {held:?}"
    );
    assert!(dead_letter(&arena).await.is_none());
    arena.worker.setup_gate.add_permits(1);
    let first = arena.finish(first).await;
    assert_eq!(first.acks, 1, "{first:?}");
    assert_eq!(
        watch.handle_advisory(&advisory).await.unwrap(),
        AdvisoryOutcome::RunStarted
    );
    assert!(dead_letter(&arena).await.is_none());
    assert_eq!(kept(&arena), 1);
}

/// **A trigger whose claim a copy is still handling is dead-lettered once
/// that copy ends with nothing on record.** As above, but the first copy's
/// task dies in setup, before its first WAL write: held while it was
/// handled, the verdict is then the dead letter it always was, and the
/// trigger is not lost.
#[tokio::test]
async fn a_trigger_still_handled_is_dead_lettered_once_its_copy_ends_with_nothing_on_record() {
    let (arena, _server) = arena().await;
    publish(&arena).await;
    arena.worker.queue(Run::WaitsInSetupThenDiesBeforeStart);
    let first = arena.spawn_delivery(SEQ, 1, false);
    arena.worker.until_waiting(1).await;
    let advisory = exhaust(&arena).await;
    let watch = watch(&arena);

    let held = watch.handle_advisory(&advisory).await.unwrap();

    assert!(
        matches!(held, AdvisoryOutcome::Held { .. }),
        "the verdict is held while a copy handles the claim: {held:?}"
    );
    arena.worker.setup_gate.add_permits(1);
    let first = arena.finish(first).await;
    assert!(first.panicked, "{first:?}");
    assert_eq!(
        watch.handle_advisory(&advisory).await.unwrap(),
        AdvisoryOutcome::Emitted
    );
    assert!(dead_letter(&arena).await.is_some());
    assert_eq!(kept(&arena), 0);
}

/// **An advisory from an earlier incarnation of the trigger stream does not
/// consult today's claims.** The claim table keys on the stream's
/// incarnation, which the advisory does not carry; a sequence the broker
/// reported before the stream was last created names another message, so
/// the claim at that sequence today says nothing about it. Such an advisory
/// is dead-lettered as before, and the claim is left alone, though here the
/// claim at that sequence has a start on record.
#[tokio::test]
async fn an_advisory_from_an_earlier_incarnation_of_the_stream_does_not_consult_the_claims() {
    let (arena, _server) = arena().await;
    publish(&arena).await;
    fail(&arena, 4).await;
    arena.worker.queue(Run::DiesAfterStart);
    arena.deliver(5, false).await;
    let advisory = exhaust(&arena).await;
    let mut earlier: serde_json::Value = serde_json::from_slice(&advisory).unwrap();
    earlier["timestamp"] = json!("2000-01-01T00:00:00Z");
    let earlier = serde_json::to_vec(&earlier).unwrap();

    let outcome = watch(&arena).handle_advisory(&earlier).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::Emitted);
    assert_eq!(
        claim(&arena).await.map(|(started, _)| started),
        Some(false),
        "the claim is not consulted, so not marked"
    );
    assert_eq!(kept(&arena), 0);
}

/// The watch's own durable, as the broker reports it.
async fn watch_consumer(arena: &Arena) -> async_nats::jetstream::consumer::Info {
    arena
        .bus
        .jetstream()
        .get_stream(ADVISORY_STREAM_NAME)
        .await
        .expect("the advisory stream")
        .get_consumer::<async_nats::jetstream::consumer::pull::Config>(WATCH_CONSUMER)
        .await
        .expect("the watch's durable")
        .info()
        .await
        .expect("its info")
        .clone()
}

/// Exhaust every published trigger of the arena's agent on one scratch
/// durable that never acks, and wait until the broker's advisories for
/// all `messages` of them are on the watch's stream.
async fn exhaust_all(arena: &Arena, messages: u64) {
    let consumer = arena
        .bus
        .jetstream()
        .get_stream(TRIGGER_STREAM_NAME)
        .await
        .expect("the trigger stream")
        .create_consumer(async_nats::jetstream::consumer::pull::Config {
            durable_name: Some(format!("exhaust-{}", uuid::Uuid::now_v7().simple())),
            ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
            filter_subject: subjects::trigger(&arena.agent),
            max_deliver: 2,
            ack_wait: Duration::from_secs(1),
            ..Default::default()
        })
        .await
        .expect("a scratch durable");
    let mut deliveries = consumer.messages().await.expect("its messages");
    for delivery in 1..=2 * messages {
        let _unacked = tokio::time::timeout(Duration::from_secs(5), deliveries.next())
            .await
            .unwrap_or_else(|_| panic!("delivery {delivery} within 5s"))
            .expect("the durable is open")
            .expect("a delivery");
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let captured = arena
            .bus
            .jetstream()
            .get_stream(ADVISORY_STREAM_NAME)
            .await
            .expect("the advisory stream")
            .info()
            .await
            .expect("its info")
            .state
            .messages;
        if captured >= messages {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{captured} of {messages} advisories within 15s"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The dead letter for the trigger at `seq`, if one was written.
async fn dead_letter_at(arena: &Arena, seq: u64) -> Option<Event> {
    dead_letter(arena)
        .await
        .filter(|event| event.annotations.0.get(DEAD_LETTER_STREAM_SEQ_KEY) == Some(&json!(seq)))
}

/// **The watch's loop holds a verdict until its cause ends, and decides
/// the advisories behind it meanwhile.** This drives the real loop,
/// `AdvisoryWatch::run`, on the watch's own durable, where every other
/// test here calls the decision by hand. Two triggers run out of
/// deliveries. A copy is still handling the first's claim, so its verdict
/// is held: the loop NAKs the advisory, which stays pending on the durable
/// and comes back. The second, with nothing on record, is dead-lettered
/// meanwhile, so a held verdict does not hold up the rest. Once the copy
/// dies before its first WAL write, the held advisory's next delivery
/// dead-letters the first trigger. A loop that acked a held advisory as
/// done would lose that trigger for good.
#[tokio::test]
async fn the_watch_loop_holds_a_verdict_until_its_cause_ends() {
    let (arena, _server) = arena().await;
    publish(&arena).await;
    let second = arena
        .bus
        .publish_trigger(
            &AgentId::new(&arena.agent).unwrap(),
            &json!({ "input": "two" }),
        )
        .await
        .expect("publish the second trigger");
    assert_eq!(second.stream_seq, SEQ + 1);
    arena.worker.queue(Run::WaitsInSetupThenDiesBeforeStart);
    let first = arena.spawn_delivery(SEQ, 1, false);
    arena.worker.until_waiting(1).await;
    exhaust_all(&arena, 2).await;

    let (stop, stopped) = tokio::sync::oneshot::channel();
    let looping = tokio::spawn(watch(&arena).run(stopped));

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while dead_letter_at(&arena, second.stream_seq).await.is_none() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the second trigger, with nothing on record, is dead-lettered while the first is held"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let held = watch_consumer(&arena).await;
        if held.num_redelivered >= 1 {
            assert_eq!(
                held.num_ack_pending, 1,
                "the held advisory is pending on the durable, not acked: {held:?}"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the held advisory was not delivered again within 10s: {held:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        dead_letter_at(&arena, SEQ).await.is_none(),
        "the first trigger is not dead-lettered while its copy handles the claim"
    );

    arena.worker.setup_gate.add_permits(1);
    let first = arena.finish(first).await;
    assert!(first.panicked, "{first:?}");
    // Past the longest NAK delay this advisory can have reached by now.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(70);
    while dead_letter_at(&arena, SEQ).await.is_none() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the held verdict was not taken once its copy ended"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(watch_consumer(&arena).await.num_ack_pending, 0);
    let _ = stop.send(());
    looping
        .await
        .expect("the loop's task")
        .expect("the loop stops cleanly");
}

/// **The watch asks whether a copy is handling the claim before it reads
/// the records.** The order is load-bearing, as it is in the own-claim arm.
/// The first copy waits in setup, so nothing is on record; the watch's read
/// of the records, if it makes one, is held there; the copy then makes its
/// first WAL write and dies in the same poll, and only then does the watch
/// go on. Asked first, the note holds the verdict, and asked again the
/// watch finds the start. Asked after the records, the watch would have
/// read an empty WAL, found no note once the copy died, and dead-lettered a
/// trigger whose run started.
#[tokio::test]
async fn the_watch_asks_whether_a_copy_is_handling_the_claim_before_it_reads_the_records() {
    let (arena, _server) = arena().await;
    publish(&arena).await;
    arena.worker.queue(Run::WaitsInSetupThenDiesAfterStart);
    let first = arena.spawn_delivery(SEQ, 1, false);
    arena.worker.until_waiting(1).await;
    let advisory = exhaust(&arena).await;
    arena
        .worker
        .hold_wal_reads
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let the_watch = std::sync::Arc::new(watch(&arena));

    let asking = {
        let the_watch = std::sync::Arc::clone(&the_watch);
        let advisory = advisory.clone();
        tokio::spawn(async move { the_watch.handle_advisory(&advisory).await.unwrap() })
    };
    // The watch answers at once, or is held reading the records.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !asking.is_finished()
        && arena
            .worker
            .wal_reads_held
            .load(std::sync::atomic::Ordering::SeqCst)
            == 0
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the watch neither answered nor read the records within 10s"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    arena.worker.setup_gate.add_permits(1);
    let first = arena.finish(first).await;
    assert!(
        first.panicked,
        "the copy dies after its first WAL write: {first:?}"
    );
    arena
        .worker
        .hold_wal_reads
        .store(false, std::sync::atomic::Ordering::SeqCst);
    arena.worker.wal_read_gate.add_permits(1);
    let outcome = asking.await.expect("the watch's task");

    assert!(
        dead_letter(&arena).await.is_none(),
        "a trigger whose run started was dead-lettered: {outcome:?}"
    );
    assert!(
        matches!(outcome, AdvisoryOutcome::Held { .. }),
        "the copy was handling the claim when the watch asked: {outcome:?}"
    );
    assert_eq!(
        the_watch.handle_advisory(&advisory).await.unwrap(),
        AdvisoryOutcome::RunStarted
    );
}
