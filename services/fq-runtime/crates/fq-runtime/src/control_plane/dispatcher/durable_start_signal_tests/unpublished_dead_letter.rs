//! A trigger whose last delivery fails before its run's first WAL write,
//! and whose dead letter the dispatcher cannot publish (issue 1062).
//!
//! The dispatcher does not acknowledge such a delivery. It gives the
//! claim back and NAKs the delivery with no delay; the broker answers a
//! last delivery's NAK with its max-deliveries advisory at once, and the
//! dead-letter watch decides the trigger by the records and writes its dead
//! letter, retrying until the events stream takes it. Before issue 1062 the
//! delivery was acked all the same, which also kept the broker from sending
//! its advisory, so the trigger was consumed with no run and no dead letter
//! from either emitter.
//!
//! **On the dispatcher's own consumer.** Each delivery here comes off a
//! durable configured as the dispatcher's is ([`Pulled::new`]): the trigger
//! retry ladder is its backoff, so a last delivery's ack window is the
//! ladder's last entry, twelve minutes, and an advisory within [`AT_ONCE`]
//! is the NAK's doing and nothing else's. The first test pins the broker
//! behaviour the design rests on, against the pinned broker: a NAK with no
//! delay raises the advisory at once, and one that carries a delay, even of
//! zero, does not, because the broker counts that delay from the consumer's
//! first ack window and then waits out the ladder's last entry.
//!
//! **What is real:** the dispatcher's `handle`, its claim store, the
//! broker, the advisory capture stream, and the dead-letter watch, run as
//! the daemon runs it in one test and handed the captured advisory in
//! another. The events stream is made unusable by deleting it, so that a
//! publish finds no stream and fails at once, or by having it store a
//! publish without acknowledging it, so that the publish lands and its
//! acknowledgement never comes. The advisory is read off the advisory
//! stream, as the watch reads it, and timed by the stamp the broker puts on
//! it ([`captured_advisory_within`] says why not off a subscription). The
//! answers to the delivery are read off a subscription of the test's own
//! ([`Pulled::watch_answers`]), as across this file: the first answer comes
//! only after the claim store and the events stream have been called,
//! which leaves the subscription far longer to reach the broker than the
//! probe's NAK did, though not a guarantee (issue 1045). Every
//! assertion is on a decision: what the delivery was answered with, what
//! the broker raised and when, what the claim store and the events stream
//! were left with, and the watch's outcome.

use std::time::Duration;

use async_nats::jetstream::AckKind;
use serde_json::json;
use tokio::sync::oneshot;

use super::{
    CapturedAdvisory, Pulled, Resolve, Returns, Turn, captured_advisory_within, dead_letters_of,
    test_key, within_the_test_deadline,
};
use crate::bus::{ADVISORY_STREAM_NAME, EventBus, STREAM_NAME};
use crate::control_plane::advisory_watch::{
    AdvisoryOutcome, AdvisoryWatch, CONSUMER_NAME as WATCH_CONSUMER,
};
use crate::dead_letter::{
    DEAD_LETTER_PAYLOAD_KEY, DEAD_LETTER_SOURCE_KEY, DEAD_LETTER_STREAM_SEQ_KEY,
    DEAD_LETTER_SUBJECT_KEY, DEAD_LETTER_TRIGGER_ID_KEY,
};
use crate::events::{Event, EventPayload, FailureKind, subjects};
use crate::trigger::agent_id_from_subject;

/// How soon after the NAK that ends a trigger's last delivery the broker
/// has to emit its max-deliveries advisory to count as raising it at once:
/// far inside the twelve minutes of the last delivery's ack window that
/// would explain it otherwise, with room for a loaded machine.
const AT_ONCE: Duration = Duration::from_secs(3);

/// How long a test waits to see that no advisory comes.
const NONE_COMES: Duration = Duration::from_secs(4);

/// How long the dead-letter watch may take to write the dead letter once
/// the events stream is back: its retries back off from one second,
/// doubling, and the stream comes back after its first or second.
const WRITTEN_WITHIN: Duration = Duration::from_secs(15);

/// Assert that `advisory` was emitted after `naked_at`, when the NAK that
/// ends the last delivery went out, and less than [`AT_ONCE`] after it: it
/// is that NAK's doing, and it came at once. The broker stamps its
/// advisory by the same clock, on the same machine, as the test.
fn assert_raised_at_once_by(advisory: &CapturedAdvisory, naked_at: chrono::DateTime<chrono::Utc>) {
    let after = advisory.emitted - naked_at;
    assert!(
        after >= chrono::TimeDelta::zero() && after.to_std().unwrap_or_default() < AT_ONCE,
        "the advisory was emitted {after} after the NAK, not at once after it"
    );
}

/// The advisory the broker raised for `pulled`'s trigger, as the advisory
/// stream captured it: what the dead-letter watch is handed.
async fn captured_advisory(pulled: &Pulled) -> CapturedAdvisory {
    captured_advisory_within(pulled, AT_ONCE)
        .await
        .expect("the advisory stream captured no max-deliveries advisory for the trigger")
}

/// The agent `pulled`'s trigger is for.
fn agent(pulled: &Pulled) -> String {
    agent_id_from_subject(&pulled.msg.subject)
        .expect("a trigger subject")
        .to_string()
}

/// The agent's most recent `failed` event, if the events stream holds one.
async fn last_failed(pulled: &Pulled) -> Option<Event> {
    let raw = pulled
        .bus
        .jetstream()
        .get_stream(STREAM_NAME)
        .await
        .ok()?
        .get_last_raw_message_by_subject(&subjects::agent_failed(&agent(pulled)))
        .await
        .ok()?;
    Some(serde_json::from_slice(&raw.payload).expect("the event parses"))
}

/// Make the events stream store what is published to it without
/// acknowledging it, or acknowledge again: a publish meanwhile lands, and
/// its publisher waits for an acknowledgement that never comes.
async fn acknowledge_publishes(bus: &EventBus, acknowledge: bool) {
    let stream = bus
        .jetstream()
        .get_stream(STREAM_NAME)
        .await
        .expect("the events stream");
    let mut config = stream.cached_info().config.clone();
    config.no_ack = !acknowledge;
    bus.jetstream()
        .update_stream(config)
        .await
        .expect("change whether the events stream acknowledges");
}

/// Assert that the delivery was NAKed once with no delay, never acked, and
/// that nothing followed the NAK.
fn assert_naked_once_with_no_delay(sent: &super::Sent) {
    assert!(
        sent.acks == 0 && sent.other == ["-NAK"] && sent.last.as_deref() == Some("-NAK"),
        "the last delivery, its dead letter not published, was NAKed once with no delay, not \
         acked, and nothing followed: {sent:?}"
    );
}

/// **The broker raises its max-deliveries advisory at once for a last
/// delivery NAKed with no delay, and not for one NAKed with a delay, even of
/// zero.** On the dispatcher's own consumer each trigger is brought to its
/// fifth and last delivery, whose ack window is twelve minutes, and that
/// delivery is NAKed three ways. With no delay the broker puts the message
/// back to be delivered at once, finds its deliveries spent, and sends the
/// advisory. With a delay it instead dates the delivery back by the
/// consumer's first ack window (thirty seconds) and forward by the delay,
/// and then waits for the last ack window to lapse, so the advisory comes
/// eleven and a half minutes later, plus the delay. This is why the
/// dispatcher hands an unpublished dead letter over with a bare NAK.
///
/// The advisory is read off the advisory stream, as the watch reads it,
/// with the time the broker stamped on it.
#[tokio::test]
async fn the_broker_raises_the_advisory_at_once_only_for_a_nak_with_no_delay() {
    within_the_test_deadline(async {
        let probes = [None, Some(Duration::ZERO), Some(Duration::from_millis(100))].map(
            |delay| async move {
                let mut pulled =
                    Pulled::new(Resolve::Drop, Turn::Later, Returns::TransientError).await;
                pulled.redeliver_until_the_last().await;
                let naked_at = chrono::Utc::now();
                pulled
                    .msg
                    .ack_with(AckKind::Nak(delay))
                    .await
                    .expect("NAK the last delivery");
                let advisory = captured_advisory_within(&pulled, NONE_COMES).await;
                (delay, naked_at, advisory)
            },
        );
        for (delay, naked_at, advisory) in futures::future::join_all(probes).await {
            match (delay, advisory) {
                (None, Some(advisory)) => assert_raised_at_once_by(&advisory, naked_at),
                (None, None) => panic!(
                    "a NAK with no delay on the last delivery raised no advisory within \
                     {NONE_COMES:?}"
                ),
                (Some(_), None) => {}
                (Some(delay), Some(advisory)) => panic!(
                    "a NAK with a delay of {delay:?} on the last delivery raised the advisory \
                     within {NONE_COMES:?}, {} after the NAK",
                    advisory.emitted - naked_at
                ),
            }
        }
    })
    .await;
}

/// **A last delivery whose dead letter cannot be published is NAKed, not
/// acked, the broker reports the trigger exhausted at once, and the
/// dead-letter watch writes one dead letter naming the trigger once the
/// events stream is back.**
///
/// Four deliveries are spent and the events stream is deleted. The fifth
/// delivery fails transiently before its first WAL write; the dispatcher's
/// dead letter finds no stream, so the claim is given back and the delivery
/// NAKed with no delay. The broker's advisory follows within [`AT_ONCE`].
/// The real watch runs its loop as the daemon does: it hears the advisory,
/// finds nothing on record for the trigger, fails to publish and NAKs the
/// advisory to try again, which the test waits to see. The events stream is
/// then created again, and the watch's next try writes the dead letter:
/// one, from the advisory, with the trigger's agent, subject, payload,
/// sequence and id.
#[tokio::test]
async fn a_last_delivery_whose_dead_letter_cannot_be_published_is_dead_lettered_by_the_watch() {
    within_the_test_deadline(async {
        let mut pulled = Pulled::new(Resolve::Drop, Turn::Later, Returns::TransientError).await;
        pulled.redeliver_until_the_last().await;
        let watch = AdvisoryWatch::new(pulled.bus.clone(), &pulled.dispatcher);
        let (stop_watching, stop) = oneshot::channel();
        let watching = tokio::spawn(watch.run(stop));
        pulled
            .bus
            .jetstream()
            .delete_stream(STREAM_NAME)
            .await
            .expect("delete the events stream");
        let mut answers = pulled.watch_answers().await;
        let handled_at = chrono::Utc::now();

        pulled.handle_within(Duration::from_secs(10)).await;

        let (trigger_id, _) = pulled.named();
        let sent = answers.sent().await;
        assert_naked_once_with_no_delay(&sent);
        // When the NAK was seen, by the clock the broker stamps by. The
        // broker can emit the advisory a moment before the test sees the
        // NAK, so that it came after the NAK is checked against the start of
        // the handling instead.
        let naked_at = chrono::Utc::now()
            - chrono::TimeDelta::from_std(sent.settled_at().elapsed()).expect("a short time");
        let advisory = captured_advisory(&pulled).await;
        assert!(
            advisory.emitted >= handled_at,
            "the advisory was emitted before the last delivery was handled"
        );
        assert!(
            advisory.emitted - naked_at < chrono::TimeDelta::from_std(AT_ONCE).unwrap(),
            "the advisory was emitted {} after the NAK, not at once",
            advisory.emitted - naked_at
        );
        let key = test_key(&pulled.bus, pulled.stream_seq());
        assert!(
            pulled
                .claims
                .trigger_claim(key)
                .await
                .expect("read the claim")
                .is_none(),
            "the claim was given back before the NAK, so the watch finds nothing on record"
        );

        // The watch tried, and NAKed the advisory to try again.
        let mut watch_consumer = pulled
            .bus
            .jetstream()
            .get_stream(ADVISORY_STREAM_NAME)
            .await
            .expect("the advisory stream")
            .get_consumer::<async_nats::jetstream::consumer::pull::Config>(WATCH_CONSUMER)
            .await
            .expect("the watch's consumer");
        let retried_by = tokio::time::Instant::now() + WRITTEN_WITHIN;
        while watch_consumer
            .info()
            .await
            .expect("the watch consumer's state")
            .num_redelivered
            == 0
        {
            assert!(
                tokio::time::Instant::now() < retried_by,
                "the watch never tried the advisory again while the events stream was gone"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            last_failed(&pulled).await.is_none(),
            "nothing was written while the events stream was gone"
        );

        EventBus::connect(&pulled.url)
            .await
            .expect("create the events stream again");
        let written_by = tokio::time::Instant::now() + WRITTEN_WITHIN;
        let event = loop {
            if let Some(event) = last_failed(&pulled).await {
                break event;
            }
            assert!(
                tokio::time::Instant::now() < written_by,
                "the watch wrote no dead letter within {WRITTEN_WITHIN:?} of the events stream's \
                 return"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let _ = stop_watching.send(());
        tokio::time::timeout(Duration::from_secs(10), watching)
            .await
            .expect("the watch stops")
            .expect("the watch task joins")
            .expect("the watch ran without error");

        assert_eq!(dead_letters_of(&pulled).await, 1, "one dead letter");
        assert_eq!(event.envelope.agent_id.as_str(), agent(&pulled));
        assert!(
            matches!(&event.payload, EventPayload::Failed(failed)
                if matches!(failed.error_kind, FailureKind::TriggerExhausted)),
            "a `trigger_exhausted` dead letter: {:?}",
            event.payload
        );
        let annotations = &event.annotations.0;
        for (key, value) in [
            (DEAD_LETTER_SOURCE_KEY, json!("advisory")),
            (DEAD_LETTER_SUBJECT_KEY, json!(pulled.msg.subject.as_str())),
            (DEAD_LETTER_PAYLOAD_KEY, json!({"input": "hi"})),
            (DEAD_LETTER_STREAM_SEQ_KEY, json!(pulled.stream_seq())),
            (DEAD_LETTER_TRIGGER_ID_KEY, json!(trigger_id.to_string())),
        ] {
            assert_eq!(
                annotations.get(key),
                Some(&value),
                "`{key}` in {annotations:?}"
            );
        }
    })
    .await;
}

/// **A dead letter whose publish reached the events stream, and whose
/// acknowledgement was lost, is written once.** The events stream is set to
/// store what is published to it without acknowledging it, so the
/// dispatcher's dead letter lands and its publish times out. The dispatcher
/// cannot tell that from a publish that never landed, so it hands the
/// trigger over as for any other: the claim given back, the delivery NAKed
/// with no delay. The broker raises its advisory, and the watch, handed it
/// with the stream acknowledging again, finds the dispatcher's dead letter
/// as the agent's most recent `failed` event, carrying the trigger's
/// sequence, and writes no second one.
#[tokio::test]
async fn a_dead_letter_that_landed_but_whose_acknowledgement_was_lost_is_written_once() {
    within_the_test_deadline(async {
        let mut pulled = Pulled::new(Resolve::Drop, Turn::Later, Returns::TransientError).await;
        pulled.redeliver_until_the_last().await;
        acknowledge_publishes(&pulled.bus, false).await;
        let mut answers = pulled.watch_answers().await;

        pulled.handle_within(Duration::from_secs(20)).await;

        assert_naked_once_with_no_delay(&answers.sent().await);
        acknowledge_publishes(&pulled.bus, true).await;
        let inline = last_failed(&pulled)
            .await
            .expect("the dispatcher's dead letter landed");
        assert_eq!(
            inline.annotations.0.get(DEAD_LETTER_SOURCE_KEY),
            Some(&json!("inline"))
        );
        let advisory = captured_advisory(&pulled).await;

        let outcome = AdvisoryWatch::new(pulled.bus.clone(), &pulled.dispatcher)
            .handle_advisory(&advisory.payload, 1)
            .await
            .expect("the watch handles the advisory");

        assert_eq!(outcome, AdvisoryOutcome::AlreadySurfaced);
        assert_eq!(
            dead_letters_of(&pulled).await,
            1,
            "the dispatcher's dead letter, and no second one from the watch"
        );
    })
    .await;
}
