//! What the watch's broker-backed tests share: exhausting a trigger so the
//! broker emits its MAX_DELIVERIES advisory, stamping that advisory at the
//! stream's creation, writing a `failed` event this build cannot read, and
//! reading back the dead letter written. Used by the watch's own tests and
//! by the dispatcher's dead-letter watch tests.

use std::time::Duration;

use async_nats::jetstream::stream::RawMessageErrorKind;
use futures::StreamExt;
use serde_json::json;
use uuid::Uuid;

use crate::agent::AgentId;
use crate::bus::{ADVISORY_STREAM_NAME, EventBus, STREAM_NAME, TRIGGER_STREAM_NAME};
use crate::events::{
    Event, EventPayload, FailedPayload, FailureKind, FailurePhase, InvocationTotals, subjects,
};

/// A limit an advisory stamped at the stream's creation is past by the time
/// its trigger is exhausted: that takes two one-second ack windows.
pub(crate) const PAST: Duration = Duration::from_millis(500);

/// Exhaust every trigger published for `agent` on a scratch durable of the
/// trigger stream that never acks, with a one-second ack window and two
/// deliveries, and return the broker's MAX_DELIVERIES advisory as the
/// watch's stream captured it. The advisory names the message, not the
/// durable that ran out, so a scratch durable stands for the dispatcher's.
pub(crate) async fn exhaust(bus: &EventBus, agent: &str) -> Vec<u8> {
    let name = format!("exhaust-{}", Uuid::now_v7().simple());
    let consumer = bus
        .jetstream()
        .get_stream(TRIGGER_STREAM_NAME)
        .await
        .expect("the trigger stream")
        .create_consumer(async_nats::jetstream::consumer::pull::Config {
            durable_name: Some(name.clone()),
            ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
            filter_subject: subjects::trigger(agent),
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
    let advisories = bus
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

/// The advisory, stamped at the moment the trigger stream was created: as
/// old as an advisory of this incarnation can be, a few seconds by the time
/// a test has exhausted its trigger, so past [`PAST`] without sleeping.
pub(crate) fn stamped_at_the_streams_creation(bus: &EventBus, advisory: &[u8]) -> Vec<u8> {
    let mut stamped: serde_json::Value = serde_json::from_slice(advisory).unwrap();
    let created = chrono::DateTime::from_timestamp_nanos(bus.trigger_stream_epoch());
    stamped["timestamp"] = json!(created.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true));
    serde_json::to_vec(&stamped).unwrap()
}

/// Publish under `agent` a `failed` event this build cannot read: a dead
/// letter but for its failure kind, one a later build might add.
pub(crate) async fn unreadable_failure(bus: &EventBus, agent: &str) {
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
    let payload = EventPayload::Failed(FailedPayload {
        error_kind: FailureKind::TriggerExhausted,
        error_message: "a failure this build cannot read".to_string(),
        phase: FailurePhase::Setup,
        partial_totals: InvocationTotals::default(),
        detail: None,
    });
    let event = if agent == AgentId::SYSTEM_STR {
        Event::system(Uuid::now_v7(), payload)
    } else {
        Event::new(AgentId::new(agent).unwrap(), Uuid::now_v7(), payload)
    };
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
        .publish(subjects::agent_failed(agent), payload.into())
        .await
        .expect("publish")
        .await
        .expect("acked");
}

/// The last `failed` event under `agent`, or `None` when the broker says
/// there is none. Any other answer fails the test: read as "none", a read
/// that failed would pass every assertion that nothing was written.
pub(crate) async fn last_failed(bus: &EventBus, agent: &str) -> Option<Event> {
    let read = bus
        .jetstream()
        .get_stream(STREAM_NAME)
        .await
        .expect("the event stream")
        .get_last_raw_message_by_subject(&subjects::agent_failed(agent))
        .await;
    match read {
        Ok(raw) => Some(serde_json::from_slice(&raw.payload).expect("the event parses")),
        Err(err) if err.kind() == RawMessageErrorKind::NoMessageFound => None,
        Err(err) => panic!("reading the last `failed` event under {agent}: {err}"),
    }
}
