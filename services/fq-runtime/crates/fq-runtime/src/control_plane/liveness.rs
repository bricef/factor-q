//! What "not making progress" means — **the** definition, for
//! invocations, in the same module as the one for workers.
//!
//! The control plane already owns the worker half of this question:
//! [`is_stale`] is the predicate behind the
//! stale-worker sweep. This module is the invocation half. The two are
//! deliberately siblings — a worker that stops beating and an
//! invocation that stops stepping are the same class of finding, found
//! by the same periodic tick — but they are answered from different
//! evidence, and after finding F they no longer answer the same
//! question by accident: a beat says the process is alive, and a step
//! boundary says the *work* is.
//!
//! Two things live here and there is exactly one of each:
//!
//! * [`classify_liveness`] — the verdict for one in-flight row. `fq
//!   doctor`'s `ExecutionsView`, the active-invocations table and the
//!   sweep below all call it, with the same thresholds, so the report
//!   an operator reads and the event that woke them cannot disagree.
//! * [`StuckSweep`] — the periodic pass that turns that verdict into
//!   `invocation.stuck`, once per crossing.
//!
//! The threshold itself is neither of those: it is derived from the
//! call deadlines by [`crate::config::Config::stuck_after`], and both
//! callers are handed the same number by the daemon.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use tracing::{debug, error, warn};
use uuid::Uuid;

use crate::bus::EventBus;
use crate::events::{Event, EventPayload, InvocationStuckPayload};
use crate::views::Liveness;
use crate::worker::store::{WorkerStore, WorkerStoreError};

use super::store::{ControlPlaneStore, ControlPlaneStoreError, OwnerStatus, is_stale};

/// One verdict for one in-flight row.
///
/// Two clocks, because two things can legitimately keep a WAL row
/// silent. An **open dispatch** — a tool call or a model call the
/// runtime is waiting on — is judged by its own age: while it is
/// younger than `long_dispatch_threshold_ms` the invocation is
/// *working*, however quiet its row. Otherwise the row's own age
/// decides: past `stuck_after_ms` with nothing open, the reducer has
/// stopped crossing step boundaries and there is no call to blame.
pub fn classify_liveness(
    newest_open_dispatch_at: Option<i64>,
    updated_at: i64,
    now_ms: i64,
    stuck_after_ms: i64,
    long_dispatch_threshold_ms: i64,
) -> Liveness {
    if let Some(open_at) = newest_open_dispatch_at
        && !is_stale(open_at, now_ms, long_dispatch_threshold_ms)
    {
        return Liveness::Working;
    }
    if is_stale(updated_at, now_ms, stuck_after_ms) {
        Liveness::Stuck
    } else {
        Liveness::Advancing
    }
}

/// Has the control plane already closed this invocation?
///
/// The worker's own `terminal_at IS NULL` is not the whole answer: a
/// row whose archive hand-off has been acknowledged by the control
/// plane is finished even if the local row has not been cleaned up yet.
/// Asking here rather than at each caller is what keeps the doctor's
/// in-flight population and the sweep's identical.
pub(crate) async fn is_closed_by_control_plane(
    control_plane: &ControlPlaneStore,
    invocation_id: &str,
) -> Result<bool, ControlPlaneStoreError> {
    let owner_terminal = control_plane
        .get_invocation_owner(invocation_id)
        .await?
        .is_some_and(|owner| matches!(owner.status, OwnerStatus::Completed | OwnerStatus::Failed));
    Ok(owner_terminal || control_plane.get_archive(invocation_id).await?.is_some())
}

/// The periodic pass that reports stuck invocations.
///
/// **Emit-only.** It publishes `invocation.stuck` and does nothing
/// else: no kill, no re-dispatch, no ownership change. Recovery of a
/// wedged invocation needs a real example to reason from, and until
/// there is one an automatic remedy would be a guess with the power to
/// destroy work.
///
/// **Once per crossing, not once per tick.** The sweep remembers the
/// step boundary it flagged each invocation at. A row that is still
/// stuck at the same boundary is the same finding and is not re-sent;
/// a row that advanced and then stalled again has crossed a second
/// time and is. That is what makes the event usable as an alert: its
/// arrival rate is the rate of *new* stalls, not of ticks.
///
/// The memory records only what was actually published, so a crossing
/// whose publish failed is reported on the next tick rather than lost.
pub struct StuckSweep {
    bus: EventBus,
    worker_store: Arc<WorkerStore>,
    control_plane: Arc<ControlPlaneStore>,
    stuck_after_ms: i64,
    long_dispatch_threshold_ms: i64,
    /// Invocation id → the `updated_at` it was flagged at. Bounded by
    /// the in-flight population: entries for invocations that are no
    /// longer in flight are dropped on the tick that notices.
    flagged: Mutex<HashMap<String, i64>>,
}

impl StuckSweep {
    pub fn new(
        bus: EventBus,
        worker_store: Arc<WorkerStore>,
        control_plane: Arc<ControlPlaneStore>,
        stuck_after_ms: i64,
    ) -> Self {
        Self {
            bus,
            worker_store,
            control_plane,
            stuck_after_ms,
            long_dispatch_threshold_ms: crate::views::DEFAULT_LONG_DISPATCH_THRESHOLD_MS,
            flagged: Mutex::new(HashMap::new()),
        }
    }

    /// The threshold this sweep applies, in ms — the number the event
    /// carries and `fq doctor` prints.
    pub fn stuck_after_ms(&self) -> i64 {
        self.stuck_after_ms
    }

    /// One pass. Publish failures are logged and not retried *within*
    /// the tick, and they do not consume the crossing: the WAL row is
    /// still there and the next tick reports it again.
    ///
    /// That is the opposite of the stale-worker sweep's at-most-once
    /// posture, deliberately. There the store write consumes an
    /// alive→stale transition that cannot recur, so re-publishing on
    /// failure would mean re-publishing on every tick forever. Here the
    /// condition is a *standing* one the WAL keeps stating, so a
    /// dropped notice costs nothing to restate and losing it costs an
    /// operator the only signal there was.
    pub async fn tick(&self, now_ms: i64) -> Result<(), StuckSweepError> {
        // Every ten seconds, and it carries each row's `state_blob` and
        // `trigger_payload` along with the two fields the verdict needs.
        // Bounded by `max_concurrent_invocations` (default 1) today.
        // TODO(#37 follow-up): MAX(updated_at) WHERE terminal_at IS NULL
        // once worker/store.rs has headroom
        let rows = self.worker_store.find_in_flight_invocations().await?;
        let mut live: Vec<String> = Vec::with_capacity(rows.len());
        for row in rows {
            if is_closed_by_control_plane(&self.control_plane, &row.invocation_id).await? {
                continue;
            }
            live.push(row.invocation_id.clone());
            let newest_open = self.newest_open_dispatch_at(&row.invocation_id).await?;
            let verdict = classify_liveness(
                newest_open,
                row.updated_at,
                now_ms,
                self.stuck_after_ms,
                self.long_dispatch_threshold_ms,
            );
            if verdict != Liveness::Stuck {
                continue;
            }
            if self.already_reported(&row.invocation_id, row.updated_at) {
                debug!(
                    invocation_id = %row.invocation_id,
                    "invocation still stuck at an already-reported step boundary; not re-emitting"
                );
                continue;
            }
            // Record only what actually went out. Recording first would
            // let a failed publish consume the crossing, and that stall
            // would then never be reported at that boundary — the one
            // failure mode this memory must not have.
            if self.emit(&row).await {
                self.record_crossing(&row.invocation_id, row.updated_at);
            }
        }
        self.forget_all_but(&live);
        Ok(())
    }

    /// The newest open tool or model call for one invocation, by the
    /// time it went out.
    async fn newest_open_dispatch_at(
        &self,
        invocation_id: &str,
    ) -> Result<Option<i64>, WorkerStoreError> {
        let tools = self
            .worker_store
            .open_tool_dispatches_for_invocation(invocation_id)
            .await?
            .into_iter()
            .map(|d| d.dispatched_at.unwrap_or(d.intent_at))
            .max();
        let llms = self
            .worker_store
            .open_llm_dispatches_for_invocation(invocation_id)
            .await?
            .into_iter()
            .map(|d| d.dispatched_at.unwrap_or(d.intent_at))
            .max();
        Ok(tools.max(llms))
    }

    /// Has this invocation already been reported at this exact step
    /// boundary? A row still stuck where it was last flagged is the
    /// same finding; one flagged at an older boundary has moved since
    /// and stalled again.
    fn already_reported(&self, invocation_id: &str, updated_at: i64) -> bool {
        let flagged = self.flagged.lock().expect("stuck-sweep memory poisoned");
        flagged.get(invocation_id) == Some(&updated_at)
    }

    /// Remember a crossing that was actually published.
    fn record_crossing(&self, invocation_id: &str, updated_at: i64) {
        let mut flagged = self.flagged.lock().expect("stuck-sweep memory poisoned");
        flagged.insert(invocation_id.to_string(), updated_at);
    }

    /// Drop the memory of invocations that are no longer in flight, so
    /// the map tracks the live population rather than every invocation
    /// this daemon has ever run.
    fn forget_all_but(&self, live: &[String]) {
        let mut flagged = self.flagged.lock().expect("stuck-sweep memory poisoned");
        flagged.retain(|id, _| live.iter().any(|live_id| live_id == id));
    }

    /// Publish one report. `true` when it reached the bus — which is
    /// what the caller records the crossing on.
    ///
    /// An unaddressable row (an id or agent id the envelope cannot
    /// carry) returns `false` too. It will be retried every tick and
    /// fail every tick, which is right: the log line is the only place
    /// that corruption can surface, and silently forgetting the row
    /// would hide it.
    async fn emit(&self, row: &crate::worker::store::InvocationStateRow) -> bool {
        let Ok(agent_id) = crate::agent::AgentId::new(row.agent_id.clone()) else {
            error!(
                invocation_id = %row.invocation_id,
                agent_id = %row.agent_id,
                "stuck invocation has an invalid agent id; cannot address the event"
            );
            return false;
        };
        let Ok(invocation_id) = Uuid::parse_str(&row.invocation_id) else {
            error!(
                invocation_id = %row.invocation_id,
                "stuck invocation has an invalid id; cannot address the event"
            );
            return false;
        };
        let event = Event::new(
            agent_id,
            invocation_id,
            EventPayload::InvocationStuck(InvocationStuckPayload {
                last_step_at_ms: row.updated_at,
                stuck_after_ms: self.stuck_after_ms,
                phase: row.phase.clone(),
                step_index: row.step_index,
            }),
        );
        if let Err(err) = self.bus.publish(&event).await {
            error!(
                invocation_id = %row.invocation_id,
                error = %err,
                "failed to publish invocation.stuck event; the next sweep will report it again"
            );
            return false;
        }
        warn!(
            invocation_id = %row.invocation_id,
            phase = %row.phase,
            last_step_at_ms = row.updated_at,
            stuck_after_ms = self.stuck_after_ms,
            "invocation has not crossed a step boundary within the stuck threshold"
        );
        true
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StuckSweepError {
    #[error("worker store error: {0}")]
    WorkerStore(#[from] WorkerStoreError),
    #[error("control-plane store error: {0}")]
    ControlPlane(#[from] ControlPlaneStoreError),
}

#[cfg(test)]
mod tests;
