//! Periodic retention sweeps over control-plane state.
//!
//! Three windows, one task, because they are the same job on
//! different tables and an hourly tick is right for all of them:
//!
//! 1. **Archive + projected events** (step 10 of
//!    `data-architecture-v1`): deletes rows whose timestamp is
//!    older than `state.retention_days`. It's the consumer of
//!    the archive table that step 8 started populating —
//!    without it, the archive grows without bound.
//! 2. **Stale worker registrations**: deletes
//!    `coordination_worker` rows that went stale longer ago
//!    than `state.stale_worker_retention_days`.
//! 3. **Trigger claims**: deletes `trigger_claim` rows claimed
//!    longer ago than `state.trigger_claim_retention_days`. See
//!    [the trigger-claim window](#the-trigger-claim-window).
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
//! ## The trigger-claim window
//!
//! The dispatcher writes one `trigger_claim` row per trigger
//! delivery (ADR-0032), and a row marked `durably_started` was
//! never deleted, so the table grew by one row per trigger for the
//! life of the instance. The window deletes:
//!
//! - a `durably_started` row claimed before the cutoff, whatever
//!   its stream incarnation;
//! - a `claimed` row claimed before the cutoff and held by any
//!   claimant but this daemon's dispatcher: a claim a process that
//!   has exited left behind.
//!
//! It never deletes a `claimed` row this daemon holds, at any age
//! (it may be a trigger the daemon is still holding behind a paused
//! model or a full agent), nor any row inside the cutoff. A sweeper
//! that has not been told its daemon's claimant
//! ([`RetentionSweeper::with_claimant`]) deletes no `claimed` row,
//! because it cannot tell which are live.
//!
//! Both deletions rest on one fact: a claim is never older than its
//! message, and the trigger stream drops a message older than its
//! maximum age, so a row older than that names a message that is
//! gone and that nothing can deliver again
//! ([`crate::control_plane::store::ControlPlaneStore::sweep_started_trigger_claims`]
//! has the argument). The config refuses a window shorter than that
//! age plus a day ([`crate::config::trigger_claim_retention_floor_days`]),
//! and the sweep skips one that reaches it some other way — a
//! `StateConfig` built in code — with a warning naming the window and
//! the floor.
//!
//! The same number is `fq doctor`'s horizon: the report stops
//! listing a claim an exited process left once it is this old, the
//! age at which this sweep deletes it. **The ownership table has no
//! retention, and three things depend on that.** The first costs a
//! duplicate run, not a wrong report: startup recovery
//! ([`crate::control_plane::dispatcher::TriggerClaims::recover`])
//! marks the claim of a run that finished while its claim still said
//! `claimed`, and once the run is archived and its WAL row deleted,
//! the run's row in `coordination_invocation_owner` is the only
//! record recovery finds it by (#967). If that table ever gains a
//! window shorter than this one, such a claim is no longer marked at
//! the next start once the run's row is gone, and a redelivered or
//! replayed copy of its trigger, while the stream still holds the
//! message, is adopted and runs the work a second time. The report
//! reads the same row to tell a claim whose run finished from one
//! that never started; with it gone, an old `claimed` row whose run
//! finished loses its record of the run, and the report would turn
//! "started, never marked" into "never started". And once the
//! archive window above has swept a finished run's archive row,
//! [`crate::views::Views::invocation`] finds the run by its
//! ownership row (or by its cost-bearing projection rows, which this
//! sweep keeps, if it made a model call). That is what lets
//! `fq invocation show` name the trigger this window keeps past
//! `retention_days`; a shorter window on the ownership table could
//! leave such a run unfindable while its claim, and so its trigger
//! link, is still kept. Keep any such window at or above this one;
//! for startup recovery and the report, the alternative is to give
//! the claim its own record of the start.
//!
//! ## Behaviour
//! - Any window at `>= 0` → that sweep is active.
//! - Any window at `< 0` → that sweep is skipped. The task
//!   only exits early when *every* window is disabled.
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

/// The cutoff the trigger-claim window sweeps at, or `None` when
/// `retention_days` is the `< 0` that keeps claim rows for ever.
pub fn trigger_claim_cutoff_ms(now_ms: i64, retention_days: i64) -> Option<i64> {
    (retention_days >= 0).then(|| sweep_cutoff_ms(now_ms, retention_days))
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
    trigger_claim_retention_days: i64,
    /// The worker id this daemon's dispatcher claims triggers under.
    /// `None` until [`Self::with_claimant`]: the trigger-claim window
    /// then deletes no `claimed` row, since any of them could be live.
    claimant: Option<String>,
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
            trigger_claim_retention_days: state.trigger_claim_retention_days,
            claimant: None,
            sweep_interval_seconds: state.sweep_interval_seconds,
        }
    }

    /// Include the rebuildable event projection in each scheduled sweep.
    pub fn with_projection_store(mut self, store: Arc<ProjectionStore>) -> Self {
        self.projection_store = Some(store);
        self
    }

    /// The worker id this daemon's dispatcher claims triggers under —
    /// the one id whose `claimed` rows the trigger-claim window must
    /// never delete. Every other claimant has exited, so its old
    /// `claimed` rows go on the window's schedule.
    pub fn with_claimant(mut self, claimant: impl Into<String>) -> Self {
        self.claimant = Some(claimant.into());
        self
    }

    /// Run until `shutdown` fires. Exits immediately (with a log
    /// line) only when *every* window is disabled — a `-1` on
    /// one must not silently switch off another, since they
    /// bound unrelated tables.
    pub async fn run(self, mut shutdown: oneshot::Receiver<()>) {
        if self.retention_days < 0
            && self.stale_worker_retention_days < 0
            && self.trigger_claim_retention_days < 0
        {
            info!(
                retention_days = self.retention_days,
                stale_worker_retention_days = self.stale_worker_retention_days,
                trigger_claim_retention_days = self.trigger_claim_retention_days,
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
            trigger_claim_retention_days = self.trigger_claim_retention_days,
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
        let (archive_deleted, event_deleted, signal_deleted) = if self.retention_days < 0 {
            (0, 0, 0)
        } else {
            self.sweep_archive_and_events(sweep_cutoff_ms(now_ms, self.retention_days))
                .await?
        };
        let worker_deleted = self.sweep_stale_workers(now_ms).await?;
        let (started_claims_deleted, abandoned_claims_deleted) =
            self.sweep_trigger_claims(now_ms).await?;
        let deleted = archive_deleted
            + event_deleted
            + signal_deleted
            + worker_deleted
            + started_claims_deleted
            + abandoned_claims_deleted;
        if deleted > 0 {
            info!(
                deleted_rows = deleted,
                archive_deleted_rows = archive_deleted,
                event_deleted_rows = event_deleted,
                // Its own figure rather than folded into the events'.
                // They are two populations with two retention rules —
                // one sweeps everything past the cutoff, the other
                // keeps every alert — so a single number could not be
                // read against either.
                operator_signal_deleted_rows = signal_deleted,
                stale_worker_deleted_rows = worker_deleted,
                // Apart for the same reason: a started claim is the
                // dedupe record of a run, an abandoned one a delivery
                // an exited process never settled.
                started_trigger_claim_deleted_rows = started_claims_deleted,
                abandoned_trigger_claim_deleted_rows = abandoned_claims_deleted,
                retention_days = self.retention_days,
                stale_worker_retention_days = self.stale_worker_retention_days,
                trigger_claim_retention_days = self.trigger_claim_retention_days,
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

    /// The step-10 half: archived invocations, projected events, and
    /// the pane's index — counted apart, because they are three
    /// populations under three rules.
    async fn sweep_archive_and_events(
        &self,
        cutoff_ms: i64,
    ) -> Result<(u64, u64, u64), super::store::ControlPlaneStoreError> {
        let archive_deleted = self.store.sweep_archive(cutoff_ms).await?;
        let Some(store) = &self.projection_store else {
            return Ok((archive_deleted, 0, 0));
        };
        let event_deleted = store.sweep_events(cutoff_ms).await.map_err(|err| {
            super::store::ControlPlaneStoreError::Backend(format!(
                "projection retention sweep failed: {err}"
            ))
        })?;
        // The pane's index ages on the same window and in the same
        // tick, minus the alerts it never sweeps — see
        // `sweep_operator_signals`.
        let signal_deleted = store
            .sweep_operator_signals(cutoff_ms)
            .await
            .map_err(|err| {
                super::store::ControlPlaneStoreError::Backend(format!(
                    "operator-signal retention sweep failed: {err}"
                ))
            })?;
        Ok((archive_deleted, event_deleted, signal_deleted))
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

    /// The trigger-claim half: `durably_started` rows past the cutoff,
    /// and `claimed` rows past it that another claimant left. Counted
    /// apart. See
    /// [the module docs](crate::control_plane::retention#the-trigger-claim-window).
    async fn sweep_trigger_claims(
        &self,
        now_ms: i64,
    ) -> Result<(u64, u64), super::store::ControlPlaneStoreError> {
        let days = self.trigger_claim_retention_days;
        // Config load refuses a window below the floor, so only a
        // `StateConfig` built in code can bring one here. Acting on it
        // could delete a started claim whose message the stream can
        // still deliver, so the window is skipped, loudly, every tick.
        if let Some(floor_days) = crate::config::trigger_claim_retention_below_floor(days) {
            warn!(
                trigger_claim_retention_days = days,
                floor_days,
                "trigger-claim retention of {days} days is below its floor of {floor_days} \
                 days; skipping the claim window rather than delete started claims whose \
                 trigger messages the stream may still deliver"
            );
            return Ok((0, 0));
        }
        let Some(cutoff_ms) = trigger_claim_cutoff_ms(now_ms, days) else {
            return Ok((0, 0));
        };
        let started = self.store.sweep_started_trigger_claims(cutoff_ms).await?;
        let abandoned = match &self.claimant {
            Some(claimant) => {
                self.store
                    .sweep_abandoned_trigger_claims(cutoff_ms, claimant)
                    .await?
            }
            None => 0,
        };
        Ok((started, abandoned))
    }

    /// Run a single sweep without ticking. Exposed for tests
    /// so they don't have to wait for the interval timer.
    #[cfg(test)]
    pub(crate) async fn sweep_now(&self) -> Result<(), super::store::ControlPlaneStoreError> {
        self.sweep_once().await
    }
}

#[cfg(test)]
mod trigger_claim_tests;

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
            trigger_claim_retention_days: -1,
            sweep_interval_seconds: 3600,
            ..Default::default()
        }
    }

    /// `[state]` with only the stale-worker window on.
    fn workers_only(stale_worker_retention_days: i64) -> StateConfig {
        StateConfig {
            retention_days: -1,
            stale_worker_retention_days,
            trigger_claim_retention_days: -1,
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

    /// **The tick actually sweeps the pane's index, and keeps the
    /// alerts.**
    ///
    /// `sweep_operator_signals` had a two-directional unit test and no
    /// caller test, so deleting the call from `sweep_archive_and_events`
    /// left every test green while notifications accumulated for ever.
    /// This is the wiring, asserted where the wiring is: one old
    /// notification and one old alert of identical age, one tick, and
    /// the two outcomes must differ.
    #[tokio::test]
    async fn the_retention_tick_sweeps_notifications_and_keeps_alerts() {
        use super::super::projection::ProjectionStore;
        use crate::events::{
            Event, EventPayload, OperatorSignalPayload, SignalKind, operator_signal::kinds,
        };
        use chrono::Utc;
        use tempfile::tempdir;
        use uuid::Uuid;

        fn signal(payload: OperatorSignalPayload, age_days: i64) -> Event {
            let mut event = Event::system(Uuid::now_v7(), EventPayload::OperatorSignal(payload));
            event.envelope.timestamp = Utc::now() - chrono::Duration::days(age_days);
            event
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

        let swept = signal(
            OperatorSignalPayload::notification(
                SignalKind::registered(kinds::PRICING_CHANGE_REFUSED),
                "a price moved 6.2x; kept the prior price",
            ),
            3,
        );
        let kept = signal(
            OperatorSignalPayload::alert(
                SignalKind::registered(kinds::PRICING_STALE),
                "the pricing table has not refreshed in 31h",
            ),
            3,
        );
        for event in [&swept, &kept] {
            projection.insert_event(event, None).await.unwrap();
        }
        assert_eq!(
            projection
                .query_operator_signals(None, None, None, 10)
                .await
                .unwrap()
                .len(),
            2
        );

        RetentionSweeper::new(store, &archive_only(1))
            .with_projection_store(projection.clone())
            .sweep_now()
            .await
            .unwrap();

        let rows = projection
            .query_operator_signals(None, None, None, 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "the tick swept the notification: {rows:?}");
        assert_eq!(rows[0].event_id, kept.envelope.event_id.to_string());
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
