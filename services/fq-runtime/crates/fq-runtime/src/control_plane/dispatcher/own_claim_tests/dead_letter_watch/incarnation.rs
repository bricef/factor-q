//! Which trigger the dead-letter watch names, by the incarnation of the
//! trigger stream an advisory was emitted for (issue 1054).
//!
//! The broker's MAX_DELIVERIES advisory carries the stream's name and the
//! message's sequence, not which incarnation of the stream it was emitted
//! for. A recreated stream numbers its messages from 1 again, so an
//! advisory emitted before the stream was last created names a sequence
//! that this stream's message at the same sequence does not answer to. The
//! watch tells the two apart by the advisory's timestamp against the
//! stream's creation, and these tests drive the real
//! [`AdvisoryWatch::handle_advisory`] with the broker's own advisory,
//! stamped before, at or without that moment.

use serde_json::json;

use super::{
    Arena, LOOKED, SEQ, arena, claim, dead_letter, dead_letter_at, exhaust, kept, publish,
    stamped_at_the_streams_creation, watch,
};
use crate::agent::AgentId;
use crate::bus::STREAM_NAME;
use crate::control_plane::advisory_watch::AdvisoryOutcome;
use crate::dead_letter::{DEAD_LETTER_STREAM_SEQ_KEY, DEAD_LETTER_TRIGGER_ID_KEY, from_event};
use crate::events::{Event, subjects};
use crate::trigger::PublishedTrigger;

/// The advisory, stamped a second before the trigger stream was created:
/// one the broker emitted for the stream before this one, at the same
/// sequence as a message this stream holds.
fn stamped_before_the_streams_creation(arena: &Arena, advisory: &[u8]) -> Vec<u8> {
    let created = chrono::DateTime::from_timestamp_nanos(arena.bus.trigger_stream_epoch());
    let before = created - chrono::Duration::seconds(1);
    restamped(
        advisory,
        Some(&before.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)),
    )
}

/// The advisory with its timestamp set to `timestamp`, or taken away.
fn restamped(advisory: &[u8], timestamp: Option<&str>) -> Vec<u8> {
    let mut restamped: serde_json::Value = serde_json::from_slice(advisory).unwrap();
    let fields = restamped.as_object_mut().expect("an advisory is an object");
    match timestamp {
        Some(at) => fields.insert("timestamp".to_string(), json!(at)),
        None => fields.remove("timestamp"),
    };
    serde_json::to_vec(&restamped).unwrap()
}

/// The last dead letter the watch wrote under the system sentinel, if any.
async fn system_dead_letter(arena: &Arena) -> Option<Event> {
    let raw = arena
        .bus
        .jetstream()
        .get_stream(STREAM_NAME)
        .await
        .expect("the event stream")
        .get_last_raw_message_by_subject(&subjects::agent_failed(AgentId::system().as_str()))
        .await
        .ok()?;
    Some(serde_json::from_slice(&raw.payload).expect("the event parses"))
}

/// The dead letter names the trigger this stream holds at [`SEQ`]: its
/// agent, subject, payload and id, with the same annotations as always.
async fn attributed_to(arena: &Arena, published: &PublishedTrigger) {
    let event = dead_letter(arena)
        .await
        .expect("dead-lettered under the trigger's agent");
    let dead = from_event(&event).expect("a dead letter");
    assert_eq!(dead.agent_id, arena.agent);
    assert_eq!(dead.trigger_subject, subjects::trigger(&arena.agent));
    assert_eq!(dead.trigger_payload, json!({ "input": "hi" }));
    assert_eq!(dead.trigger_stream_seq, Some(SEQ));
    assert_eq!(dead.source, "advisory");
    assert_eq!(dead.start_unchecked, None);
    assert_eq!(dead.unattributed, None);
    assert!(
        dead.error_message
            .starts_with("trigger exhausted after 2 deliveries"),
        "{}",
        dead.error_message
    );
    assert_eq!(
        event.annotations.0.get(DEAD_LETTER_TRIGGER_ID_KEY),
        Some(&json!(published.id.to_string()))
    );
    let keys: Vec<&str> = event.annotations.0.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "dead_letter_source",
            "trigger_id",
            "trigger_payload",
            "trigger_stream_seq",
            "trigger_subject",
        ],
        "the annotations a dead letter of this incarnation has always had, and no other"
    );
}

/// **An advisory from an earlier incarnation of the trigger stream names no
/// trigger of the current one.** Its sequence is a position on the stream
/// before this one, whose messages went with it; the message this stream
/// holds at the same sequence is another trigger, which here ran once, its
/// claim marked started. The watch does not read that message: it writes
/// the dead letter under the system sentinel, with no subject, payload or
/// trigger id, as it does for a trigger that aged out of the stream, so the
/// exhaustion is still recorded and nothing false is attributed, and says
/// on it why it names none, with the stream's creation time. The
/// current trigger's claim and run are left as they were. Attributed to
/// the current message, the dead letter would name a trigger that ran, and
/// requeuing it would run that work twice.
#[tokio::test]
async fn an_advisory_from_an_earlier_incarnation_names_no_trigger_of_the_current_stream() {
    let (arena, _server) = arena().await;
    let current = publish(&arena).await;
    let ran = arena.deliver(1, false).await;
    assert_eq!(ran.acks, 1, "the current trigger ran: {ran:?}");
    let run = arena.worker.starts()[0].0.to_string();
    assert_eq!(claim(&arena).await, Some((true, Some(run.clone()))));
    let wal = arena.wal().await;
    let earlier = stamped_before_the_streams_creation(&arena, &exhaust(&arena).await);

    let outcome = watch(&arena)
        .handle_advisory(&earlier, LOOKED)
        .await
        .unwrap();

    assert_eq!(outcome, AdvisoryOutcome::Emitted);
    assert!(
        dead_letter(&arena).await.is_none(),
        "nothing is dead-lettered under the current trigger's agent"
    );
    let event = system_dead_letter(&arena)
        .await
        .expect("the exhaustion is still recorded, under the system sentinel");
    let dead = from_event(&event).expect("a dead letter");
    assert_eq!(dead.agent_id, AgentId::system().as_str());
    assert_eq!(dead.trigger_subject, "", "no subject");
    assert_eq!(dead.trigger_payload, serde_json::Value::Null, "no payload");
    assert_eq!(
        event.annotations.0.get(DEAD_LETTER_TRIGGER_ID_KEY),
        None,
        "no trigger id, so `dead_letter.requeue` refuses it; the current trigger's is {}",
        current.id
    );
    assert_eq!(dead.trigger_stream_seq, Some(SEQ));
    assert_eq!(dead.source, "advisory");
    assert_eq!(dead.start_unchecked, None);
    let created = chrono::DateTime::from_timestamp_nanos(arena.bus.trigger_stream_epoch())
        .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true);
    let why = dead.unattributed.expect("it says why it names no trigger");
    assert!(
        why.starts_with("its advisory came from an earlier incarnation of the trigger stream")
            && why.contains(&format!("before the stream was created at {created}")),
        "{why}"
    );
    assert_eq!(
        claim(&arena).await,
        Some((true, Some(run))),
        "the current trigger's claim is untouched"
    );
    assert_eq!(arena.wal().await, wal, "its run is untouched");
    assert_eq!(arena.worker.starts().len(), 1, "nothing started");
    assert_eq!(kept(&arena), 0);
}

/// **An advisory from an earlier incarnation is not taken for the current
/// trigger's dead letter.** The trigger this stream holds at the sequence
/// was dead-lettered already. Read at that sequence, the earlier advisory
/// met that dead letter in the watch's check against a second dead letter
/// for one exhaustion, was dropped as already surfaced, and the earlier
/// trigger's exhaustion was recorded nowhere. Naming no trigger, it is
/// recorded under the system sentinel.
#[tokio::test]
async fn an_advisory_from_an_earlier_incarnation_is_not_dropped_for_the_current_triggers_dead_letter()
 {
    let (arena, _server) = arena().await;
    publish(&arena).await;
    let advisory = exhaust(&arena).await;
    let watch = watch(&arena);
    assert_eq!(
        watch.handle_advisory(&advisory, LOOKED).await.unwrap(),
        AdvisoryOutcome::Emitted
    );
    assert!(dead_letter_at(&arena, SEQ).await.is_some());
    let earlier = stamped_before_the_streams_creation(&arena, &advisory);

    let outcome = watch.handle_advisory(&earlier, LOOKED).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::Emitted);
    let event = system_dead_letter(&arena)
        .await
        .expect("recorded under the system sentinel");
    assert_eq!(
        event.annotations.0.get(DEAD_LETTER_STREAM_SEQ_KEY),
        Some(&json!(SEQ))
    );
}

/// **An advisory of this incarnation is attributed as before**, to the
/// message the stream holds at its sequence. Stamped at the very moment the
/// stream was created, it is as old as an advisory of this incarnation can
/// be, and still this incarnation's.
#[tokio::test]
async fn an_advisory_stamped_at_the_streams_creation_names_the_message_at_its_sequence() {
    let (arena, _server) = arena().await;
    let published = publish(&arena).await;
    let advisory = stamped_at_the_streams_creation(&arena, &exhaust(&arena).await);

    let outcome = watch(&arena)
        .handle_advisory(&advisory, LOOKED)
        .await
        .unwrap();

    assert_eq!(outcome, AdvisoryOutcome::Emitted);
    attributed_to(&arena, &published).await;
    assert!(system_dead_letter(&arena).await.is_none());
    assert_eq!(kept(&arena), 0);
}

/// **An advisory with no timestamp the watch can read is taken as this
/// incarnation's**, so the message the stream holds at its sequence names
/// it. The broker always stamps one, so nothing should produce such an
/// advisory; this pins how the watch reads one, so that it changes only on
/// purpose. With no timestamp, the dead letter names the trigger this
/// stream holds; with one the watch cannot parse, the same advisory finds
/// that dead letter already surfaced, which it can only do under that
/// trigger's agent.
#[tokio::test]
async fn an_advisory_with_no_readable_timestamp_names_the_message_at_its_sequence() {
    let (arena, _server) = arena().await;
    let published = publish(&arena).await;
    let advisory = exhaust(&arena).await;
    let watch = watch(&arena);

    let unstamped = watch
        .handle_advisory(&restamped(&advisory, None), LOOKED)
        .await
        .unwrap();
    let unreadable = watch
        .handle_advisory(&restamped(&advisory, Some("not a time")), LOOKED)
        .await
        .unwrap();

    assert_eq!(unstamped, AdvisoryOutcome::Emitted);
    attributed_to(&arena, &published).await;
    assert_eq!(unreadable, AdvisoryOutcome::AlreadySurfaced);
    assert!(system_dead_letter(&arena).await.is_none());
}
