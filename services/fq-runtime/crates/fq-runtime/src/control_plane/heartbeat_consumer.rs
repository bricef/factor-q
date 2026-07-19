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
//! Delivery semantics (the loop and ack policy live in
//! [`super::durable_consumer`]):
//! - **At-least-once** from JetStream. The store update is
//!   idempotent (a repeated `heartbeat_worker` call is a no-op
//!   on already-current data), so re-delivery is safe.
//! - **Parse errors** are logged and acked — a malformed
//!   heartbeat event is not worth retrying.
//! - **Store errors** are transient handler errors: NAK'd to
//!   trigger redelivery.

use std::sync::Arc;

use tokio::sync::oneshot;
use tracing::debug;

use crate::bus::{BusError, EventBus};
use crate::events::{Event, EventPayload};

use super::durable_consumer::{
    DeliverFrom, DurableConsumerConfig, DurableConsumerError, HandlerError, run_durable_consumer,
};
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
    pub async fn run(self, shutdown: oneshot::Receiver<()>) -> Result<(), HeartbeatConsumerError> {
        let config = DurableConsumerConfig {
            durable_name: CONSUMER_NAME.to_string(),
            filter_subjects: vec![FILTER_SUBJECT.to_string()],
            deliver_from: DeliverFrom::Beginning,
        };
        run_durable_consumer(&self.bus, config, shutdown, |event| {
            self.handle_event(event)
        })
        .await
        .map_err(HeartbeatConsumerError::from)
    }

    /// Handle one heartbeat event. The shared loop owns the ack:
    /// `Ok` acks, a transient error NAKs for redelivery.
    async fn handle_event(&self, event: Event) -> Result<(), HandlerError> {
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
                return Ok(());
            }
        };

        // The envelope's timestamp is the authoritative "when did
        // the worker emit this" value. Convert to ms since epoch
        // for the store column.
        let ts_ms = event.envelope.timestamp.timestamp_millis();
        let worker_id = payload.worker_id.as_str();

        self.store
            .heartbeat_worker(worker_id, ts_ms)
            .await
            .map_err(HandlerError::transient)?;
        debug!(
            worker_id = %worker_id,
            ts_ms,
            "updated coordination_worker.last_heartbeat"
        );
        Ok(())
    }
}

impl From<DurableConsumerError> for HeartbeatConsumerError {
    fn from(err: DurableConsumerError) -> Self {
        match err {
            DurableConsumerError::Bus(err) => Self::Bus(err),
            DurableConsumerError::Stream(err) => Self::Stream(err),
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
