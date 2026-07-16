//! Worker heartbeat consumer.
//!
//! Subscribes via a durable JetStream consumer to
//! `fq.worker.*.heartbeat` events emitted by
//! [`crate::worker::HeartbeatProducer`]. On each receipt, updates
//! `coordination_worker.last_heartbeat` (and sets
//! `status = Alive`) via [`ControlPlaneStore::heartbeat_worker`].
//!
//! The consumer is the receiving half of the worker→
//! control-plane heartbeat protocol. It is separate from the
//! [`super::coordination_consumer::CoordinationConsumer`] so the
//! two concerns (invocation lifecycle events vs. worker liveness)
//! stay cohesive, and so the filter subjects don't need a
//! multi-subject JetStream consumer.
//!
//! Delivery semantics:
//! - **At-least-once** from JetStream. The store update is
//!   idempotent (a repeated `heartbeat_worker` call is a no-op
//!   on already-current data), so re-delivery is safe.
//! - **Parse errors** are logged and acked — a malformed
//!   heartbeat event is not worth retrying.
//! - **Store errors** are NAK'd to trigger redelivery.

use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

use crate::bus::{BusError, EventBus};
use crate::events::{Event, EventPayload};

use super::store::{ControlPlaneStore, ControlPlaneStoreError};

/// Name of the durable JetStream consumer the heartbeat
/// consumer creates. Distinct from `fq-coordination` so the
/// two consumers advance independently.
pub const CONSUMER_NAME: &str = "fq-heartbeat";

/// Subject filter. Matches every worker's heartbeat across the
/// cluster.
pub const FILTER_SUBJECT: &str = "fq.worker.*.heartbeat";

/// Heartbeat consumer. Owns the bus and the control-plane
/// store; spawn it as a tokio task via [`Self::run`].
pub struct HeartbeatConsumer {
    bus: EventBus,
    store: Arc<ControlPlaneStore>,
}

impl HeartbeatConsumer {
    pub fn new(bus: EventBus, store: Arc<ControlPlaneStore>) -> Self {
        Self { bus, store }
    }

    /// Run the consumer loop until `shutdown` fires.
    pub async fn run(
        self,
        mut shutdown: oneshot::Receiver<()>,
    ) -> Result<(), HeartbeatConsumerError> {
        info!(
            filter = FILTER_SUBJECT,
            "worker heartbeat consumer starting"
        );
        let consumer = self
            .bus
            .durable_consumer_with_filter(CONSUMER_NAME, FILTER_SUBJECT)
            .await?;
        let mut messages = consumer
            .messages()
            .await
            .map_err(|err| HeartbeatConsumerError::Stream(err.to_string()))?;

        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => {
                    info!("worker heartbeat consumer received shutdown signal");
                    break;
                }
                msg = messages.next() => {
                    match msg {
                        Some(Ok(msg)) => {
                            self.handle_message(&msg).await;
                        }
                        Some(Err(err)) => {
                            warn!(error = %err, "error reading heartbeat message");
                        }
                        None => {
                            warn!("heartbeat message stream ended unexpectedly");
                            break;
                        }
                    }
                }
            }
        }

        info!("worker heartbeat consumer stopped");
        Ok(())
    }

    async fn handle_message(&self, msg: &async_nats::jetstream::Message) {
        let event = match serde_json::from_slice::<Event>(&msg.payload) {
            Ok(e) => e,
            Err(err) => {
                warn!(error = %err, "failed to deserialise heartbeat message; acking");
                if let Err(e) = msg.ack().await {
                    error!(error = %e, "failed to ack malformed heartbeat message");
                }
                return;
            }
        };

        let payload = match &event.payload {
            EventPayload::WorkerHeartbeat(p) => p,
            _ => {
                // Filter is narrow to fq.worker.*.heartbeat, but
                // be defensive: a future co-tenant on this
                // subject space would land here.
                debug!(
                    event_id = %event.envelope.event_id,
                    "non-heartbeat event arrived on heartbeat filter; ignoring"
                );
                let _ = msg.ack().await;
                return;
            }
        };

        // The envelope's timestamp is the authoritative "when did
        // the worker emit this" value. Convert to ms since epoch
        // for the store column.
        let ts_ms = event.envelope.timestamp.timestamp_millis();
        let worker_id = payload.worker_id.as_str();

        match self.store.heartbeat_worker(worker_id, ts_ms).await {
            Ok(()) => {
                debug!(
                    worker_id = %worker_id,
                    ts_ms,
                    "updated coordination_worker.last_heartbeat"
                );
                if let Err(err) = msg.ack().await {
                    error!(error = %err, worker_id = %worker_id, "failed to ack heartbeat");
                }
            }
            Err(err) => {
                error!(
                    worker_id = %worker_id,
                    error = %err,
                    "heartbeat_worker store update failed; will be redelivered"
                );
                if let Err(e) = msg
                    .ack_with(async_nats::jetstream::AckKind::Nak(None))
                    .await
                {
                    error!(error = %e, "failed to NAK heartbeat message");
                }
            }
        }
    }
}

/// Errors from the heartbeat consumer's run loop.
#[derive(Debug, thiserror::Error)]
pub enum HeartbeatConsumerError {
    #[error("bus error: {0}")]
    Bus(#[from] BusError),

    #[error("heartbeat consumer stream error: {0}")]
    Stream(String),

    #[error("control-plane store error: {0}")]
    Store(#[from] ControlPlaneStoreError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::WorkerHeartbeatPayload;
    use crate::worker::WorkerId;
    use std::time::Duration;
    use tempfile::tempdir;
    use uuid::Uuid;

    #[tokio::test]
    async fn heartbeat_consumer_updates_coordination_row_end_to_end() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();
        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let dir = tempdir().unwrap();
        let store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("events.db"))
                .await
                .expect("open store"),
        );

        let worker_id = WorkerId::new(format!("hb-test-{}", Uuid::now_v7().simple())).unwrap();
        // Register the worker first; heartbeat_worker is a no-op
        // if the row doesn't exist (by design — the test would
        // succeed but assert-nothing without this register).
        store
            .register_worker(worker_id.as_str(), "test-host", 1_000)
            .await
            .expect("register");

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let consumer = HeartbeatConsumer::new(bus.clone(), store.clone());
        let handle = tokio::spawn(consumer.run(shutdown_rx));

        // Let the durable consumer register.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Publish a heartbeat. envelope.timestamp is set to "now"
        // by Event::system internally.
        let event = Event::system(
            Uuid::now_v7(),
            EventPayload::WorkerHeartbeat(WorkerHeartbeatPayload {
                worker_id: worker_id.clone(),
            }),
        );
        bus.publish(&event).await.expect("publish heartbeat");

        // Wait for the consumer to process it. The store row's
        // last_heartbeat should advance past the initial 1_000.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if tokio::time::Instant::now() > deadline {
                let _ = shutdown_tx.send(());
                let _ = handle.await;
                panic!("heartbeat was not reflected in coordination_worker row");
            }
            let row = store
                .get_worker(worker_id.as_str())
                .await
                .expect("get_worker")
                .expect("row");
            if row.last_heartbeat > 1_000 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let _ = shutdown_tx.send(());
        let _ = handle.await;
    }
}
