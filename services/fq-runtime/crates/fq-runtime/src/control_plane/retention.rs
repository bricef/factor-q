//! Periodic retention sweep for `invocation_archive` rows.
//!
//! Step 10 of `data-architecture-v1`. The sweep is a
//! control-plane background task that deletes rows whose
//! `archived_at` is older than `state.retention_days`. It's
//! the consumer of the archive table that step 8 started
//! populating — without it, the archive grows without bound.
//!
//! Behaviour:
//! - `retention_days >= 0` → sweep is active.
//! - `retention_days < 0`  → sweep is disabled and the task
//!   exits immediately after logging the choice.
//! - Each tick emits an `info!` log line with the rows-
//!   deleted count (including zero), so operators can see
//!   the sweep is alive.
//! - The sweep itself is idempotent: deleting the same
//!   already-deleted rows on the next tick is a no-op.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::oneshot;
use tracing::{info, warn};

use super::projection::ProjectionStore;
use super::store::ControlPlaneStore;

/// Number of milliseconds in one day. Used to convert
/// `retention_days` into the cutoff offset the store query
/// expects.
const MS_PER_DAY: i64 = 24 * 60 * 60 * 1000;

/// Pure cutoff calculation: `now_ms - retention_days * 1d`.
/// Tests can exercise this independently of any store.
pub fn sweep_cutoff_ms(now_ms: i64, retention_days: i64) -> i64 {
    now_ms.saturating_sub(retention_days.saturating_mul(MS_PER_DAY))
}

/// Periodic retention sweep task.
pub struct RetentionSweeper {
    store: Arc<ControlPlaneStore>,
    projection_store: Option<Arc<ProjectionStore>>,
    retention_days: i64,
    sweep_interval_seconds: u64,
}

impl RetentionSweeper {
    pub fn new(
        store: Arc<ControlPlaneStore>,
        retention_days: i64,
        sweep_interval_seconds: u64,
    ) -> Self {
        Self {
            store,
            projection_store: None,
            retention_days,
            sweep_interval_seconds,
        }
    }

    /// Include the rebuildable event projection in each scheduled sweep.
    pub fn with_projection_store(mut self, store: Arc<ProjectionStore>) -> Self {
        self.projection_store = Some(store);
        self
    }

    /// Run until `shutdown` fires. Exits immediately (with a
    /// log line) when the sweep is disabled
    /// (`retention_days < 0`).
    pub async fn run(self, mut shutdown: oneshot::Receiver<()>) {
        if self.retention_days < 0 {
            info!(
                retention_days = self.retention_days,
                "retention sweep disabled (retention_days < 0)"
            );
            // Still observe the shutdown channel so the
            // caller's join_handle drains cleanly.
            let _ = shutdown.await;
            return;
        }

        info!(
            retention_days = self.retention_days,
            sweep_interval_seconds = self.sweep_interval_seconds,
            "retention sweep starting"
        );
        let mut ticker = tokio::time::interval(Duration::from_secs(self.sweep_interval_seconds));
        // The first tick fires immediately. Consume it so we
        // wait one full interval before the first delete —
        // matches the worker's archive_retry pattern and lets
        // tests prove "no work happens at T+0" with a quick
        // probe.
        ticker.tick().await;

        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => {
                    info!("retention sweep received shutdown signal");
                    break;
                }
                _ = ticker.tick() => {
                    if let Err(err) = self.sweep_once().await {
                        warn!(error = %err, "retention sweep tick failed; will retry");
                    }
                }
            }
        }
    }

    async fn sweep_once(&self) -> Result<(), super::store::ControlPlaneStoreError> {
        let now_ms = Utc::now().timestamp_millis();
        let cutoff_ms = sweep_cutoff_ms(now_ms, self.retention_days);
        let archive_deleted = self.store.sweep_archive(cutoff_ms).await?;
        let event_deleted = if let Some(store) = &self.projection_store {
            store.sweep_events(cutoff_ms).await.map_err(|err| {
                super::store::ControlPlaneStoreError::Backend(format!(
                    "projection retention sweep failed: {err}"
                ))
            })?
        } else {
            0
        };
        let deleted = archive_deleted + event_deleted;
        if deleted > 0 {
            info!(
                deleted_rows = deleted,
                archive_deleted_rows = archive_deleted,
                event_deleted_rows = event_deleted,
                cutoff_ms,
                retention_days = self.retention_days,
                "retention sweep deleted rows"
            );
        } else {
            // Log even on no-op so an operator tailing the
            // log can see the task is alive.
            info!(
                deleted_rows = 0u64,
                cutoff_ms, "retention sweep tick (no rows past cutoff)"
            );
        }
        Ok(())
    }

    /// Run a single sweep without ticking. Exposed for tests
    /// so they don't have to wait for the interval timer.
    #[cfg(test)]
    pub(crate) async fn sweep_now(&self) -> Result<(), super::store::ControlPlaneStoreError> {
        self.sweep_once().await
    }
}

#[cfg(test)]
#[allow(unused_imports)]
mod tests {
    use super::*;

    #[test]
    fn cutoff_subtracts_retention_in_ms() {
        let now: i64 = 1_700_000_000_000;
        assert_eq!(sweep_cutoff_ms(now, 0), now);
        assert_eq!(sweep_cutoff_ms(now, 1), now - MS_PER_DAY);
        assert_eq!(sweep_cutoff_ms(now, 30), now - 30 * MS_PER_DAY);
    }

    #[test]
    fn cutoff_saturates_for_huge_retention() {
        // i64 doesn't blow up; saturating_mul + saturating_sub
        // give us a floor at i64::MIN. Practical retention
        // is in single-digit years; this is a defence-in-
        // depth check.
        let now: i64 = 0;
        let result = sweep_cutoff_ms(now, i64::MAX);
        assert!(result <= 0);
    }

    #[tokio::test]
    async fn sweep_once_deletes_only_aged_rows() {
        use super::super::store::InvocationArchiveRow;
        use chrono::Utc;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("cp.db"))
                .await
                .unwrap(),
        );
        let now_ms = Utc::now().timestamp_millis();
        // One row "3 days old" — past the 1-day cutoff.
        store
            .insert_archive(&InvocationArchiveRow {
                invocation_id: "old".to_string(),
                agent_id: "a".to_string(),
                final_phase: "completed".to_string(),
                final_state_blob: vec![],
                started_at: now_ms - 3 * MS_PER_DAY,
                terminal_at: now_ms - 3 * MS_PER_DAY,
                archived_at: now_ms - 3 * MS_PER_DAY,
            })
            .await
            .unwrap();
        // One row "12 hours old" — inside the 1-day cutoff.
        store
            .insert_archive(&InvocationArchiveRow {
                invocation_id: "recent".to_string(),
                agent_id: "a".to_string(),
                final_phase: "completed".to_string(),
                final_state_blob: vec![],
                started_at: now_ms - MS_PER_DAY / 2,
                terminal_at: now_ms - MS_PER_DAY / 2,
                archived_at: now_ms - MS_PER_DAY / 2,
            })
            .await
            .unwrap();

        let sweeper = RetentionSweeper::new(store.clone(), 1, 3600);
        sweeper.sweep_now().await.unwrap();

        assert!(store.get_archive("old").await.unwrap().is_none());
        assert!(store.get_archive("recent").await.unwrap().is_some());
    }

    /// With a projection store attached, one scheduled sweep prunes
    /// both stores — covers the daemon wiring, the combined
    /// accounting, and the projection error-mapping path (#175).
    #[tokio::test]
    async fn sweep_with_projection_store_prunes_both_stores() {
        use super::super::projection::ProjectionStore;
        use super::super::store::InvocationArchiveRow;
        use crate::agent::AgentId;
        use crate::events::{
            ConfigSnapshot, Event, EventPayload, SandboxSnapshot, TriggerSource, TriggeredPayload,
        };
        use chrono::Utc;
        use tempfile::tempdir;
        use uuid::Uuid;

        fn triggered(agent: &str) -> Event {
            Event::new(
                AgentId::new(agent).expect("test agent id must be valid"),
                Uuid::now_v7(),
                EventPayload::Triggered(TriggeredPayload {
                    trigger_source: TriggerSource::Manual,
                    trigger_subject: None,
                    trigger_payload: serde_json::json!({}),
                    config_snapshot: ConfigSnapshot {
                        name: agent.to_string(),
                        model: "claude-haiku-4-5".to_string(),
                        system_prompt: "You are a test.".to_string(),
                        tools: vec![],
                        sandbox: SandboxSnapshot::default(),
                        budget: None,
                        ..Default::default()
                    },
                }),
            )
        }

        let dir = tempdir().unwrap();
        let store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("cp.db"))
                .await
                .unwrap(),
        );
        let projection = Arc::new(
            ProjectionStore::open(&dir.path().join("projection.db"))
                .await
                .unwrap(),
        );

        let now_ms = Utc::now().timestamp_millis();
        store
            .insert_archive(&InvocationArchiveRow {
                invocation_id: "old".to_string(),
                agent_id: "a".to_string(),
                final_phase: "completed".to_string(),
                final_state_blob: vec![],
                started_at: now_ms - 3 * MS_PER_DAY,
                terminal_at: now_ms - 3 * MS_PER_DAY,
                archived_at: now_ms - 3 * MS_PER_DAY,
            })
            .await
            .unwrap();

        // One projected event backdated past the 1-day cutoff (the
        // insert binds the envelope timestamp), one fresh.
        let mut old_event = triggered("old-agent");
        old_event.envelope.timestamp = Utc::now() - chrono::Duration::days(3);
        projection.insert_event(&old_event).await.unwrap();
        projection
            .insert_event(&triggered("fresh-agent"))
            .await
            .unwrap();

        let sweeper =
            RetentionSweeper::new(store.clone(), 1, 3600).with_projection_store(projection.clone());
        sweeper.sweep_now().await.unwrap();

        assert!(store.get_archive("old").await.unwrap().is_none());
        assert_eq!(projection.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn sweep_handles_empty_archive() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("cp.db"))
                .await
                .unwrap(),
        );
        let sweeper = RetentionSweeper::new(store, 1, 3600);
        // No panic, no error on an empty table.
        sweeper.sweep_now().await.unwrap();
    }

    #[tokio::test]
    async fn sweep_idempotent_across_runs() {
        use super::super::store::InvocationArchiveRow;
        use chrono::Utc;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("cp.db"))
                .await
                .unwrap(),
        );
        let now_ms = Utc::now().timestamp_millis();
        store
            .insert_archive(&InvocationArchiveRow {
                invocation_id: "old".to_string(),
                agent_id: "a".to_string(),
                final_phase: "completed".to_string(),
                final_state_blob: vec![],
                started_at: now_ms - 5 * MS_PER_DAY,
                terminal_at: now_ms - 5 * MS_PER_DAY,
                archived_at: now_ms - 5 * MS_PER_DAY,
            })
            .await
            .unwrap();

        let sweeper = RetentionSweeper::new(store.clone(), 1, 3600);
        sweeper.sweep_now().await.unwrap();
        sweeper.sweep_now().await.unwrap();
        // Still gone, no panic on second run.
        assert!(store.get_archive("old").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn disabled_sweep_returns_on_shutdown_without_work() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("cp.db"))
                .await
                .unwrap(),
        );
        let sweeper = RetentionSweeper::new(store, -1, 1);
        let (tx, rx) = oneshot::channel();
        let handle = tokio::spawn(sweeper.run(rx));
        // Disabled-mode immediately awaits shutdown. Fire it.
        tx.send(()).unwrap();
        // Should join near-instantly. 1s deadline is plenty.
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("disabled sweeper should join after shutdown")
            .expect("task panic");
    }
}
