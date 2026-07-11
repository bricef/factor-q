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

use serde::Serialize;

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
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct WorkerView {
    pub worker_id: String,
    pub host: String,
    pub registered_at_ms: i64,
    pub last_heartbeat_ms: i64,
    /// `alive` / `stale` / `shutdown`, as recorded by the control-plane.
    pub status: String,
}

impl From<WorkerRow> for WorkerView {
    fn from(r: WorkerRow) -> Self {
        WorkerView {
            worker_id: r.worker_id,
            host: r.host,
            registered_at_ms: r.registered_at,
            last_heartbeat_ms: r.last_heartbeat,
            status: r.status.as_str().to_string(),
        }
    }
}

/// Recovery-state counts — the data behind `fq status`'s recovery block and
/// the dashboard's health tile. Computed against a caller-supplied `now_ms`
/// and threshold so the view stays pure (no wall-clock inside).
#[derive(Serialize, Debug, Clone, PartialEq, Eq, Default)]
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
#[derive(Serialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecutionsView {
    pub in_flight: i64,
    /// In-flight invocations whose WAL row has not advanced within the
    /// caller-supplied stuck threshold.
    pub stuck: i64,
    pub stuck_ids: Vec<String>,
}

/// One coordination-ownership row in the invocation list.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct InvocationSummaryView {
    pub invocation_id: String,
    pub worker_id: String,
    /// `in_flight` / `completed` / `failed` / `ambiguous`.
    pub status: String,
    pub assigned_at_ms: i64,
}

impl From<OwnerRow> for InvocationSummaryView {
    fn from(r: OwnerRow) -> Self {
        InvocationSummaryView {
            invocation_id: r.invocation_id,
            worker_id: r.worker_id,
            status: r.status.as_str().to_string(),
            assigned_at_ms: r.assigned_at,
        }
    }
}

/// A finalised invocation's archive record.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
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
#[derive(Serialize, Debug, Clone, PartialEq)]
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
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct CostView {
    pub agent_id: String,
    pub event_count: i64,
    pub total_cost: f64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
}

impl From<CostSummary> for CostView {
    fn from(r: CostSummary) -> Self {
        CostView {
            agent_id: r.agent_id,
            event_count: r.event_count,
            total_cost: r.total_cost,
            total_input_tokens: r.total_input_tokens,
            total_output_tokens: r.total_output_tokens,
        }
    }
}

/// Per-agent costs plus the grand totals, so a caller renders both without
/// re-summing.
#[derive(Serialize, Debug, Clone, PartialEq, Default)]
pub struct CostReport {
    pub agents: Vec<CostView>,
    pub total_cost: f64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
}

/// One terminal-failure bucket, grouped by kind.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
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
#[derive(Serialize, Debug, Clone, PartialEq)]
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
#[derive(Serialize, Debug, Clone, PartialEq)]
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
#[derive(Serialize, Debug, Clone, PartialEq)]
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
#[derive(Serialize, Debug, Clone, PartialEq)]
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
            report.agents.push(CostView::from(r));
        }
        Ok(report)
    }

    /// Terminal failures grouped by kind.
    pub async fn failures(&self) -> Result<Vec<FailureView>, ViewsError> {
        let rows = self.projection.failure_summary().await?;
        Ok(rows.into_iter().map(FailureView::from).collect())
    }

    /// The worker roster.
    pub async fn workers(&self) -> Result<Vec<WorkerView>, ViewsError> {
        let rows = self.control_plane.list_workers().await?;
        Ok(rows.into_iter().map(WorkerView::from).collect())
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

    /// Coordination-ownership rows, optionally filtered by status, newest
    /// first, capped at `limit`.
    pub async fn invocations(
        &self,
        status: Option<OwnerStatus>,
        limit: i64,
    ) -> Result<Vec<InvocationSummaryView>, ViewsError> {
        let rows = self.control_plane.list_invocations(status, limit).await?;
        Ok(rows.into_iter().map(InvocationSummaryView::from).collect())
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
            agent_id,
            owner: owner.map(InvocationSummaryView::from),
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
        assert_eq!(views.recovery(1_000, 30_000).await.unwrap().ambiguous, 0);
        assert_eq!(views.executions(1_000, 30_000).await.unwrap().in_flight, 0);
        assert!(views.invocation("no-such-id").await.unwrap().is_none());
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
    }
}
