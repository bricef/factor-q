//! Read-only operator *views* over the runtime's stores.
//!
//! `views` is the single read model behind every operator surface: the `fq`
//! CLI read commands (as a formatter over these DTOs) and, later, the
//! read surface the daemon serves the operator dashboard from
//! (`docs/plans/closed/2026-07-10-operator-dashboard.md`). It opens the
//! projection, control-plane, and worker-WAL stores read-only against their
//! per-store SQLite files and returns typed, `Serialize` view DTOs whose shape is owned
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
//! with the edge handler, not here.

// The cost reads, and the allocation rule (#466) that decides which of
// them count spend the engine owes to no invocation.
mod costs;

// The conversation reads — payload-bearing, WAL-backed, and the only
// reads here that are not header folds.
mod transcript;

// The view types themselves are `fq_ops::views` and are re-exported
// here, so a caller reaches them by the same path as before. What is
// left in this module is the reading: the store handles, and the
// `From<Row>` conversions that sit beside the rows they convert.
//
// `since` went with them and for the same reason: parsing a spelling
// into a bound is pure, and `fq` does it at the argument. Comparing
// that bound against a column is what stayed.
pub use fq_ops::views::*;

use crate::agent::AgentId;
use crate::control_plane::projection::ProjectionStore;
use crate::control_plane::projection::store::{
    CostBucketSummary, CostSummary, EventFilter, EventRow, FailureSummary, InvocationCostSummary,
    ModelCostSummary, StoreError,
};
use crate::control_plane::store::{
    ControlPlaneStore, ControlPlaneStoreError, InvocationArchiveRow, OwnerRow, OwnerStatus,
    WorkerRow, is_stale,
};
use crate::db::RuntimeDbPaths;
use crate::worker::store::{LlmDispatchRow, ToolDispatchRow, WorkerStore, WorkerStoreError};

pub use crate::control_plane::projection::store::EventLocation;

/// How many recent events to scan / retain when assembling an invocation
/// detail view. Mirrors the CLI's `invocation show`: the projection has no
/// per-invocation query, so we over-fetch by agent and filter in memory —
/// fine for triage volumes.
const INVOCATION_EVENT_SCAN: i64 = 200;
const INVOCATION_EVENT_KEEP: usize = 20;

fn archived_agent_id(agent_id: String) -> Option<String> {
    (!matches!(
        agent_id.as_str(),
        AgentId::SYSTEM_STR | AgentId::SUMMARY_STR | AgentId::OPERATOR_STR
    ))
    .then_some(agent_id)
}

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
    #[error(
        "no projection watermark on this read path: min_seq waiting needs an \
         in-process projection consumer (the daemon has one; a direct CLI \
         read does not)"
    )]
    WatermarkUnavailable,
    #[error(transparent)]
    Watermark(#[from] crate::watermark::WatermarkError),
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

/// Default age past which an OPEN dispatch (tool or LLM) stops counting
/// as *working* — the runtime's default `exec` maximum, ten minutes. An
/// open dispatch's own age, not the invocation's WAL timestamp, decides
/// when it is suspicious (#130). This is a *default* for the
/// `long_dispatch_threshold_ms` parameter of [`Views::executions`] —
/// views stay pure, thresholds are caller-supplied — shared so `fq
/// doctor` and the read service cannot drift apart. It mirrors the
/// configurable exec maximum by assumption; plumb it from config if
/// that value ever becomes load-bearing elsewhere.
pub const DEFAULT_LONG_DISPATCH_THRESHOLD_MS: i64 = 600_000;

/// One verdict for one in-flight row — the single classification the
/// health counts and the row views all flow through.
fn classify_liveness(
    newest_open_dispatch_at: Option<i64>,
    updated_at: i64,
    now_ms: i64,
    stuck_threshold_ms: i64,
    long_dispatch_threshold_ms: i64,
) -> Liveness {
    if let Some(open_at) = newest_open_dispatch_at
        && !is_stale(open_at, now_ms, long_dispatch_threshold_ms)
    {
        return Liveness::Working;
    }
    if is_stale(updated_at, now_ms, stuck_threshold_ms) {
        Liveness::Stuck
    } else {
        Liveness::Advancing
    }
}

/// Server-side cap on [`OpenToolView::command`] — long enough to read
/// a real command, short enough that a pathological argv cannot bloat
/// every active-table poll.
pub const OPEN_TOOL_COMMAND_CAP: usize = 200;

/// The command line carried by a tool dispatch's parameters, if any.
/// Tool-agnostic on purpose: anything with a `command` field benefits
/// (exec's argv array, shell's string), and tools without one return
/// `None` naturally instead of needing a name allowlist.
fn open_tool_command(parameters: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(parameters).ok()?;
    let line = match value.get("command")? {
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        serde_json::Value::String(s) => s.clone(),
        _ => return None,
    };
    if line.is_empty() {
        return None;
    }
    if line.chars().count() > OPEN_TOOL_COMMAND_CAP {
        let capped: String = line.chars().take(OPEN_TOOL_COMMAND_CAP).collect();
        return Some(format!("{capped}…"));
    }
    Some(line)
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
            summary: None,
        }
    }
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
            error_message: r.error_message,
            duration_ms: r.duration_ms,
        }
    }
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
            invocation_count: r.invocation_count,
            framework_cost: r.framework_cost,
        }
    }
}

impl From<InvocationCostSummary> for InvocationCostView {
    fn from(r: InvocationCostSummary) -> Self {
        InvocationCostView {
            started_at_ms: chrono::DateTime::parse_from_rfc3339(&r.first_timestamp)
                .map(|d| d.timestamp_millis())
                .unwrap_or(0),
            invocation_id: r.invocation_id,
            event_count: r.event_count,
            total_cost: r.total_cost,
            total_input_tokens: r.total_input_tokens,
            total_output_tokens: r.total_output_tokens,
            total_cache_read_tokens: r.total_cache_read_tokens,
            total_cache_write_tokens: r.total_cache_write_tokens,
        }
    }
}

impl From<ModelCostSummary> for ModelCostView {
    fn from(r: ModelCostSummary) -> Self {
        ModelCostView {
            model: r.model,
            event_count: r.event_count,
            total_cost: r.total_cost,
            total_input_tokens: r.total_input_tokens,
            total_output_tokens: r.total_output_tokens,
        }
    }
}

impl From<CostBucketSummary> for CostBucketView {
    fn from(r: CostBucketSummary) -> Self {
        CostBucketView {
            bucket: r.bucket,
            total_cost: r.total_cost,
        }
    }
}

impl From<FailureSummary> for FailureView {
    fn from(r: FailureSummary) -> Self {
        FailureView {
            error_kind: r.error_kind,
            count: r.count,
        }
    }
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

// ============================================================
// Views — the read handle.
// ============================================================

/// Read-only handle over the runtime's three SQLite-backed stores (one file
/// per store, each opened `?mode=ro` — see [`crate::db::RuntimeDbPaths`]).
/// Cheap to construct relative to the queries it serves; a caller can hold
/// one for the lifetime of a request loop.
pub struct Views {
    pub(crate) projection: ProjectionStore, // read by `crate::trigger`'s Views block
    control_plane: ControlPlaneStore,
    worker: WorkerStore,
    /// The projection consumer's progress, when one runs in this
    /// process (the daemon). Absent on direct CLI reads — those
    /// serve whatever the fold currently holds and refuse `min_seq`.
    watermark: Option<crate::watermark::Watermark>,
}

impl Views {
    /// Open all three stores read-only against their per-store files. Errors
    /// if a file does not exist or a store's schema is incompatible; callers
    /// that want to distinguish "not initialised" should check the files
    /// first (as the CLI does).
    pub async fn open(paths: &RuntimeDbPaths) -> Result<Self, ViewsError> {
        let projection = ProjectionStore::open_read_only(&paths.projection).await?;
        let control_plane = ControlPlaneStore::open_read_only(&paths.control_plane).await?;
        let worker = WorkerStore::open_read_only(&paths.worker).await?;
        Ok(Views {
            projection,
            control_plane,
            worker,
            watermark: None,
        })
    }

    /// Attach the in-process projection consumer's watermark, enabling
    /// [`Views::at_watermark`] on this handle (the daemon's read path).
    pub fn with_watermark(mut self, watermark: crate::watermark::Watermark) -> Self {
        self.watermark = Some(watermark);
        self
    }

    /// Gate a read at a watermark: with `min_seq` absent this is free;
    /// otherwise wait — bounded by `bound` — until the projection's
    /// fold includes at least `min_seq` (the read-your-writes
    /// composition: `min_seq` comes from a command receipt's
    /// `watermark(domain)`). Read paths without an in-process
    /// projection refuse rather than serving a silently-stale answer.
    pub async fn at_watermark(
        &self,
        min_seq: Option<u64>,
        bound: std::time::Duration,
    ) -> Result<(), ViewsError> {
        let Some(min_seq) = min_seq else {
            return Ok(());
        };
        let Some(watermark) = &self.watermark else {
            return Err(ViewsError::WatermarkUnavailable);
        };
        watermark.wait_for(min_seq, bound).await?;
        Ok(())
    }

    async fn coordination_is_terminal(&self, invocation_id: &str) -> Result<bool, ViewsError> {
        let owner_terminal = self
            .control_plane
            .get_invocation_owner(invocation_id)
            .await?
            .is_some_and(|owner| {
                matches!(owner.status, OwnerStatus::Completed | OwnerStatus::Failed)
            });
        Ok(owner_terminal
            || self
                .control_plane
                .get_archive(invocation_id)
                .await?
                .is_some())
    }

    /// Total event count in the projection.
    pub async fn event_count(&self) -> Result<i64, ViewsError> {
        Ok(self.projection.count().await?)
    }

    /// Where one event's payload sits in the log — `event.get`'s
    /// first hop, resolving an identity to a position the log can be
    /// read at. See [`EventLocation`] for why three answers.
    pub async fn event_location(&self, event_id: &str) -> Result<EventLocation, ViewsError> {
        Ok(self.projection.event_location(event_id).await?)
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
    /// WAL. An in-flight invocation with an open dispatch — tool *or*
    /// LLM: a long `exec` and a long model turn both leave the WAL row
    /// silent (#130) — younger than `long_dispatch_threshold_ms` is
    /// *working*; otherwise it is *stuck* once its WAL row has not
    /// advanced within `stuck_threshold_ms`.
    pub async fn executions(
        &self,
        now_ms: i64,
        stuck_threshold_ms: i64,
        long_dispatch_threshold_ms: i64,
    ) -> Result<ExecutionsView, ViewsError> {
        let in_flight = self.worker.find_in_flight_invocations().await?;
        let mut view = ExecutionsView::default();
        for row in in_flight {
            if self.coordination_is_terminal(&row.invocation_id).await? {
                continue;
            }
            view.in_flight += 1;
            // Newest open dispatch of either kind. One dispatch-list
            // query per in-flight row — bounded by
            // max_concurrent_invocations, the same shape as
            // `active_invocations`.
            let open_tool_at = self
                .worker
                .open_tool_dispatches_for_invocation(&row.invocation_id)
                .await?
                .into_iter()
                .map(|d| d.dispatched_at.unwrap_or(d.intent_at))
                .max();
            let open_llm_at = self
                .worker
                .open_llm_dispatches_for_invocation(&row.invocation_id)
                .await?
                .into_iter()
                .map(|d| d.dispatched_at.unwrap_or(d.intent_at))
                .max();
            match classify_liveness(
                open_tool_at.max(open_llm_at),
                row.updated_at,
                now_ms,
                stuck_threshold_ms,
                long_dispatch_threshold_ms,
            ) {
                Liveness::Working => {
                    view.working += 1;
                    view.working_ids.push(row.invocation_id);
                }
                Liveness::Stuck => {
                    view.stuck += 1;
                    view.stuck_ids.push(row.invocation_id);
                }
                Liveness::Advancing => {}
            }
        }
        Ok(view)
    }

    /// Every currently-executing invocation as a row (the list behind
    /// [`Views::executions`]' counts), longest-running first, each with
    /// its open tool/LLM dispatches — the "what is running right now"
    /// table.
    pub async fn active_invocations(
        &self,
        now_ms: i64,
        stuck_threshold_ms: i64,
        long_dispatch_threshold_ms: i64,
    ) -> Result<Vec<ActiveInvocationView>, ViewsError> {
        let mut rows = self.worker.find_in_flight_invocations().await?;
        rows.sort_by_key(|r| r.started_at);
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            if self.coordination_is_terminal(&row.invocation_id).await? {
                continue;
            }
            let tool_rows = self
                .worker
                .open_tool_dispatches_for_invocation(&row.invocation_id)
                .await?;
            let llm_rows = self
                .worker
                .open_llm_dispatches_for_invocation(&row.invocation_id)
                .await?;
            let newest_open = tool_rows
                .iter()
                .map(|d| d.dispatched_at.unwrap_or(d.intent_at))
                .chain(
                    llm_rows
                        .iter()
                        .map(|d| d.dispatched_at.unwrap_or(d.intent_at)),
                )
                .max();
            let liveness = classify_liveness(
                newest_open,
                row.updated_at,
                now_ms,
                stuck_threshold_ms,
                long_dispatch_threshold_ms,
            );
            let open_tools = tool_rows
                .into_iter()
                .map(|t| OpenToolView {
                    command: open_tool_command(&t.parameters),
                    tool_name: t.tool_name,
                })
                .collect();
            let open_llms = llm_rows.into_iter().map(|l| l.model).collect();
            out.push(ActiveInvocationView {
                invocation_id: row.invocation_id,
                agent_id: row.agent_id,
                phase: row.phase,
                step_index: row.step_index,
                started_at_ms: row.started_at,
                updated_at_ms: row.updated_at,
                liveness,
                open_tools,
                open_llms,
                summary: None,
            });
        }
        // Join the one-line summaries (#216) in one pass.
        let ids: Vec<String> = out.iter().map(|v| v.invocation_id.clone()).collect();
        let mut summaries = self.projection.summaries_for(&ids).await?;
        for view in &mut out {
            view.summary = summaries.remove(&view.invocation_id);
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
                    agent_id: archived_agent_id(arc.agent_id),
                    worker_id: String::new(),
                    status: arc.final_phase,
                    assigned_at_ms: arc.archived_at,
                    started_at_ms: arc.started_at,
                    archived: true,
                    summary: None,
                });
            }
        }
        // Join the one-line summaries (#216) in one pass.
        let ids: Vec<String> = items.iter().map(|v| v.invocation_id.clone()).collect();
        let mut summaries = self.projection.summaries_for(&ids).await?;
        for view in &mut items {
            view.summary = summaries.remove(&view.invocation_id);
        }
        Ok(items)
    }

    /// The most recently archived invocations, newest first, capped at
    /// `limit`.
    pub async fn recent_archives(&self, limit: i64) -> Result<Vec<ArchiveView>, ViewsError> {
        let rows = self.control_plane.list_archives_recent(limit).await?;
        Ok(rows.into_iter().map(ArchiveView::from).collect())
    }

    /// The agent that owns an invocation, resolved from the projection's
    /// event rows. A thin point lookup for callers that need only the
    /// subject token (e.g. `fq invocation transcript --follow`) without
    /// paying for the full [`Views::invocation`] composition (#261).
    pub async fn agent_id_for_invocation(
        &self,
        invocation_id: &str,
    ) -> Result<Option<String>, ViewsError> {
        Ok(self
            .projection
            .agent_id_for_invocation(invocation_id)
            .await?)
    }

    /// Everything known about one invocation, composed across the projection,
    /// control-plane, and worker stores. Returns `None` when no store has any
    /// trace of the id.
    pub async fn invocation(
        &self,
        invocation_id: &str,
        now_ms: i64,
        stuck_threshold_ms: i64,
        long_dispatch_threshold_ms: i64,
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

        let coordination_terminal = owner
            .as_ref()
            .is_some_and(|o| matches!(o.status, OwnerStatus::Completed | OwnerStatus::Failed))
            || archive.is_some();
        let live = match state {
            Some(s) if !coordination_terminal => {
                let tools = self
                    .worker
                    .list_tool_dispatches_for_invocation(invocation_id)
                    .await?;
                let llms = self
                    .worker
                    .list_llm_dispatches_for_invocation(invocation_id)
                    .await?;
                let newest_open = tools
                    .iter()
                    .filter(|t| t.status != crate::worker::store::DispatchStatus::Completed)
                    .map(|t| t.dispatched_at.unwrap_or(t.intent_at))
                    .chain(
                        llms.iter()
                            .filter(|l| l.status != crate::worker::store::DispatchStatus::Completed)
                            .map(|l| l.dispatched_at.unwrap_or(l.intent_at)),
                    )
                    .max();
                Some(LiveExecutionView {
                    liveness: classify_liveness(
                        newest_open,
                        s.updated_at,
                        now_ms,
                        stuck_threshold_ms,
                        long_dispatch_threshold_ms,
                    ),
                    phase: s.phase,
                    step_index: s.step_index,
                    started_at_ms: s.started_at,
                    updated_at_ms: s.updated_at,
                    terminal_at_ms: s.terminal_at,
                    tools: tools.into_iter().map(ToolDispatchView::from).collect(),
                    llms: llms.into_iter().map(LlmDispatchView::from).collect(),
                })
            }
            Some(_) | None => None,
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

        let has_transcript = self.transcript(invocation_id).await?.is_some();

        let summary = self
            .projection
            .summaries_for(&[invocation_id.to_string()])
            .await?
            .remove(invocation_id);

        let cost = self
            .projection
            .cost_of_invocation(invocation_id)
            .await?
            .map(InvocationCostView::from);

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
            has_transcript,
            summary,
            cost,
        }))
    }
}

#[cfg(test)]
mod tests;
