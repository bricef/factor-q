//! A redelivered advisory writes the dead letter that names no trigger
//! once (issue 1066), against a real broker.
//!
//! The watch's check against writing one exhaustion twice looked under the
//! dead letter's agent, so it never ran for a dead letter that names no
//! trigger, which goes under the system sentinel: a watch that stopped
//! between publishing such a dead letter and acking its advisory wrote it
//! again when the advisory came back. These tests hand the real
//! [`AdvisoryWatch::handle_advisory`] the same advisory more than once.

use futures::StreamExt;
use serde_json::json;

use super::test_kit::{PAST, exhaust, last_failed, stamped_at_the_streams_creation};
use super::*;
use crate::agent::AgentId;
use crate::dead_letter::from_event;
use crate::event_tail::TailedMessage;
use crate::events::subjects;
use crate::events::{
    Event, EventPayload, FailedPayload, FailureKind, FailurePhase, InvocationTotals,
};
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

/// **The same advisory handled twice writes one dead letter.** The second
/// handling is what a watch that stopped after the publish and before the
/// ack meets when the advisory comes back.
#[tokio::test]
async fn the_same_advisory_handled_twice_writes_one_nameless_dead_letter() {
    let (_server, bus, agent) = broker().await;
    let advisory = gone(&bus, &agent).await;
    let watch = AdvisoryWatch::without_records(bus.clone());

    let first = watch.handle_advisory(&advisory, 1).await.unwrap();
    let again = watch.handle_advisory(&advisory, 2).await.unwrap();

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

/// Publish under the system sentinel a `failed` event this build cannot
/// read: its failure kind is one a later build might add.
async fn unreadable_failure(bus: &EventBus) {
    fn replace(value: &mut serde_json::Value, key: &str, new: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(slot) = map.get_mut(key) {
                    *slot = new.clone();
                    return true;
                }
                map.values_mut().any(|v| replace(v, key, new))
            }
            _ => false,
        }
    }
    let event = Event::system(
        Uuid::now_v7(),
        EventPayload::Failed(FailedPayload {
            error_kind: FailureKind::TriggerExhausted,
            error_message: "a failure this build cannot read".to_string(),
            phase: FailurePhase::Setup,
            partial_totals: InvocationTotals::default(),
            detail: None,
        }),
    );
    let mut wire = serde_json::to_value(&event).unwrap();
    assert!(replace(
        &mut wire,
        "error_kind",
        &json!("a_kind_a_later_build_added")
    ));
    let payload = serde_json::to_vec(&wire).unwrap();
    assert!(
        Event::from_wire(&payload).is_err(),
        "this build cannot read it"
    );
    bus.jetstream()
        .publish(subjects::agent_failed(AgentId::SYSTEM_STR), payload.into())
        .await
        .expect("publish")
        .await
        .expect("acked");
}

/// **An event under the system sentinel this build cannot read is skipped**,
/// not taken as a failed read: one sits between the advisory and its dead
/// letter, and on the advisory's redelivery the dead letter is still found.
#[tokio::test]
async fn an_event_this_build_cannot_read_does_not_stop_the_check() {
    let (_server, bus, agent) = broker().await;
    let advisory = gone(&bus, &agent).await;
    unreadable_failure(&bus).await;
    let watch = AdvisoryWatch::without_records(bus.clone());

    let first = watch.handle_advisory(&advisory, 1).await.unwrap();
    let again = watch.handle_advisory(&advisory, 2).await.unwrap();

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
    let again = watch.handle_advisory(&advisory, 2).await.unwrap();

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
