//! NATS-backed event bus.
//!
//! Provides a typed interface for publishing and subscribing to factor-q
//! events. All events flow through a single JetStream stream, using
//! subject-based filtering for consumption.
//!
//! See `docs/design/event-schema.md` for the event schema and subject
//! hierarchy.

use async_nats::jetstream::{self, consumer, stream};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::events::Event;

/// Name of the JetStream stream that holds all factor-q events.
pub const STREAM_NAME: &str = "fq-events";

/// Subjects captured by the event stream.
///
/// We narrow this from the original `fq.>` so the separate trigger
/// stream (`fq.trigger.>`) can claim its subject without overlap.
/// NATS does not allow two JetStream streams to claim overlapping
/// subjects.
pub const EVENT_STREAM_SUBJECTS: &[&str] = &["fq.agent.>", "fq.system.>"];

/// Default retention for the event stream.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60); // 30 days

/// Name of the JetStream stream that holds pending agent triggers.
/// Separate from the event stream because triggers have different
/// semantics: work-queue delivery (one consumer per message), short
/// retention, and no compression. See
/// `docs/design/storage-and-scaling.md` for the rationale.
pub const TRIGGER_STREAM_NAME: &str = "fq-triggers";

/// Subject pattern matching all agent triggers.
pub const ALL_TRIGGERS_SUBJECT: &str = "fq.trigger.>";

/// Default retention for the trigger stream. Triggers are short-lived
/// — the dispatcher consumes them within seconds under normal
/// operation. A 24h window is a safety net against a runaway
/// backlog, not a promise that normal triggers live that long.
pub const DEFAULT_TRIGGER_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60); // 24 hours

/// Build a trigger subject for a given agent id.
pub fn trigger_subject(agent_id: &str) -> String {
    format!("fq.trigger.{agent_id}")
}

/// Extract an agent id from a trigger subject of the form
/// `fq.trigger.<agent_id>`. Agent ids are validated to contain no
/// dots (see `AgentId::new`), so the subject has exactly three
/// dot-separated tokens.
pub fn agent_id_from_trigger_subject(subject: &str) -> Option<&str> {
    let mut parts = subject.splitn(3, '.');
    let first = parts.next()?;
    let second = parts.next()?;
    let third = parts.next()?;
    if first != "fq" || second != "trigger" || third.is_empty() {
        return None;
    }
    Some(third)
}

/// Errors from the event bus.
#[derive(Debug, thiserror::Error)]
pub enum BusError {
    #[error("failed to connect to NATS: {0}")]
    Connect(#[from] async_nats::ConnectError),

    #[error("failed to ensure stream: {0}")]
    Stream(String),

    #[error("failed to publish event: {0}")]
    Publish(String),

    #[error("failed to subscribe: {0}")]
    Subscribe(String),

    #[error("failed to serialise event: {0}")]
    Serialise(#[from] serde_json::Error),
}

impl From<async_nats::jetstream::context::CreateStreamError> for BusError {
    fn from(err: async_nats::jetstream::context::CreateStreamError) -> Self {
        BusError::Stream(err.to_string())
    }
}

impl From<async_nats::jetstream::context::PublishError> for BusError {
    fn from(err: async_nats::jetstream::context::PublishError) -> Self {
        BusError::Publish(err.to_string())
    }
}

impl From<async_nats::SubscribeError> for BusError {
    fn from(err: async_nats::SubscribeError) -> Self {
        BusError::Subscribe(err.to_string())
    }
}

/// The factor-q event bus. Wraps a NATS client and JetStream context.
#[derive(Clone)]
pub struct EventBus {
    client: async_nats::Client,
    jetstream: jetstream::Context,
}

impl EventBus {
    /// Connect to a NATS server and ensure both the event and
    /// trigger streams exist.
    pub async fn connect(url: &str) -> Result<Self, BusError> {
        info!(nats_url = url, "connecting to NATS");
        let client = async_nats::connect(url).await?;
        let jetstream = jetstream::new(client.clone());

        let bus = Self { client, jetstream };
        bus.ensure_event_stream().await?;
        bus.ensure_trigger_stream().await?;
        Ok(bus)
    }

    /// Ensure the factor-q event stream exists, creating it if necessary.
    ///
    /// S2 compression is enabled on creation. Events are text-heavy
    /// (JSON with large system prompts and tool outputs) and compress
    /// 2–4x with negligible CPU cost, which meaningfully extends the
    /// retention window at a given storage budget. See
    /// `docs/design/storage-and-scaling.md` for the rationale.
    async fn ensure_event_stream(&self) -> Result<(), BusError> {
        debug!(stream = STREAM_NAME, "ensuring JetStream event stream exists");
        self.jetstream
            .get_or_create_stream(stream::Config {
                name: STREAM_NAME.to_string(),
                subjects: EVENT_STREAM_SUBJECTS
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                retention: stream::RetentionPolicy::Limits,
                storage: stream::StorageType::File,
                max_age: DEFAULT_MAX_AGE,
                compression: Some(stream::Compression::S2),
                ..Default::default()
            })
            .await?;
        Ok(())
    }

    /// Ensure the factor-q trigger stream exists, creating it if
    /// necessary.
    ///
    /// Uses `Limits` retention with a short `max_age` rather than
    /// `WorkQueue`. Work-queue streams disallow overlapping consumer
    /// filters at the NATS level (error code 10100), which makes
    /// parallel test consumers and broad production consumers
    /// fundamentally incompatible. `Limits` retention allows any
    /// number of consumers with any filters — each consumer just
    /// tracks its own position, and messages age out after the
    /// retention window.
    ///
    /// For phase 1 single-runtime deployments this is equivalent in
    /// practice: the production dispatcher consumes each trigger
    /// quickly, and the 24h retention window ensures space is
    /// reclaimed even if the runtime is down. If horizontal
    /// scaling of dispatchers becomes a concern, we will revisit
    /// (likely with an explicit queue-group pattern on top of the
    /// limits stream, or a separate stream per runtime instance).
    ///
    /// Unlike the event stream, the trigger stream is not compressed
    /// — messages are short-lived and small, so the CPU cost of
    /// compression is not justified.
    async fn ensure_trigger_stream(&self) -> Result<(), BusError> {
        debug!(stream = TRIGGER_STREAM_NAME, "ensuring JetStream trigger stream exists");
        self.jetstream
            .get_or_create_stream(stream::Config {
                name: TRIGGER_STREAM_NAME.to_string(),
                subjects: vec![ALL_TRIGGERS_SUBJECT.to_string()],
                retention: stream::RetentionPolicy::Limits,
                storage: stream::StorageType::File,
                max_age: DEFAULT_TRIGGER_MAX_AGE,
                ..Default::default()
            })
            .await?;
        Ok(())
    }

    /// Publish an event to the bus.
    ///
    /// The event's subject is derived from its payload type via
    /// [`Event::subject`]. Publishing awaits the JetStream ack, confirming
    /// the event was durably stored.
    pub async fn publish(&self, event: &Event) -> Result<(), BusError> {
        let subject = event.subject();
        let payload = serde_json::to_vec(event)?;
        debug!(subject = %subject, event_id = %event.event_id, "publishing event");

        self.jetstream
            .publish(subject, Bytes::from(payload))
            .await?
            .await?;
        Ok(())
    }

    /// Publish a trigger for a given agent. The JSON-encoded
    /// payload becomes the message body. The delivery is ack'd by
    /// JetStream once it's durably accepted, so this returns only
    /// after the trigger is persisted.
    pub async fn publish_trigger(
        &self,
        agent_id: &str,
        payload: &serde_json::Value,
    ) -> Result<(), BusError> {
        let subject = trigger_subject(agent_id);
        let body = serde_json::to_vec(payload)?;
        debug!(subject = %subject, "publishing trigger");
        self.jetstream
            .publish(subject, Bytes::from(body))
            .await?
            .await?;
        Ok(())
    }

    /// Create (or open) a durable JetStream pull consumer on the
    /// trigger stream, filtered to all trigger subjects.
    pub async fn trigger_consumer(
        &self,
        name: &str,
    ) -> Result<consumer::PullConsumer, BusError> {
        self.trigger_consumer_with_filter(name, ALL_TRIGGERS_SUBJECT)
            .await
    }

    /// Create (or open) a durable JetStream pull consumer on the
    /// trigger stream with an explicit filter subject.
    ///
    /// Work-queue streams require every consumer to be "filtered".
    /// Production callers use [`Self::trigger_consumer`] which
    /// passes the broad `fq.trigger.>` pattern. Tests use
    /// narrower filters (e.g. a specific agent's trigger subject)
    /// so that parallel test consumers do not compete for each
    /// other's messages on the same work-queue stream. NATS
    /// delivers each published trigger to exactly one consumer
    /// whose filter matches; with disjoint per-test filters, tests
    /// do not cross-talk.
    pub async fn trigger_consumer_with_filter(
        &self,
        name: &str,
        filter_subject: &str,
    ) -> Result<consumer::PullConsumer, BusError> {
        debug!(
            consumer = name,
            filter = filter_subject,
            "getting/creating durable trigger consumer"
        );
        let stream = self
            .jetstream
            .get_stream(TRIGGER_STREAM_NAME)
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        let consumer = stream
            .get_or_create_consumer(
                name,
                consumer::pull::Config {
                    durable_name: Some(name.to_string()),
                    ack_policy: consumer::AckPolicy::Explicit,
                    filter_subject: filter_subject.to_string(),
                    ..Default::default()
                },
            )
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        Ok(consumer)
    }

    /// Create (or open) a durable JetStream pull consumer on the
    /// factor-q event stream.
    ///
    /// Durable consumers remember their position across restarts, so
    /// the projection consumer can be stopped and restarted without
    /// losing events or redelivering old ones. The returned
    /// [`consumer::PullConsumer`] can be used with `.messages()` to
    /// iterate over delivered messages.
    pub async fn durable_consumer(
        &self,
        name: &str,
    ) -> Result<consumer::PullConsumer, BusError> {
        debug!(consumer = name, "getting/creating durable JetStream consumer");
        let stream = self
            .jetstream
            .get_stream(STREAM_NAME)
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        let consumer = stream
            .get_or_create_consumer(
                name,
                consumer::pull::Config {
                    durable_name: Some(name.to_string()),
                    ack_policy: consumer::AckPolicy::Explicit,
                    ..Default::default()
                },
            )
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        Ok(consumer)
    }

    /// Subscribe to events matching a subject filter.
    ///
    /// Uses core NATS subscribe (not a durable JetStream consumer), so the
    /// stream only delivers events published after the subscription is
    /// established. Suitable for live tailing.
    ///
    /// Each item in the returned stream is either a deserialised [`Event`]
    /// or a [`BusError`] if deserialisation fails.
    pub async fn subscribe(
        &self,
        subject_filter: impl Into<String>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Event, BusError>> + Send>>, BusError> {
        let subject = subject_filter.into();
        debug!(subject = %subject, "subscribing to events");

        let subscriber = self.client.subscribe(subject).await?;
        let stream = subscriber.map(|msg| {
            serde_json::from_slice::<Event>(&msg.payload).map_err(|err| {
                warn!(error = %err, "failed to deserialise event");
                BusError::Serialise(err)
            })
        });
        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ConfigSnapshot, EventPayload, SandboxSnapshot, TriggerSource, TriggeredPayload};
    use serde_json::json;
    use uuid::Uuid;

    fn sample_event(agent_id: &str) -> Event {
        Event::new(
            agent_id,
            Uuid::now_v7(),
            EventPayload::Triggered(TriggeredPayload {
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
                },
            }),
        )
    }

    /// Integration test that requires a running NATS server on the default URL.
    ///
    /// Run with `just infra-up` before invoking:
    ///
    /// ```sh
    /// just fq-test
    /// ```
    ///
    /// Skipped unless the `FQ_NATS_URL` environment variable is set.
    #[tokio::test]
    async fn publish_and_subscribe_round_trip() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        let bus = EventBus::connect(&url).await.expect("connect to NATS");
        let agent_id = format!("bus-test-{}", Uuid::now_v7().simple());
        let event = sample_event(&agent_id);
        let expected_id = event.event_id;

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

        assert_eq!(received.event_id, expected_id);
        assert_eq!(received.agent_id, agent_id);
    }
}
