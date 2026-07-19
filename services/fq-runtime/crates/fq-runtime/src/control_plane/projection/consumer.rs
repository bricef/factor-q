//! NATS JetStream consumer that feeds the SQLite projection store.
//!
//! The consumer runs as a long-lived tokio task. It creates a
//! durable JetStream consumer (so restarts resume from the last
//! acknowledged position), iterates over delivered events, inserts
//! each into the [`ProjectionStore`], and acks.
//!
//! Delivery semantics (the loop and ack policy live in
//! [`crate::control_plane::durable_consumer`]):
//! - **At-least-once** from JetStream. The store's insert is
//!   idempotent on `event_id`, so re-delivery is safe.
//! - **Parse errors** are logged and acked. An event whose JSON we
//!   can't decode will never become valid on retry, so leaving it
//!   un-acked would just create a redelivery loop.
//! - **DB errors** are transient handler errors: logged and NAK'd
//!   so the message is redelivered after the ack timeout.
//!   Transient errors recover; persistent errors become visible
//!   in logs and metrics.

use std::sync::Arc;

use tokio::sync::oneshot;
use tracing::{debug, info};

use crate::bus::{BusError, EventBus};
use crate::control_plane::durable_consumer::{
    DeliverFrom, DurableConsumerConfig, DurableConsumerError, HandlerError, run_durable_consumer,
};
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
    pub async fn run(self, shutdown: oneshot::Receiver<()>) -> Result<(), ConsumerError> {
        // This exact line is a readiness contract:
        // tests/smoke/smoke.sh greps the daemon log for it before
        // driving the walking skeleton.
        info!("projection consumer starting");
        let config = DurableConsumerConfig {
            durable_name: CONSUMER_NAME.to_string(),
            filter_subjects: Vec::new(),
            deliver_from: DeliverFrom::Beginning,
        };
        run_durable_consumer(&self.bus, config, shutdown, |event| {
            self.handle_event(event)
        })
        .await
        .map_err(ConsumerError::from)
    }

    /// Insert one event into the projection. The shared loop owns
    /// the ack: `Ok` acks, a transient error NAKs for redelivery.
    async fn handle_event(&self, event: Event) -> Result<(), HandlerError> {
        debug!(
            event_id = %event.envelope.event_id,
            agent_id = %event.envelope.agent_id,
            "projecting event"
        );
        self.store
            .insert_event(&event)
            .await
            .map_err(HandlerError::transient)
    }
}

impl From<DurableConsumerError> for ConsumerError {
    fn from(err: DurableConsumerError) -> Self {
        match err {
            DurableConsumerError::Bus(err) => Self::Bus(err),
            DurableConsumerError::Stream(err) => Self::Stream(err),
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
    use crate::agent::AgentId;
    use crate::events::{
        CompletedPayload, ConfigSnapshot, Event, EventPayload, SandboxSnapshot, TaskStatus,
        TriggerSource, TriggeredPayload,
    };
    use serde_json::json;
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio::sync::oneshot;
    use uuid::Uuid;

    /// Wrap `&str` test input in `AgentId`. Panics on invalid
    /// inputs — only the hardcoded fixture strings flow through.
    fn aid(s: &str) -> AgentId {
        AgentId::new(s).expect("test agent id must be valid")
    }

    fn triggered(agent: &str) -> Event {
        Event::new(
            aid(agent),
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
                    ..Default::default()
                },
            }),
        )
    }

    fn completed(agent: &str, inv: Uuid) -> Event {
        Event::new(
            aid(agent),
            inv,
            EventPayload::Completed(CompletedPayload {
                task_status: TaskStatus::default(),
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

    /// End-to-end: publish events, spin up the shared durable
    /// loop over the projection's insert handler with a unique
    /// durable name, verify events land in SQLite, then shut
    /// down cleanly. Exercises the same loop code
    /// `ProjectionConsumer::run` delegates to (#192), including
    /// the catch-up-from-history semantics a restart relies on.
    #[tokio::test]
    async fn consumer_projects_events_into_store() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("projection.db");
        let store = Arc::new(ProjectionStore::open(&db_path).await.expect("open store"));

        // Publish a couple of events BEFORE starting the consumer
        // so we prove the durable consumer picks them up from the
        // stream history.
        let agent_id = format!("proj-test-{}", Uuid::now_v7().simple());
        let ev1 = triggered(&agent_id);
        let inv = ev1.envelope.invocation_id;
        bus.publish(&ev1).await.expect("publish 1");
        bus.publish(&completed(&agent_id, inv))
            .await
            .expect("publish 2");

        // Spin up the shared loop with a fresh durable name so it
        // starts from the beginning of the stream.
        let config = DurableConsumerConfig {
            durable_name: unique_consumer_name(),
            // Narrow filter so the durable consumer doesn't
            // have to chew through every event the shared
            // stream has accumulated across days of runs (#118).
            filter_subjects: vec![format!("fq.agent.{agent_id}.>")],
            deliver_from: DeliverFrom::Beginning,
        };

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let bus_for_loop = bus.clone();
        let store_for_loop = store.clone();
        let handle = tokio::spawn(async move {
            run_durable_consumer(&bus_for_loop, config, shutdown_rx, |event| {
                let store = store_for_loop.clone();
                async move {
                    store
                        .insert_event(&event)
                        .await
                        .map_err(HandlerError::transient)
                }
            })
            .await
        });

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
}
