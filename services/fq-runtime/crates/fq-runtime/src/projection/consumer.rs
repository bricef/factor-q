//! NATS JetStream consumer that feeds the SQLite projection store.
//!
//! The consumer runs as a long-lived tokio task. It creates a
//! durable JetStream consumer (so restarts resume from the last
//! acknowledged position), iterates over delivered events, inserts
//! each into the [`ProjectionStore`], and acks.
//!
//! Delivery semantics:
//! - **At-least-once** from JetStream. The store's insert is
//!   idempotent on `event_id`, so re-delivery is safe.
//! - **Parse errors** are logged and acked. An event whose JSON we
//!   can't decode will never become valid on retry, so leaving it
//!   un-acked would just create a redelivery loop.
//! - **DB errors** are logged and NAK'd so the message is
//!   redelivered after the ack timeout. Transient errors recover;
//!   persistent errors become visible in logs and metrics.

use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

use crate::bus::{BusError, EventBus};
use crate::events::Event;

use super::store::{ProjectionStore, StoreError};

/// Name of the durable JetStream consumer the projector creates.
pub const CONSUMER_NAME: &str = "fq-projector";

/// The projection consumer. Owns the bus reference and the store,
/// not the tokio task itself — call [`ProjectionConsumer::run`] to
/// drive it.
pub struct ProjectionConsumer {
    bus: EventBus,
    store: Arc<ProjectionStore>,
}

impl ProjectionConsumer {
    pub fn new(bus: EventBus, store: Arc<ProjectionStore>) -> Self {
        Self { bus, store }
    }

    /// Run the consumer loop until `shutdown` fires.
    ///
    /// Creates the durable consumer if it doesn't exist, catches up
    /// on any events published while the projector was not running,
    /// and then follows the live stream. The `shutdown` receiver is
    /// a oneshot so the caller can send `()` to request a graceful
    /// exit.
    pub async fn run(self, mut shutdown: oneshot::Receiver<()>) -> Result<(), ConsumerError> {
        info!("projection consumer starting");
        let consumer = self.bus.durable_consumer(CONSUMER_NAME).await?;
        let mut messages = consumer
            .messages()
            .await
            .map_err(|err| ConsumerError::Stream(err.to_string()))?;

        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => {
                    info!("projection consumer received shutdown signal");
                    break;
                }
                msg = messages.next() => {
                    match msg {
                        Some(Ok(msg)) => {
                            self.handle(&msg).await;
                        }
                        Some(Err(err)) => {
                            warn!(error = %err, "error reading next JetStream message");
                        }
                        None => {
                            warn!("JetStream message stream ended unexpectedly");
                            break;
                        }
                    }
                }
            }
        }

        info!("projection consumer stopped");
        Ok(())
    }

    async fn handle(&self, msg: &async_nats::jetstream::Message) {
        let event = match serde_json::from_slice::<Event>(&msg.payload) {
            Ok(event) => event,
            Err(err) => {
                warn!(error = %err, "failed to deserialise event, acking to avoid redelivery loop");
                if let Err(ack_err) = msg.ack().await {
                    error!(error = %ack_err, "failed to ack malformed message");
                }
                return;
            }
        };

        debug!(
            event_id = %event.event_id,
            agent_id = %event.agent_id,
            "projecting event"
        );

        match self.store.insert_event(&event).await {
            Ok(()) => {
                if let Err(err) = msg.ack().await {
                    error!(error = %err, event_id = %event.event_id, "failed to ack after insert");
                }
            }
            Err(err) => {
                error!(
                    error = %err,
                    event_id = %event.event_id,
                    "failed to insert event — will be redelivered"
                );
                // Nak to trigger redelivery after the ack timeout.
                if let Err(nak_err) = msg
                    .ack_with(async_nats::jetstream::AckKind::Nak(None))
                    .await
                {
                    error!(error = %nak_err, "failed to NAK message");
                }
            }
        }
    }
}

/// Errors that prevent the consumer from starting or progressing.
///
/// Transient failures (a single bad message, a transient DB error)
/// are handled inside the loop and do not surface as this error.
#[derive(Debug, thiserror::Error)]
pub enum ConsumerError {
    #[error("bus error: {0}")]
    Bus(#[from] BusError),

    #[error("store error: {0}")]
    Store(#[from] StoreError),

    #[error("jetstream message stream error: {0}")]
    Stream(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{
        CompletedPayload, ConfigSnapshot, Event, EventPayload, SandboxSnapshot, TriggerSource,
        TriggeredPayload,
    };
    use serde_json::json;
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio::sync::oneshot;
    use uuid::Uuid;

    fn triggered(agent: &str) -> Event {
        Event::new(
            agent,
            Uuid::now_v7(),
            EventPayload::Triggered(TriggeredPayload {
                trigger_source: TriggerSource::Manual,
                trigger_subject: None,
                trigger_payload: json!({}),
                config_snapshot: ConfigSnapshot {
                    name: agent.to_string(),
                    model: "claude-haiku-4-5".to_string(),
                    system_prompt: "test".to_string(),
                    tools: vec![],
                    sandbox: SandboxSnapshot::default(),
                    budget: None,
                },
            }),
        )
    }

    fn completed(agent: &str, inv: Uuid) -> Event {
        Event::new(
            agent,
            inv,
            EventPayload::Completed(CompletedPayload {
                result_summary: Some("ok".to_string()),
                total_llm_calls: 1,
                total_tool_calls: 0,
                total_cost: 0.001,
                total_duration_ms: 10,
            }),
        )
    }

    fn unique_consumer_name() -> String {
        format!("fq-projector-test-{}", Uuid::now_v7().simple())
    }

    /// End-to-end: publish events, spin up a consumer with a
    /// unique durable name, verify events land in SQLite, then
    /// shut down the consumer cleanly.
    #[tokio::test]
    async fn consumer_projects_events_into_store() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("events.db");
        let store = Arc::new(ProjectionStore::open(&db_path).await.expect("open store"));

        // Publish a couple of events BEFORE starting the consumer
        // so we prove the durable consumer picks them up from the
        // stream history.
        let agent_id = format!("proj-test-{}", Uuid::now_v7().simple());
        let ev1 = triggered(&agent_id);
        let inv = ev1.invocation_id;
        bus.publish(&ev1).await.expect("publish 1");
        bus.publish(&completed(&agent_id, inv)).await.expect("publish 2");

        // Spin up the consumer with a fresh name so it starts from
        // the beginning of the stream.
        let consumer = ProjectionConsumerWithName {
            bus: bus.clone(),
            store: store.clone(),
            consumer_name: unique_consumer_name(),
        };

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(async move { consumer.run(shutdown_rx).await });

        // Wait until *our* specific events have been projected.
        // A durable consumer starts from the beginning of the
        // stream, so `store.count()` may include events from
        // previous test runs. Check agent-scoped rows only.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let agent_filter = super::super::store::EventFilter {
            agent: Some(&agent_id),
            ..Default::default()
        };
        loop {
            let rows = store.query_events(&agent_filter, 100).await.unwrap();
            if rows.len() >= 2 {
                break;
            }
            if tokio::time::Instant::now() > deadline {
                panic!(
                    "store did not catch up in time for agent {}; rows={}",
                    agent_id,
                    rows.len()
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        shutdown_tx.send(()).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

        // Verify the rows we expect.
        let filter = super::super::store::EventFilter {
            agent: Some(&agent_id),
            ..Default::default()
        };
        let rows = store.query_events(&filter, 100).await.unwrap();
        assert_eq!(rows.len(), 2, "expected 2 rows, got {}", rows.len());
        let types: Vec<&str> = rows.iter().map(|r| r.event_type.as_str()).collect();
        assert!(types.contains(&"triggered"));
        assert!(types.contains(&"completed"));
    }

    /// A variant of ProjectionConsumer that accepts a custom
    /// durable name so test runs don't step on each other.
    struct ProjectionConsumerWithName {
        bus: EventBus,
        store: Arc<ProjectionStore>,
        consumer_name: String,
    }

    impl ProjectionConsumerWithName {
        async fn run(self, mut shutdown: oneshot::Receiver<()>) -> Result<(), ConsumerError> {
            let consumer = self.bus.durable_consumer(&self.consumer_name).await?;
            let mut messages = consumer
                .messages()
                .await
                .map_err(|err| ConsumerError::Stream(err.to_string()))?;

            loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown => break,
                    msg = messages.next() => {
                        match msg {
                            Some(Ok(msg)) => {
                                let event: Event = match serde_json::from_slice(&msg.payload) {
                                    Ok(e) => e,
                                    Err(_) => { let _ = msg.ack().await; continue; }
                                };
                                if let Err(err) = self.store.insert_event(&event).await {
                                    eprintln!("insert error: {err}");
                                    let _ = msg.ack_with(async_nats::jetstream::AckKind::Nak(None)).await;
                                } else {
                                    let _ = msg.ack().await;
                                }
                            }
                            Some(Err(_)) | None => break,
                        }
                    }
                }
            }
            Ok(())
        }
    }
}
