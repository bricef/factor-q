//! A redelivered advisory writes the dead letter that names no trigger
//! once (issue 1066), against a real broker.
//!
//! The watch's check against writing one exhaustion twice looked under the
//! dead letter's agent, so it never ran for a dead letter that names no
//! trigger, which goes under the system sentinel: a watch that stopped
//! between publishing such a dead letter and acking its advisory wrote it
//! again when the advisory came back. These tests hand the real
//! [`AdvisoryWatch::handle_advisory`] the same advisory more than once. A
//! watch remembers what it wrote since it started, so a test that pins the
//! read of the log hands the advisory back to a watch made afresh, as after
//! a restart ([`restarted`]).

use futures::StreamExt;
use serde_json::json;

use super::test_kit::{
    PAST, exhaust, last_failed, stamped_at_the_streams_creation, unreadable_failure,
};
use super::*;
use crate::agent::AgentId;
use crate::dead_letter::from_event;
use crate::event_tail::TailedMessage;
use crate::events::subjects;
use crate::test_support::nats::{NatsServer, test_nats};

/// A broker, a bus on it, and an agent name of the test's own.
async fn broker() -> (NatsServer, EventBus, String) {
    let server = test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let agent = format!("once-{}", Uuid::now_v7().simple());
    (server, bus, agent)
}

/// Publish a trigger for `agent`, exhaust it, and delete its message: the
/// broker's advisory for it names a trigger the stream no longer holds.
async fn gone(bus: &EventBus, agent: &str) -> Vec<u8> {
    let published = bus
        .publish_trigger(&AgentId::new(agent).unwrap(), &json!({ "input": "work" }))
        .await
        .expect("publish the trigger");
    let advisory = exhaust(bus, agent).await;
    assert!(
        bus.jetstream()
            .get_stream(TRIGGER_STREAM_NAME)
            .await
            .expect("the trigger stream")
            .delete_message(published.stream_seq)
            .await
            .expect("delete the trigger's message")
    );
    advisory
}

/// How many dead letters the system sentinel holds. An event this build
/// cannot read is passed over; the last on the subject must be one it can.
async fn nameless_dead_letters(bus: &EventBus) -> usize {
    let subject = subjects::agent_failed(AgentId::SYSTEM_STR);
    let tip = bus
        .last_event_seq_matching(&subject)
        .await
        .expect("the subject's tip");
    if tip == 0 {
        return 0;
    }
    let mut events = bus.events_from(&subject, 1).await.expect("a tail");
    let mut written = 0;
    while let Some(next) = events.next().await {
        let Ok(TailedMessage { seq, event }) = next else {
            continue;
        };
        if event.as_ref().and_then(from_event).is_some() {
            written += 1;
        }
        if seq >= tip {
            break;
        }
    }
    written
}

/// A watch made afresh on `bus`, as the daemon makes one when it starts
/// again: it remembers nothing it wrote, so it can only read the log.
fn restarted(bus: &EventBus) -> AdvisoryWatch {
    AdvisoryWatch::without_records(bus.clone())
}

/// **The same advisory handled twice writes one dead letter.** The second
/// handling is what a watch that stopped after the publish and before the
/// ack meets when the advisory comes back.
#[tokio::test]
async fn the_same_advisory_handled_twice_writes_one_nameless_dead_letter() {
    let (_server, bus, agent) = broker().await;
    let advisory = gone(&bus, &agent).await;
    let watch = AdvisoryWatch::without_records(bus.clone());

    let first = watch.handle_advisory(&advisory, 1).await.unwrap();
    let again = restarted(&bus).handle_advisory(&advisory, 2).await.unwrap();

    assert_eq!(first, AdvisoryOutcome::Emitted);
    assert_eq!(again, AdvisoryOutcome::AlreadySurfaced);
    assert_eq!(nameless_dead_letters(&bus).await, 1);
}

/// **Two advisories for two triggers gone from the stream write two dead
/// letters**: the check tells advisories apart, not just forms.
#[tokio::test]
async fn two_advisories_for_triggers_gone_from_the_stream_write_two() {
    let (_server, bus, agent) = broker().await;
    let one = gone(&bus, &agent).await;
    let two = gone(&bus, &format!("{agent}-two")).await;
    let watch = AdvisoryWatch::without_records(bus.clone());

    assert_eq!(
        watch.handle_advisory(&one, 1).await.unwrap(),
        AdvisoryOutcome::Emitted
    );
    assert_eq!(
        watch.handle_advisory(&two, 1).await.unwrap(),
        AdvisoryOutcome::Emitted
    );

    assert_eq!(nameless_dead_letters(&bus).await, 2);
}

/// **An advisory that comes back after another nameless dead letter was
/// written is still written once.** After a restart the watch can decide
/// other advisories before a redelivered one, so the dead letter it wrote
/// for that one need not be the system sentinel's last; nor its first, so
/// both advisories come back.
#[tokio::test]
async fn a_redelivered_advisory_is_written_once_though_another_came_between() {
    let (_server, bus, agent) = broker().await;
    let one = gone(&bus, &agent).await;
    let two = gone(&bus, &format!("{agent}-two")).await;
    let watch = AdvisoryWatch::without_records(bus.clone());
    assert_eq!(
        watch.handle_advisory(&one, 1).await.unwrap(),
        AdvisoryOutcome::Emitted
    );
    assert_eq!(
        watch.handle_advisory(&two, 1).await.unwrap(),
        AdvisoryOutcome::Emitted
    );

    let watch = restarted(&bus);
    let again = watch.handle_advisory(&one, 2).await.unwrap();
    let last = watch.handle_advisory(&two, 2).await.unwrap();

    assert_eq!(again, AdvisoryOutcome::AlreadySurfaced);
    assert_eq!(last, AdvisoryOutcome::AlreadySurfaced);
    assert_eq!(nameless_dead_letters(&bus).await, 2);
}

/// **A dead letter that names the trigger may follow a nameless one for the
/// same advisory.** Past the limit with the trigger stream gone, the
/// advisory's dead letter names no trigger. The stream then comes back
/// with the trigger on it (to this watch, which keeps the epoch it
/// connected with, one stream that was briefly unreadable), and the
/// advisory is redelivered: the read now names the trigger, and that dead
/// letter is written, since it is the one that can be requeued. The check
/// by the advisory's id holds back only a second nameless one.
#[tokio::test]
async fn a_dead_letter_naming_the_trigger_may_follow_a_nameless_one() {
    let (server, bus, agent) = broker().await;
    let published = bus
        .publish_trigger(&AgentId::new(&agent).unwrap(), &json!({ "input": "work" }))
        .await
        .expect("publish the trigger");
    let advisory = stamped_at_the_streams_creation(&bus, &exhaust(&bus, &agent).await);
    bus.jetstream()
        .delete_stream(TRIGGER_STREAM_NAME)
        .await
        .expect("delete the trigger stream");
    let watch = AdvisoryWatch::without_records(bus.clone()).with_held_verdict_limit(PAST);
    assert_eq!(
        watch
            .handle_advisory(&advisory, HELD_VERDICT_MIN_LOOKS)
            .await
            .unwrap(),
        AdvisoryOutcome::Emitted
    );
    assert_eq!(nameless_dead_letters(&bus).await, 1);
    let again = EventBus::connect(server.url()).await.expect("reconnect");
    again
        .publish_trigger_named(
            &AgentId::new(&agent).unwrap(),
            published.id,
            &json!({ "input": "work" }),
        )
        .await
        .expect("the trigger is back on the stream");

    let redelivered = watch
        .handle_advisory(&advisory, HELD_VERDICT_MIN_LOOKS + 1)
        .await
        .unwrap();

    assert_eq!(redelivered, AdvisoryOutcome::Emitted);
    let named = last_failed(&bus, &agent)
        .await
        .expect("dead-lettered under the trigger's agent");
    assert_eq!(
        from_event(&named).expect("a dead letter").unattributed,
        None
    );
    assert_eq!(nameless_dead_letters(&bus).await, 1);
}

/// **An event under the system sentinel this build cannot read is skipped**,
/// not taken as a failed read: one sits between the advisory and its dead
/// letter, and on the advisory's redelivery the dead letter is still found.
#[tokio::test]
async fn an_event_this_build_cannot_read_does_not_stop_the_check() {
    let (_server, bus, agent) = broker().await;
    let advisory = gone(&bus, &agent).await;
    unreadable_failure(&bus, AgentId::SYSTEM_STR).await;
    let watch = AdvisoryWatch::without_records(bus.clone());

    let first = watch.handle_advisory(&advisory, 1).await.unwrap();
    let again = restarted(&bus).handle_advisory(&advisory, 2).await.unwrap();

    assert_eq!(first, AdvisoryOutcome::Emitted);
    assert_eq!(again, AdvisoryOutcome::AlreadySurfaced);
    assert_eq!(nameless_dead_letters(&bus).await, 1);
}

/// **A trigger whose subject names no legal agent is written once too.** Its
/// message is read, so its dead letter records the subject and payload, and
/// says no "why"; but it goes under the system sentinel, so it carries the
/// advisory's id like the nameless forms, and a redelivery finds it.
#[tokio::test]
async fn a_trigger_whose_subject_names_no_agent_is_written_once() {
    let (_server, bus, agent) = broker().await;
    let not_an_agent = format!("{agent}.not-an-agent");
    bus.jetstream()
        .publish(
            subjects::trigger(&not_an_agent),
            serde_json::to_vec(&json!({ "input": "work" }))
                .unwrap()
                .into(),
        )
        .await
        .expect("publish")
        .await
        .expect("acked");
    let advisory = exhaust(&bus, &not_an_agent).await;
    let watch = AdvisoryWatch::without_records(bus.clone());

    let first = watch.handle_advisory(&advisory, 1).await.unwrap();
    let again = restarted(&bus).handle_advisory(&advisory, 2).await.unwrap();

    assert_eq!(first, AdvisoryOutcome::Emitted);
    assert_eq!(again, AdvisoryOutcome::AlreadySurfaced);
    assert_eq!(nameless_dead_letters(&bus).await, 1);
    let event = last_failed(&bus, AgentId::SYSTEM_STR)
        .await
        .expect("under the system sentinel");
    let dead = from_event(&event).expect("a dead letter");
    assert_eq!(dead.trigger_subject, subjects::trigger(&not_an_agent));
    assert_eq!(dead.unattributed, None);
}

/// **The read starts at the advisory.** A dead letter for an advisory cannot
/// be older than the advisory, so the `failed` events from before it are
/// not read: one carrying the same id from before the advisory was emitted
/// (which no watch writes) is not found, and the dead letter is written.
#[tokio::test]
async fn the_read_starts_at_the_advisory() {
    let (_server, bus, agent) = broker().await;
    let earlier = gone(&bus, &agent).await;
    let watch = AdvisoryWatch::without_records(bus.clone());
    assert_eq!(
        watch.handle_advisory(&earlier, 1).await.unwrap(),
        AdvisoryOutcome::Emitted
    );
    let later = {
        let mut later: serde_json::Value =
            serde_json::from_slice(&gone(&bus, &format!("{agent}-two")).await).unwrap();
        let earlier: serde_json::Value = serde_json::from_slice(&earlier).unwrap();
        later["id"] = earlier["id"].clone();
        serde_json::to_vec(&later).unwrap()
    };

    let outcome = restarted(&bus).handle_advisory(&later, 1).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::Emitted);
    assert_eq!(nameless_dead_letters(&bus).await, 2);
}

/// **The read starts at the advisory though newer events follow it**, so
/// the read itself, not only the look at the subject's last event, starts
/// there: as above, with another dead letter written after the later
/// advisory, so the subject's last event is newer than it and the whole
/// span is read. The earlier dead letter carrying the same id is still not
/// found, and the later advisory's is written.
#[tokio::test]
async fn the_read_starts_at_the_advisory_though_newer_events_follow() {
    let (_server, bus, agent) = broker().await;
    let earlier = gone(&bus, &agent).await;
    let watch = AdvisoryWatch::without_records(bus.clone());
    assert_eq!(
        watch.handle_advisory(&earlier, 1).await.unwrap(),
        AdvisoryOutcome::Emitted
    );
    let later = {
        let mut later: serde_json::Value =
            serde_json::from_slice(&gone(&bus, &format!("{agent}-two")).await).unwrap();
        let earlier: serde_json::Value = serde_json::from_slice(&earlier).unwrap();
        later["id"] = earlier["id"].clone();
        serde_json::to_vec(&later).unwrap()
    };
    let newer = gone(&bus, &format!("{agent}-three")).await;
    assert_eq!(
        watch.handle_advisory(&newer, 1).await.unwrap(),
        AdvisoryOutcome::Emitted
    );

    let outcome = restarted(&bus).handle_advisory(&later, 1).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::Emitted);
    assert_eq!(nameless_dead_letters(&bus).await, 3);
}

/// **A read the broker answers with an error decides again**, as one out of
/// time does: the events stream is gone, so the read of the last `failed`
/// event is answered at once with an error. Inside
/// the limit the advisory comes back as that error, to be NAKed and decided
/// again; the watch does not take it as "not written" and go on to publish.
#[tokio::test]
async fn a_read_the_broker_answers_with_an_error_decides_again() {
    let (_server, bus, agent) = broker().await;
    let advisory = gone(&bus, &agent).await;
    bus.jetstream()
        .delete_stream(crate::bus::STREAM_NAME)
        .await
        .expect("delete the events stream");
    let watch = AdvisoryWatch::without_records(bus.clone());

    let outcome = watch.handle_advisory(&advisory, 1).await;

    assert!(
        matches!(outcome, Err(AdvisoryWatchError::Log(_))),
        "the read's error, not the publish's: {outcome:?}"
    );
}

/// **An advisory whose id is empty has none.** Two advisories that each
/// carry `"id": ""` are two exhaustions, and each gets its dead letter; the
/// empty id is not written on either, since it tells nothing apart.
#[tokio::test]
async fn advisories_with_an_empty_id_are_each_written() {
    let (_server, bus, agent) = broker().await;
    let emptied = |advisory: Vec<u8>| {
        let mut advisory: serde_json::Value = serde_json::from_slice(&advisory).unwrap();
        advisory["id"] = json!("");
        serde_json::to_vec(&advisory).unwrap()
    };
    let one = emptied(gone(&bus, &agent).await);
    let two = emptied(gone(&bus, &format!("{agent}-two")).await);
    let watch = AdvisoryWatch::without_records(bus.clone());

    let first = watch.handle_advisory(&one, 1).await.unwrap();
    let second = watch.handle_advisory(&two, 1).await.unwrap();

    assert_eq!(first, AdvisoryOutcome::Emitted);
    assert_eq!(second, AdvisoryOutcome::Emitted);
    assert_eq!(nameless_dead_letters(&bus).await, 2);
    let last = last_failed(&bus, AgentId::SYSTEM_STR)
        .await
        .expect("the second dead letter");
    assert_eq!(
        last.annotations
            .0
            .get(crate::dead_letter::DEAD_LETTER_ADVISORY_ID_KEY),
        None
    );
}

/// **A copy of an advisory this watch wrote is answered from what it
/// remembers.** Past the limit a check that cannot be made writes the dead
/// letter; copies of the same advisory that were waiting in the watch's
/// buffer meanwhile each met the same check and wrote one more. The watch
/// remembers the advisories it wrote since it started, so a copy is
/// answered before the log is read, here given no time to read it at all.
#[tokio::test]
async fn a_copy_of_an_advisory_this_watch_wrote_is_answered_from_memory() {
    let (_server, bus, agent) = broker().await;
    let advisory = stamped_at_the_streams_creation(&bus, &gone(&bus, &agent).await);
    let watch = AdvisoryWatch::without_records(bus.clone())
        .with_held_verdict_limit(PAST)
        .with_scan_limit(std::time::Duration::ZERO);

    let first = watch
        .handle_advisory(&advisory, HELD_VERDICT_MIN_LOOKS)
        .await
        .unwrap();
    let copies = [
        watch
            .handle_advisory(&advisory, HELD_VERDICT_MIN_LOOKS + 1)
            .await
            .unwrap(),
        watch
            .handle_advisory(&advisory, HELD_VERDICT_MIN_LOOKS + 2)
            .await
            .unwrap(),
    ];

    assert_eq!(first, AdvisoryOutcome::Emitted);
    assert_eq!(
        copies,
        [
            AdvisoryOutcome::AlreadySurfaced,
            AdvisoryOutcome::AlreadySurfaced
        ]
    );
    assert_eq!(nameless_dead_letters(&bus).await, 1);
}

/// **Only the broker's "no message" is "none written".** Each check reads
/// the last `failed` event on the subject it reads to know where to stop;
/// an answer that is an error of any other kind is an error, never "there
/// are none".
#[test]
fn only_no_message_is_none() {
    use async_nats::jetstream::stream::{RawMessageError, RawMessageErrorKind};
    assert!(matches!(
        surfaced::last_message(Err(RawMessageError::new(
            RawMessageErrorKind::NoMessageFound
        ))),
        Ok(None)
    ));
    assert!(matches!(
        surfaced::last_message(Err(RawMessageError::new(RawMessageErrorKind::Other))),
        Err(AdvisoryWatchError::Log(_))
    ));
}
