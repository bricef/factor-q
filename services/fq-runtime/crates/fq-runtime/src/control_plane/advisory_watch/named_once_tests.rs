//! A trigger's exhaustion gets one dead letter from the watch, whichever
//! form came first (issue 1066, item 5), against a real broker.
//!
//! A dead letter that names its trigger goes under the trigger's agent,
//! whether the dispatcher wrote it or the watch did. The watch's check
//! against a second one compared only the agent's most recent `failed`
//! event, so another failure of the agent recorded in between hid the
//! first dead letter and the watch wrote a second, and an older dead letter
//! at the same sequence, from before the trigger was published, was taken
//! for this trigger's. And a dead letter the watch filed under the system
//! sentinel looked only there, so one that named the trigger, written
//! before its message went, was not seen. These tests write the
//! dispatcher's dead letter the way it writes it, or let the watch write
//! its own, and hand the real [`AdvisoryWatch::handle_advisory`] the
//! broker's advisory for the same trigger. A watch remembers what it wrote
//! since it started, so a test that pins the read of the log hands the
//! advisory back to a watch made afresh, as after a restart ([`restarted`]).

use futures::StreamExt;
use serde_json::json;

use super::test_kit::{PAST, exhaust, stamped_at_the_streams_creation, unreadable_failure};
use super::*;
use crate::agent::AgentId;
use crate::dead_letter::from_event;
use crate::event_tail::TailedMessage;
use crate::events::subjects;
use crate::test_support::nats::{NatsServer, test_nats};

/// A broker, a bus on it, and an agent name of the test's own.
pub(super) async fn broker() -> (NatsServer, EventBus, String) {
    let server = test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let agent = format!("named-once-{}", Uuid::now_v7().simple());
    (server, bus, agent)
}

/// A `failed` event under `agent`, of `kind`.
fn failed(agent: &str, kind: FailureKind) -> Event {
    Event::new(
        AgentId::new(agent).unwrap(),
        Uuid::now_v7(),
        EventPayload::Failed(FailedPayload {
            error_kind: kind,
            error_message: "trigger exhausted after 5 deliveries (limit 5)".to_string(),
            phase: FailurePhase::Setup,
            partial_totals: InvocationTotals::default(),
            detail: None,
        }),
    )
}

/// Record the dispatcher's own dead letter for the trigger at `stream_seq`,
/// shaped as it writes one: under the agent, with the trigger's subject,
/// payload, sequence, the id it handled the delivery under, and the
/// incarnation of the trigger stream `bus` connected to.
pub(super) async fn dispatcher_dead_letter(
    bus: &EventBus,
    agent: &str,
    stream_seq: u64,
    trigger_id: Uuid,
) {
    marked_dead_letter(
        bus,
        agent,
        stream_seq,
        trigger_id,
        Some(bus.trigger_stream_epoch()),
    )
    .await;
}

/// [`dispatcher_dead_letter`], recording the incarnation `epoch`, or none,
/// as a build before dead letters recorded it wrote one.
async fn marked_dead_letter(
    bus: &EventBus,
    agent: &str,
    stream_seq: u64,
    trigger_id: Uuid,
    epoch: Option<i64>,
) {
    let event = failed(agent, FailureKind::TriggerExhausted)
        .annotate(
            DEAD_LETTER_SUBJECT_KEY,
            serde_json::Value::String(subjects::trigger(agent)),
        )
        .annotate(DEAD_LETTER_PAYLOAD_KEY, json!({ "input": "work" }))
        .annotate(DEAD_LETTER_STREAM_SEQ_KEY, json!(stream_seq))
        .annotate(DEAD_LETTER_SOURCE_KEY, json!("inline"))
        .annotate(DEAD_LETTER_TRIGGER_ID_KEY, json!(trigger_id.to_string()));
    let event = match epoch {
        Some(epoch) => event.annotate("trigger_stream_epoch", json!(epoch.to_string())),
        None => event,
    };
    bus.publish(&event)
        .await
        .expect("the dispatcher's dead letter");
}

/// A watch made afresh on `bus`, as the daemon makes one when it starts
/// again: it remembers nothing it wrote, so it can only read the log.
fn restarted(bus: &EventBus) -> AdvisoryWatch {
    AdvisoryWatch::without_records(bus.clone())
}

/// Record another failure of `agent`, not a dead letter.
async fn another_failure(bus: &EventBus, agent: &str) {
    bus.publish(&failed(agent, FailureKind::RuntimeError))
        .await
        .expect("another failure");
}

/// Publish a trigger for `agent` with an id of its own.
async fn trigger(bus: &EventBus, agent: &str) -> crate::trigger::PublishedTrigger {
    bus.publish_trigger(&AgentId::new(agent).unwrap(), &json!({ "input": "work" }))
        .await
        .expect("publish the trigger")
}

/// Publish a trigger for `agent` with no id, as `fq-cron` does; its
/// sequence.
pub(super) async fn untagged_trigger(bus: &EventBus, agent: &str) -> u64 {
    bus.jetstream()
        .publish(
            subjects::trigger(agent),
            serde_json::to_vec(&json!({ "input": "work" }))
                .unwrap()
                .into(),
        )
        .await
        .expect("publish")
        .await
        .expect("acked")
        .sequence
}

/// How many dead letters `subject` holds.
async fn dead_letters_on(bus: &EventBus, subject: &str) -> usize {
    let tip = bus
        .last_event_seq_matching(subject)
        .await
        .expect("the subject's tip");
    if tip == 0 {
        return 0;
    }
    let mut events = bus.events_from(subject, 1).await.expect("a tail");
    let mut written = 0;
    while let Some(next) = events.next().await {
        let TailedMessage { seq, event } = next.expect("an event");
        if event.as_ref().and_then(from_event).is_some() {
            written += 1;
        }
        if seq >= tip {
            break;
        }
    }
    written
}

/// How many dead letters `agent` holds.
async fn dead_letters(bus: &EventBus, agent: &str) -> usize {
    dead_letters_on(bus, &subjects::agent_failed(agent)).await
}

/// **Another failure of the agent in between does not hide the
/// dispatcher's dead letter.** The dispatcher wrote the trigger's dead
/// letter, and its acknowledgement was lost, so the trigger came to the
/// watch; meanwhile the agent failed again. The watch finds the dead letter
/// and writes no second.
#[tokio::test]
async fn another_failure_in_between_does_not_hide_the_dispatchers_dead_letter() {
    let (_server, bus, agent) = broker().await;
    let published = trigger(&bus, &agent).await;
    let advisory = exhaust(&bus, &agent).await;
    dispatcher_dead_letter(&bus, &agent, published.stream_seq, published.id).await;
    another_failure(&bus, &agent).await;
    let watch = AdvisoryWatch::without_records(bus.clone());

    let outcome = watch.handle_advisory(&advisory, 1).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::AlreadySurfaced);
    assert_eq!(dead_letters(&bus, &agent).await, 1);
}

/// **The watch's own dead letter is found the same way**: written, its
/// advisory's ack lost, another failure of the agent, the advisory back.
#[tokio::test]
async fn another_failure_in_between_does_not_hide_the_watchs_own_dead_letter() {
    let (_server, bus, agent) = broker().await;
    trigger(&bus, &agent).await;
    let advisory = exhaust(&bus, &agent).await;
    let watch = AdvisoryWatch::without_records(bus.clone());
    assert_eq!(
        watch.handle_advisory(&advisory, 1).await.unwrap(),
        AdvisoryOutcome::Emitted
    );
    another_failure(&bus, &agent).await;

    let again = restarted(&bus).handle_advisory(&advisory, 2).await.unwrap();

    assert_eq!(again, AdvisoryOutcome::AlreadySurfaced);
    assert_eq!(dead_letters(&bus, &agent).await, 1);
}

/// **A dispatcher's dead letter older than the agent's last failure is
/// still found**, here when that last failure is the dead letter of
/// another of the agent's triggers.
#[tokio::test]
async fn a_dead_letter_older_than_the_agents_last_failure_is_found() {
    let (_server, bus, agent) = broker().await;
    let published = trigger(&bus, &agent).await;
    let advisory = exhaust(&bus, &agent).await;
    dispatcher_dead_letter(&bus, &agent, published.stream_seq, published.id).await;
    dispatcher_dead_letter(&bus, &agent, published.stream_seq + 1, Uuid::now_v7()).await;
    let watch = AdvisoryWatch::without_records(bus.clone());

    let outcome = watch.handle_advisory(&advisory, 1).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::AlreadySurfaced);
    assert_eq!(dead_letters(&bus, &agent).await, 2);
}

/// **An earlier incarnation's dead letter at the same sequence does not
/// stand for a new trigger's.** A trigger at sequence 1 is dead-lettered;
/// the trigger stream is deleted and created again, so the next trigger is
/// at sequence 1 too, and it is exhausted. Its dead letter is written: the
/// first records the earlier incarnation (and is older than the new
/// message). The trigger carries no id, so the incarnation is what tells
/// the two apart.
#[tokio::test]
async fn an_earlier_incarnations_dead_letter_does_not_stand_for_a_new_trigger() {
    let (server, bus, agent) = broker().await;
    let old = untagged_trigger(&bus, &agent).await;
    assert_eq!(old, 1);
    dispatcher_dead_letter(&bus, &agent, old, Uuid::now_v7()).await;
    bus.jetstream()
        .delete_stream(TRIGGER_STREAM_NAME)
        .await
        .expect("delete the trigger stream");
    let bus = EventBus::connect(server.url())
        .await
        .expect("reconnect, creating the stream again");
    let new = untagged_trigger(&bus, &agent).await;
    assert_eq!(new, 1, "a recreated stream numbers from 1 again");
    let advisory = exhaust(&bus, &agent).await;
    let watch = AdvisoryWatch::without_records(bus.clone());

    let outcome = watch.handle_advisory(&advisory, 1).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::Emitted);
    assert_eq!(dead_letters(&bus, &agent).await, 2);
}

/// **A dead letter at the trigger's sequence that names another trigger is
/// not this trigger's**, though it was written after this one was
/// published: the dispatcher can still be finishing a delivery of the
/// earlier stream's message at that sequence when the stream is recreated.
/// A trigger that carries an id is told apart by it; the watch writes this
/// trigger's dead letter.
#[tokio::test]
async fn a_dead_letter_at_the_sequence_naming_another_trigger_is_not_its() {
    let (_server, bus, agent) = broker().await;
    let published = trigger(&bus, &agent).await;
    let advisory = exhaust(&bus, &agent).await;
    dispatcher_dead_letter(&bus, &agent, published.stream_seq, Uuid::now_v7()).await;
    let watch = AdvisoryWatch::without_records(bus.clone());

    let outcome = watch.handle_advisory(&advisory, 1).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::Emitted);
    assert_eq!(dead_letters(&bus, &agent).await, 2);
}

/// **A trigger published without an id is found too.** The dispatcher
/// gives such a trigger an id for each delivery, and its dead letter
/// carries one the watch cannot know; the sequence, since the trigger was
/// published, ties the two. Another failure in between does not hide it.
#[tokio::test]
async fn a_trigger_published_without_an_id_is_found_by_its_sequence() {
    let (_server, bus, agent) = broker().await;
    let seq = untagged_trigger(&bus, &agent).await;
    let advisory = exhaust(&bus, &agent).await;
    dispatcher_dead_letter(&bus, &agent, seq, Uuid::now_v7()).await;
    another_failure(&bus, &agent).await;
    let watch = AdvisoryWatch::without_records(bus.clone());

    let outcome = watch.handle_advisory(&advisory, 1).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::AlreadySurfaced);
    assert_eq!(dead_letters(&bus, &agent).await, 1);
}

/// **A dead letter that names the trigger is not followed by a nameless
/// one.** The watch wrote the trigger's dead letter; the advisory's ack was
/// lost, and by the time it comes back the trigger's message is gone. The
/// dead letter it would write now names no trigger; the one it wrote
/// carries the advisory's id, under the trigger's agent, and is found.
#[tokio::test]
async fn a_nameless_dead_letter_does_not_follow_a_named_one() {
    let (_server, bus, agent) = broker().await;
    let published = trigger(&bus, &agent).await;
    let advisory = exhaust(&bus, &agent).await;
    let watch = AdvisoryWatch::without_records(bus.clone());
    assert_eq!(
        watch.handle_advisory(&advisory, 1).await.unwrap(),
        AdvisoryOutcome::Emitted
    );
    assert!(
        bus.jetstream()
            .get_stream(TRIGGER_STREAM_NAME)
            .await
            .expect("the trigger stream")
            .delete_message(published.stream_seq)
            .await
            .expect("delete the trigger's message")
    );

    let again = restarted(&bus).handle_advisory(&advisory, 2).await.unwrap();

    assert_eq!(again, AdvisoryOutcome::AlreadySurfaced);
    assert_eq!(dead_letters(&bus, &agent).await, 1);
    assert_eq!(
        dead_letters_on(&bus, &subjects::agent_failed(AgentId::SYSTEM_STR)).await,
        0
    );
}

/// **A search for a dead letter that names the trigger that runs out of
/// time decides again inside the limit, and writes past it.** The agent
/// failed again after the dispatcher's dead letter, so the watch has to
/// read the agent's failures since the trigger was published, and here it
/// is given no time to. Inside the limit the advisory comes back as an
/// error, to be NAKed and decided again, and nothing is written; past the
/// limit the dead letter is written, a second one, saying the check could
/// not be made.
#[tokio::test]
async fn a_search_out_of_time_decides_again_and_past_the_limit_writes() {
    let (_server, bus, agent) = broker().await;
    let published = trigger(&bus, &agent).await;
    let advisory = stamped_at_the_streams_creation(&bus, &exhaust(&bus, &agent).await);
    dispatcher_dead_letter(&bus, &agent, published.stream_seq, published.id).await;
    another_failure(&bus, &agent).await;
    let hurried =
        AdvisoryWatch::without_records(bus.clone()).with_scan_limit(std::time::Duration::ZERO);

    let inside = hurried.handle_advisory(&advisory, 1).await;
    assert!(
        matches!(inside, Err(AdvisoryWatchError::Log(_))),
        "{inside:?}"
    );
    assert_eq!(dead_letters(&bus, &agent).await, 1);

    let past = hurried
        .with_held_verdict_limit(PAST)
        .handle_advisory(&advisory, HELD_VERDICT_MIN_LOOKS)
        .await
        .unwrap();
    assert_eq!(past, AdvisoryOutcome::Emitted);
    assert_eq!(dead_letters(&bus, &agent).await, 2);
    let written = super::test_kit::last_failed(&bus, &agent)
        .await
        .expect("the dead letter written past the limit");
    assert_eq!(
        written.annotations.0.get("duplicate_check"),
        Some(&json!(
            "could not be made: event log read error: reading the log for a dead letter \
             already written took longer than 0ns"
        )),
        "the dead letter says its duplicate check could not be made"
    );
    assert_eq!(
        written
            .annotations
            .0
            .get(crate::dead_letter::DEAD_LETTER_TRIGGER_ID_KEY),
        Some(&json!(published.id.to_string()))
    );
}

/// **A read the broker refuses decides again; it never answers "not
/// there".** The events stream is gone, so the read for a dead letter
/// already written fails at once (not for want of time): inside the limit
/// the advisory comes back as the read's error, to be decided again, and
/// the watch does not go on to write.
#[tokio::test]
async fn a_read_the_broker_refuses_decides_again() {
    let (_server, bus, agent) = broker().await;
    trigger(&bus, &agent).await;
    let advisory = exhaust(&bus, &agent).await;
    bus.jetstream()
        .delete_stream(crate::bus::STREAM_NAME)
        .await
        .expect("delete the events stream");
    let watch = AdvisoryWatch::without_records(bus.clone());

    let outcome = watch.handle_advisory(&advisory, 1).await;

    assert!(
        matches!(outcome, Err(AdvisoryWatchError::Log(_))),
        "the read's error, not the write's: {outcome:?}"
    );
}

/// **An event under the agent this build cannot read is passed over**, the
/// agent's last failure included: after the dispatcher's dead letter, a
/// `failed` event of a kind a later build might add. It is not a dead
/// letter this build wrote, and the watch still finds the one it did.
#[tokio::test]
async fn an_event_this_build_cannot_read_does_not_hide_the_dead_letter() {
    let (_server, bus, agent) = broker().await;
    let published = trigger(&bus, &agent).await;
    let advisory = exhaust(&bus, &agent).await;
    dispatcher_dead_letter(&bus, &agent, published.stream_seq, published.id).await;
    unreadable_failure(&bus, &agent).await;
    let watch = AdvisoryWatch::without_records(bus.clone());

    let outcome = watch.handle_advisory(&advisory, 1).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::AlreadySurfaced);
}

/// **An earlier stream's dead letter written after the new trigger is not
/// its.** As above, but the earlier stream's dead letter at sequence 1 is
/// written after the new id-less trigger landed at sequence 1: a daemon
/// left running over the deleted stream, still finishing its last delivery
/// of the old message. It is newer than the new trigger's message, so the
/// time does not tell the two apart; the incarnation it records does, and
/// the new exhaustion's dead letter is written.
#[tokio::test]
async fn an_earlier_streams_dead_letter_written_after_the_new_trigger_is_not_its() {
    let (server, old_bus, agent) = broker().await;
    let old = untagged_trigger(&old_bus, &agent).await;
    assert_eq!(old, 1);
    old_bus
        .jetstream()
        .delete_stream(TRIGGER_STREAM_NAME)
        .await
        .expect("delete the trigger stream");
    let bus = EventBus::connect(server.url())
        .await
        .expect("reconnect, creating the stream again");
    let new = untagged_trigger(&bus, &agent).await;
    assert_eq!(new, 1, "a recreated stream numbers from 1 again");
    dispatcher_dead_letter(&old_bus, &agent, old, Uuid::now_v7()).await;
    let advisory = exhaust(&bus, &agent).await;
    let watch = AdvisoryWatch::without_records(bus.clone());

    let outcome = watch.handle_advisory(&advisory, 1).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::Emitted);
    assert_eq!(dead_letters(&bus, &agent).await, 2);
}

/// **A dead letter an older build wrote is not taken for an id-less
/// trigger's.** It records no incarnation, so for a trigger published
/// without an id nothing on it ties it to this stream's message at the
/// sequence: the watch writes, a second dead letter at worst.
#[tokio::test]
async fn an_older_builds_dead_letter_is_not_taken_for_an_id_less_triggers() {
    let (_server, bus, agent) = broker().await;
    let seq = untagged_trigger(&bus, &agent).await;
    let advisory = exhaust(&bus, &agent).await;
    marked_dead_letter(&bus, &agent, seq, Uuid::now_v7(), None).await;
    let watch = AdvisoryWatch::without_records(bus.clone());

    let outcome = watch.handle_advisory(&advisory, 1).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::Emitted);
    assert_eq!(dead_letters(&bus, &agent).await, 2);
}

/// **A dead letter an older build wrote is not taken, even on the
/// trigger's id**: it records no incarnation, and every match rests on
/// one. The same id can be published again on a recreated stream, so the
/// id alone does not make it this message's. The watch writes, a second
/// dead letter at worst.
#[tokio::test]
async fn an_older_builds_dead_letter_is_not_taken_even_on_the_triggers_id() {
    let (_server, bus, agent) = broker().await;
    let published = trigger(&bus, &agent).await;
    let advisory = exhaust(&bus, &agent).await;
    marked_dead_letter(&bus, &agent, published.stream_seq, published.id, None).await;
    another_failure(&bus, &agent).await;
    let watch = AdvisoryWatch::without_records(bus.clone());

    let outcome = watch.handle_advisory(&advisory, 1).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::Emitted);
    assert_eq!(dead_letters(&bus, &agent).await, 2);
}

/// **A dead letter naming the trigger on another incarnation is not its**:
/// the same id at the same sequence, recorded for another stream, is the
/// same id published again on a recreated stream, not this message's
/// exhaustion. The watch writes, a second dead letter for that id at worst.
#[tokio::test]
async fn a_dead_letter_naming_the_trigger_on_another_incarnation_is_not_its() {
    let (_server, bus, agent) = broker().await;
    let published = trigger(&bus, &agent).await;
    let advisory = exhaust(&bus, &agent).await;
    marked_dead_letter(
        &bus,
        &agent,
        published.stream_seq,
        published.id,
        Some(bus.trigger_stream_epoch() - 1),
    )
    .await;
    let watch = AdvisoryWatch::without_records(bus.clone());

    let outcome = watch.handle_advisory(&advisory, 1).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::Emitted);
    assert_eq!(dead_letters(&bus, &agent).await, 2);
}

/// **A copy of an advisory whose named dead letter this watch wrote is
/// answered from what it remembers**, past the limit with no time to read
/// the log: the first look writes the dead letter, saying its check could
/// not be made; a copy that waited in the buffer writes none.
#[tokio::test]
async fn a_copy_of_an_advisory_whose_named_dead_letter_this_watch_wrote_writes_none() {
    let (_server, bus, agent) = broker().await;
    trigger(&bus, &agent).await;
    let advisory = stamped_at_the_streams_creation(&bus, &exhaust(&bus, &agent).await);
    let watch = AdvisoryWatch::without_records(bus.clone())
        .with_held_verdict_limit(PAST)
        .with_scan_limit(std::time::Duration::ZERO);

    let first = watch
        .handle_advisory(&advisory, HELD_VERDICT_MIN_LOOKS)
        .await
        .unwrap();
    let copy = watch
        .handle_advisory(&advisory, HELD_VERDICT_MIN_LOOKS + 1)
        .await
        .unwrap();

    assert_eq!(first, AdvisoryOutcome::Emitted);
    assert_eq!(copy, AdvisoryOutcome::AlreadySurfaced);
    assert_eq!(dead_letters(&bus, &agent).await, 1);
}

/// **A dead letter at another sequence is not this trigger's**, though it
/// records this incarnation and the trigger carries no id to tell them
/// apart: the agent's next trigger, also published without an id, is
/// dead-lettered after this one was published. This trigger's own dead
/// letter is written.
#[tokio::test]
async fn a_dead_letter_at_another_sequence_is_not_its() {
    let (_server, bus, agent) = broker().await;
    let seq = untagged_trigger(&bus, &agent).await;
    let advisory = exhaust(&bus, &agent).await;
    dispatcher_dead_letter(&bus, &agent, seq + 1, Uuid::now_v7()).await;
    let watch = AdvisoryWatch::without_records(bus.clone());

    let outcome = watch.handle_advisory(&advisory, 1).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::Emitted);
    assert_eq!(dead_letters(&bus, &agent).await, 2);
}

/// **A watch holding an earlier stream's incarnation does not trust a
/// match that rests on it.** A daemon left running over a deleted and
/// recreated trigger stream holds the old stream's creation time; its
/// dispatcher, finishing its last delivery of the old message at sequence
/// 1, writes that message's dead letter recording the old time, after an
/// id-less trigger of the same agent landed at sequence 1 of the new
/// stream. When the new trigger is exhausted and its advisory reaches the
/// old daemon's watch, the dead letter at sequence 1 records the very
/// incarnation the watch holds. With no trigger id to confirm the match,
/// the watch asks the broker for the stream's current creation time, finds
/// it is not the one it holds, and writes.
#[tokio::test]
async fn a_watch_holding_an_earlier_streams_incarnation_writes_rather_than_trust_it() {
    let (server, old_bus, agent) = broker().await;
    assert_eq!(untagged_trigger(&old_bus, &agent).await, 1);
    old_bus
        .jetstream()
        .delete_stream(TRIGGER_STREAM_NAME)
        .await
        .expect("delete the trigger stream");
    let new_bus = EventBus::connect(server.url())
        .await
        .expect("reconnect, creating the stream again");
    assert_ne!(
        new_bus.trigger_stream_epoch(),
        old_bus.trigger_stream_epoch()
    );
    assert_eq!(untagged_trigger(&new_bus, &agent).await, 1);
    dispatcher_dead_letter(&old_bus, &agent, 1, Uuid::now_v7()).await;
    let advisory = exhaust(&new_bus, &agent).await;
    let stale = AdvisoryWatch::without_records(old_bus.clone());

    let outcome = stale.handle_advisory(&advisory, 1).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::Emitted);
    assert_eq!(dead_letters(&new_bus, &agent).await, 2);
}

/// **A failure that is not a dead letter is not one**, though it carries
/// every annotation a dead letter for this trigger would: only a `failed`
/// event of a trigger that ran out of deliveries counts.
#[tokio::test]
async fn a_failure_that_is_not_a_dead_letter_does_not_stand_for_one() {
    let (_server, bus, agent) = broker().await;
    let seq = untagged_trigger(&bus, &agent).await;
    let advisory = exhaust(&bus, &agent).await;
    let lookalike = failed(&agent, FailureKind::RuntimeError)
        .annotate(DEAD_LETTER_STREAM_SEQ_KEY, json!(seq))
        .annotate(DEAD_LETTER_SOURCE_KEY, json!("inline"))
        .annotate(
            "trigger_stream_epoch",
            json!(bus.trigger_stream_epoch().to_string()),
        );
    bus.publish(&lookalike)
        .await
        .expect("a failure that looks like one");
    let watch = AdvisoryWatch::without_records(bus.clone());

    let outcome = watch.handle_advisory(&advisory, 1).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::Emitted);
    assert_eq!(dead_letters(&bus, &agent).await, 1);
}
