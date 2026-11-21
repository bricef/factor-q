//! The stale-worker sweep: a worker whose heartbeat lapsed is marked
//! stale, once per transition, and `worker.orphaned` is published for it.
//! Moved from `coordination_consumer.rs`, which the size cap holds at 800
//! production lines, ahead of the refused-write guard (issue 1070).

use chrono::Utc;
use tracing::{error, warn};

use super::CoordinationConsumer;
use crate::control_plane::store::{ControlPlaneStoreError, WorkerStatus};
use crate::events::{Event, EventPayload, WorkerOrphanedPayload};

impl CoordinationConsumer {
    pub(super) async fn sweep_stale_workers(&self) -> Result<(), ControlPlaneStoreError> {
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
            // The conditional update consumes the alive→stale
            // transition: only the sweep that flips the row wins the
            // right to publish, so `worker.orphaned` fires once per
            // transition (never per sweep tick).
            if !self.store.mark_worker_stale(&worker.worker_id).await? {
                continue;
            }
            let worker_id = match crate::worker::WorkerId::new(worker.worker_id.clone()) {
                Ok(id) => id,
                Err(err) => {
                    error!(worker_id = %worker.worker_id, error = %err, "stale worker has invalid id");
                    continue;
                }
            };
            let event = Event::system(
                self.runtime_id,
                EventPayload::WorkerOrphaned(WorkerOrphanedPayload {
                    worker_id,
                    last_heartbeat_ms: worker.last_heartbeat,
                }),
            );
            // Deliberately at-most-once: the transition is already
            // consumed, so a failed publish is logged and never
            // retried. A lost signal here still leaves the worker
            // visible via `fq workers list --stale-only`; the
            // alternative (publish-then-mark) re-fires on every sweep
            // tick whenever the store write fails, which is the
            // unbounded-noise mode #64 rules out.
            if let Err(err) = self.bus.publish(&event).await {
                error!(worker_id = %worker.worker_id, error = %err, "failed to publish worker.orphaned event");
            } else {
                warn!(worker_id = %worker.worker_id, "worker heartbeat lapsed; emitted worker.orphaned");
            }
        }
        Ok(())
    }
}
