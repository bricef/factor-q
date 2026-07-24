//! Read-only operator *views* over the runtime's stores.
//!
//! `views` is the single read model behind every operator surface: the `fq`
//! CLI read commands (as a formatter over these DTOs) and, later, the
//! read-only tarpc service that backs the operator dashboard
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
//! with the tarpc service, not here.

// DTOs are Deserialize as well as Serialize so they can travel over the
// read service's bincode wire (#105 layer 2), not just out as JSON.
use serde::{Deserialize, Serialize};

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
    /// In-flight invocations with a fresh open dispatch (tool or LLM).
    /// Both can legitimately outlive a reducer step's WAL silence, so
    /// they are judged by the dispatch's own age (#130).
    pub working: i64,
    pub working_ids: Vec<String>,
    /// In-flight invocations whose WAL row has not advanced within the
    /// caller-supplied stuck threshold and have no fresh open dispatch.
    pub stuck: i64,
    pub stuck_ids: Vec<String>,
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

/// The per-invocation liveness verdict the health page counts —
/// shared by every surface that shows an in-flight row, so the health
/// tile, the active table, and the detail page cannot drift apart.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Liveness {
    /// A fresh open dispatch (tool or LLM) — long runs are fine as
    /// long as the dispatch itself is younger than the long-dispatch
    /// threshold (#130).
    Working,
    /// Nothing open, but the WAL row advanced recently: the reducer is
    /// between steps. The quiet, healthy in-between.
    Advancing,
    /// No fresh dispatch AND the WAL row has not advanced within the
    /// stuck threshold — the row the operator needs to look at.
    Stuck,
}

impl Liveness {
    pub fn as_str(&self) -> &'static str {
        match self {
            Liveness::Working => "working",
            Liveness::Advancing => "advancing",
            Liveness::Stuck => "stuck",
        }
    }
}

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
    /// The health page's verdict for this row, colour-coded on the
    /// dashboard (see [`Liveness`]).
    pub liveness: Liveness,
    /// Open (non-completed) tool dispatches right now — name plus
    /// the command line when the tool is command-shaped.
    pub open_tools: Vec<OpenToolView>,
    /// Models with an open (non-completed) LLM dispatch right now.
    pub open_llms: Vec<String>,
    /// One-line operator summary (#216), when the summariser has
    /// produced one. `None` with the summariser disabled or before
    /// the first line lands.
    #[serde(default)]
    pub summary: Option<String>,
}

/// One open tool dispatch on a live invocation: the tool's name, plus
/// its command line when the parameters carry one — so the "doing"
/// column can say WHAT is running, not just which tool has been open
/// for four minutes.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OpenToolView {
    pub tool_name: String,
    /// The dispatch's command, when its parameters have a `command`
    /// field: exec-style argv arrays join with spaces, shell-style
    /// strings pass through. Capped server-side at
    /// [`OPEN_TOOL_COMMAND_CAP`] chars; `None` for tools without one.
    pub command: Option<String>,
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
    /// One-line operator summary (#216); see
    /// [`ActiveInvocationView::summary`].
    #[serde(default)]
    pub summary: Option<String>,
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
    pub error_message: Option<String>,
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
            error_message: r.error_message,
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
    /// Distinct invocations behind the aggregate.
    pub invocation_count: i64,
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
        }
    }
}

/// One invocation's share of an agent's spend.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct InvocationCostView {
    pub invocation_id: String,
    /// Epoch ms of the invocation's first cost event (its effective
    /// start as the projection sees it); 0 when the stored timestamp
    /// fails to parse.
    pub started_at_ms: i64,
    pub event_count: i64,
    pub total_cost: f64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    pub total_cache_write_tokens: i64,
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

/// One model's share of an agent's spend.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ModelCostView {
    pub model: String,
    pub event_count: i64,
    pub total_cost: f64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
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

/// One agent's cost drill-down: its own totals plus per-model and
/// per-invocation breakdowns — the dashboard's `/costs/<agent>` page
/// and any future `fq costs show <agent>`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AgentCostDetailView {
    pub agent_id: String,
    pub totals: CostView,
    /// Biggest spender first.
    pub models: Vec<ModelCostView>,
    /// Newest first, capped by the caller's limit;
    /// `totals.invocation_count` carries the uncapped count.
    pub invocations: Vec<InvocationCostView>,
}

/// One time bucket's cost sum — a day or an hour, keyed by its
/// fixed-width UTC timestamp prefix (`YYYY-MM-DD` / `YYYY-MM-DDTHH`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct CostBucketView {
    pub bucket: String,
    pub total_cost: f64,
}

impl From<CostBucketSummary> for CostBucketView {
    fn from(r: CostBucketSummary) -> Self {
        CostBucketView {
            bucket: r.bucket,
            total_cost: r.total_cost,
        }
    }
}

/// Per-agent costs plus the per-model split and the grand totals, so a
/// caller renders all three without re-summing.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct CostReport {
    pub agents: Vec<CostView>,
    /// Spend over time within the window — daily buckets, or hourly
    /// when the caller asked for them. Sparse: quiet buckets are
    /// absent (display layers fill gaps). Oldest first.
    #[serde(default)]
    pub buckets: Vec<CostBucketView>,
    /// The same cost rows grouped by model, biggest spender first —
    /// spend by capability tier rather than by consumer.
    pub models: Vec<ModelCostView>,
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
    /// The health page's verdict for this run (see [`Liveness`]).
    pub liveness: Liveness,
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
    /// Whether the worker WAL contains dispatch rows for a transcript.
    #[serde(default)]
    pub has_transcript: bool,
    /// One-line operator summary (#216); see
    /// [`ActiveInvocationView::summary`].
    #[serde(default)]
    pub summary: Option<String>,
    /// The invocation's cost so far — llm calls, tokens, and spend
    /// summed from the projection's cost-bearing events. Grows while
    /// the run is live; `None` before the first priced call lands.
    #[serde(default)]
    pub cost: Option<InvocationCostView>,
}

// ============================================================
// Views — the read handle.
// ============================================================

/// Read-only handle over the runtime's three SQLite-backed stores (one file
/// per store, each opened `?mode=ro` — see [`crate::db::RuntimeDbPaths`]).
/// Cheap to construct relative to the queries it serves; a caller can hold
/// one for the lifetime of a request loop.
pub struct Views {
    projection: ProjectionStore,
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
        hourly_buckets: bool,
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
        report.models = self
            .projection
            .cost_by_model(agent, since)
            .await?
            .into_iter()
            .map(ModelCostView::from)
            .collect();
        // The time series ignores the agent filter deliberately: the
        // chart answers "what is the fleet burning", the agent filter
        // narrows the tables. Revisit if a per-agent chart is wanted.
        report.buckets = self
            .projection
            .cost_by_time_bucket(hourly_buckets, since)
            .await?
            .into_iter()
            .map(CostBucketView::from)
            .collect();
        Ok(report)
    }

    /// One agent's cost drill-down — totals plus per-model and
    /// per-invocation breakdowns (invocations newest first, capped at
    /// `invocation_limit`). `None` when the agent has no cost events
    /// in the window.
    pub async fn agent_costs(
        &self,
        agent: &str,
        since: Option<&str>,
        invocation_limit: i64,
    ) -> Result<Option<AgentCostDetailView>, ViewsError> {
        let mut rows = self.projection.cost_summary(Some(agent), since).await?;
        let Some(totals) = rows.pop() else {
            return Ok(None);
        };
        let models = self
            .projection
            .cost_by_model(Some(agent), since)
            .await?
            .into_iter()
            .map(ModelCostView::from)
            .collect();
        let invocations = self
            .projection
            .cost_by_invocation(agent, since, invocation_limit)
            .await?
            .into_iter()
            .map(InvocationCostView::from)
            .collect();
        Ok(Some(AgentCostDetailView {
            agent_id: agent.to_string(),
            totals: CostView::from(totals),
            models,
            invocations,
        }))
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
        let mut view = ExecutionsView {
            in_flight: in_flight.len() as i64,
            ..Default::default()
        };
        for row in in_flight {
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

    /// The payload-bearing transcript for one invocation, reconstructed
    /// from the worker WAL (`llm_dispatch` + `tool_dispatch` — the only
    /// place payloads persist; those rows outlive archival, so this
    /// works for completed invocations too). `None` when the id has no
    /// dispatch rows at all.
    pub async fn transcript(
        &self,
        invocation_id: &str,
    ) -> Result<Option<Vec<crate::transcript::TranscriptEntry>>, ViewsError> {
        let llm = self
            .worker
            .list_llm_dispatches_for_invocation(invocation_id)
            .await?;
        let tools = self
            .worker
            .list_tool_dispatches_for_invocation(invocation_id)
            .await?;
        if llm.is_empty() && tools.is_empty() {
            return Ok(None);
        }
        let mut entries = crate::transcript::collect_transcript(&llm, &tools);

        // Close the story: a terminal invocation gets an explicit
        // Outcome entry so the transcript states whether more turns are
        // expected. The live WAL row (if still present) knows the
        // terminal phase; after archive hand-off the archive row does.
        let terminal = match self.worker.get_invocation_state(invocation_id).await? {
            Some(state) => state.terminal_at.map(|at| (at, state.phase)),
            None => self
                .control_plane
                .get_archive(invocation_id)
                .await?
                .map(|a| (a.terminal_at, a.final_phase)),
        };
        if let Some((timestamp_ms, phase)) = terminal {
            entries.push(crate::transcript::TranscriptEntry::Outcome {
                timestamp_ms,
                phase,
            });
        }
        Ok(Some(entries))
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
mod tests {
    use super::*;
    use crate::control_plane::store::{InvocationArchiveRow, WorkerRow, WorkerStatus};
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
            seq: None,
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
                invocation_count: 1,
            });
        }
        assert_eq!(report.agents.len(), 2);
        assert!((report.total_cost - 3.5).abs() < f64::EPSILON);
        assert_eq!(report.total_input_tokens, 15);
        assert_eq!(report.total_output_tokens, 27);
    }

    /// The RFC3339 projection timestamp becomes epoch ms on the view;
    /// an unparseable value degrades to 0, never a panic.
    #[test]
    fn invocation_cost_view_parses_rfc3339_start() {
        let summary = |ts: &str| InvocationCostSummary {
            invocation_id: "inv-1".into(),
            first_timestamp: ts.into(),
            event_count: 1,
            total_cost: 0.1,
            total_input_tokens: 10,
            total_output_tokens: 5,
            total_cache_read_tokens: 0,
            total_cache_write_tokens: 0,
        };
        let v = InvocationCostView::from(summary("1970-01-01T00:00:01+00:00"));
        assert_eq!(v.started_at_ms, 1_000);
        let bad = InvocationCostView::from(summary("not-a-time"));
        assert_eq!(bad.started_at_ms, 0);
    }

    /// The command gist: argv arrays join, strings pass through,
    /// absent/odd shapes are None, and the cap truncates on a char
    /// boundary with an ellipsis.
    #[test]
    fn open_tool_command_reads_both_shapes_and_caps() {
        assert_eq!(
            open_tool_command(r#"{"command":["cargo","test","--lib"],"cwd":"/w"}"#),
            Some("cargo test --lib".to_string())
        );
        assert_eq!(
            open_tool_command(r#"{"command":"[\"ls\", \"-la\"]","cwd":"/w"}"#),
            Some("[\"ls\", \"-la\"]".to_string())
        );
        assert_eq!(open_tool_command(r#"{"path":"/tmp/x"}"#), None);
        assert_eq!(open_tool_command(r#"{"command":42}"#), None);
        assert_eq!(open_tool_command("not json"), None);
        assert_eq!(open_tool_command(r#"{"command":[]}"#), None);
        let long = format!(r#"{{"command":["{}"]}}"#, "x".repeat(300));
        let capped = open_tool_command(&long).unwrap();
        assert_eq!(capped.chars().count(), OPEN_TOOL_COMMAND_CAP + 1);
        assert!(capped.ends_with('…'));
    }

    // ---- DB wiring smoke test (empty, freshly-created stores) ----

    /// Create the three stores' schemas in one temp DB file, then open a
    /// read-only `Views` over it and assert the query methods wire up and
    /// return empty / not-found on an empty database.
    #[tokio::test]
    async fn open_and_query_empty_db() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimeDbPaths::under(dir.path());

        // Open each store in write mode once to create its tables in the
        // shared file, then drop the write handles.
        {
            let _cp = ControlPlaneStore::open(&paths.control_plane).await.unwrap();
            let _ws = WorkerStore::open(&paths.worker).await.unwrap();
            let _proj = ProjectionStore::open(&paths.projection).await.unwrap();
        }

        let views = Views::open(&paths).await.unwrap();

        assert_eq!(views.event_count().await.unwrap(), 0);
        assert!(views.workers().await.unwrap().is_empty());
        assert!(views.events(None, None, None, 50).await.unwrap().is_empty());
        assert!(
            views
                .costs(None, None, false)
                .await
                .unwrap()
                .agents
                .is_empty()
        );
        assert!(
            views
                .agent_costs("no-such-agent", None, 10)
                .await
                .unwrap()
                .is_none()
        );
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
        assert_eq!(
            views
                .executions(1_000, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
                .await
                .unwrap()
                .in_flight,
            0
        );
        assert!(
            views
                .active_invocations(1_000, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            views
                .invocation(
                    "no-such-id",
                    1_000,
                    30_000,
                    DEFAULT_LONG_DISPATCH_THRESHOLD_MS
                )
                .await
                .unwrap()
                .is_none()
        );
        assert!(views.worker("no-such-worker").await.unwrap().is_none());
    }

    /// Archive-only tombstones are written by operator recovery with a reserved
    /// agent id; they have no attributable agent on the invocation list.
    #[tokio::test]
    async fn invocation_index_hides_sentinel_archive_agents() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimeDbPaths::under(dir.path());
        {
            let cp = ControlPlaneStore::open(&paths.control_plane).await.unwrap();
            for (id, agent_id) in [("tombstone", AgentId::OPERATOR_STR), ("real", "agent-a")] {
                cp.insert_archive(&InvocationArchiveRow {
                    invocation_id: id.into(),
                    agent_id: agent_id.into(),
                    final_phase: "failed".into(),
                    final_state_blob: vec![],
                    started_at: 10,
                    terminal_at: 20,
                    archived_at: if id == "tombstone" { 30 } else { 40 },
                })
                .await
                .unwrap();
            }
            let _ws = WorkerStore::open(&paths.worker).await.unwrap();
            let _proj = ProjectionStore::open(&paths.projection).await.unwrap();
        }

        let index = Views::open(&paths)
            .await
            .unwrap()
            .invocation_index(None, true, 50)
            .await
            .unwrap();
        assert_eq!(
            index
                .iter()
                .find(|row| row.invocation_id == "tombstone")
                .unwrap()
                .agent_id,
            None
        );
        assert_eq!(
            index
                .iter()
                .find(|row| row.invocation_id == "real")
                .unwrap()
                .agent_id
                .as_deref(),
            Some("agent-a")
        );
    }

    /// #216: the one-line summary joins onto both invocation surfaces
    /// (the active list and the invocation index) from the
    /// projection's `invocation_summary` table.
    #[tokio::test]
    async fn summary_line_joins_onto_invocation_surfaces() {
        use crate::agent::AgentId;
        use crate::events::{Event, EventPayload, InvocationSummaryPayload, SummaryKind};

        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimeDbPaths::under(dir.path());
        let inv = uuid::Uuid::now_v7();
        {
            let cp = ControlPlaneStore::open(&paths.control_plane).await.unwrap();
            let ws = WorkerStore::open(&paths.worker).await.unwrap();
            let proj = ProjectionStore::open(&paths.projection).await.unwrap();
            cp.register_worker("w1", "host", 100).await.unwrap();
            cp.assign_invocation(&inv.to_string(), "w1", 100)
                .await
                .unwrap();
            let row = InvocationStateRow {
                invocation_id: inv.to_string(),
                agent_id: "agent-a".into(),
                schema_version: 1,
                phase: "reducing".into(),
                state_blob: vec![],
                step_index: 1,
                started_at: 100,
                updated_at: 150,
                terminal_at: None,
                workspace_ref: None,
                archive_status: None,
                archive_published_at: None,
                trigger_source: None,
                trigger_subject: None,
                trigger_payload: None,
            };
            ws.upsert_invocation_state(&row).await.unwrap();
            proj.insert_event(&Event::new(
                AgentId::summary(),
                inv,
                EventPayload::InvocationSummary(InvocationSummaryPayload {
                    kind: SummaryKind::Progress,
                    summary: "Fixing #7: editing widget.rs".to_string(),
                }),
            ))
            .await
            .unwrap();
        }

        let views = Views::open(&paths).await.unwrap();

        let active = views
            .active_invocations(200, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
            .await
            .unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(
            active[0].summary.as_deref(),
            Some("Fixing #7: editing widget.rs")
        );

        let index = views.invocation_index(None, true, 50).await.unwrap();
        assert_eq!(index.len(), 1);
        assert_eq!(
            index[0].summary.as_deref(),
            Some("Fixing #7: editing widget.rs")
        );
    }

    /// Seed a worker and an in-flight invocation, then read them back through
    /// `Views` — exercises the cross-store composition end to end.
    #[tokio::test]
    async fn reads_back_seeded_worker_and_invocation() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimeDbPaths::under(dir.path());

        {
            let cp = ControlPlaneStore::open(&paths.control_plane).await.unwrap();
            cp.register_worker("w1", "localhost", 100).await.unwrap();

            let ws = WorkerStore::open(&paths.worker).await.unwrap();
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
            ws.write_tool_intent("inv-1", "call-1", "exec", "{}", 160)
                .await
                .unwrap();
            ws.write_tool_dispatched("inv-1", "call-1", 170)
                .await
                .unwrap();
            let _proj = ProjectionStore::open(&paths.projection).await.unwrap();
        }

        let views = Views::open(&paths).await.unwrap();

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
        let execs = views
            .executions(200, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
            .await
            .unwrap();
        assert_eq!(execs.in_flight, 1);
        assert_eq!(execs.stuck, 0);

        // An open exec dispatch remains working even though the invocation
        // WAL has not advanced for longer than the ordinary stuck threshold.
        let execs_working = views
            .executions(60_000, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
            .await
            .unwrap();
        assert_eq!(execs_working.working, 1);
        assert_eq!(execs_working.stuck, 0);

        // ...but is flagged stuck once the dispatch itself is too old.
        let execs_late = views
            .executions(1_000_000, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
            .await
            .unwrap();
        assert_eq!(execs_late.working, 0);
        assert_eq!(execs_late.stuck, 1);
        assert_eq!(execs_late.stuck_ids, vec!["inv-1".to_string()]);

        // The detail view composes the live WAL state.
        let detail = views
            .invocation("inv-1", 200, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
            .await
            .unwrap()
            .unwrap();
        let live = detail.live.expect("in-flight invocation has live state");
        assert_eq!(live.phase, "reducing");
        assert_eq!(live.step_index, 3);
        assert_eq!(live.tools.len(), 1);
        assert_eq!(live.tools[0].tool_name, "exec");

        // The active list carries the same WAL row, row-form.
        let active = views
            .active_invocations(200, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
            .await
            .unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].invocation_id, "inv-1");
        assert_eq!(active[0].agent_id, "agent-a");
        assert_eq!(active[0].phase, "reducing");
        assert_eq!(active[0].step_index, 3);
        assert_eq!(active[0].open_tools.len(), 1);
        assert_eq!(active[0].open_tools[0].tool_name, "exec");
        assert_eq!(active[0].open_tools[0].command, None);
        // Fresh open dispatch → the same verdict health counts.
        assert_eq!(active[0].liveness, Liveness::Working);

        // Same row viewed much later: the dispatch has gone stale and
        // the WAL never advanced — the active table says stuck exactly
        // when the health tile does.
        let active_late = views
            .active_invocations(1_000_000, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
            .await
            .unwrap();
        assert_eq!(active_late[0].liveness, Liveness::Stuck);
    }

    /// An open LLM dispatch counts as working the same way a tool dispatch
    /// does (#130) — a long reducer-side model call is not a stuck invocation.
    #[tokio::test]
    async fn open_llm_dispatch_counts_as_working() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimeDbPaths::under(dir.path());

        {
            let _cp = ControlPlaneStore::open(&paths.control_plane).await.unwrap();
            let ws = WorkerStore::open(&paths.worker).await.unwrap();
            let row = InvocationStateRow {
                invocation_id: "inv-llm".into(),
                agent_id: "agent-a".into(),
                schema_version: 1,
                phase: "reducing".into(),
                state_blob: vec![],
                step_index: 1,
                started_at: 100,
                updated_at: 150,
                terminal_at: None,
                workspace_ref: None,
                archive_status: None,
                archive_published_at: None,
                trigger_source: None,
                trigger_subject: None,
                trigger_payload: None,
            };
            ws.upsert_invocation_state(&row).await.unwrap();
            ws.write_llm_intent("inv-llm", "req-1", "claude-opus-4-8", "{}", 160)
                .await
                .unwrap();
            ws.write_llm_dispatched("inv-llm", "req-1", 170)
                .await
                .unwrap();
            let _proj = ProjectionStore::open(&paths.projection).await.unwrap();
        }

        let views = Views::open(&paths).await.unwrap();

        // WAL is stale past the stuck threshold, but the LLM dispatch is
        // fresh — working, not stuck.
        let execs = views
            .executions(60_000, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
            .await
            .unwrap();
        assert_eq!(execs.working, 1);
        assert_eq!(execs.working_ids, vec!["inv-llm".to_string()]);
        assert_eq!(execs.stuck, 0);

        // Once the dispatch itself exceeds the long-dispatch threshold the
        // invocation falls through to the stuck check.
        let execs_late = views
            .executions(1_000_000, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
            .await
            .unwrap();
        assert_eq!(execs_late.working, 0);
        assert_eq!(execs_late.stuck, 1);
    }

    /// A terminal invocation's transcript closes with an Outcome entry —
    /// the explicit "no more turns expected" signal; a live one carries
    /// no Outcome (#105 SSE slice).
    #[tokio::test]
    async fn transcript_outcome_reflects_terminality() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimeDbPaths::under(dir.path());
        {
            let ws = WorkerStore::open(&paths.worker).await.unwrap();
            for (inv, terminal_at) in [("inv-done", Some(150_i64)), ("inv-live", None)] {
                ws.write_llm_intent(inv, "req-1", "m", "{}", 100)
                    .await
                    .unwrap();
                ws.write_llm_dispatched(inv, "req-1", 101).await.unwrap();
                ws.write_llm_completed(inv, "req-1", r#"{"content":"done"}"#, false, 0.01, 102)
                    .await
                    .unwrap();
                let row = InvocationStateRow {
                    invocation_id: inv.into(),
                    agent_id: "agent-a".into(),
                    schema_version: 1,
                    phase: if terminal_at.is_some() {
                        "completed".into()
                    } else {
                        "reducing".into()
                    },
                    state_blob: vec![],
                    step_index: 4,
                    started_at: 100,
                    updated_at: 140,
                    terminal_at,
                    workspace_ref: None,
                    archive_status: None,
                    archive_published_at: None,
                    trigger_source: None,
                    trigger_subject: None,
                    trigger_payload: None,
                };
                ws.upsert_invocation_state(&row).await.unwrap();
            }
            let _cp = ControlPlaneStore::open(&paths.control_plane).await.unwrap();
            let _proj = ProjectionStore::open(&paths.projection).await.unwrap();
        }

        let views = Views::open(&paths).await.unwrap();

        let done = views.transcript("inv-done").await.unwrap().expect("some");
        match done.last().expect("entries") {
            crate::transcript::TranscriptEntry::Outcome {
                phase,
                timestamp_ms,
            } => {
                assert_eq!(phase, "completed");
                assert_eq!(*timestamp_ms, 150);
            }
            other => panic!("expected Outcome last, got {other:?}"),
        }

        let live = views.transcript("inv-live").await.unwrap().expect("some");
        assert!(
            !live
                .iter()
                .any(|e| matches!(e, crate::transcript::TranscriptEntry::Outcome { .. })),
            "live invocation must not carry an Outcome"
        );
    }

    /// An in-flight row whose `updated_at` is in the future (worker clock
    /// ahead) is not "stuck" — `is_stale`'s saturating age handles skew.
    /// This guard moved here from `fq doctor`'s tests when the stuck
    /// determination moved into `executions()` (#105 layer 1).
    #[tokio::test]
    async fn executions_ignore_clock_skew() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimeDbPaths::under(dir.path());

        const NOW: i64 = 1_000_000;
        {
            let ws = WorkerStore::open(&paths.worker).await.unwrap();
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
            let _cp = ControlPlaneStore::open(&paths.control_plane).await.unwrap();
            let _proj = ProjectionStore::open(&paths.projection).await.unwrap();
        }

        let views = Views::open(&paths).await.unwrap();
        let execs = views
            .executions(NOW, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
            .await
            .unwrap();
        assert_eq!(execs.in_flight, 1);
        assert_eq!(execs.stuck, 0, "future updated_at must not read as stuck");
    }
}
