//! Periodic retention sweeps over control-plane state.
//!
//! Two windows, one task, because they are the same job on
//! different tables and an hourly tick is right for both:
//!
//! 1. **Archive + projected events** (step 10 of
//!    `data-architecture-v1`): deletes rows whose timestamp is
//!    older than `state.retention_days`. It's the consumer of
//!    the archive table that step 8 started populating —
//!    without it, the archive grows without bound.
//! 2. **Stale worker registrations**: deletes
//!    `coordination_worker` rows that went stale longer ago
//!    than `state.stale_worker_retention_days`.
//!
//! ## Why (2) is a daemon task and not an operator verb
//!
//! `coordination_worker` is primary state, not a fold —
//! `register_worker` writes it directly and the coordination
//! consumer flips `status` on heartbeat timeout — and
//! `worker_id` *is* the daemon's `runtime_id`, a fresh UUID per
//! run. So the table gains a row on every restart and grows
//! without bound. The only thing that ever reclaimed those rows
//! was `fq workers prune`, which needed a human to remember it.  allow-dead-command: retired verb, named as history
//!
//! **The system should not depend on operator remediations to
//! work normally.** So the verb was retired and the reclamation
//! moved here. It is not an evented command and emits no
//! `worker_pruned` event: nothing decided anything an operator
//! could have decided differently, and the alive→stale
//! transition that *is* interesting already publishes
//! `worker.orphaned`.
//!
//! ## Stale is not prunable
//!
//! A worker is marked `stale` after ~30s of missed heartbeats.
//! That threshold is aggressive on purpose — orphan recovery
//! wants to react while the work is fresh. Deleting on the same
//! threshold would delete the operator's only evidence that a
//! worker died, and could remove the row out from under a
//! `worker.orphaned` that has not been acted on yet. The
//! deletion window is therefore its own, much longer setting;
//! see [`crate::config::StateConfig::stale_worker_retention_days`].
//!
//! ## Behaviour
//! - Either window at `>= 0` → that sweep is active.
//! - Either window at `< 0` → that sweep is skipped. The task
//!   only exits early when *both* are disabled.
//! - Each tick emits an `info!` log line with the rows-
//!   deleted count (including zero), so operators can see
//!   the sweep is alive.
//! - The sweeps are idempotent: deleting the same
//!   already-deleted rows on the next tick is a no-op.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::oneshot;
use tracing::{info, warn};

use super::projection::ProjectionStore;
use super::store::{ControlPlaneStore, OwnerStatus, WorkerStatus};
use crate::config::StateConfig;

/// Number of milliseconds in one day. Used to convert
/// `retention_days` into the cutoff offset the store query
/// expects.
const MS_PER_DAY: i64 = 24 * 60 * 60 * 1000;

/// Pure cutoff calculation: `now_ms - retention_days * 1d`.
/// Tests can exercise this independently of any store.
pub fn sweep_cutoff_ms(now_ms: i64, retention_days: i64) -> i64 {
    now_ms.saturating_sub(retention_days.saturating_mul(MS_PER_DAY))
}

/// Does an ownership row still need the worker registration it
/// points at?
///
/// `coordination_invocation_owner.worker_id` is a plain column,
/// not a foreign key, so deleting the worker leaves a dangling
/// reference rather than failing loudly — which is exactly why
/// this predicate is spelled out.
///
/// Written as an exhaustive `match` rather than a `matches!`: a
/// new [`OwnerStatus`] variant must fail to compile here, so
/// whoever adds it has to say whether it pins a worker row.
pub fn owner_status_pins_worker(status: OwnerStatus) -> bool {
    match status {
        // Unresolved. `in_flight` is work the control plane still
        // believes is running; `ambiguous` is work whose outcome
        // is unknown. Both are resolved by `fq invocation resume`/`drop`, which
        // reads `worker_id` back to decide what to re-drive.
        OwnerStatus::InFlight | OwnerStatus::Ambiguous => true,
        // Terminal. The record of what happened is in
        // `invocation_archive`; the worker row is no longer load-
        // bearing for it.
        OwnerStatus::Completed | OwnerStatus::Failed => false,
    }
}

/// May this worker registration row be deleted?
///
/// The whole policy in one place, and pure so it can be tested
/// without a pool — the same split that keeps [`sweep_cutoff_ms`]
/// honest.
///
/// Three independent conditions, each of which has bitten
/// something:
/// - **status is `Stale`.** Alive workers are in use. Shutdown
///   workers exited cleanly and are kept as history under the
///   archive's window, not this one.
/// - **the lapse is older than the retention cutoff.** Not the
///   30s stale threshold — see the module docs.
/// - **nothing live still points here.** See
///   [`owner_status_pins_worker`].
pub fn worker_is_collectable(
    status: WorkerStatus,
    last_heartbeat_ms: i64,
    cutoff_ms: i64,
    pinned_by_live_work: bool,
) -> bool {
    status == WorkerStatus::Stale && last_heartbeat_ms < cutoff_ms && !pinned_by_live_work
}

/// Periodic retention sweep task.
pub struct RetentionSweeper {
    store: Arc<ControlPlaneStore>,
    projection_store: Option<Arc<ProjectionStore>>,
    retention_days: i64,
    stale_worker_retention_days: i64,
    sweep_interval_seconds: u64,
}

impl RetentionSweeper {
    /// Configured by `[state]` wholesale rather than knob-by-knob:
    /// every window this task honours is a `[state]` key, so
    /// handing it the section keeps the next one from churning
    /// this signature and every caller of it.
    pub fn new(store: Arc<ControlPlaneStore>, state: &StateConfig) -> Self {
        Self {
            store,
            projection_store: None,
            retention_days: state.retention_days,
            stale_worker_retention_days: state.stale_worker_retention_days,
            sweep_interval_seconds: state.sweep_interval_seconds,
        }
    }

    /// Include the rebuildable event projection in each scheduled sweep.
    pub fn with_projection_store(mut self, store: Arc<ProjectionStore>) -> Self {
        self.projection_store = Some(store);
        self
    }

    /// Run until `shutdown` fires. Exits immediately (with a log
    /// line) only when *both* windows are disabled — a `-1` on
    /// one must not silently switch off the other, since they
    /// bound unrelated tables.
    pub async fn run(self, mut shutdown: oneshot::Receiver<()>) {
        if self.retention_days < 0 && self.stale_worker_retention_days < 0 {
            info!(
                retention_days = self.retention_days,
                stale_worker_retention_days = self.stale_worker_retention_days,
                "retention sweep disabled (every window < 0)"
            );
            // Still observe the shutdown channel so the
            // caller's join_handle drains cleanly.
            let _ = shutdown.await;
            return;
        }

        info!(
            retention_days = self.retention_days,
            stale_worker_retention_days = self.stale_worker_retention_days,
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
        let (archive_deleted, event_deleted) = if self.retention_days < 0 {
            (0, 0)
        } else {
            self.sweep_archive_and_events(sweep_cutoff_ms(now_ms, self.retention_days))
                .await?
        };
        let worker_deleted = self.sweep_stale_workers(now_ms).await?;
        let deleted = archive_deleted + event_deleted + worker_deleted;
        if deleted > 0 {
            info!(
                deleted_rows = deleted,
                archive_deleted_rows = archive_deleted,
                event_deleted_rows = event_deleted,
                stale_worker_deleted_rows = worker_deleted,
                retention_days = self.retention_days,
                stale_worker_retention_days = self.stale_worker_retention_days,
                "retention sweep deleted rows"
            );
        } else {
            // Log even on no-op so an operator tailing the
            // log can see the task is alive.
            info!(
                deleted_rows = 0u64,
                "retention sweep tick (no rows past cutoff)"
            );
        }
        Ok(())
    }

    /// The step-10 half: archived invocations and projected events.
    async fn sweep_archive_and_events(
        &self,
        cutoff_ms: i64,
    ) -> Result<(u64, u64), super::store::ControlPlaneStoreError> {
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
        Ok((archive_deleted, event_deleted))
    }

    /// The membership half: `coordination_worker` rows that went
    /// stale before the cutoff and that no live invocation pins.
    ///
    /// Row-at-a-time rather than one `DELETE ... WHERE`, because
    /// the ownership guard is not expressible as a cheap join we'd
    /// want to trust here, and the candidate set is tiny — one row
    /// per daemon restart, filtered to those stale for *days*. The
    /// per-candidate probe is index-covered
    /// (`idx_owner_worker_status`).
    async fn sweep_stale_workers(
        &self,
        now_ms: i64,
    ) -> Result<u64, super::store::ControlPlaneStoreError> {
        if self.stale_worker_retention_days < 0 {
            return Ok(0);
        }
        let cutoff_ms = sweep_cutoff_ms(now_ms, self.stale_worker_retention_days);
        let mut deleted = 0u64;
        for worker in self.store.list_workers().await? {
            // First pass with `pinned = false`: a row that fails on
            // status or age can never be collectable however its
            // ownership looks, so only real candidates pay for the
            // query below.
            if !worker_is_collectable(worker.status, worker.last_heartbeat, cutoff_ms, false) {
                continue;
            }
            let pinned = self
                .store
                .list_invocations_for_worker(&worker.worker_id)
                .await?
                .into_iter()
                .any(|owned| owner_status_pins_worker(owned.status));
            if !worker_is_collectable(worker.status, worker.last_heartbeat, cutoff_ms, pinned) {
                // Loud, because this is a stuck invocation, not a
                // tidy-up problem: something has been in_flight or
                // ambiguous on a dead worker for the whole retention
                // window and no operator has resolved it.
                warn!(
                    worker_id = %worker.worker_id,
                    "stale worker past its retention window still owns unresolved \
                     invocations; keeping the row so `fq invocation resume`/`drop` can still \
                      resolve them"
                );
                continue;
            }
            if self.store.delete_worker(&worker.worker_id).await? {
                deleted += 1;
                info!(
                    worker_id = %worker.worker_id,
                    last_heartbeat_ms = worker.last_heartbeat,
                    cutoff_ms,
                    "collected stale worker registration"
                );
            }
        }
        Ok(deleted)
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

    /// `[state]` with only the archive window on — the shape every
    /// pre-existing test here assumed when `new` took loose ints.
    fn archive_only(retention_days: i64) -> StateConfig {
        StateConfig {
            retention_days,
            stale_worker_retention_days: -1,
            sweep_interval_seconds: 3600,
            ..Default::default()
        }
    }

    /// `[state]` with only the stale-worker window on.
    fn workers_only(stale_worker_retention_days: i64) -> StateConfig {
        StateConfig {
            retention_days: -1,
            stale_worker_retention_days,
            sweep_interval_seconds: 3600,
            ..Default::default()
        }
    }

    async fn fresh_store() -> (Arc<ControlPlaneStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("cp.db"))
                .await
                .unwrap(),
        );
        (store, dir)
    }

    /// Register a worker, then backdate its heartbeat to `age_days` ago
    /// and put it in `status`. Backdating is how a test reaches a
    /// retention window measured in days without waiting.
    async fn seed_worker(
        store: &ControlPlaneStore,
        id: &str,
        age_days: i64,
        status: WorkerStatus,
    ) -> i64 {
        let heartbeat_ms = Utc::now().timestamp_millis() - age_days * MS_PER_DAY;
        store
            .register_worker(id, "test-host", heartbeat_ms)
            .await
            .unwrap();
        match status {
            WorkerStatus::Alive => {}
            WorkerStatus::Stale => assert!(store.mark_worker_stale(id).await.unwrap()),
            WorkerStatus::Shutdown => store.mark_worker_shutdown(id).await.unwrap(),
        }
        heartbeat_ms
    }

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

        let sweeper = RetentionSweeper::new(store.clone(), &archive_only(1));
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
                    trigger_id: None,
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
        projection.insert_event(&old_event, None).await.unwrap();
        projection
            .insert_event(&triggered("fresh-agent"), None)
            .await
            .unwrap();

        let sweeper = RetentionSweeper::new(store.clone(), &archive_only(1))
            .with_projection_store(projection.clone());
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
        let sweeper = RetentionSweeper::new(store, &archive_only(1));
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

        let sweeper = RetentionSweeper::new(store.clone(), &archive_only(1));
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
        let sweeper = RetentionSweeper::new(store, &archive_only(-1));
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

    // ---------------------------------------------------------------
    // Stale-worker collection.
    //
    // The decision function first, exhaustively and without a pool,
    // then the store-backed sweep to prove the wiring agrees with it.
    // ---------------------------------------------------------------

    /// The one thing that must never regress: `stale` alone is not a
    // allow-dead-command: `fq workers prune` is retired; the doc says so.
    /// licence to delete. The retired `fq workers prune` deleted on
    /// exactly that predicate, which is why it could not become a
    /// timer.
    #[test]
    fn stale_status_alone_does_not_authorise_deletion() {
        let cutoff = 1_000_000i64;
        // Stale, but the lapse is newer than the cutoff — this is the
        // ~30s-old row `fq workers list --stale-only` exists to show.
        assert!(!worker_is_collectable(
            WorkerStatus::Stale,
            cutoff + 1,
            cutoff,
            false
        ));
        // Same row, aged past the window: now collectable.
        assert!(worker_is_collectable(
            WorkerStatus::Stale,
            cutoff - 1,
            cutoff,
            false
        ));
    }

    #[test]
    fn only_stale_workers_are_ever_collectable() {
        let cutoff = 1_000_000i64;
        let ancient = 0i64;
        for status in [WorkerStatus::Alive, WorkerStatus::Shutdown] {
            assert!(
                !worker_is_collectable(status, ancient, cutoff, false),
                "{status:?} must survive the sweep at any age"
            );
        }
        assert!(worker_is_collectable(
            WorkerStatus::Stale,
            ancient,
            cutoff,
            false
        ));
    }

    #[test]
    fn live_ownership_vetoes_an_otherwise_collectable_worker() {
        let cutoff = 1_000_000i64;
        assert!(worker_is_collectable(WorkerStatus::Stale, 0, cutoff, false));
        assert!(
            !worker_is_collectable(WorkerStatus::Stale, 0, cutoff, true),
            "a worker that still owns unresolved work is never collectable, \
             however old the lapse"
        );
    }

    /// Which owner statuses pin the worker row, spelled out per variant
    /// so the table is reviewable rather than implied by a `matches!`.
    #[test]
    fn unresolved_owner_statuses_pin_the_worker_row() {
        assert!(owner_status_pins_worker(OwnerStatus::InFlight));
        assert!(owner_status_pins_worker(OwnerStatus::Ambiguous));
        assert!(!owner_status_pins_worker(OwnerStatus::Completed));
        assert!(!owner_status_pins_worker(OwnerStatus::Failed));
    }

    #[tokio::test]
    async fn sweep_collects_stale_worker_past_the_window() {
        let (store, _dir) = fresh_store().await;
        seed_worker(&store, "long-dead", 30, WorkerStatus::Stale).await;

        RetentionSweeper::new(store.clone(), &workers_only(7))
            .sweep_now()
            .await
            .unwrap();

        assert!(store.get_worker("long-dead").await.unwrap().is_none());
    }

    /// Everything the sweep must leave alone, in one store: a stale
    /// worker inside the window, and alive/shutdown workers old enough
    /// to be collected if status were not checked.
    #[tokio::test]
    async fn sweep_spares_recent_stale_and_every_non_stale_worker() {
        let (store, _dir) = fresh_store().await;
        seed_worker(&store, "recently-stale", 1, WorkerStatus::Stale).await;
        seed_worker(&store, "ancient-alive", 400, WorkerStatus::Alive).await;
        seed_worker(&store, "ancient-shutdown", 400, WorkerStatus::Shutdown).await;

        RetentionSweeper::new(store.clone(), &workers_only(7))
            .sweep_now()
            .await
            .unwrap();

        for id in ["recently-stale", "ancient-alive", "ancient-shutdown"] {
            assert!(
                store.get_worker(id).await.unwrap().is_some(),
                "{id} must survive the sweep"
            );
        }
    }

    /// The guard that matters most now the sweep is on a timer: a dead
    /// worker whose invocations were never recovered keeps its row, so
    /// `fq invocation list --status=ambiguous` can still follow `worker_id` back to them.
    ///
    /// Reachable, not theoretical: nothing consumes `worker.orphaned`.
    /// It is published for observability and never handled, so the only
    /// things that clear an `in_flight` owner row are the worker's own
    /// `invocation.archived` (which a dead worker never sends) and the
    /// operator recovery path. A worker can therefore sit stale, owning
    /// live work, indefinitely — well past any retention window.
    #[tokio::test]
    async fn sweep_spares_stale_worker_that_still_owns_live_work() {
        let (store, _dir) = fresh_store().await;
        let heartbeat = seed_worker(&store, "dead-but-owning", 30, WorkerStatus::Stale).await;
        store
            .assign_invocation("inv-unrecovered", "dead-but-owning", heartbeat)
            .await
            .unwrap();

        RetentionSweeper::new(store.clone(), &workers_only(7))
            .sweep_now()
            .await
            .unwrap();

        assert!(
            store.get_worker("dead-but-owning").await.unwrap().is_some(),
            "deleting this row strands `inv-unrecovered`: its worker_id \
             would point at nothing"
        );
    }

    /// The complement: once the same invocation reaches a terminal
    /// status, nothing is pinning the row and the next tick takes it.
    #[tokio::test]
    async fn sweep_collects_once_owned_work_is_resolved() {
        let (store, _dir) = fresh_store().await;
        let heartbeat = seed_worker(&store, "dead-and-done", 30, WorkerStatus::Stale).await;
        store
            .assign_invocation("inv-finished", "dead-and-done", heartbeat)
            .await
            .unwrap();
        let sweeper = RetentionSweeper::new(store.clone(), &workers_only(7));

        sweeper.sweep_now().await.unwrap();
        assert!(store.get_worker("dead-and-done").await.unwrap().is_some());

        store
            .upsert_invocation_ownership(
                "inv-finished",
                "dead-and-done",
                heartbeat,
                OwnerStatus::Completed,
            )
            .await
            .unwrap();

        sweeper.sweep_now().await.unwrap();
        assert!(store.get_worker("dead-and-done").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn negative_stale_worker_retention_disables_collection() {
        let (store, _dir) = fresh_store().await;
        seed_worker(&store, "long-dead", 400, WorkerStatus::Stale).await;

        RetentionSweeper::new(store.clone(), &workers_only(-1))
            .sweep_now()
            .await
            .unwrap();

        assert!(
            store.get_worker("long-dead").await.unwrap().is_some(),
            "the -1 sentinel must disable collection outright"
        );
    }

    /// The two windows are independent: disabling the archive sweep
    /// must not disable worker collection, and vice versa. This is the
    /// regression the shared `run()` early-exit would otherwise cause.
    #[tokio::test]
    async fn each_window_disables_only_its_own_sweep() {
        use super::super::store::InvocationArchiveRow;

        let aged_archive = |id: &str| InvocationArchiveRow {
            invocation_id: id.to_string(),
            agent_id: "a".to_string(),
            final_phase: "completed".to_string(),
            final_state_blob: vec![],
            started_at: 0,
            terminal_at: 0,
            archived_at: 0,
        };

        // Archive window off, worker window on.
        let (store, _dir) = fresh_store().await;
        store.insert_archive(&aged_archive("arch")).await.unwrap();
        seed_worker(&store, "w", 30, WorkerStatus::Stale).await;
        RetentionSweeper::new(store.clone(), &workers_only(7))
            .sweep_now()
            .await
            .unwrap();
        assert!(store.get_archive("arch").await.unwrap().is_some());
        assert!(store.get_worker("w").await.unwrap().is_none());

        // Worker window off, archive window on.
        let (store, _dir) = fresh_store().await;
        store.insert_archive(&aged_archive("arch")).await.unwrap();
        seed_worker(&store, "w", 30, WorkerStatus::Stale).await;
        RetentionSweeper::new(store.clone(), &archive_only(7))
            .sweep_now()
            .await
            .unwrap();
        assert!(store.get_archive("arch").await.unwrap().is_none());
        assert!(store.get_worker("w").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn worker_sweep_is_idempotent_and_empty_safe() {
        let (store, _dir) = fresh_store().await;
        let sweeper = RetentionSweeper::new(store.clone(), &workers_only(7));
        // Empty table: no panic, no error.
        sweeper.sweep_now().await.unwrap();

        seed_worker(&store, "long-dead", 30, WorkerStatus::Stale).await;
        sweeper.sweep_now().await.unwrap();
        sweeper.sweep_now().await.unwrap();
        assert!(store.get_worker("long-dead").await.unwrap().is_none());
    }

    /// A sweeper with only the worker window enabled must still run —
    /// the early exit is for "every window disabled", not "the archive
    /// window disabled".
    #[tokio::test]
    async fn worker_window_alone_keeps_the_task_running() {
        let (store, _dir) = fresh_store().await;
        let sweeper = RetentionSweeper::new(store, &workers_only(7));
        let (tx, rx) = oneshot::channel();
        let handle = tokio::spawn(sweeper.run(rx));
        // Not the disabled path, so it is sitting on the ticker; the
        // shutdown signal is what ends it.
        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("sweeper should join after shutdown")
            .expect("task panic");
    }
}
