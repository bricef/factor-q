//! Unit tests for [`super`]. Extracted from the parent module so the
//! file that ships is the file you read (#390); `super::*` keeps the
//! same access it had inline.

use super::*;
use crate::agent::AgentId;

#[test]
fn url_credentials_parses_token_user_pass_and_bare_forms() {
    assert_eq!(
        url_credentials("nats://fq-dev-token@127.0.0.1:4222"),
        Some(("fq-dev-token".to_string(), None)),
        "bare userinfo is a token"
    );
    assert_eq!(
        url_credentials("nats://fq:secret@localhost:4222"),
        Some(("fq".to_string(), Some("secret".to_string()))),
        "user:pass form"
    );
    assert_eq!(url_credentials("nats://127.0.0.1:4222"), None);
    assert_eq!(url_credentials("not a url"), None);
}
use crate::events::{
    ConfigSnapshot, EventPayload, SandboxSnapshot, TriggerSource, TriggeredPayload,
};
use serde_json::json;
use uuid::Uuid;

fn aid(s: &str) -> AgentId {
    AgentId::new(s).expect("test agent id must be valid")
}

fn sample_event(agent_id: &str) -> Event {
    Event::new(
        aid(agent_id),
        Uuid::now_v7(),
        EventPayload::Triggered(TriggeredPayload {
            trigger_id: None,
            trigger_source: TriggerSource::Manual,
            trigger_subject: None,
            trigger_payload: json!({"input": "hello"}),
            config_snapshot: ConfigSnapshot {
                name: agent_id.to_string(),
                model: "claude-haiku".to_string(),
                system_prompt: "Test.".to_string(),
                tools: vec![],
                sandbox: SandboxSnapshot {
                    fs_read: vec![],
                    fs_write: vec![],
                    network: vec![],
                    env: vec![],
                    exec_cwd: vec![],
                },
                budget: None,
                ..Default::default()
            },
        }),
    )
}

/// Round-trips a publish through a private `nats-server` this test spawns
/// (#233) — no shared broker, no skip.
#[tokio::test]
async fn publish_and_subscribe_round_trip() {
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    let bus = EventBus::connect(&url).await.expect("connect to NATS");
    let agent_id = format!("bus-test-{}", Uuid::now_v7().simple());
    let event = sample_event(&agent_id);
    let expected_id = event.envelope.event_id;

    let mut subscriber = bus
        .subscribe(format!("fq.agent.{agent_id}.>"))
        .await
        .expect("subscribe");

    // Give the subscription a moment to register before publishing.
    tokio::time::sleep(Duration::from_millis(50)).await;
    bus.publish(&event).await.expect("publish");

    let received = tokio::time::timeout(Duration::from_secs(2), subscriber.next())
        .await
        .expect("timeout waiting for event")
        .expect("stream closed")
        .expect("deserialise");

    assert_eq!(received.envelope.event_id, expected_id);
    assert_eq!(received.envelope.agent_id.as_str(), agent_id);
}

/// The pre-flight guard (issue #4) rejects a payload larger than
/// the server's advertised `max_payload` with a clear, attributable
/// error, and never reaches NATS. Exercised against the pure seam
/// so it needs no live server.
#[test]
fn payload_guard_rejects_oversized_and_accepts_within_limit() {
    // Strictly over the limit -> rejected with size and limit.
    match check_payload_size(1_048_577, 1_048_576) {
        Err(BusError::PayloadTooLarge { size, limit }) => {
            assert_eq!(size, 1_048_577);
            assert_eq!(limit, 1_048_576);
        }
        other => panic!("expected PayloadTooLarge, got {other:?}"),
    }
    // Exactly at the limit and below are accepted (NATS accepts a
    // body equal to max_payload; only strictly-greater is a
    // violation).
    assert!(check_payload_size(1_048_576, 1_048_576).is_ok());
    assert!(check_payload_size(0, 1_048_576).is_ok());
}

/// End-to-end at the serialisation boundary: a real oversized
/// event (a system prompt padded past a small limit) serialises
/// to more bytes than the limit, and the guard rejects it cleanly
/// with the actual serialised size — no NATS round-trip.
#[test]
fn oversized_event_is_rejected_by_the_guard() {
    let limit = 1_024usize;
    let mut event = sample_event("guard-test");
    if let EventPayload::Triggered(ref mut p) = event.payload {
        p.config_snapshot.system_prompt = "x".repeat(4_096);
    } else {
        panic!("sample_event should be a Triggered payload");
    }
    let payload = serde_json::to_vec(&event).expect("serialise event");
    assert!(
        payload.len() > limit,
        "test event must exceed the limit to be meaningful"
    );
    match check_payload_size(payload.len(), limit) {
        Err(BusError::PayloadTooLarge { size, limit: l }) => {
            assert_eq!(size, payload.len());
            assert_eq!(l, limit);
        }
        other => panic!("expected PayloadTooLarge, got {other:?}"),
    }
}

/// Annotations live on the wire — the barrier (envelope-refactor
/// plan step 4) is at the consumer-context boundary, not at the
/// bus. A producer can attach annotations to a published event
/// and a subscriber that deserialises the same event sees them
/// intact; only `Event::for_consumer_context` strips them when
/// building a downstream agent's prompt input.
#[tokio::test]
async fn annotations_preserved_through_publish_round_trip() {
    use crate::events::annotation_keys;
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();
    let bus = EventBus::connect(&url).await.expect("connect to NATS");
    let agent_id = format!("bus-anno-{}", Uuid::now_v7().simple());
    let event = sample_event(&agent_id)
        .annotate(annotation_keys::NOTES, json!("hi"))
        .annotate(annotation_keys::CONFIDENCE, json!(0.8));

    let mut subscriber = bus
        .subscribe(format!("fq.agent.{agent_id}.>"))
        .await
        .expect("subscribe");
    tokio::time::sleep(Duration::from_millis(50)).await;
    bus.publish(&event).await.expect("publish");

    let received = tokio::time::timeout(Duration::from_secs(2), subscriber.next())
        .await
        .expect("timeout waiting for event")
        .expect("stream closed")
        .expect("deserialise");

    assert_eq!(received.annotations.0.len(), 2);
    assert_eq!(
        received.annotations.0.get(annotation_keys::NOTES),
        Some(&json!("hi"))
    );
    assert_eq!(
        received.annotations.0.get(annotation_keys::CONFIDENCE),
        Some(&json!(0.8))
    );
}
