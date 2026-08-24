//! The typed payload layer: what each event *says*, one struct per
//! `event_type`.
//!
//! The payload is the contract between graph nodes — the only layer
//! that drives downstream agent behaviour (see the [`events`](super)
//! module header for the three layers and their differing write
//! permissions). The variants that select between these shapes are
//! [`EventPayload`](super::EventPayload); the LLM-call cluster has its
//! own file, [`llm`](super::llm).
//!
//! Shapes only. Filling one in from what the runtime just did — a
//! reducer step, a WAL row, a worker's recovery scan — stays in
//! `fq-runtime` beside the state it reads.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::agent::{CapabilityValidation, ElicitationGrant, RootsGrant, SamplingGrant};
use crate::events::ToolCallId;
use crate::worker::WorkerId;

/// The fixed sentinel every host-notice body is wrapped in
/// (`<host-notice>…</host-notice>`) — one marker for every producer,
/// forever (#88). It separates "runtime ambient info" from "principal
/// speaking" in the conversation, and the equivalence oracle strips
/// sentinel-prefixed user messages when comparing resumed traces
/// against uninterrupted references.
pub const HOST_NOTICE_SENTINEL: &str = "<host-notice>";

/// A durable host notice injected at a reducer step boundary (#155).
/// `body` is the producer-rendered text, sentinel included — the exact
/// string persisted in the WAL and replayed verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostNoticePayload {
    /// Producer discriminator (`resume` | `tools_changed` |
    /// `context_pressure`, …).
    pub kind: String,
    pub body: String,
}

/// Which moment of the invocation an [`EventPayload::InvocationSummary`](super::EventPayload::InvocationSummary)
/// describes. `Outcome` is terminal — the last summary an invocation
/// receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryKind {
    /// From the trigger payload: what work was expected.
    Start,
    /// Rolling update from the latest model turn: what it is doing now.
    Progress,
    /// Final line on `completed`/`failed`, naming the failure kind.
    Outcome,
}

/// A one-line operator-facing invocation summary (#216). The
/// summariser's token usage and cost ride `envelope.cost`
/// ([`CostMetadata`](super::CostMetadata)) exactly as they do for `llm_response` events, so
/// the payload carries only the line itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationSummaryPayload {
    pub kind: SummaryKind,
    /// The single summary line (bounded by `[summary].max_line_chars`).
    pub summary: String,
}

/// Published when an agent invocation begins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggeredPayload {
    /// Which trigger this invocation answers (the runtime's `trigger`
    /// module owns the trigger itself).
    ///
    /// Optional in the *deserialised* form only: events on the log
    /// predate the identity, and a required field would break replay
    /// and older peers. Every event written since carries one.
    #[serde(default)]
    pub trigger_id: Option<Uuid>,
    pub trigger_source: TriggerSource,
    pub trigger_subject: Option<String>,
    pub trigger_payload: Value,
    pub config_snapshot: ConfigSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TriggerSource {
    Manual,
    Subject,
    Schedule,
}

/// Snapshot of the agent's configuration at trigger time.
///
/// Captured on `triggered` so that replay is meaningful even if the agent
/// definition is later modified.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    pub name: String,
    pub model: String,
    pub system_prompt: String,
    pub tools: Vec<String>,
    pub sandbox: SandboxSnapshot,
    pub budget: Option<f64>,
    /// MCP capability grants (ADR-0017) captured for audit. Absent for
    /// snapshots written before Step 8 / for agents that grant nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling: Option<SamplingGrant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roots: Option<RootsGrant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elicitation: Option<ElicitationGrant>,
    /// Per-capability validation policy (redaction + evaluator gates),
    /// captured for audit. Default-empty for agents that configure none.
    #[serde(default, skip_serializing_if = "CapabilityValidation::is_empty")]
    pub sampling_validation: CapabilityValidation,
    #[serde(default, skip_serializing_if = "CapabilityValidation::is_empty")]
    pub elicitation_validation: CapabilityValidation,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SandboxSnapshot {
    #[serde(default)]
    pub fs_read: Vec<String>,
    #[serde(default)]
    pub fs_write: Vec<String>,
    #[serde(default)]
    pub network: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub exec_cwd: Vec<String>,
}

/// Published when the agent invokes a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallPayload {
    /// The initiating turn's Round; 0 on pre-field events.
    #[serde(default)]
    pub round: u64,
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub parameters: Value,
}

/// WAL middle-state event for tool dispatch. Emitted between
/// [`ToolCallPayload`] and [`ToolResultPayload`] once the tool
/// has returned control to the runtime — before the result is
/// durably written.
///
/// Operationally informational: downstream consumers can ignore
/// it (existing consumers do). Recovery uses the matching
/// `tool_dispatch.status = 'dispatched'` row in the worker
/// store, not this event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDispatchedPayload {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
}

/// Payload for [`EventPayload::InvocationAmbiguous`](super::EventPayload::InvocationAmbiguous). Carries
/// the minimum context an operator needs to make a recovery
/// decision: which kind of dispatch was stuck, and which
/// call_id it was on. The full context (parameters, request
/// payload, etc.) is in the worker's WAL and surfaced via
/// `fq recover` (step 9).
///
/// A failed automatic resume also rides this event (#64) with
/// the sentinel `stuck_entity: "recovery"`; there is no stuck
/// dispatch in that mode, so `stuck_call_id` carries the
/// invocation id and `note` carries the resume error. Emission
/// is guarded once-per-invocation by the worker store's
/// `ambiguous_reported_at` stamp (see `mark_ambiguous_reported`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationAmbiguousPayload {
    /// Which entity in the WAL was stuck: `tool_dispatch` or
    /// `llm_dispatch`. Domain name, not a relational table
    /// reference (see WorkerStoreError::WalTransitionFailed).
    pub stuck_entity: String,
    /// The `tool_call_id` (for tools) or `request_id` (for LLM
    /// calls) of the stuck dispatch.
    pub stuck_call_id: String,
    /// Free-form note describing the operator-relevant context.
    pub note: String,
}

/// Payload for [`EventPayload::InvocationArchived`](super::EventPayload::InvocationArchived). Carries
/// the data the control-plane needs to populate
/// `invocation_archive`: emitting worker, terminal phase, final
/// state blob, and the timestamps the archive row's primary
/// index uses. `agent_id` and `invocation_id` live on the
/// envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationArchivedPayload {
    /// Worker that owned the invocation. The control-plane uses
    /// this to address the ack back at
    /// `fq.worker.{worker_id}.invocation.archive_acked`.
    pub worker_id: WorkerId,
    /// Phase label as written into `invocation_state.phase` at
    /// terminal — `completed` or `failed`. Domain string, not a
    /// typed enum, because the phase vocabulary lives in the
    /// reducer harness, not the events layer.
    pub final_phase: String,
    /// Reducer state at the time of terminal. Opaque blob; the
    /// control-plane stores it as-is. Default serde encoding
    /// (JSON array of integers) is used to keep parity with the
    /// worker store's `state_blob` shape; if blob sizes start to
    /// strain the wire format, swap in `serde_bytes` here and
    /// in `InvocationStateRow`.
    pub final_state_blob: Vec<u8>,
    /// `invocation_state.started_at` (unix ms).
    pub started_at_ms: i64,
    /// `invocation_state.terminal_at` (unix ms).
    pub terminal_at_ms: i64,
}

/// Payload for [`EventPayload::InvocationArchiveAcked`](super::EventPayload::InvocationArchiveAcked). The
/// invocation id lives on the envelope; the payload carries
/// `worker_id` only because the subject token comes from there.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationArchiveAckedPayload {
    pub worker_id: WorkerId,
}

/// Payload for [`EventPayload::InvocationOperatorRecovered`](super::EventPayload::InvocationOperatorRecovered).
/// Operator-issued terminal transition for an invocation.
/// The `invocation_id` and `agent_id` live on the envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationOperatorRecoveredPayload {
    /// Action the operator took. v1 is always `"drop"`; the
    /// field exists so future actions (`resume`, `requeue`)
    /// can be distinguished without minting a new variant.
    pub action: String,
    /// Phase the invocation should be marked at. v1 is
    /// always `"failed"`; a future `resume` would set
    /// `"completed"`.
    pub final_phase: String,
    /// Free-form reason supplied by the operator (e.g. via
    /// `--reason`). Audit-only; consumers must not parse it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Audit payload for `fq invocation resume`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationOperatorResumedPayload {
    pub completed_call_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Published when a tool invocation completes (success or failure).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultPayload {
    /// The Round this result belongs to — its initiating assistant
    /// turn's round. 0 on pre-field events.
    #[serde(default)]
    pub round: u64,
    /// The tool's name, restated so a result renders standalone
    /// (the full parameters still ride the earlier `tool.call`
    /// event; `parent_event_id` traces the chain). Empty on
    /// pre-field events.
    #[serde(default)]
    pub tool_name: String,
    pub tool_call_id: ToolCallId,
    pub output: String,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<ToolErrorKind>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorKind {
    SandboxViolation,
    InvalidParameters,
    ExecutionFailed,
    Timeout,
    PermissionDenied,
}

/// Published when an invocation finishes successfully.
/// Agent-declared task outcome (#125). The serde spellings are the
/// wire contract and must stay in lockstep with fq-tools'
/// `TASK_STATUS_VALUES` (the `report_outcome` schema enum) — the
/// harness parses those strings into this type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    #[default]
    Success,
    Failed,
    Blocked,
    Partial,
}

impl TaskStatus {
    /// Parse a wire spelling; `None` for anything unrecognised (the
    /// harness treats that as "not a valid declaration").
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "success" => Some(TaskStatus::Success),
            "failed" => Some(TaskStatus::Failed),
            "blocked" => Some(TaskStatus::Blocked),
            "partial" => Some(TaskStatus::Partial),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletedPayload {
    /// The agent's own declaration of how the *task* went (#125) —
    /// orthogonal to the runtime axis (`FailedPayload`/`FailureKind`
    /// model runtime failure; this models "the runtime worked, and
    /// here is whether the goal was achieved"). Declared via the
    /// terminal `report_outcome` tool; an invocation that never
    /// declares defaults to `Success`, so pre-#125 events and
    /// undeclared runs read exactly as before.
    #[serde(default)]
    pub task_status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<String>,
    pub total_llm_calls: u32,
    pub total_tool_calls: u32,
    pub total_cost: f64,
    pub total_duration_ms: u64,
}

/// Published when an invocation terminates with an error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailedPayload {
    pub error_kind: FailureKind,
    pub error_message: String,
    pub phase: FailurePhase,
    pub partial_totals: InvocationTotals,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    BudgetExceeded,
    LlmError,
    /// The agent's `max_iterations` cap was reached before the model
    /// declared a final answer. Distinct from `RuntimeError`: hitting
    /// the cap is a configured limit doing its job, not a defect.
    MaxIterations,
    ToolError,
    SandboxViolation,
    RuntimeError,
    /// A transient pre-WAL trigger failure exhausted the consumer's
    /// delivery bound (#49) and was dead-lettered. Distinct from
    /// `RuntimeError` so operators can count and list dead letters
    /// (`fq doctor`, the dashboard) rather than losing them in the
    /// generic bucket.
    TriggerExhausted,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailurePhase {
    Setup,
    Reducer,
    HostStepBudget,
    Budget,
    LlmRequest,
    LlmResponse,
    ToolCall,
    ToolResult,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct InvocationTotals {
    pub total_llm_calls: u32,
    pub total_tool_calls: u32,
    pub total_cost: f64,
    pub total_duration_ms: u64,
    /// Cumulative spend on server-initiated sampling within this
    /// invocation (a subset of `total_cost`), tracked separately so
    /// the sampling sub-budget can be enforced (ADR-0018). Defaults
    /// to 0 for totals written before sampling existed.
    #[serde(default)]
    pub sampling_cost: f64,
    /// Cumulative spend on server-initiated elicitation within this
    /// invocation (a subset of `total_cost`), tracked separately so
    /// the elicitation sub-budget can be enforced (ADR-0018).
    #[serde(default)]
    pub elicitation_cost: f64,
}

/// Published when the `fq run` daemon starts up.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemStartupPayload {
    /// Unique id for this daemon run. All system events from a
    /// single `fq run` invocation share this id.
    pub runtime_id: Uuid,
    /// Version of the fq binary (the value of `CARGO_PKG_VERSION`
    /// at build time).
    pub version: String,
    /// NATS URL the daemon is connected to.
    pub nats_url: String,
    /// Number of agents loaded from the configured agents
    /// directory at startup.
    pub agents_loaded: u32,
    /// Number of pricing entries loaded.
    pub pricing_entries: u32,
}

/// Published when the `fq run` daemon shuts down.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemShutdownPayload {
    pub runtime_id: Uuid,
    /// Short machine-readable reason, e.g. `"ctrl_c"`,
    /// `"task_failed"`, `"error"`.
    pub reason: String,
    /// True if the shutdown was requested gracefully (Ctrl-C,
    /// operator intervention), false if it was triggered by an
    /// unexpected task failure or error.
    pub clean: bool,
}

/// Published when one of the hosted tasks inside `fq run` (the
/// projection consumer, the trigger dispatcher, etc.) exits with
/// an error before a graceful shutdown was requested.
///
/// These events are the canary for "the daemon looks alive but a
/// piece of it silently stopped working". The runtime publishes
/// one per task failure and then shuts itself down so operators
/// don't unknowingly rely on a half-broken daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemTaskFailedPayload {
    pub runtime_id: Uuid,
    /// Symbolic name of the task that failed (e.g.
    /// `projection_consumer`, `trigger_dispatcher`).
    pub task_name: String,
    pub error_message: String,
}

/// Counts of in-flight invocations classified by recovery
/// category at daemon startup. Emitted once per `fq run`
/// after the worker recovery scan completes.
///
/// The same counts are surfaced live via `fq status`; this
/// event records the snapshot so historical recovery
/// behaviour is queryable through the existing event
/// projection (`fq events query --type=system_recovery`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemRecoveryPayload {
    pub runtime_id: Uuid,
    pub worker_id: String,
    /// Number of invocations classified as safe-resume
    /// (intent-only or no dispatches; can be auto-recovered
    /// by re-running from the persisted state).
    pub safe_resume: u32,
    /// Number of invocations classified as safe-replay
    /// (action completed; result fed to next reducer step).
    pub safe_replay: u32,
    /// Number of invocations classified as ambiguous
    /// (dispatched-without-completed; surfaced to operator
    /// rather than auto-recovered).
    pub ambiguous: u32,
    /// Total = safe_resume + safe_replay + ambiguous.
    pub total: u32,
}

/// Payload for [`EventPayload::WorkerHeartbeat`](super::EventPayload::WorkerHeartbeat). Identifies
/// which worker the heartbeat is for; the timestamp lives on
/// the envelope.
///
/// The payload is deliberately minimal. Future "what is this
/// worker up to" fields (in-flight invocation count, load,
/// version, host info) belong here when there's a consumer
/// that uses them — today the only consumer is the
/// coordination consumer's `last_heartbeat` update, which
/// reads only the `worker_id` and the envelope timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerHeartbeatPayload {
    pub worker_id: WorkerId,
}

/// Payload for [`EventPayload::WorkerOrphaned`](super::EventPayload::WorkerOrphaned).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerOrphanedPayload {
    pub worker_id: WorkerId,
    pub last_heartbeat_ms: i64,
}

/// A log record a connected MCP server emitted (`notifications/message`),
/// forwarded to the event bus by the daemon's notification drain
/// (ADR-0020). Daemon-scoped: shared MCP servers are not tied to a
/// single agent or invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerLogPayload {
    /// The MCP server that emitted the record (its declared name).
    pub server: String,
    /// MCP log level name (`"debug"`..`"emergency"`).
    pub level: String,
    /// Optional logger / category tag from the server.
    pub logger: Option<String>,
    /// The structured log payload as the server sent it.
    pub data: serde_json::Value,
}
