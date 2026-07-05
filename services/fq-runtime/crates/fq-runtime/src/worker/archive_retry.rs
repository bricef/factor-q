//! Worker archive retry sweeper.
//!
//! A long-lived tokio task spawned alongside the worker. It
//! periodically scans `invocation_state` for terminal rows that
//! are still in archive flow (`archive_status = "pending"` or
//! NULL with `terminal_at != NULL`) and republishes
//! `invocation.archived`. The control-plane's coordination
//! consumer is idempotent on `invocation_id`, so a duplicate
//! `archived` is safe — the second insert is a no-op and a
//! fresh ack is published, which is exactly what an offline
//! worker would have missed on first transmission.
//!
//! Two configurable timings:
//!
//! - `retry_interval_ms` — how often the sweeper wakes up.
//!   Default 10s, matching the heartbeat cadence. Lower
//!   intervals shorten the time-to-recovery after a CP
//!   restart at the cost of more NATS traffic during steady-
//!   state outages.
//!
//! - `warn_after_ms` — how long after `terminal_at` to log a
//!   warning once per pending row. Default 60s. The sweeper
//!   keeps republishing indefinitely after the warn point —
//!   the plan's "correctness over cleanup" rule says the row
//!   must be held, not deleted, even if the CP never acks.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

use crate::agent::AgentId;
use crate::bus::EventBus;
use crate::events::{Event, EventPayload, InvocationArchivedPayload};

use super::WorkerId;
use super::store::{InvocationStateRow, WorkerStore};

/// Default cadence between sweeps (10 seconds). Pair this with
/// the heartbeat producer's cadence — the two share the same
/// "I'm alive, things are flowing" rhythm.
pub const DEFAULT_RETRY_INTERVAL_MS: u64 = 10_000;

/// Default age (since terminal) at which to log a warning once
/// per pending row. The sweeper keeps republishing past this
/// point; the warn is the operator-visible signal that the
/// control-plane has not acknowledged for an unusually long
/// time.
pub const DEFAULT_WARN_AFTER_MS: i64 = 60_000;

/// Retry sweeper. Spawn its [`run`](Self::run) method as a
/// tokio task and feed it a shutdown signal.
pub struct ArchiveRetrySweeper {
    bus: EventBus,
    worker_id: WorkerId,
    store: Arc<WorkerStore>,
    retry_interval_ms: u64,
    warn_after_ms: i64,
}

impl ArchiveRetrySweeper {
    pub fn new(bus: EventBus, worker_id: WorkerId, store: Arc<WorkerStore>) -> Self {
        Self {
            bus,
            worker_id,
            store,
            retry_interval_ms: DEFAULT_RETRY_INTERVAL_MS,
            warn_after_ms: DEFAULT_WARN_AFTER_MS,
        }
    }

    /// Override the retry cadence. Production callers set this
    /// from the `[worker]` section of `fq.toml`; tests call it
    /// to shorten the loop.
    pub fn with_retry_interval_ms(mut self, ms: u64) -> Self {
        self.retry_interval_ms = ms;
        self
    }

    /// Override the warn threshold.
    pub fn with_warn_after_ms(mut self, ms: i64) -> Self {
        self.warn_after_ms = ms;
        self
    }

    /// Run the sweep loop until `shutdown` fires.
    pub async fn run(self, mut shutdown: oneshot::Receiver<()>) -> Result<(), ArchiveRetryError> {
        info!(
            worker_id = %self.worker_id,
            retry_interval_ms = self.retry_interval_ms,
            warn_after_ms = self.warn_after_ms,
            "archive retry sweeper starting"
        );
        let mut ticker = tokio::time::interval(Duration::from_millis(self.retry_interval_ms));
        // The first tick fires immediately; consume it so we
        // wait one full interval before the first sweep. This
        // gives the happy path (publish-then-ack inside
        // retry_interval_ms) a chance to finish without the
        // sweeper republishing.
        ticker.tick().await;

        // Rows we've already logged at the warn threshold —
        // prevents log spam on every sweep tick.
        let mut warned: HashSet<String> = HashSet::new();

        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => {
                    info!(
                        worker_id = %self.worker_id,
                        "archive retry sweeper received shutdown signal"
                    );
                    break;
                }
                _ = ticker.tick() => {
                    if let Err(err) = self.sweep_once(&mut warned).await {
                        warn!(
                            worker_id = %self.worker_id,
                            error = %err,
                            "archive retry sweep failed; will retry next tick"
                        );
                    }
                }
            }
        }

        info!(worker_id = %self.worker_id, "archive retry sweeper stopped");
        Ok(())
    }

    /// One sweep pass. Public for tests.
    pub async fn sweep_once(&self, warned: &mut HashSet<String>) -> Result<(), ArchiveRetryError> {
        let rows = self
            .store
            .list_archive_pending()
            .await
            .map_err(|err| ArchiveRetryError::Store(err.to_string()))?;
        let now_ms = Utc::now().timestamp_millis();
        for row in rows {
            self.maybe_warn_once(&row, now_ms, warned);
            self.republish(&row).await?;
        }
        Ok(())
    }

    fn maybe_warn_once(&self, row: &InvocationStateRow, now_ms: i64, warned: &mut HashSet<String>) {
        let Some(terminal_at_ms) = row.terminal_at else {
            return;
        };
        if now_ms - terminal_at_ms < self.warn_after_ms {
            return;
        }
        if warned.contains(&row.invocation_id) {
            return;
        }
        warn!(
            invocation_id = %row.invocation_id,
            agent_id = %row.agent_id,
            terminal_age_ms = now_ms - terminal_at_ms,
            "archive hand-off pending past warn threshold; row will be held"
        );
        warned.insert(row.invocation_id.clone());
    }

    async fn republish(&self, row: &InvocationStateRow) -> Result<(), ArchiveRetryError> {
        let Some(terminal_at_ms) = row.terminal_at else {
            // list_archive_pending filters to terminal rows, so
            // this branch is unreachable in practice. Treat as
            // a corrupt row and skip — the sweeper must not
            // crash on a malformed input.
            error!(
                invocation_id = %row.invocation_id,
                "list_archive_pending returned a non-terminal row; skipping"
            );
            return Ok(());
        };
        let agent_id = AgentId::new(&row.agent_id).map_err(|err| {
            ArchiveRetryError::InvalidAgentId(format!(
                "agent_id {:?} from invocation_state: {err}",
                row.agent_id
            ))
        })?;
        let invocation_id = uuid::Uuid::parse_str(&row.invocation_id).map_err(|err| {
            ArchiveRetryError::InvalidInvocationId(format!(
                "invocation_id {:?} from invocation_state: {err}",
                row.invocation_id
            ))
        })?;

        let event = Event::new(
            agent_id,
            invocation_id,
            EventPayload::InvocationArchived(InvocationArchivedPayload {
                worker_id: self.worker_id.clone(),
                final_phase: row.phase.clone(),
                final_state_blob: row.state_blob.clone(),
                started_at_ms: row.started_at,
                terminal_at_ms,
            }),
        );
        self.bus
            .publish(&event)
            .await
            .map_err(|err| ArchiveRetryError::Publish(err.to_string()))?;
        debug!(
            invocation_id = %row.invocation_id,
            "republished invocation.archived"
        );
        self.store
            .set_archive_pending(&row.invocation_id, Utc::now().timestamp_millis())
            .await
            .map_err(|err| ArchiveRetryError::Store(err.to_string()))?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ArchiveRetryError {
    #[error("worker store error: {0}")]
    Store(String),

    #[error("bus publish error: {0}")]
    Publish(String),

    #[error("invalid agent_id on persisted row: {0}")]
    InvalidAgentId(String),

    #[error("invalid invocation_id on persisted row: {0}")]
    InvalidInvocationId(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventPayload;
    use crate::worker::store::InvocationStateRow;
    use futures::StreamExt;
    use std::time::Duration;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn terminal_row(inv: &str, agent: &str, terminal_at_ms: i64) -> InvocationStateRow {
        InvocationStateRow {
            invocation_id: inv.to_string(),
            agent_id: agent.to_string(),
            schema_version: 1,
            phase: "completed".to_string(),
            state_blob: vec![1, 2, 3],
            iteration: 0,
            started_at: 0,
            updated_at: terminal_at_ms,
            terminal_at: Some(terminal_at_ms),
            workspace_ref: None,
            archive_status: None,
            archive_published_at: None,
            trigger_source: None,
            trigger_subject: None,
            trigger_payload: None,
        }
    }

    #[tokio::test]
    async fn sweep_republishes_pending_terminal_rows() {
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
            WorkerId::new(format!("sweep-test-{}", Uuid::now_v7().simple())).expect("worker id");
        let invocation_id = Uuid::now_v7();
        let inv_str = invocation_id.to_string();
        let agent_id = format!("agent-{}", Uuid::now_v7().simple());

        store
            .upsert_invocation_state(&terminal_row(&inv_str, &agent_id, 1_000))
            .await
            .unwrap();
        store.set_archive_pending(&inv_str, 1_000).await.unwrap();

        let mut sub = bus
            .subscribe(format!("fq.agent.{agent_id}.invocation.archived"))
            .await
            .expect("subscribe");
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Run one explicit sweep — avoids waiting for the ticker.
        let sweeper = ArchiveRetrySweeper::new(bus.clone(), worker_id.clone(), store.clone());
        let mut warned = HashSet::new();
        sweeper.sweep_once(&mut warned).await.unwrap();

        let event = tokio::time::timeout(Duration::from_secs(2), sub.next())
            .await
            .expect("republish timeout")
            .expect("stream closed")
            .expect("deserialise");
        assert_eq!(event.envelope.invocation_id, invocation_id);
        match &event.payload {
            EventPayload::InvocationArchived(p) => {
                assert_eq!(p.worker_id, worker_id);
                assert_eq!(p.final_phase, "completed");
                assert_eq!(p.final_state_blob, vec![1, 2, 3]);
                assert_eq!(p.terminal_at_ms, 1_000);
            }
            other => panic!("wrong payload variant: {other:?}"),
        }

        // Sweep bumped archive_published_at.
        let row = store.get_invocation_state(&inv_str).await.unwrap().unwrap();
        let published = row.archive_published_at.expect("publish time set");
        assert!(
            published > 1_000,
            "archive_published_at should advance past the seed value"
        );
    }

    #[tokio::test]
    async fn sweep_warns_once_after_threshold() {
        // Pure unit test against the warn-once gate; no bus,
        // no real time. The sweep_once method needs the bus to
        // republish, so we exercise the warn logic directly via
        // maybe_warn_once.
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
        let worker_id = WorkerId::new("warn-test").expect("worker id");
        let sweeper = ArchiveRetrySweeper::new(bus, worker_id, store).with_warn_after_ms(1_000);

        let row = terminal_row("inv-1", "agent-1", 100);
        let mut warned = HashSet::new();

        // now_ms - terminal_at_ms < warn_after_ms → no warn.
        sweeper.maybe_warn_once(&row, 500, &mut warned);
        assert!(warned.is_empty());

        // now_ms - terminal_at_ms >= warn_after_ms → warn once.
        sweeper.maybe_warn_once(&row, 2_000, &mut warned);
        assert_eq!(warned.len(), 1);
        assert!(warned.contains("inv-1"));

        // Calling again at a later timestamp does NOT add a
        // duplicate entry — warn-once is idempotent per row.
        sweeper.maybe_warn_once(&row, 5_000, &mut warned);
        assert_eq!(warned.len(), 1);
    }
}
