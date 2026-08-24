//! The operator surface's view types — the shapes every read answers
//! with.
//!
//! These are data and nothing else. Producing them means reading the
//! runtime's stores, which is `fq-runtime`'s job: the `From<Row>`
//! conversions stay there, beside the rows they convert. A consumer
//! that renders a view rather than assembling one links this crate and
//! none of the storage behind it.

// The `since` argument every time-narrowed read takes, and the one
// grammar its callers share. Parsing a spelling into a bound is pure,
// so it sits with the shapes; comparing that bound against a column is
// the store's, and stays with the store.
pub mod since;

use serde::{Deserialize, Serialize};

// ============================================================
// View DTOs — the shape the CLI and the API both consume.
// All timestamps are surfaced with explicit units in the field
// name so a browser/JSON consumer never has to guess.
// ============================================================

/// One worker in the roster: the Worker view's **index** row (`worker.list`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
pub struct WorkerView {
    pub worker_id: String,
    pub host: String,
    pub registered_at_ms: i64,
    pub last_heartbeat_ms: i64,
    /// `alive` / `stale` / `shutdown`, as recorded by the control-plane.
    pub status: String,
    /// Invocations this worker currently owns in a non-terminal state
    /// (`in_flight` or `ambiguous`). Counted by the reader that
    /// assembles the roster; a bare row conversion leaves it 0.
    pub in_flight_count: i64,
}

/// One worker plus the invocations it currently owns: the Worker
/// view's **state**, what `worker.get` answers with. `worker` is
/// nested rather than serde-flattened, and stays that way — the
/// nesting is `fq workers show --json`'s committed shape.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
pub struct WorkerDetailView {
    pub worker: WorkerView,
    /// Every ownership row for this worker (any status), newest first.
    pub owned: Vec<InvocationSummaryView>,
}

/// Recovery-state counts — the data behind `fq status`'s recovery block and
/// the dashboard's health tile. Computed against a caller-supplied `now_ms`
/// and threshold so the view stays pure (no wall-clock inside).
///
/// Schema'd because `control.status` declares it as part of its output.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, schemars::JsonSchema)]
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

/// The per-invocation liveness verdict the health page counts —
/// shared by every surface that shows an in-flight row, so the health
/// tile, the active table, and the detail page cannot drift apart.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Liveness {
    /// A fresh open dispatch (tool or LLM) — long runs are fine as
    /// long as the dispatch itself is younger than the long-dispatch
    /// threshold.
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

/// One invocation this daemon is executing right now: how far it has
/// got, when it last advanced, and which tool or model calls it
/// currently has open. The row form of [`ExecutionsView`]'s counts.
// `invocation.active` declares this as its output, which means these
// comments are PUBLISHED: schemars lifts them onto the operator
// surface, where the reader has none of this repository's context.
// Say what a stranger needs; keep the reasons below, off the wire.
//
// Implementation note, deliberately not a doc comment so it stays off
// the surface: these rows are read from the worker-local WAL rather
// than the control plane's ownership table, because trigger dispatch
// does not populate the latter's `in_flight` status yet (#50). That is
// why this report and `invocation.list{status:"in_flight"}` can
// disagree today. Closing that gap would not merge them — a fold
// answered at a watermark and live machinery state remain different
// questions, which is what makes this a report rather than a filter.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
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
    /// One-line operator summary, when the summariser has
    /// produced one. `None` with the summariser disabled or before
    /// the first line lands.
    #[serde(default)]
    pub summary: Option<String>,
}

/// One open tool dispatch on a live invocation: the tool's name, plus
/// its command line when the parameters carry one — so the "doing"
/// column can say WHAT is running, not just which tool has been open
/// for four minutes.
///
/// Schema'd because `invocation.active` declares it as part of its
/// output.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
pub struct OpenToolView {
    pub tool_name: String,
    /// The dispatch's command, when its parameters have a `command`
    /// field: exec-style argv arrays join with spaces, shell-style
    /// strings pass through. Capped server-side at
    /// the reader's command cap; `None` for tools without one.
    pub command: Option<String>,
}

/// One row in the invocation list: a coordination-ownership row, or (in
/// the merged index) an archive-only row flagged `archived`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
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
    /// One-line operator summary; see
    /// [`ActiveInvocationView::summary`].
    #[serde(default)]
    pub summary: Option<String>,
}

/// A finalised invocation's archive record.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
pub struct ArchiveView {
    pub invocation_id: String,
    pub agent_id: String,
    pub final_phase: String,
    pub started_at_ms: i64,
    pub terminal_at_ms: i64,
    pub archived_at_ms: i64,
}

/// One event row from the projection — the Event atom's **index**
/// row (`event.list`). Extracted fields, never the payload;
/// `event_id` is the identity that reads the whole event back
/// through `event.get`, whenever the payload is still retained.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, schemars::JsonSchema)]
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

/// Per-agent cost/token aggregate.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, schemars::JsonSchema)]
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
    /// Summary costs: engine spend on this agent's behalf that belongs
    /// to no one invocation. Included in `total_cost` and excluded
    /// from every per-invocation figure, so
    /// `total_cost - framework_cost` is what those account for.
    ///
    /// Zero for an ordinary agent. For the reserved `summary` agent it
    /// is the whole row, which is why that drill-down shows spend with
    /// no invocations under it.
    #[serde(default)]
    pub framework_cost: f64,
}

/// One invocation's share of an agent's spend.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, schemars::JsonSchema)]
pub struct InvocationCostView {
    pub invocation_id: String,
    /// Epoch ms of the invocation's first cost event (its effective
    /// start as the projection sees it); 0 when the stored timestamp
    /// fails to parse.
    pub started_at_ms: i64,
    pub event_count: i64,
    /// This invocation's own spend. Does not include summary costs —
    /// those are carried by the agent's `framework_cost`.
    pub total_cost: f64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    pub total_cache_write_tokens: i64,
}

/// One model's share of an agent's spend.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, schemars::JsonSchema)]
pub struct ModelCostView {
    pub model: String,
    pub event_count: i64,
    pub total_cost: f64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
}

/// One agent's cost drill-down: its own totals plus per-model and
/// per-invocation breakdowns — the dashboard's `/costs/<agent>` page
/// and any future `fq costs show <agent>`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, schemars::JsonSchema)]
pub struct AgentCostDetailView {
    pub agent_id: String,
    pub totals: CostView,
    /// Biggest spender first.
    pub models: Vec<ModelCostView>,
    /// Newest first, capped by the caller's limit;
    /// `totals.invocation_count` carries the uncapped count. Summary
    /// costs are not here — uncapped, these sum to
    /// `totals.total_cost - totals.framework_cost`.
    pub invocations: Vec<InvocationCostView>,
}

/// One time bucket's cost sum — a day or an hour, keyed by its
/// fixed-width UTC timestamp prefix (`YYYY-MM-DD` / `YYYY-MM-DDTHH`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, schemars::JsonSchema)]
pub struct CostBucketView {
    pub bucket: String,
    pub total_cost: f64,
}

/// Per-agent costs plus the per-model split and the grand totals, so a
/// caller renders all three without re-summing.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, schemars::JsonSchema)]
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
    /// Summary costs across every agent. Included in `total_cost`, and
    /// named here so `total = invocations + framework` reads off the
    /// page rather than looking like a discrepancy.
    #[serde(default)]
    pub framework_cost: f64,
}

/// One terminal-failure bucket, grouped by kind.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct FailureView {
    pub error_kind: String,
    pub count: i64,
}

/// One in-flight tool dispatch (worker WAL).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, schemars::JsonSchema)]
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

/// One in-flight LLM dispatch (worker WAL).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, schemars::JsonSchema)]
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

/// Live execution state of an in-flight invocation, from the worker WAL —
/// the "what is it doing right now" view. Present only while the invocation
/// has a WAL row (deleted on archive hand-off).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, schemars::JsonSchema)]
pub struct LiveExecutionView {
    /// The health page's verdict for this run (see [`Liveness`]).
    pub liveness: Liveness,
    pub phase: String,
    /// Reducer *step* counter (every model and tool step) — not the
    /// model-turn count that `max_iterations` gates.
    pub step_index: u32,
    pub started_at_ms: i64,
    pub updated_at_ms: i64,
    pub terminal_at_ms: Option<i64>,
    pub tools: Vec<ToolDispatchView>,
    pub llms: Vec<LlmDispatchView>,
}

/// Everything known about one invocation, composed across the three stores.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, schemars::JsonSchema)]
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
    /// One-line operator summary; see
    /// [`ActiveInvocationView::summary`].
    #[serde(default)]
    pub summary: Option<String>,
    /// The invocation's cost so far — llm calls, tokens, and spend
    /// summed from the projection's cost-bearing events. Grows while
    /// the run is live; `None` before the first priced call lands.
    #[serde(default)]
    pub cost: Option<InvocationCostView>,
}
