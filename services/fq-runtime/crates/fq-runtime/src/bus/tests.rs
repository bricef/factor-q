//! Unit tests for [`super`]. Extracted from the parent module so the
//! file that ships is the file you read (#390); `super::*` keeps the
//! same access it had inline.

use super::*;
use crate::agent::AgentId;

/// The token reaches the broker as a connect option, never as URL
/// userinfo (#540): against a token-authenticated private broker the
/// tokened connect succeeds and the anonymous one is refused. Both
/// attempts are bounded, because "refused" and "never answers" are the
/// same verdict here and neither may hang the suite.
#[tokio::test]
async fn connect_with_token_authenticates_against_a_token_broker() {
    let token = format!("tok-{}", Uuid::now_v7().simple());
    let server = crate::test_support::nats::NatsServer::start_with_token(&token);
    let url = server.url().to_string();
    assert!(
        !url.contains(&token),
        "the broker's client URL must stay credential-free: {url}"
    );

    let bound = std::time::Duration::from_secs(10);
    let anonymous = tokio::time::timeout(bound, EventBus::connect(&url)).await;
    assert!(
        !matches!(anonymous, Ok(Ok(_))),
        "an anonymous connect must be refused by a token broker"
    );

    let bus = tokio::time::timeout(bound, EventBus::connect_with_token(&url, Some(&token)))
        .await
        .expect("tokened connect timed out")
        .expect("tokened connect");
    // Not just a socket: the streams were ensured, so JetStream accepted
    // the authenticated client too.
    bus.publish(&sample_event(&format!(
        "bus-token-{}",
        Uuid::now_v7().simple()
    )))
    .await
    .expect("publish over the tokened connection");
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

/// Repeating an event publish is acknowledged as a duplicate and leaves one
/// stream entry because the envelope identity is the JetStream message id.
#[tokio::test]
async fn publishing_the_same_event_twice_is_deduplicated() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url())
        .await
        .expect("connect to NATS");
    let event = sample_event(&format!("dedup-{}", Uuid::now_v7().simple()));

    let first = bus.publish(&event).await.expect("first publish");
    let second = bus.publish(&event).await.expect("duplicate publish");
    assert_eq!(second, first, "a duplicate ack names the original sequence");

    let mut headers = async_nats::HeaderMap::new();
    headers.insert(
        async_nats::header::NATS_MESSAGE_ID,
        event.envelope.event_id.to_string(),
    );
    let ack = bus
        .jetstream()
        .publish_with_headers(
            event.subject(),
            headers,
            Bytes::from(serde_json::to_vec(&event).expect("serialise event")),
        )
        .await
        .expect("publish duplicate")
        .await
        .expect("duplicate ack");
    assert!(
        ack.duplicate,
        "JetStream must report the repeated id as duplicate"
    );

    let info = bus
        .jetstream()
        .get_stream(STREAM_NAME)
        .await
        .expect("event stream")
        .info()
        .await
        .expect("stream info")
        .clone();
    assert_eq!(info.state.messages, 1, "only the original is stored");
}

/// Repeating a named trigger publish uses the trigger id as the deduplication
/// key, so only the original trigger occupies the stream.
#[tokio::test]
async fn publishing_the_same_named_trigger_twice_is_deduplicated() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url())
        .await
        .expect("connect to NATS");
    let id = Uuid::now_v7();
    let agent = aid("dedup-trigger");

    let first = bus
        .publish_trigger_named(&agent, id, &json!({"work": 1}))
        .await
        .expect("first trigger");
    let second = bus
        .publish_trigger_named(&agent, id, &json!({"work": 1}))
        .await
        .expect("duplicate trigger");
    assert_eq!(second.stream_seq, first.stream_seq);

    let info = bus
        .jetstream()
        .get_stream(TRIGGER_STREAM_NAME)
        .await
        .expect("trigger stream")
        .info()
        .await
        .expect("stream info")
        .clone();
    assert_eq!(info.state.messages, 1, "only the original is stored");
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

/// A changed maintenance-stream config reaches a broker that already
/// has the stream (#187's class).
///
/// The pre-existing stream is made by connecting once — which is how
/// every deployed broker got its — and then moving `max_age` out from
/// under it, standing in for the build whose constant was different.
/// The second connect must put it back: with `get_or_create_stream`
/// alone this assertion fails, because the server keeps the config it
/// was created with.
#[tokio::test]
async fn ensuring_the_maintenance_stream_applies_a_changed_config_to_an_existing_stream() {
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    let first = EventBus::connect(&url).await.expect("connect to NATS");
    let stale_max_age = Duration::from_secs(60);
    assert_ne!(
        stale_max_age, DEFAULT_MAINTENANCE_MAX_AGE,
        "the stand-in config has to differ from the one under test"
    );
    first
        .jetstream()
        .update_stream(&jetstream::stream::Config {
            name: MAINTENANCE_STREAM_NAME.to_string(),
            subjects: vec![ALL_MAINTENANCE.to_string()],
            retention: jetstream::stream::RetentionPolicy::Limits,
            storage: jetstream::stream::StorageType::File,
            max_age: stale_max_age,
            ..Default::default()
        })
        .await
        .expect("age the existing stream out of date");

    let second = EventBus::connect(&url).await.expect("reconnect to NATS");
    let info = second
        .jetstream()
        .get_stream(MAINTENANCE_STREAM_NAME)
        .await
        .expect("maintenance stream")
        .info()
        .await
        .expect("stream info")
        .clone();
    assert_eq!(
        info.config.max_age, DEFAULT_MAINTENANCE_MAX_AGE,
        "connecting must apply this build's maintenance stream config to an existing stream"
    );
    assert_eq!(
        info.config.subjects,
        vec![ALL_MAINTENANCE.to_string()],
        "the subject set is part of that config"
    );
}

/// Reconciliation heals subjects owned by factor-q without resetting broker
/// fields that operators own.
#[tokio::test]
async fn event_stream_reconciliation_heals_subjects_without_clobbering_unmanaged_fields() {
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    let first = EventBus::connect(&url).await.expect("connect to NATS");
    let mut stream = first
        .jetstream()
        .get_stream(STREAM_NAME)
        .await
        .expect("event stream");
    let mut stale = stream.info().await.expect("stream info").config.clone();
    stale.subjects = vec!["fq.agent.>".to_string(), "fq.system.>".to_string()];
    stale.duplicate_window = Duration::from_secs(60);
    stale.description = Some("operator-owned description".to_string());
    first
        .jetstream()
        .update_stream(&stale)
        .await
        .expect("install stale, operator-tuned config");

    let second = EventBus::connect(&url).await.expect("reconnect to NATS");
    let mut stream = second
        .jetstream()
        .get_stream(STREAM_NAME)
        .await
        .expect("event stream");
    let config = stream.info().await.expect("stream info").config.clone();
    assert_eq!(
        config.subjects,
        EVENT_STREAM_SUBJECTS
            .iter()
            .map(|subject| subject.to_string())
            .collect::<Vec<_>>(),
        "startup must heal a subject set that predates fq.worker.>"
    );
    assert_eq!(
        config.duplicate_window,
        Duration::from_secs(120),
        "startup must restore the managed duplicate window"
    );
    assert_eq!(
        config.description.as_deref(),
        Some("operator-owned description"),
        "reconciliation must preserve fields factor-q does not manage"
    );
}

/// The pure comparison seam makes an already-reconciled second connect a no-op,
/// including when an unmanaged field differs from factor-q's creation template.
#[test]
fn managed_stream_config_diff_is_empty_when_only_unmanaged_fields_differ() {
    let desired = stream::Config {
        name: STREAM_NAME.to_string(),
        subjects: EVENT_STREAM_SUBJECTS
            .iter()
            .map(|subject| subject.to_string())
            .collect(),
        retention: stream::RetentionPolicy::Limits,
        storage: stream::StorageType::File,
        max_age: DEFAULT_MAX_AGE,
        compression: Some(stream::Compression::S2),
        ..Default::default()
    };
    let current = stream::Config {
        description: Some("operator-owned description".to_string()),
        ..desired.clone()
    };

    assert!(managed_stream_config_diff(&current, &desired).is_empty());
}

#[test]
fn managed_stream_config_diff_names_the_duplicate_window() {
    let current = stream::Config::default();
    let desired = stream::Config {
        duplicate_window: Duration::from_secs(120),
        ..Default::default()
    };

    let changes = managed_stream_config_diff(&current, &desired);
    assert_eq!(changes.len(), 1);
    assert!(changes[0].starts_with("duplicate_window:"), "{changes:?}");
}
