//! Worker archive-ack consumer.
//!
//! A long-lived tokio task spawned alongside the worker. It
//! subscribes to `fq.worker.{worker_id}.invocation.archive_acked`
//! and on each ack, deletes the matching `invocation_state` row
//! from the worker store. The acked event is the control-plane's
//! signal that the archive row has been persisted on its side and
//! the worker no longer needs to hold the row.
//!
//! Subscription model: core NATS (not durable JetStream). Missed
//! acks while the consumer is offline are recovered by the
//! worker's retry sweeper, which republishes
//! `invocation.archived` on a schedule until a fresh ack arrives.
//! Durable consumption would also work but adds a JetStream
//! consumer per worker — and the retry path is the primary
//! correctness mechanism either way.
//!
//! Failure policy: log-and-continue. A delete failure (e.g.
//! transient DB busy) is logged; the next ack republish will
//! retry the delete on the same row, and the row stays terminal
//! and pending in the meantime — recovery sees it as already-done
//! and ignores it.

use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

use crate::bus::EventBus;
use crate::events::{EventPayload, subjects};

use super::WorkerId;
use super::store::WorkerStore;

/// Archive-ack consumer. Spawn its [`run`](Self::run) method as a
/// tokio task and feed it a shutdown signal.
pub struct ArchiveAckConsumer {
    bus: EventBus,
    worker_id: WorkerId,
    store: Arc<WorkerStore>,
}

impl ArchiveAckConsumer {
    pub fn new(bus: EventBus, worker_id: WorkerId, store: Arc<WorkerStore>) -> Self {
        Self {
            bus,
            worker_id,
            store,
        }
    }

    /// Run the consumer loop until `shutdown` fires.
    pub async fn run(self, mut shutdown: oneshot::Receiver<()>) -> Result<(), ArchiveAckError> {
        let subject = subjects::worker_invocation_archive_acked(self.worker_id.as_str());
        info!(
            worker_id = %self.worker_id,
            subject = %subject,
            "archive-ack consumer starting"
        );
        let mut messages = self
            .bus
            .subscribe(subject)
            .await
            .map_err(|err| ArchiveAckError::Subscribe(err.to_string()))?;

        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => {
                    info!(
                        worker_id = %self.worker_id,
                        "archive-ack consumer received shutdown signal"
                    );
                    break;
                }
                msg = messages.next() => {
                    match msg {
                        Some(Ok(event)) => {
                            self.handle_event(event).await;
                        }
                        Some(Err(err)) => {
                            warn!(
                                worker_id = %self.worker_id,
                                error = %err,
                                "error decoding archive-ack message"
                            );
                        }
                        None => {
                            warn!(
                                worker_id = %self.worker_id,
                                "archive-ack subscription stream ended"
                            );
                            break;
                        }
                    }
                }
            }
        }

        info!(worker_id = %self.worker_id, "archive-ack consumer stopped");
        Ok(())
    }

    async fn handle_event(&self, event: crate::events::Event) {
        let payload = match event.payload {
            EventPayload::InvocationArchiveAcked(p) => p,
            other => {
                // Subject filter narrows the stream to ack
                // events but a producer error could publish the
                // wrong variant on this subject. Log and skip.
                debug!(
                    worker_id = %self.worker_id,
                    payload = ?other,
                    "ignoring non-ack event on archive-ack subject"
                );
                return;
            }
        };
        // Defense in depth: the control-plane addressed this ack
        // to a specific worker_id via the subject token, so a
        // mismatch here means either a producer bug or a misrouted
        // subscription. Don't act on someone else's invocation.
        if payload.worker_id != self.worker_id {
            warn!(
                self_worker_id = %self.worker_id,
                payload_worker_id = %payload.worker_id,
                "archive-ack worker_id mismatch; ignoring"
            );
            return;
        }
        let invocation_id = event.envelope.invocation_id.to_string();
        match self.store.delete_invocation_state(&invocation_id).await {
            Ok(0) => {
                // No row to delete — either the worker has
                // already deleted it (a previous redelivered
                // ack) or the row was never written. Either is
                // benign in the retry-sweeper world.
                debug!(
                    invocation_id = %invocation_id,
                    "archive-ack received but no invocation_state row exists"
                );
            }
            Ok(_) => {
                debug!(
                    invocation_id = %invocation_id,
                    "invocation archive hand-off complete; local row deleted"
                );
            }
            Err(err) => {
                warn!(
                    invocation_id = %invocation_id,
                    error = %err,
                    "failed to delete invocation_state on ack; sweeper will republish"
                );
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ArchiveAckError {
    #[error("failed to establish ack subscription: {0}")]
    Subscribe(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentId;
    use crate::events::{Event, EventPayload, InvocationArchiveAckedPayload};
    use crate::worker::store::InvocationStateRow;
    use std::time::Duration;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn pending_terminal_row(inv: &str) -> InvocationStateRow {
        InvocationStateRow {
            invocation_id: inv.to_string(),
            agent_id: "a".to_string(),
            schema_version: 1,
            phase: "completed".to_string(),
            state_blob: vec![],
            iteration: 0,
            started_at: 1,
            updated_at: 2,
            terminal_at: Some(2),
            workspace_ref: None,
            archive_status: None,
            archive_published_at: None,
        }
    }

    #[tokio::test]
    async fn ack_deletes_matching_invocation_state_row() {
        let Some(url) = crate::test_support::events::require_nats() else {
            return;
        };

        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let dir = tempdir().unwrap();
        let store = Arc::new(
            WorkerStore::open(&dir.path().join("events.db"))
                .await
                .expect("worker store"),
        );

        let worker_id =
            WorkerId::new(format!("ack-test-{}", Uuid::now_v7().simple())).expect("worker id");
        let invocation_id = Uuid::now_v7();
        let inv_str = invocation_id.to_string();

        // Seed a terminal pending row.
        store
            .upsert_invocation_state(&pending_terminal_row(&inv_str))
            .await
            .unwrap();
        store.set_archive_pending(&inv_str, 100).await.unwrap();
        assert!(
            store
                .get_invocation_state(&inv_str)
                .await
                .unwrap()
                .is_some()
        );

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let consumer = ArchiveAckConsumer::new(bus.clone(), worker_id.clone(), store.clone());
        let handle = tokio::spawn(consumer.run(shutdown_rx));

        // Give the consumer a moment to subscribe.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Publish an ack.
        let agent_id = AgentId::new("agent-test").unwrap();
        let event = Event::new(
            agent_id,
            invocation_id,
            EventPayload::InvocationArchiveAcked(InvocationArchiveAckedPayload {
                worker_id: worker_id.clone(),
            }),
        );
        bus.publish(&event).await.expect("publish ack");

        // Wait for the row to disappear.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            if store
                .get_invocation_state(&inv_str)
                .await
                .unwrap()
                .is_none()
            {
                break;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("invocation_state row was not deleted in time");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    // No unit test for the defense-in-depth `worker_id`
    // mismatch check: the bus does not expose a way to publish
    // on an arbitrary subject (Event::subject() is derived from
    // payload), so a misaddressed ack can only be constructed
    // via a producer bug at the bus layer. Subject routing
    // alone is the load-bearing protection; the in-handler
    // check is a tripwire.
}
