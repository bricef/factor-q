//! Worker heartbeat producer.
//!
//! A long-lived tokio task spawned alongside the worker. It
//! publishes a `WorkerHeartbeat` event on
//! `fq.worker.{worker_id}.heartbeat` immediately on startup and
//! then periodically. The control-plane's heartbeat consumer
//! updates `coordination_worker.last_heartbeat` on receipt; the
//! stale-worker sweep marks workers stale when the signal falls
//! behind its threshold.
//!
//! Failure policy: log-and-continue on publish failure. A
//! transient NATS hiccup shouldn't take the daemon down, and
//! sustained outage surfaces naturally — the worker just appears
//! stale to the coordination view, which is the correct
//! observable behaviour.

use std::time::Duration;

use tokio::sync::oneshot;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::bus::EventBus;
use crate::events::{Event, EventPayload, WorkerHeartbeatPayload};

use super::WorkerId;

/// Default cadence between heartbeats (10 seconds).
///
/// Pair this with the coordination consumer's
/// `DEFAULT_STALE_THRESHOLD_MS` (30 seconds) — three missed
/// heartbeats is the threshold for staleness. Shorter cadence
/// is more defensive but doubles NATS traffic; longer cadence
/// risks one missed beat being interpreted as stale. The 10s/30s
/// pair is the symmetric default.
pub const DEFAULT_INTERVAL_MS: u64 = 10_000;

/// Heartbeat producer. Spawn its [`run`](Self::run) method as a
/// tokio task and feed it a shutdown signal.
pub struct HeartbeatProducer {
    bus: EventBus,
    worker_id: WorkerId,
    runtime_id: Uuid,
    interval_ms: u64,
}

impl HeartbeatProducer {
    pub fn new(bus: EventBus, worker_id: WorkerId, runtime_id: Uuid) -> Self {
        Self {
            bus,
            worker_id,
            runtime_id,
            interval_ms: DEFAULT_INTERVAL_MS,
        }
    }

    /// Override the cadence. Test-only — production callers should
    /// use the default.
    pub fn with_interval_ms(mut self, ms: u64) -> Self {
        self.interval_ms = ms;
        self
    }

    /// Run the producer loop. Fires one heartbeat immediately on
    /// startup, then ticks every `interval_ms`. Returns `Ok(())`
    /// when `shutdown` fires.
    ///
    /// Publish failures are logged and the loop continues — the
    /// task does not fail the daemon over a transient bus issue.
    pub async fn run(self, mut shutdown: oneshot::Receiver<()>) -> Result<(), HeartbeatError> {
        info!(
            worker_id = %self.worker_id,
            interval_ms = self.interval_ms,
            "worker heartbeat producer starting"
        );

        // Fire the first heartbeat immediately so the worker
        // appears alive between self-registration and the first
        // periodic tick. Without this, a sweep that runs within
        // `interval_ms` of startup could mark the worker stale.
        self.emit_one().await;

        let mut ticker = tokio::time::interval(Duration::from_millis(self.interval_ms));
        // The first tick fires immediately; we already emitted, so
        // consume it before entering the loop.
        ticker.tick().await;

        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => {
                    info!(
                        worker_id = %self.worker_id,
                        "worker heartbeat producer received shutdown signal"
                    );
                    break;
                }
                _ = ticker.tick() => {
                    self.emit_one().await;
                }
            }
        }

        info!(worker_id = %self.worker_id, "worker heartbeat producer stopped");
        Ok(())
    }

    /// Build and publish a single heartbeat event. Logs on
    /// failure; does not propagate the error.
    async fn emit_one(&self) {
        let event = Event::system(
            self.runtime_id,
            EventPayload::WorkerHeartbeat(WorkerHeartbeatPayload {
                worker_id: self.worker_id.clone(),
            }),
        );
        match self.bus.publish(&event).await {
            Ok(_seq) => {
                debug!(worker_id = %self.worker_id, "heartbeat published");
            }
            Err(err) => {
                warn!(
                    worker_id = %self.worker_id,
                    error = %err,
                    "heartbeat publish failed; will retry on next tick"
                );
            }
        }
    }
}

/// Producer-side errors. The producer logs and continues on
/// publish failure, so today this enum is empty in practice —
/// it exists so the task's return type composes with the rest
/// of the daemon's managed-task error handling (uniform
/// `Result<(), E>` shape).
#[derive(Debug, thiserror::Error)]
pub enum HeartbeatError {
    // Reserved for unrecoverable failures the task wants to
    // surface up through the daemon's task-failed pathway. None
    // exist yet; a future "bus dropped entirely" condition could
    // land here.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn heartbeat_producer_emits_immediately_and_on_tick() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();
        let bus = EventBus::connect(&url).await.expect("connect to NATS");
        let worker_id = WorkerId::new(format!("hb-test-{}", Uuid::now_v7().simple())).unwrap();
        let runtime_id = Uuid::now_v7();

        let mut sub = bus
            .subscribe(format!("fq.worker.{}.heartbeat", worker_id))
            .await
            .expect("subscribe");
        tokio::time::sleep(Duration::from_millis(50)).await;

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let producer = HeartbeatProducer::new(bus.clone(), worker_id.clone(), runtime_id)
            .with_interval_ms(200);
        let handle = tokio::spawn(producer.run(shutdown_rx));

        // First heartbeat fires immediately on startup. Should
        // arrive well within 500ms.
        use futures::StreamExt;
        let first = tokio::time::timeout(Duration::from_secs(2), sub.next())
            .await
            .expect("first heartbeat timeout")
            .expect("stream closed")
            .expect("deserialise");
        assert!(matches!(first.payload, EventPayload::WorkerHeartbeat(_)));

        // Second heartbeat arrives after the first tick (~200ms).
        let second = tokio::time::timeout(Duration::from_secs(2), sub.next())
            .await
            .expect("second heartbeat timeout")
            .expect("stream closed")
            .expect("deserialise");
        match &second.payload {
            EventPayload::WorkerHeartbeat(p) => assert_eq!(p.worker_id, worker_id),
            other => panic!("wrong payload variant: {other:?}"),
        }

        let _ = shutdown_tx.send(());
        let result = handle.await.expect("join");
        assert!(result.is_ok());
    }
}
