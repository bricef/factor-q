//! Read-only operator *views* over the runtime's stores.
//!
//! `views` is the single read model behind every operator surface: the `fq`
//! CLI read commands (as a formatter over these DTOs) and, later, the
//! read-only tarpc service that backs the operator dashboard
//! (`docs/plans/active/2026-07-10-operator-dashboard.md`). It opens the
//! projection, control-plane, and worker-WAL stores read-only against the one
//! SQLite file and returns typed, `Serialize` view DTOs whose shape is owned
//! here — deliberately decoupled from the internal `*Row` types so the wire /
//! JSON shape can evolve without leaking storage internals.
//!
//! The name is `views` rather than `state` because `state` is already taken
//! throughout the crate (`invocation_state`, `state_blob`, worker execution
//! state); these are read-only views *over* that state.
//!
//! This module performs **no NATS access**. The live JetStream health probe
//! (stream depth / consumer lag) is a separate concern that composes the
//! DB-backed counts from here with a NATS probe at the daemon layer; it lands
//! with the tarpc service, not here.

use std::path::Path;

// DTOs are Deserialize as well as Serialize so they can travel over the
// read service's bincode wire (#105 layer 2), not just out as JSON.
use serde::{Deserialize, Serialize};

use crate::control_plane::projection::ProjectionStore;
use crate::control_plane::projection::store::{
    CostSummary, EventFilter, EventRow, FailureSummary, StoreError,
};
use crate::control_plane::store::{
    ControlPlaneStore, ControlPlaneStoreError, InvocationArchiveRow, OwnerRow, OwnerStatus,
    WorkerRow, is_stale,
};
use crate::worker::store::{LlmDispatchRow, ToolDispatchRow, WorkerStore, WorkerStoreError};

/// How many recent events to scan / retain when assembling an invocation
/// detail view. Mirrors the CLI's `invocation show`: the projection has no
/// per-invocation query, so we over-fetch by agent and filter in memory —
/// fine for triage volumes.
const INVOCATION_EVENT_SCAN: i64 = 200;
const INVOCATION_EVENT_KEEP: usize = 20;

/// Errors surfaced by the read views. Each variant wraps the originating
/// store's error so callers can distinguish which store failed; the public
/// shape stays a flat message via `Display`.
#[derive(Debug, thiserror::Error)]
pub enum ViewsError {
    #[error("projection store: {0}")]
    Projection(#[from] StoreError),
    #[error("control-plane store: {0}")]
    ControlPlane(#[from] ControlPlaneStoreError),
    #[error("worker store: {0}")]
    Worker(#[from] WorkerStoreError),
}

// ============================================================
// View DTOs — the shape the CLI and the API both consume.
// All timestamps are surfaced with explicit units in the field
// name so a browser/JSON consumer never has to guess.
// ============================================================

/// One worker in the roster.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WorkerView {
    pub worker_id: String,
    pub host: String,
    pub registered_at_ms: i64,
    pub last_heartbeat_ms: i64,
    /// `alive` / `stale` / `shutdown`, as recorded by the control-plane.
    pub status: String,
    /// Invocations this worker currently owns in a non-terminal state
    /// (`in_flight` or `ambiguous`). Filled by [`Views::workers`] /
    /// [`Views::worker`]; the bare `From<WorkerRow>` leaves it 0.
    pub in_flight_count: i64,
}

impl From<WorkerRow> for WorkerView {
    fn from(r: WorkerRow) -> Self {
        WorkerView {
            worker_id: r.worker_id,
            host: r.host,
            registered_at_ms: r.registered_at,
            last_heartbeat_ms: r.last_heartbeat,
            status: r.status.as_str().to_string(),
            in_flight_count: 0,
        }
    }
}

/// One worker plus the invocations it currently owns — the `fq workers
/// show` / dashboard worker-detail view. `worker` is nested (not
/// serde-flattened): flatten is JSON-only sugar that bincode — the read
/// service's wire format — cannot serialize.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WorkerDetailView {
    pub worker: WorkerView,
    /// Every ownership row for this worker (any status), newest first.
    pub owned: Vec<InvocationSummaryView>,
}

/// Recovery-state counts — the data behind `fq status`'s recovery block and
/// the dashboard's health tile. Computed against a caller-supplied `now_ms`
/// and threshold so the view stays pure (no wall-clock inside).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct RecoveryView {
    /// Ambiguous invocations awaiting operator triage.
    pub ambiguous: i64,
    /// Workers past the stale threshold (and not shut down).
    pub stale_workers: i64,
    /// Ids of those stale workers, so a caller can act without a second query.
    pub stale_worker_ids: Vec<String>,
}

/// In-flight / stuck execution counts, read from the worker WAL — the
/// reliable live view (the CP owner table's `in_flight` is not populated by
/// trigger dispatch yet; see issue #50).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecutionsView {
    pub in_flight: i64,
    /// In-flight invocations whose WAL row has not advanced within the
    /// caller-supplied stuck threshold.
    pub stuck: i64,
    pub stuck_ids: Vec<String>,
}

/// One currently-executing invocation, straight from the worker WAL —
/// the row form of [`ExecutionsView`]'s counts, for the dashboard's
/// "active" table. Sourced from the WAL rather than the ownership
/// table because dispatch does not populate the latter yet (#50), so
/// the WAL is the only place live work is guaranteed to appear.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ActiveInvocationView {
    pub invocation_id: String,
    pub agent_id: String,
    pub phase: String,
    /// Reducer *step* counter — see [`LiveExecutionView::step_index`].
    pub step_index: u32,
    pub started_at_ms: i64,
    /// Last WAL advance; long tool runs legitimately leave this old.
    pub updated_at_ms: i64,
    /// Tool names with an open (non-completed) dispatch right now.
    pub open_tools: Vec<String>,
    /// Models with an open (non-completed) LLM dispatch right now.
    pub open_llms: Vec<String>,
}

/// One row in the invocation list: a coordination-ownership row, or (in
/// the merged index) an archive-only row flagged `archived`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct InvocationSummaryView {
    pub invocation_id: String,
    /// From the projection; `None` when no event for the id has landed.
    pub agent_id: Option<String>,
    /// Empty for archive-only rows (the archive keeps no worker).
    pub worker_id: String,
    /// `in_flight` / `completed` / `failed` / `ambiguous`, or the
    /// archive's `final_phase` for archive-only rows.
    pub status: String,
    /// `assigned_at` for ownership rows; `archived_at` for archive-only
    /// rows.
    pub assigned_at_ms: i64,
    /// When the invocation began: `assigned_at` (dispatch time — the
    /// closest thing to a start the coordination store records) for
    /// ownership rows, the archive's true `started_at` for archive-only
    /// rows. Unlike `assigned_at_ms`, this means the same thing on both
    /// row kinds — the list surface's "started" column.
    pub started_at_ms: i64,
    /// True when the row came from `invocation_archive` (no live
    /// ownership row remains).
    pub archived: bool,
}

impl From<OwnerRow> for InvocationSummaryView {
    fn from(r: OwnerRow) -> Self {
        InvocationSummaryView {
            invocation_id: r.invocation_id,
            agent_id: None,
            worker_id: r.worker_id,
            status: r.status.as_str().to_string(),
            assigned_at_ms: r.assigned_at,
            started_at_ms: r.assigned_at,
            archived: false,
        }
    }
}

/// A finalised invocation's archive record.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ArchiveView {
    pub invocation_id: String,
    pub agent_id: String,
    pub final_phase: String,
    pub started_at_ms: i64,
    pub terminal_at_ms: i64,
    pub archived_at_ms: i64,
}

impl From<InvocationArchiveRow> for ArchiveView {
    fn from(r: InvocationArchiveRow) -> Self {
        ArchiveView {
            invocation_id: r.invocation_id,
            agent_id: r.agent_id,
            final_phase: r.final_phase,
            started_at_ms: r.started_at,
            terminal_at_ms: r.terminal_at,
            archived_at_ms: r.archived_at,
        }
    }
}

/// One event row from the projection.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct EventView {
    pub event_id: String,
    pub timestamp: String,
    pub agent_id: String,
    pub invocation_id: String,
    pub event_type: String,
    pub model: Option<String>,
    pub total_cost: Option<f64>,
    pub error_kind: Option<String>,
    pub duration_ms: Option<i64>,
}

impl From<EventRow> for EventView {
    fn from(r: EventRow) -> Self {
        EventView {
            event_id: r.event_id,
            timestamp: r.timestamp,
            agent_id: r.agent_id,
            invocation_id: r.invocation_id,
            event_type: r.event_type,
            model: r.model,
            total_cost: r.total_cost,
            error_kind: r.error_kind,
            duration_ms: r.duration_ms,
        }
    }
}

/// Per-agent cost/token aggregate.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct CostView {
    pub agent_id: String,
    pub event_count: i64,
    pub total_cost: f64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    pub total_cache_write_tokens: i64,
}

impl From<CostSummary> for CostView {
    fn from(r: CostSummary) -> Self {
        CostView {
            agent_id: r.agent_id,
            event_count: r.event_count,
            total_cost: r.total_cost,
            total_input_tokens: r.total_input_tokens,
            total_output_tokens: r.total_output_tokens,
            total_cache_read_tokens: r.total_cache_read_tokens,
            total_cache_write_tokens: r.total_cache_write_tokens,
        }
    }
}

/// Per-agent costs plus the grand totals, so a caller renders both without
/// re-summing.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct CostReport {
    pub agents: Vec<CostView>,
    pub total_cost: f64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    pub total_cache_write_tokens: i64,
}

/// One terminal-failure bucket, grouped by kind.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct FailureView {
    pub error_kind: String,
    pub count: i64,
}

impl From<FailureSummary> for FailureView {
    fn from(r: FailureSummary) -> Self {
        FailureView {
            error_kind: r.error_kind,
            count: r.count,
        }
    }
}

/// One in-flight tool dispatch (worker WAL).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ToolDispatchView {
    pub tool_call_id: String,
    pub tool_name: String,
    /// `intent` / `dispatched` / `completed`.
    pub status: String,
    pub is_error: Option<bool>,
    pub intent_at_ms: i64,
    pub dispatched_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
}

impl From<ToolDispatchRow> for ToolDispatchView {
    fn from(r: ToolDispatchRow) -> Self {
        ToolDispatchView {
            tool_call_id: r.tool_call_id,
            tool_name: r.tool_name,
            status: r.status.as_str().to_string(),
            is_error: r.is_error,
            intent_at_ms: r.intent_at,
            dispatched_at_ms: r.dispatched_at,
            completed_at_ms: r.completed_at,
        }
    }
}

/// One in-flight LLM dispatch (worker WAL).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct LlmDispatchView {
    pub request_id: String,
    pub model: String,
    /// `intent` / `dispatched` / `completed`.
    pub status: String,
    pub cost_usd: Option<f64>,
    pub is_error: Option<bool>,
    pub intent_at_ms: i64,
    pub dispatched_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
}

impl From<LlmDispatchRow> for LlmDispatchView {
    fn from(r: LlmDispatchRow) -> Self {
        LlmDispatchView {
            request_id: r.request_id,
            model: r.model,
            status: r.status.as_str().to_string(),
            cost_usd: r.cost_usd,
            is_error: r.is_error,
            intent_at_ms: r.intent_at,
            dispatched_at_ms: r.dispatched_at,
            completed_at_ms: r.completed_at,
        }
    }
}

/// Live execution state of an in-flight invocation, from the worker WAL —
/// the "what is it doing right now" view. Present only while the invocation
/// has a WAL row (deleted on archive hand-off).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct LiveExecutionView {
    pub phase: String,
    /// Reducer *step* counter (every model and tool step) — not the
    /// model-turn count that `max_iterations` gates (issue #109).
    pub step_index: u32,
    pub started_at_ms: i64,
    pub updated_at_ms: i64,
    pub terminal_at_ms: Option<i64>,
    pub tools: Vec<ToolDispatchView>,
    pub llms: Vec<LlmDispatchView>,
}

/// Everything known about one invocation, composed across the three stores.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct InvocationDetailView {
    pub invocation_id: String,
    pub agent_id: Option<String>,
    /// Coordination ownership row, if any.
    pub owner: Option<InvocationSummaryView>,
    /// Archive record, if the invocation has finalised.
    pub archive: Option<ArchiveView>,
    /// Live WAL execution state, if the invocation is still in flight.
    pub live: Option<LiveExecutionView>,
    /// Most recent events for this invocation (newest first).
    pub recent_events: Vec<EventView>,
}

// ============================================================
// Views — the read handle.
// ============================================================

/// Read-only handle over the runtime's three SQLite-backed stores (all one
/// file, opened `?mode=ro`). Cheap to construct relative to the queries it
/// serves; a caller can hold one for the lifetime of a request loop.
pub struct Views {
    projection: ProjectionStore,
    control_plane: ControlPlaneStore,
    worker: WorkerStore,
}

impl Views {
    /// Open all three stores read-only against `db_path`. Errors if the file
    /// does not exist or a store's schema is incompatible; callers that want
    /// to distinguish "not initialised" should check for the file first (as
    /// the CLI does).
    pub async fn open(db_path: &Path) -> Result<Self, ViewsError> {
        let projection = ProjectionStore::open_read_only(db_path).await?;
        let control_plane = ControlPlaneStore::open_read_only(db_path).await?;
        let worker = WorkerStore::open_read_only(db_path).await?;
        Ok(Views {
            projection,
            control_plane,
            worker,
        })
    }

    /// Total event count in the projection.
    pub async fn event_count(&self) -> Result<i64, ViewsError> {
        Ok(self.projection.count().await?)
    }

    /// Recent events, newest first, filtered by agent / type / since.
    pub async fn events(
        &self,
        agent: Option<&str>,
        event_type: Option<&str>,
        since: Option<&str>,
        limit: i64,
    ) -> Result<Vec<EventView>, ViewsError> {
        let filter = EventFilter {
            agent,
            event_type,
            since,
        };
        let rows = self.projection.query_events(&filter, limit).await?;
        Ok(rows.into_iter().map(EventView::from).collect())
    }

    /// Per-agent cost/token aggregate plus grand totals.
    pub async fn costs(
        &self,
        agent: Option<&str>,
        since: Option<&str>,
    ) -> Result<CostReport, ViewsError> {
        let rows = self.projection.cost_summary(agent, since).await?;
        let mut report = CostReport::default();
        for r in rows {
            report.total_cost += r.total_cost;
            report.total_input_tokens += r.total_input_tokens;
            report.total_output_tokens += r.total_output_tokens;
            report.total_cache_read_tokens += r.total_cache_read_tokens;
            report.total_cache_write_tokens += r.total_cache_write_tokens;
            report.agents.push(CostView::from(r));
        }
        Ok(report)
    }

    /// Terminal failures grouped by kind.
    pub async fn failures(&self) -> Result<Vec<FailureView>, ViewsError> {
        let rows = self.projection.failure_summary().await?;
        Ok(rows.into_iter().map(FailureView::from).collect())
    }

    /// The worker roster, each with its current non-terminal ownership
    /// count.
    pub async fn workers(&self) -> Result<Vec<WorkerView>, ViewsError> {
        let rows = self.control_plane.list_workers().await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let mut view = WorkerView::from(row);
            view.in_flight_count = self.in_flight_count_for(&view.worker_id).await?;
            out.push(view);
        }
        Ok(out)
    }

    /// One worker's detail — the roster row plus every invocation it
    /// owns, newest first. `None` when the id is unknown.
    pub async fn worker(&self, worker_id: &str) -> Result<Option<WorkerDetailView>, ViewsError> {
        let Some(row) = self.control_plane.get_worker(worker_id).await? else {
            return Ok(None);
        };
        let owned: Vec<InvocationSummaryView> = self
            .control_plane
            .list_invocations_for_worker(worker_id)
            .await?
            .into_iter()
            .map(InvocationSummaryView::from)
            .collect();
        let mut worker = WorkerView::from(row);
        worker.in_flight_count = owned
            .iter()
            .filter(|o| o.status == "in_flight" || o.status == "ambiguous")
            .count() as i64;
        Ok(Some(WorkerDetailView { worker, owned }))
    }

    /// Non-terminal (`in_flight` | `ambiguous`) ownership count for one
    /// worker.
    async fn in_flight_count_for(&self, worker_id: &str) -> Result<i64, ViewsError> {
        Ok(self
            .control_plane
            .list_invocations_for_worker(worker_id)
            .await?
            .into_iter()
            .filter(|o| matches!(o.status, OwnerStatus::InFlight | OwnerStatus::Ambiguous))
            .count() as i64)
    }

    /// Recovery-state counts (ambiguous invocations + stale workers) as of
    /// `now_ms`, using `stale_threshold_ms` for worker staleness.
    pub async fn recovery(
        &self,
        now_ms: i64,
        stale_threshold_ms: i64,
    ) -> Result<RecoveryView, ViewsError> {
        let ambiguous = self
            .control_plane
            .list_invocations_with_status(OwnerStatus::Ambiguous)
            .await?
            .len() as i64;
        let stale = self
            .control_plane
            .list_stale_workers(now_ms, stale_threshold_ms)
            .await?;
        Ok(RecoveryView {
            ambiguous,
            stale_workers: stale.len() as i64,
            stale_worker_ids: stale.into_iter().map(|w| w.worker_id).collect(),
        })
    }

    /// In-flight / stuck execution counts as of `now_ms`, from the worker
    /// WAL. An in-flight invocation is "stuck" when its WAL row has not
    /// advanced within `stuck_threshold_ms`.
    pub async fn executions(
        &self,
        now_ms: i64,
        stuck_threshold_ms: i64,
    ) -> Result<ExecutionsView, ViewsError> {
        let in_flight = self.worker.find_in_flight_invocations().await?;
        let mut view = ExecutionsView {
            in_flight: in_flight.len() as i64,
            ..Default::default()
        };
        for row in in_flight {
            // Reuse the same staleness predicate the control-plane uses for
            // workers: "has not advanced in as long as the threshold" is the
            // same not-making-progress signal.
            if is_stale(row.updated_at, now_ms, stuck_threshold_ms) {
                view.stuck += 1;
                view.stuck_ids.push(row.invocation_id);
            }
        }
        Ok(view)
    }

    /// Every currently-executing invocation as a row (the list behind
    /// [`Views::executions`]' counts), longest-running first, each with
    /// its open tool/LLM dispatches — the "what is running right now"
    /// table.
    pub async fn active_invocations(&self) -> Result<Vec<ActiveInvocationView>, ViewsError> {
        let mut rows = self.worker.find_in_flight_invocations().await?;
        rows.sort_by_key(|r| r.started_at);
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let open_tools = self
                .worker
                .list_tool_dispatches_for_invocation(&row.invocation_id)
                .await?
                .into_iter()
                .filter(|t| !matches!(t.status, crate::worker::store::DispatchStatus::Completed))
                .map(|t| t.tool_name)
                .collect();
            let open_llms = self
                .worker
                .list_llm_dispatches_for_invocation(&row.invocation_id)
                .await?
                .into_iter()
                .filter(|l| !matches!(l.status, crate::worker::store::DispatchStatus::Completed))
                .map(|l| l.model)
                .collect();
            out.push(ActiveInvocationView {
                invocation_id: row.invocation_id,
                agent_id: row.agent_id,
                phase: row.phase,
                step_index: row.step_index,
                started_at_ms: row.started_at,
                updated_at_ms: row.updated_at,
                open_tools,
                open_llms,
            });
        }
        Ok(out)
    }

    /// Coordination-ownership rows, optionally filtered by status, newest
    /// first, capped at `limit`, each joined with its agent id from the
    /// projection.
    pub async fn invocations(
        &self,
        status: Option<OwnerStatus>,
        limit: i64,
    ) -> Result<Vec<InvocationSummaryView>, ViewsError> {
        let rows = self.control_plane.list_invocations(status, limit).await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let mut view = InvocationSummaryView::from(row);
            view.agent_id = self
                .projection
                .agent_id_for_invocation(&view.invocation_id)
                .await?;
            out.push(view);
        }
        Ok(out)
    }

    /// The merged invocation index: ownership rows first, then (when
    /// `include_archived`) archive-only rows — terminal invocations whose
    /// ownership row is gone — flagged `archived`, deduplicated by id.
    /// This is the invocation *list* surface (`fq invocation list`, the
    /// dashboard's invocations page); both backing tables are one list to
    /// an operator.
    pub async fn invocation_index(
        &self,
        status: Option<OwnerStatus>,
        include_archived: bool,
        limit: i64,
    ) -> Result<Vec<InvocationSummaryView>, ViewsError> {
        let mut items = self.invocations(status, limit).await?;
        if include_archived {
            for arc in self.control_plane.list_archives_recent(limit).await? {
                if items.iter().any(|i| i.invocation_id == arc.invocation_id) {
                    continue;
                }
                items.push(InvocationSummaryView {
                    invocation_id: arc.invocation_id,
                    agent_id: Some(arc.agent_id),
                    worker_id: String::new(),
                    status: arc.final_phase,
                    assigned_at_ms: arc.archived_at,
                    started_at_ms: arc.started_at,
                    archived: true,
                });
            }
        }
        Ok(items)
    }

    /// The most recently archived invocations, newest first, capped at
    /// `limit`.
    pub async fn recent_archives(&self, limit: i64) -> Result<Vec<ArchiveView>, ViewsError> {
        let rows = self.control_plane.list_archives_recent(limit).await?;
        Ok(rows.into_iter().map(ArchiveView::from).collect())
    }

    /// Everything known about one invocation, composed across the projection,
    /// control-plane, and worker stores. Returns `None` when no store has any
    /// trace of the id.
    pub async fn invocation(
        &self,
        invocation_id: &str,
    ) -> Result<Option<InvocationDetailView>, ViewsError> {
        let owner = self
            .control_plane
            .get_invocation_owner(invocation_id)
            .await?;
        let archive = self.control_plane.get_archive(invocation_id).await?;
        let agent_id = self
            .projection
            .agent_id_for_invocation(invocation_id)
            .await?;
        let state = self.worker.get_invocation_state(invocation_id).await?;

        if owner.is_none() && archive.is_none() && agent_id.is_none() && state.is_none() {
            return Ok(None);
        }

        let live = match state {
            Some(s) => {
                let tools = self
                    .worker
                    .list_tool_dispatches_for_invocation(invocation_id)
                    .await?;
                let llms = self
                    .worker
                    .list_llm_dispatches_for_invocation(invocation_id)
                    .await?;
                Some(LiveExecutionView {
                    phase: s.phase,
                    step_index: s.step_index,
                    started_at_ms: s.started_at,
                    updated_at_ms: s.updated_at,
                    terminal_at_ms: s.terminal_at,
                    tools: tools.into_iter().map(ToolDispatchView::from).collect(),
                    llms: llms.into_iter().map(LlmDispatchView::from).collect(),
                })
            }
            None => None,
        };

        // The projection has no per-invocation query; over-fetch by agent and
        // filter in memory (matches `fq invocation show`).
        let recent_events = self
            .projection
            .query_events(
                &EventFilter {
                    agent: agent_id.as_deref(),
                    event_type: None,
                    since: None,
                },
                INVOCATION_EVENT_SCAN,
            )
            .await?
            .into_iter()
            .filter(|e| e.invocation_id == invocation_id)
            .take(INVOCATION_EVENT_KEEP)
            .map(EventView::from)
            .collect();

        Ok(Some(InvocationDetailView {
            invocation_id: invocation_id.to_string(),
            agent_id: agent_id.clone(),
            owner: owner.map(|o| {
                let mut v = InvocationSummaryView::from(o);
                v.agent_id = agent_id;
                v
            }),
            archive: archive.map(ArchiveView::from),
            live,
            recent_events,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_plane::store::{WorkerRow, WorkerStatus};
    use crate::worker::store::{DispatchStatus, InvocationStateRow, ToolDispatchRow};

    // ---- Pure From-conversion tests (no DB) ----

    #[test]
    fn worker_row_maps_to_view() {
        let row = WorkerRow {
            worker_id: "w1".into(),
            host: "localhost".into(),
            registered_at: 100,
            last_heartbeat: 200,
            status: WorkerStatus::Stale,
        };
        let view = WorkerView::from(row);
        assert_eq!(view.worker_id, "w1");
        assert_eq!(view.status, "stale");
        assert_eq!(view.last_heartbeat_ms, 200);
    }

    #[test]
    fn tool_dispatch_row_maps_to_view() {
        let row = ToolDispatchRow {
            invocation_id: "i1".into(),
            tool_call_id: "tc1".into(),
            tool_name: "file_read".into(),
            status: DispatchStatus::Completed,
            parameters: "{}".into(),
            result: Some("ok".into()),
            is_error: Some(false),
            intent_at: 1,
            dispatched_at: Some(2),
            completed_at: Some(3),
        };
        let view = ToolDispatchView::from(row);
        assert_eq!(view.tool_name, "file_read");
        assert_eq!(view.status, "completed");
        assert_eq!(view.is_error, Some(false));
        assert_eq!(view.completed_at_ms, Some(3));
    }

    #[test]
    fn cost_report_totals_across_agents() {
        // Build a report the way `costs()` does, from two agents.
        let mut report = CostReport::default();
        for (agent, cost, ins, outs) in [("a", 1.5_f64, 10_i64, 20_i64), ("b", 2.0, 5, 7)] {
            report.total_cost += cost;
            report.total_input_tokens += ins;
            report.total_output_tokens += outs;
            report.agents.push(CostView {
                agent_id: agent.into(),
                event_count: 1,
                total_cost: cost,
                total_input_tokens: ins,
                total_output_tokens: outs,
                total_cache_read_tokens: 0,
                total_cache_write_tokens: 0,
            });
        }
        assert_eq!(report.agents.len(), 2);
        assert!((report.total_cost - 3.5).abs() < f64::EPSILON);
        assert_eq!(report.total_input_tokens, 15);
        assert_eq!(report.total_output_tokens, 27);
    }

    // ---- DB wiring smoke test (empty, freshly-created stores) ----

    /// Create the three stores' schemas in one temp DB file, then open a
    /// read-only `Views` over it and assert the query methods wire up and
    /// return empty / not-found on an empty database.
    #[tokio::test]
    async fn open_and_query_empty_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");

        // Open each store in write mode once to create its tables in the
        // shared file, then drop the write handles.
        {
            let _cp = ControlPlaneStore::open(&path).await.unwrap();
            let _ws = WorkerStore::open(&path).await.unwrap();
            let _proj = ProjectionStore::open(&path).await.unwrap();
        }

        let views = Views::open(&path).await.unwrap();

        assert_eq!(views.event_count().await.unwrap(), 0);
        assert!(views.workers().await.unwrap().is_empty());
        assert!(views.events(None, None, None, 50).await.unwrap().is_empty());
        assert!(views.costs(None, None).await.unwrap().agents.is_empty());
        assert!(views.failures().await.unwrap().is_empty());
        assert!(views.invocations(None, 50).await.unwrap().is_empty());
        assert!(
            views
                .invocation_index(None, true, 50)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(views.recovery(1_000, 30_000).await.unwrap().ambiguous, 0);
        assert_eq!(views.executions(1_000, 30_000).await.unwrap().in_flight, 0);
        assert!(views.active_invocations().await.unwrap().is_empty());
        assert!(views.invocation("no-such-id").await.unwrap().is_none());
        assert!(views.worker("no-such-worker").await.unwrap().is_none());
    }

    /// Seed a worker and an in-flight invocation, then read them back through
    /// `Views` — exercises the cross-store composition end to end.
    #[tokio::test]
    async fn reads_back_seeded_worker_and_invocation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");

        {
            let cp = ControlPlaneStore::open(&path).await.unwrap();
            cp.register_worker("w1", "localhost", 100).await.unwrap();

            let ws = WorkerStore::open(&path).await.unwrap();
            let row = InvocationStateRow {
                invocation_id: "inv-1".into(),
                agent_id: "agent-a".into(),
                schema_version: 1,
                phase: "reducing".into(),
                state_blob: vec![],
                step_index: 3,
                started_at: 100,
                updated_at: 150,
                terminal_at: None,
                workspace_ref: None,
                archive_status: None,
                archive_published_at: None,
                trigger_source: Some("manual".into()),
                trigger_subject: None,
                trigger_payload: None,
            };
            ws.upsert_invocation_state(&row).await.unwrap();
            let _proj = ProjectionStore::open(&path).await.unwrap();
        }

        let views = Views::open(&path).await.unwrap();

        let workers = views.workers().await.unwrap();
        assert_eq!(workers.len(), 1);
        assert_eq!(workers[0].worker_id, "w1");
        assert_eq!(workers[0].status, "alive");
        assert_eq!(workers[0].in_flight_count, 0);

        // Worker detail resolves (no ownership rows seeded → owned empty).
        let detail = views.worker("w1").await.unwrap().expect("w1 exists");
        assert_eq!(detail.worker.worker_id, "w1");
        assert!(detail.owned.is_empty());

        // In-flight execution shows up in the executions view...
        let execs = views.executions(200, 30_000).await.unwrap();
        assert_eq!(execs.in_flight, 1);
        assert_eq!(execs.stuck, 0);

        // ...and it is flagged stuck once `now` is well past the threshold.
        let execs_late = views.executions(1_000_000, 30_000).await.unwrap();
        assert_eq!(execs_late.stuck, 1);
        assert_eq!(execs_late.stuck_ids, vec!["inv-1".to_string()]);

        // The detail view composes the live WAL state.
        let detail = views.invocation("inv-1").await.unwrap().unwrap();
        let live = detail.live.expect("in-flight invocation has live state");
        assert_eq!(live.phase, "reducing");
        assert_eq!(live.step_index, 3);
        assert!(live.tools.is_empty());

        // The active list carries the same WAL row, row-form.
        let active = views.active_invocations().await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].invocation_id, "inv-1");
        assert_eq!(active[0].agent_id, "agent-a");
        assert_eq!(active[0].phase, "reducing");
        assert_eq!(active[0].step_index, 3);
        assert!(active[0].open_tools.is_empty());
    }

    /// An in-flight row whose `updated_at` is in the future (worker clock
    /// ahead) is not "stuck" — `is_stale`'s saturating age handles skew.
    /// This guard moved here from `fq doctor`'s tests when the stuck
    /// determination moved into `executions()` (#105 layer 1).
    #[tokio::test]
    async fn executions_ignore_clock_skew() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");

        const NOW: i64 = 1_000_000;
        {
            let ws = WorkerStore::open(&path).await.unwrap();
            let row = InvocationStateRow {
                invocation_id: "inv-future".into(),
                agent_id: "agent-a".into(),
                schema_version: 1,
                phase: "reducing".into(),
                state_blob: vec![],
                step_index: 1,
                started_at: NOW,
                updated_at: NOW + 60_000,
                terminal_at: None,
                workspace_ref: None,
                archive_status: None,
                archive_published_at: None,
                trigger_source: None,
                trigger_subject: None,
                trigger_payload: None,
            };
            ws.upsert_invocation_state(&row).await.unwrap();
            let _cp = ControlPlaneStore::open(&path).await.unwrap();
            let _proj = ProjectionStore::open(&path).await.unwrap();
        }

        let views = Views::open(&path).await.unwrap();
        let execs = views.executions(NOW, 30_000).await.unwrap();
        assert_eq!(execs.in_flight, 1);
        assert_eq!(execs.stuck, 0, "future updated_at must not read as stuck");
    }
}
