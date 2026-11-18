//! What the watch does when it cannot read the trigger an advisory names
//! (issue 1066), against a real broker.
//!
//! The watch reads the trigger's message at the advisory's sequence to name
//! the dead letter: its agent, subject, payload and id. The broker answers
//! that read in one of three ways, and they mean different things. The
//! message is there. The stream holds no message at that sequence, because
//! it aged out, was deleted or never was: the trigger cannot be named, ever.
//! Or the read fails for another reason (the broker does not answer in time,
//! the stream is missing): the message may well be there, and a later read
//! may name it. These tests make each happen for real and drive the real
//! [`AdvisoryWatch::handle_advisory`], with no records to consult.

use std::time::Duration;

use serde_json::json;

use super::test_kit::{PAST, exhaust, last_failed, stamped_at_the_streams_creation};
use super::*;
use crate::dead_letter::from_event;
use crate::test_support::nats::{NatsServer, test_nats};
use crate::trigger::PublishedTrigger;

/// A broker, a bus on it, and an agent name of the test's own.
async fn broker() -> (NatsServer, EventBus, String) {
    let server = test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let agent = format!("read-{}", Uuid::now_v7().simple());
    (server, bus, agent)
}

/// Publish a trigger for `agent` and exhaust it, and return it with the
/// broker's MAX_DELIVERIES advisory for it.
async fn exhausted(bus: &EventBus, agent: &str) -> (PublishedTrigger, Vec<u8>) {
    let published = bus
        .publish_trigger(&AgentId::new(agent).unwrap(), &json!({ "input": "work" }))
        .await
        .expect("publish the trigger");
    (published, exhaust(bus, agent).await)
}

/// **What the broker answers for a message that is not there.** A sequence
/// past the stream's last, a message deleted, and a message aged out of a
/// stream by its `max_age` all come back as the client's
/// `RawMessageErrorKind::NoMessageFound`, the broker's error code 10037. A
/// stream that does not exist comes back as a JetStream error with code
/// 10059, not as a missing message. This pins the broker's side of the
/// watch's line between "gone" and "cannot read yet".
#[tokio::test]
async fn the_broker_answers_a_missing_message_with_no_message_found() {
    use async_nats::jetstream::ErrorCode;
    use async_nats::jetstream::stream::RawMessageErrorKind;

    let (_server, bus, agent) = broker().await;
    let published = bus
        .publish_trigger(&AgentId::new(&agent).unwrap(), &json!({ "input": "one" }))
        .await
        .expect("publish");
    let triggers = bus
        .jetstream()
        .get_stream(TRIGGER_STREAM_NAME)
        .await
        .expect("the trigger stream");
    let kind = |result: Result<_, async_nats::jetstream::stream::RawMessageError>| {
        result.map(|_| ()).map_err(|err| err.kind())
    };

    assert_eq!(
        kind(triggers.get_raw_message(published.stream_seq).await),
        Ok(())
    );
    assert_eq!(
        kind(triggers.get_raw_message(published.stream_seq + 1).await),
        Err(RawMessageErrorKind::NoMessageFound),
        "never there"
    );
    assert!(
        triggers
            .delete_message(published.stream_seq)
            .await
            .expect("delete")
    );
    assert_eq!(
        kind(triggers.get_raw_message(published.stream_seq).await),
        Err(RawMessageErrorKind::NoMessageFound),
        "deleted"
    );

    let aging = bus
        .jetstream()
        .create_stream(async_nats::jetstream::stream::Config {
            name: "read-aging".to_string(),
            subjects: vec!["read.aging".to_string()],
            max_age: Duration::from_secs(1),
            ..Default::default()
        })
        .await
        .expect("a stream whose messages age out in a second");
    let seq = bus
        .jetstream()
        .publish("read.aging", "soon gone".into())
        .await
        .expect("publish")
        .await
        .expect("acked")
        .sequence;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let aged_out = loop {
        match aging.get_raw_message(seq).await {
            Ok(_) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the message did not age out within 10s"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(err) => break err.kind(),
        }
    };
    assert_eq!(aged_out, RawMessageErrorKind::NoMessageFound, "aged out");

    let missing = bus
        .jetstream()
        .get_stream_no_info("read-no-such-stream")
        .await
        .expect("a handle, made without asking the broker")
        .get_raw_message(1)
        .await
        .expect_err("no stream, no message");
    match missing.kind() {
        RawMessageErrorKind::JetStream(err) => {
            assert_eq!(err.error_code(), ErrorCode::STREAM_NOT_FOUND)
        }
        other => panic!("a missing stream answers {other:?}"),
    }
}

/// **A trigger whose message cannot be read is held, and named once it can
/// be.** The broker is stopped while the watch reads, so the read times out:
/// the message is on the stream, and only the read failed. Dead-lettered
/// now, the trigger would be written naming nothing, in a form that cannot
/// be requeued, though a later read names it. So the verdict is held, as it
/// is when the records cannot be read, and nothing is written. Once the
/// broker answers again, the same advisory names the trigger.
#[tokio::test]
async fn a_trigger_that_cannot_be_read_is_held_and_named_once_it_can_be() {
    let (server, bus, agent) = broker().await;
    let (published, advisory) = exhausted(&bus, &agent).await;
    let watch = AdvisoryWatch::without_records(bus.clone());

    server.pause();
    let held = watch.handle_advisory(&advisory, 1).await;
    server.resume();

    match held.expect("decided, not failed") {
        AdvisoryOutcome::Held(hold) => {
            assert!(
                hold.why
                    .starts_with("the trigger's message could not be read"),
                "{hold:?}"
            );
            assert_eq!(hold.stream_seq, published.stream_seq);
        }
        other => panic!("a trigger that cannot be read is held: {other:?}"),
    }
    assert!(last_failed(&bus, &agent).await.is_none());
    assert!(last_failed(&bus, AgentId::SYSTEM_STR).await.is_none());

    assert_eq!(
        watch.handle_advisory(&advisory, 2).await.unwrap(),
        AdvisoryOutcome::Emitted
    );
    let event = last_failed(&bus, &agent)
        .await
        .expect("dead-lettered under the trigger's agent");
    let dead = from_event(&event).expect("a dead letter");
    assert_eq!(dead.trigger_subject, subjects::trigger(&agent));
    assert_eq!(dead.trigger_payload, json!({ "input": "work" }));
    assert_eq!(dead.unattributed, None);
    assert_eq!(
        event.annotations.0.get(DEAD_LETTER_TRIGGER_ID_KEY),
        Some(&json!(published.id.to_string()))
    );
}

/// **A trigger whose message stays unreadable is dead-lettered at the
/// watch's limit, naming nothing and saying why.** The trigger stream is
/// gone, so every read fails, and not because the message is missing: the
/// broker says there is no stream. Inside the limit the verdict is held, as
/// above. Past it, an hour after the advisory and on its third look, as for
/// any held verdict, the watch writes the dead letter rather than hold the
/// advisory until it ages out of its own stream with no record at all. It
/// names no trigger, and it says the message could not be read, not that it
/// had aged out.
#[tokio::test]
async fn a_trigger_that_stays_unreadable_is_dead_lettered_at_the_limit_saying_why() {
    let (_server, bus, agent) = broker().await;
    let (published, advisory) = exhausted(&bus, &agent).await;
    let advisory = stamped_at_the_streams_creation(&bus, &advisory);
    bus.jetstream()
        .delete_stream(TRIGGER_STREAM_NAME)
        .await
        .expect("delete the trigger stream");
    let watch = AdvisoryWatch::without_records(bus.clone()).with_held_verdict_limit(PAST);

    for look in 1..HELD_VERDICT_MIN_LOOKS {
        let outcome = watch.handle_advisory(&advisory, look).await.unwrap();
        assert!(
            matches!(outcome, AdvisoryOutcome::Held(_)),
            "look {look}: {outcome:?}"
        );
        assert!(last_failed(&bus, AgentId::SYSTEM_STR).await.is_none());
    }
    let outcome = watch
        .handle_advisory(&advisory, HELD_VERDICT_MIN_LOOKS)
        .await
        .unwrap();

    assert_eq!(outcome, AdvisoryOutcome::Emitted);
    assert!(last_failed(&bus, &agent).await.is_none());
    let event = last_failed(&bus, AgentId::SYSTEM_STR)
        .await
        .expect("dead-lettered under the system sentinel");
    let dead = from_event(&event).expect("a dead letter");
    assert_eq!(dead.trigger_stream_seq, Some(published.stream_seq));
    assert_eq!(dead.trigger_payload, serde_json::Value::Null);
    assert_eq!(event.annotations.0.get(DEAD_LETTER_TRIGGER_ID_KEY), None);
    let why = dead.unattributed.expect("it says why it names no trigger");
    assert!(
        why.starts_with(
            "the trigger's message could not be read by the watch's limit: the trigger stream \
             was missing (the last read answered: "
        ) && why.contains("error code 10059")
            && !why.contains("may still be on the stream"),
        "{why}"
    );
    assert_eq!(
        dead.unattributed_kind,
        Some(crate::dead_letter::UnattributedKind::MessageUnreadable)
    );
}

/// **A trigger whose message is gone from the stream is dead-lettered as
/// before**, at once and naming nothing: the broker says there is no
/// message at that sequence, and no later read can name it. Here the
/// message was deleted; aged out, it answers the same. The dead letter
/// says why it names nothing, with when the watch read the stream and when
/// the advisory was emitted, as one from an earlier incarnation does, so
/// `unattributed` absent means the dead letter names its trigger.
#[tokio::test]
async fn a_trigger_whose_message_is_gone_is_dead_lettered_naming_nothing_at_once() {
    let (_server, bus, agent) = broker().await;
    let (published, advisory) = exhausted(&bus, &agent).await;
    assert!(
        bus.jetstream()
            .get_stream(TRIGGER_STREAM_NAME)
            .await
            .expect("the trigger stream")
            .delete_message(published.stream_seq)
            .await
            .expect("delete the trigger's message")
    );
    let watch = AdvisoryWatch::without_records(bus.clone());

    let outcome = watch.handle_advisory(&advisory, 1).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::Emitted);
    assert!(last_failed(&bus, &agent).await.is_none());
    let event = last_failed(&bus, AgentId::SYSTEM_STR)
        .await
        .expect("dead-lettered under the system sentinel");
    let dead = from_event(&event).expect("a dead letter");
    assert_eq!(dead.trigger_stream_seq, Some(published.stream_seq));
    assert_eq!(dead.trigger_payload, serde_json::Value::Null);
    assert_eq!(event.annotations.0.get(DEAD_LETTER_TRIGGER_ID_KEY), None);
    let emitted = serde_json::from_slice::<serde_json::Value>(&advisory).unwrap()["timestamp"]
        .as_str()
        .map(|at| {
            chrono::DateTime::parse_from_rfc3339(at)
                .unwrap()
                .with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
        })
        .expect("the broker stamps its advisory");
    let why = dead.unattributed.expect("it says why it names no trigger");
    assert!(
        why.starts_with(
            "the broker answered that the trigger stream held no message at this sequence when \
             the watch last read it, at "
        ) && why.contains(&format!("for an advisory emitted at {emitted}"))
            && why.contains("24h after its publish"),
        "{why}"
    );
    assert_eq!(
        dead.unattributed_kind,
        Some(crate::dead_letter::UnattributedKind::MessageGone)
    );
}
