//! NATS-backed event bus.
//!
//! Provides a typed interface for publishing and subscribing to factor-q
//! events. All events flow through a single JetStream stream, using
//! subject-based filtering for consumption.
//!
//! See `docs/design/event-schema.md` for the event schema and subject
//! hierarchy.

use async_nats::jetstream::{self, stream};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::events::Event;

/// Name of the JetStream stream that holds all factor-q events.
pub const STREAM_NAME: &str = "fq-events";

/// Subject pattern matching all factor-q events.
pub const ALL_EVENTS_SUBJECT: &str = "fq.>";

/// Default retention for the event stream.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60); // 30 days

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
    /// Connect to a NATS server and ensure the event stream exists.
    pub async fn connect(url: &str) -> Result<Self, BusError> {
        info!(nats_url = url, "connecting to NATS");
        let client = async_nats::connect(url).await?;
        let jetstream = jetstream::new(client.clone());

        let bus = Self { client, jetstream };
        bus.ensure_stream().await?;
        Ok(bus)
    }

    /// Ensure the factor-q event stream exists, creating it if necessary.
    async fn ensure_stream(&self) -> Result<(), BusError> {
        debug!(stream = STREAM_NAME, "ensuring JetStream stream exists");
        self.jetstream
            .get_or_create_stream(stream::Config {
                name: STREAM_NAME.to_string(),
                subjects: vec![ALL_EVENTS_SUBJECT.to_string()],
                retention: stream::RetentionPolicy::Limits,
                storage: stream::StorageType::File,
                max_age: DEFAULT_MAX_AGE,
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

    fn sample_event() -> Event {
        Event::new(
            "test-agent",
            Uuid::now_v7(),
            EventPayload::Triggered(TriggeredPayload {
                trigger_source: TriggerSource::Manual,
                trigger_subject: None,
                trigger_payload: json!({"input": "hello"}),
                config_snapshot: ConfigSnapshot {
                    name: "test-agent".to_string(),
                    model: "claude-haiku".to_string(),
                    system_prompt: "Test.".to_string(),
                    tools: vec![],
                    sandbox: SandboxSnapshot {
                        fs_read: vec![],
                        fs_write: vec![],
                        network: vec![],
                        env: vec![],
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
        let event = sample_event();
        let expected_id = event.event_id;

        let mut subscriber = bus
            .subscribe("fq.agent.test-agent.>")
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
        assert_eq!(received.agent_id, "test-agent");
    }
}
