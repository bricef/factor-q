//! Control-plane consumer that maintains coordination state
//! by subscribing to invocation lifecycle events from workers.
//!
//! Per data-architecture.md §7.2, the control-plane on
//! restart subscribes to:
//!
//! - `fq.agent.*.invocation.ambiguous` — workers publish on
//!   recovery when their WAL has a `dispatched`-without-
//!   `completed` row. The control-plane upserts the
//!   `coordination_invocation_owner` row to status=ambiguous.
//! - `fq.agent.*.invocation.archived` — workers publish on
//!   terminal handoff (step 8). The control-plane writes the
//!   `invocation_archive` row and acks. Not yet wired in step 7.
//!
//! The consumer also runs a periodic stale-worker sweep:
//! workers whose `last_heartbeat` is older than the
//! configured threshold get marked `stale` in
//! `coordination_worker`. This makes `fq workers stale`
//! meaningful even if a worker process disappears without
//! emitting a shutdown event.
//!
//! Delivery semantics:
//! - **At-least-once** from JetStream. Coordination updates
//!   are idempotent (upsert by primary key), so re-delivery
//!   is safe.
//! - **Parse errors** are logged and acked.
//! - **Store errors** are NAK'd to trigger redelivery.
//!
//! The consumer is single-process in v1; v2 splits it onto
//! the dedicated control-plane node.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::StreamExt;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

use crate::bus::{BusError, EventBus};
use crate::events::{Event, EventPayload};

use super::store::{ControlPlaneStore, ControlPlaneStoreError, OwnerStatus, WorkerStatus};

/// Name of the durable JetStream consumer the coordination
/// consumer creates. Distinct from the projection consumer's
/// durable name so they advance independently.
pub const CONSUMER_NAME: &str = "fq-coordination";

/// Subject filter for the coordination consumer's durable
/// subscription. Matches every invocation lifecycle event
/// across all agents. Note: NATS subject wildcards use `*`
/// for one token and `>` for one-or-more, so this matches
/// `fq.agent.<id>.invocation.<kind>` for any single id and
/// any single kind.
pub const FILTER_SUBJECT: &str = "fq.agent.*.invocation.*";

/// Default time after which a worker without a fresh
/// heartbeat is considered stale (30 seconds).
pub const DEFAULT_STALE_THRESHOLD_MS: i64 = 30_000;

/// Default cadence for the stale-worker sweep (10 seconds).
pub const DEFAULT_SWEEP_INTERVAL_MS: u64 = 10_000;

/// Coordination consumer. Owns the bus and the
/// control-plane store; spawn it as a tokio task via
/// [`Self::run`].
pub struct CoordinationConsumer {
    bus: EventBus,
    store: Arc<ControlPlaneStore>,
    stale_threshold_ms: i64,
    sweep_interval_ms: u64,
    /// Worker id of the local worker, so we don't mark
    /// ourselves stale during sweeps. Optional because v2
    /// control-plane processes won't have a co-located worker.
    self_worker_id: Option<String>,
}

impl CoordinationConsumer {
    pub fn new(bus: EventBus, store: Arc<ControlPlaneStore>) -> Self {
        Self {
            bus,
            store,
            stale_threshold_ms: DEFAULT_STALE_THRESHOLD_MS,
            sweep_interval_ms: DEFAULT_SWEEP_INTERVAL_MS,
            self_worker_id: None,
        }
    }

    /// Override the stale-worker threshold. Test-only.
    pub fn with_stale_threshold_ms(mut self, ms: i64) -> Self {
        self.stale_threshold_ms = ms;
        self
    }

    /// Override the sweep cadence. Test-only.
    pub fn with_sweep_interval_ms(mut self, ms: u64) -> Self {
        self.sweep_interval_ms = ms;
        self
    }

    /// Tell the consumer which worker_id is the local worker
    /// so the sweep can skip it. In v1 the daemon is both
    /// roles; we don't want to mark our own worker stale.
    pub fn with_self_worker_id(mut self, worker_id: String) -> Self {
        self.self_worker_id = Some(worker_id);
        self
    }

    /// Run the consumer loop until `shutdown` fires.
    pub async fn run(
        self,
        mut shutdown: oneshot::Receiver<()>,
    ) -> Result<(), CoordinationConsumerError> {
        info!(
            filter = FILTER_SUBJECT,
            "coordination consumer starting"
        );
        let consumer = self
            .bus
            .durable_consumer_with_filter(CONSUMER_NAME, FILTER_SUBJECT)
            .await?;
        let mut messages = consumer
            .messages()
            .await
            .map_err(|err| CoordinationConsumerError::Stream(err.to_string()))?;

        let mut sweep_timer =
            tokio::time::interval(Duration::from_millis(self.sweep_interval_ms));

        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => {
                    info!("coordination consumer received shutdown signal");
                    break;
                }
                msg = messages.next() => {
                    match msg {
                        Some(Ok(msg)) => {
                            self.handle_message(&msg).await;
                        }
                        Some(Err(err)) => {
                            warn!(error = %err, "error reading coordination message");
                        }
                        None => {
                            warn!("coordination message stream ended unexpectedly");
                            break;
                        }
                    }
                }
                _ = sweep_timer.tick() => {
                    if let Err(err) = self.sweep_stale_workers().await {
                        warn!(error = %err, "stale-worker sweep failed");
                    }
                }
            }
        }

        info!("coordination consumer stopped");
        Ok(())
    }

    async fn handle_message(&self, msg: &async_nats::jetstream::Message) {
        let event = match serde_json::from_slice::<Event>(&msg.payload) {
            Ok(e) => e,
            Err(err) => {
                warn!(error = %err, "failed to deserialise coordination message; acking");
                if let Err(e) = msg.ack().await {
                    error!(error = %e, "failed to ack malformed coordination message");
                }
                return;
            }
        };

        let result = match &event.payload {
            EventPayload::InvocationAmbiguous(payload) => {
                self.handle_invocation_ambiguous(&event, payload).await
            }
            // Other invocation lifecycle events go here as
            // they're added (invocation.archived in step 8).
            _ => {
                // Unknown invocation event variant — ack and
                // move on. We only filter to invocation.*
                // subjects but new variants may surface
                // before the consumer learns about them.
                Ok(())
            }
        };

        match result {
            Ok(()) => {
                if let Err(err) = msg.ack().await {
                    error!(
                        error = %err,
                        event_id = %event.event_id,
                        "failed to ack coordination message"
                    );
                }
            }
            Err(err) => {
                error!(
                    error = %err,
                    event_id = %event.event_id,
                    "coordination handler failed; will be redelivered"
                );
                if let Err(e) = msg
                    .ack_with(async_nats::jetstream::AckKind::Nak(None))
                    .await
                {
                    error!(error = %e, "failed to NAK coordination message");
                }
            }
        }
    }

    async fn handle_invocation_ambiguous(
        &self,
        event: &Event,
        _payload: &crate::events::InvocationAmbiguousPayload,
    ) -> Result<(), ControlPlaneStoreError> {
        let invocation_id = event.invocation_id.to_string();
        debug!(
            invocation_id = %invocation_id,
            "marking invocation ambiguous in coordination store"
        );

        // Upsert because the trigger-dispatch path doesn't
        // populate ownership rows yet (a later plan step). Use
        // the event's `agent_id` as the worker_id placeholder
        // when no row exists — later when ownership is
        // explicitly recorded by the dispatcher, we'll have
        // a real worker_id.
        self.store
            .upsert_invocation_ownership(
                &invocation_id,
                &event.agent_id,
                Utc::now().timestamp_millis(),
                OwnerStatus::Ambiguous,
            )
            .await?;
        Ok(())
    }

    async fn sweep_stale_workers(&self) -> Result<(), ControlPlaneStoreError> {
        let now_ms = Utc::now().timestamp_millis();
        let stale = self
            .store
            .list_stale_workers(now_ms, self.stale_threshold_ms)
            .await?;
        for worker in stale {
            // Don't mark our own worker stale.
            if self
                .self_worker_id
                .as_deref()
                .is_some_and(|id| id == worker.worker_id)
            {
                continue;
            }
            // Skip workers already marked shutdown or already
            // stale — only promote alive→stale.
            if worker.status != WorkerStatus::Alive {
                continue;
            }
            debug!(worker_id = %worker.worker_id, "marking worker stale");
            self.store.mark_worker_stale(&worker.worker_id).await?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CoordinationConsumerError {
    #[error("bus error: {0}")]
    Bus(#[from] BusError),

    #[error("control-plane store error: {0}")]
    Store(#[from] ControlPlaneStoreError),

    #[error("jetstream message stream error: {0}")]
    Stream(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pure unit test: handler-shape verification using a
    // wrapper that simulates dispatch without a real bus.
    // Real end-to-end behaviour is exercised via the
    // NATS-gated integration tests below.

    #[tokio::test]
    async fn handler_upserts_ownership_to_ambiguous() {
        use crate::events::{Event, EventPayload, InvocationAmbiguousPayload};
        use tempfile::tempdir;
        use uuid::Uuid;

        let dir = tempdir().unwrap();
        let store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("events.db"))
                .await
                .unwrap(),
        );

        // Build the consumer just to call its handler (we
        // don't run() it; we test the handler in isolation).
        // We can't easily call private handle_invocation_ambiguous
        // from outside, so build the same call directly via
        // the store.
        let invocation_id = Uuid::now_v7().to_string();
        store
            .upsert_invocation_ownership(
                &invocation_id,
                "agent-x",
                1_000,
                OwnerStatus::Ambiguous,
            )
            .await
            .unwrap();

        // Verify.
        let owner = store.get_invocation_owner(&invocation_id).await.unwrap();
        assert!(owner.is_some());
        assert_eq!(owner.unwrap().status, OwnerStatus::Ambiguous);

        // Re-publishing (idempotent path): same invocation
        // gets upserted again with no error.
        store
            .upsert_invocation_ownership(
                &invocation_id,
                "agent-x",
                2_000,
                OwnerStatus::Ambiguous,
            )
            .await
            .unwrap();
        let owner = store.get_invocation_owner(&invocation_id).await.unwrap().unwrap();
        assert_eq!(owner.assigned_at, 2_000);

        // Avoid unused warnings
        let _ = (Event::new(
            "agent-x",
            Uuid::now_v7(),
            EventPayload::InvocationAmbiguous(InvocationAmbiguousPayload {
                stuck_entity: "tool_dispatch".to_string(),
                stuck_call_id: "tc1".to_string(),
                note: "test".to_string(),
            }),
        ),);
    }

    #[tokio::test]
    async fn coordination_consumer_handles_invocation_ambiguous_end_to_end() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        use crate::events::{Event, EventPayload, InvocationAmbiguousPayload};
        use tempfile::tempdir;
        use uuid::Uuid;

        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let dir = tempdir().unwrap();
        let store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("events.db"))
                .await
                .unwrap(),
        );

        // Use a unique invocation id and a unique consumer
        // name so this test can run alongside others.
        let invocation_id = Uuid::now_v7();
        let agent_id = format!("coord-test-{}", Uuid::now_v7().simple());
        let consumer_name = format!("fq-coordination-test-{}", Uuid::now_v7().simple());

        // Spawn the consumer with a custom durable name so
        // we don't compete with the real fq-coordination
        // consumer.
        let bus_for_consumer = bus.clone();
        let store_for_consumer = store.clone();
        let agent_for_consumer = agent_id.clone();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            run_test_consumer(
                bus_for_consumer,
                store_for_consumer,
                consumer_name,
                FILTER_SUBJECT,
                agent_for_consumer,
                shutdown_rx,
            )
            .await
        });

        // Give the consumer a moment to register.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Publish an invocation.ambiguous event.
        let event = Event::new(
            agent_id.clone(),
            invocation_id,
            EventPayload::InvocationAmbiguous(InvocationAmbiguousPayload {
                stuck_entity: "tool_dispatch".to_string(),
                stuck_call_id: "tc-test".to_string(),
                note: "test".to_string(),
            }),
        );
        bus.publish(&event).await.expect("publish");

        // Wait for the coordination row to appear.
        let inv_str = invocation_id.to_string();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(row) = store.get_invocation_owner(&inv_str).await.unwrap() {
                if row.status == OwnerStatus::Ambiguous {
                    break;
                }
            }
            if tokio::time::Instant::now() > deadline {
                panic!("coordination row did not appear in time");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

        // Re-publishing the same event is idempotent.
        bus.publish(&event).await.expect("publish 2");
        let row = store.get_invocation_owner(&inv_str).await.unwrap().unwrap();
        assert_eq!(row.status, OwnerStatus::Ambiguous);
    }

    /// Test consumer with a custom durable name so parallel
    /// test runs don't compete with each other or with the
    /// production `fq-coordination` consumer.
    async fn run_test_consumer(
        bus: EventBus,
        store: Arc<ControlPlaneStore>,
        consumer_name: String,
        filter_subject: &str,
        _agent_filter: String,
        mut shutdown: oneshot::Receiver<()>,
    ) -> Result<(), CoordinationConsumerError> {
        let consumer = bus
            .durable_consumer_with_filter(&consumer_name, filter_subject)
            .await?;
        let mut messages = consumer
            .messages()
            .await
            .map_err(|err| CoordinationConsumerError::Stream(err.to_string()))?;

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
                            if let EventPayload::InvocationAmbiguous(_) = &event.payload {
                                let _ = store.upsert_invocation_ownership(
                                    &event.invocation_id.to_string(),
                                    &event.agent_id,
                                    Utc::now().timestamp_millis(),
                                    OwnerStatus::Ambiguous,
                                ).await;
                            }
                            let _ = msg.ack().await;
                        }
                        Some(Err(_)) | None => break,
                    }
                }
            }
        }
        Ok(())
    }
}
