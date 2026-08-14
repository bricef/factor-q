//! The operator surface's **declared shapes**: the keys, filters,
//! parameters and report outputs whose schemas the `fq-ops`
//! declarations capture.
//!
//! These lived beside the daemon's assembly point in `fq-cli` with a
//! note saying they would stay there "until 3e's codegen decision
//! settles their final home". D-3 settled it: there is no codegen, and
//! the interface is **shared data definitions** — the very structs a
//! client serialises are the ones whose schemas the declarations
//! publish and the ones the handlers deserialise, so schema, handler
//! and call cannot drift. That only holds while every party can name
//! the same type, which is why these are here rather than private to
//! the daemon's crate.
//!
//! They arrived when the surface acquired its second client. The
//! operator dashboard reads the same daemon the CLI does, and a
//! dashboard that hand-rolled `{"invocation_id": …}` would be a
//! parallel definition of a shape that already exists — free to drift
//! by a field rename, and caught at runtime as an `InvalidInput` from
//! a daemon that no longer recognises the request. This is the move
//! `agent_view.rs` already made for the Agent view's *output* shapes,
//! for the same reason and in the same cohort.
//!
//! **Shapes only.** What a handler *does* with one — validating a
//! `limit` against its cap, turning a `since` spelling into a bound,
//! rendering a filter back to an operator — stays with the handler or
//! the renderer that owns it. Those speak `WireError` and terminal
//! prose, neither of which belongs in the runtime's core, and none of
//! it is part of what the two clients must agree on.
//!
//! Shapes with only one consumer are deliberately still in `fq-cli`
//! (`WorkerViewKey`, `WorkerListFilter`, `EventKey`, `TurnKey`, and
//! the command inputs). One consumer needs no shared definition; they
//! follow if and when a second one appears.

use serde::{Deserialize, Serialize};

use crate::health::StreamHealth;
use crate::views::RecoveryView;

// ---------------------------------------------------------------------
// Invocation
// ---------------------------------------------------------------------

/// Get identity for the Invocation view.
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct InvocationViewKey {
    pub invocation_id: String,
}

/// List selection for the Invocation view — the typed, schema'd
/// filter (never a query language).
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct InvocationListFilter {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub include_archived: bool,
    #[serde(default = "default_invocation_list_limit")]
    pub limit: i64,
}

fn default_invocation_list_limit() -> i64 {
    50
}

/// The typed parameters of `invocation.active`. Empty, and declared
/// anyway: the answer is small — it is bounded by how much this daemon
/// is running — and this is where a future narrowing would appear.
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct ActiveParams {}

// ---------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------

/// Get identity for the Agent view.
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentViewKey {
    pub agent_id: String,
}

/// List selection for the Agent view. Empty, and declared anyway: a
/// registry is a directory of definitions the daemon holds entirely in
/// memory, so there is no narrowing worth a wire contract yet, and the
/// declaration is where a future one would appear.
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentListFilter {}

// ---------------------------------------------------------------------
// Turn
// ---------------------------------------------------------------------

/// List/Stream selection for Turns — full payloads by default; an
/// `abbreviate` option waits for a consumer that wants it (P11).
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct TurnFilter {
    pub invocation_id: String,
    #[serde(default)]
    pub limit: Option<u32>,
}

// ---------------------------------------------------------------------
// Event
// ---------------------------------------------------------------------

/// The most rows one List page may carry, whatever a caller asks for —
/// refused rather than quietly applied by the daemon that enforces it,
/// and declared on the surface as this filter's `limit` maximum so a
/// consumer reads it off the schema instead of discovering it by
/// failing.
///
/// **The number is the edge's frame, worked backwards.** One List
/// answer is one frame, and both ends of the edge frame with
/// `LengthDelimitedCodec::new()`, whose default ceiling is 8 MiB
/// (8,388,608 bytes). An index row's fixed part — two UUIDs, an
/// RFC3339 timestamp, ten keys — is 293 bytes, and whole rows measure
/// 287 / 294 / 367 bytes (min / median / max) across the golden
/// listing. So 2,000 rows leaves 8,388,608 / 2,000 = 4,194 bytes for
/// each of them: fourteen times the median row, or ~3.9 KB of
/// `error_message` on *every* row — and `error_message` is the one
/// unbounded field here, an `err.to_string()` from a provider or a
/// tool that nothing on the way in truncates. A full page of median
/// rows is 0.56 MiB, 7% of the frame. That is headroom rather than a
/// squeaker, which is the point: the cap has to survive the listing an
/// operator reaches for on the worst day, when every row is a failure
/// carrying a provider's error body.
///
/// What it replaces had no bound at all. `--limit -1` arrived here as
/// `u32::MAX` — `LIMIT 4294967295`, the whole projection table
/// materialised as one `Vec<EventView>` in daemon memory and then
/// refused by the codec somewhere past 20,000 rows (5.6 MiB of median
/// rows, 70% of the frame). The operator paid the allocation and got a
/// transport error for it, and any paired client could ask for that.
pub const EVENT_LIST_MAX_LIMIT: u32 = 2_000;

/// List/Stream selection for Events — the typed, schema'd filter.
///
/// Never a query language, and deliberately never a bus subject: a
/// subject pattern is a coordinate of the infrastructure the edge maps
/// (D8), so the selection travels in domain terms and the daemon
/// decides which subjects answer it. It carries exactly the narrowing
/// `fq events query` offers, which is the same narrowing a tail wants.
#[derive(Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EventFilter {
    /// One agent's events — the events whose **envelope** names that
    /// agent, which is the domain's answer to whose event this is.
    /// Not the events on `fq.agent.<id>.>`: an agent's archive ack is
    /// published on a worker subject, and agent `system` has no
    /// `fq.agent.*` subject at all. Absent reads every agent.
    #[serde(default)]
    pub agent: Option<String>,
    /// One event type, as the payload names itself (`llm_response`,
    /// `tool_call`, `system_startup`, …). An unrecognised value
    /// matches nothing rather than failing: event types are values,
    /// and a newer daemon has types this binary has never heard of.
    #[serde(default)]
    pub event_type: Option<String>,
    /// Only events at or after this RFC3339 instant.
    #[serde(default)]
    pub since: Option<String>,
    /// Cap on one List page — the most recent N matching rows, and at
    /// most 2000 of them (this property's `maximum`). Absent asks for
    /// the default 50.
    ///
    /// **A larger N is refused, never quietly shrunk.** So the count
    /// that comes back is always the one you asked for or the whole
    /// answer, and it reads unambiguously: fewer rows than you asked
    /// for means there are no more; exactly as many means there may
    /// be. For more than a page, narrow (`agent`, `event_type`,
    /// `since`) or read the same population from `event.stream`,
    /// which is cursored.
    ///
    /// Ignored by Stream, which is cursored rather than paged.
    #[serde(default)]
    #[schemars(range(max = EVENT_LIST_MAX_LIMIT))]
    pub limit: Option<u32>,
}

// ---------------------------------------------------------------------
// Cost
// ---------------------------------------------------------------------

/// The typed parameters of `cost.summary`.
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct CostSummaryParams {
    /// Narrow the per-agent rows to one agent. Absent reports the
    /// whole fleet.
    #[serde(default)]
    pub agent: Option<String>,
    /// Lower bound on time, in the `views::since` grammar. Absent
    /// reports the whole recorded history — cost-bearing rows are
    /// exempt from the retention sweep, so that is a real answer here
    /// rather than whatever happened to survive it.
    #[serde(default)]
    pub since: Option<String>,
    /// Bucket the time series hourly instead of daily.
    #[serde(default)]
    pub hourly_buckets: bool,
}

/// The typed parameters of `cost.by_agent`.
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct CostByAgentParams {
    /// The agent to break down. Required — this report is one agent's
    /// drill-down where `cost.summary` is the fleet's.
    pub agent: String,
    #[serde(default)]
    pub since: Option<String>,
    /// Cap on the per-invocation rows, newest first.
    #[serde(default = "default_invocation_limit")]
    pub invocation_limit: i64,
}

fn default_invocation_limit() -> i64 {
    50
}

// ---------------------------------------------------------------------
// Control — `control.status`
// ---------------------------------------------------------------------

/// The typed parameters of `control.status`. Empty, and declared
/// anyway: the report is small enough that every part of it is worth
/// having, and this is where a future narrowing would appear.
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct StatusParams {}

/// The daemon's live agent registry, censused: what it would run right
/// now, and what it could not load.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Default)]
pub struct StatusRegistry {
    /// Definitions the registry holds — the agents this daemon would
    /// run if triggered right now.
    pub agents: i64,
    /// One entry per definition file the registry rejected, phrased as
    /// the daemon phrased it; each message names the file. A daemon
    /// with load errors is running fewer agents than its directory
    /// describes, which is rarely what the operator intended.
    pub load_errors: Vec<String>,
}

impl StatusRegistry {
    /// Census one registry snapshot.
    pub fn of(registry: &crate::AgentRegistry) -> Self {
        StatusRegistry {
            agents: registry.len() as i64,
            load_errors: registry.errors().iter().map(|e| e.to_string()).collect(),
        }
    }
}

/// `control.status`'s declared output, and what `fq status --json`
/// nests under `daemon`.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    /// The build this daemon is running: semver plus the commit it was
    /// built from, so a deploy check can confirm the live process is on
    /// the expected revision.
    pub version: String,
    /// JetStream health for the runtime's core streams and their
    /// primary durable consumers — message counts, byte totals and how
    /// far each consumer has got. Probed at the daemon, over the
    /// connection it already holds.
    pub streams: Vec<StreamHealth>,
    /// The live agent registry, censused.
    pub registry: StatusRegistry,
    /// Rows in the daemon's projection index — how much of the event
    /// log has been folded into readable state.
    pub projection_rows: i64,
    /// Ambiguous invocations awaiting triage and workers past the
    /// stale threshold, with their ids.
    pub recovery: RecoveryView,
}

// ---------------------------------------------------------------------
// Control — `control.doctor`
// ---------------------------------------------------------------------

/// The typed parameters of `control.doctor`. Empty, and declared
/// anyway: every check runs, because a health report an operator can
/// narrow is one they can narrow past the problem. This is where a
/// future option — an override for the stuck threshold — would appear.
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct DoctorParams {}

/// Worker liveness counts plus the ids of any stale workers so
/// the operator can act without a second `fq workers list` call.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Default)]
pub struct DoctorWorkers {
    pub alive: i64,
    pub stale: i64,
    pub shutdown: i64,
    /// Worker ids currently past the stale threshold.
    pub stale_ids: Vec<String>,
}

/// In-flight / current-execution view, read from the worker-local
/// `invocation_state` table (the reliable live view — the CP owner
/// table's `in_flight` status is not populated by trigger dispatch
/// yet; see issue #50).
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Default)]
pub struct DoctorExecutions {
    pub in_flight: i64,
    /// In-flight invocations with a fresh open dispatch (tool or LLM) —
    /// actively working, however silent their WAL row.
    pub working: i64,
    /// Short ids of the working invocations, same convention as
    /// `stuck_ids`.
    pub working_ids: Vec<String>,
    /// In-flight invocations whose `updated_at` is older than the
    /// report's stuck threshold.
    pub stuck: i64,
    /// Short ids of the stuck invocations, for triage.
    pub stuck_ids: Vec<String>,
}

/// Dead-lettered triggers: transient pre-WAL failures that
/// exhausted the trigger consumer's delivery bound. The dispatcher
/// consumes the exhausted trigger and emits a terminal `failed` event
/// with the dead-letter kind; this counts that bucket, so the
/// report needs no extra query. The event's annotations carry the
/// trigger subject and payload for requeue/diagnosis.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct DoctorDeadLetters {
    pub exhausted_triggers: i64,
}

/// One failure-kind bucket in the report. Mirrors
/// [`crate::views::FailureView`] but owns its data so the report is a
/// self-contained serialisable value.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct DoctorFailure {
    pub error_kind: String,
    pub count: i64,
}

/// The full doctor report — `control.doctor`'s declared output, and
/// what `fq doctor --json` prints verbatim.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    pub workers: DoctorWorkers,
    pub executions: DoctorExecutions,
    /// Ambiguous invocations needing operator triage (CP owner
    /// table, `status='ambiguous'`).
    pub ambiguous: i64,
    /// Terminal failures grouped by `FailureKind` (from the
    /// projection `events` table, `event_type='failed'`).
    pub failures: Vec<DoctorFailure>,
    pub dead_letters: DoctorDeadLetters,
}

impl DoctorReport {
    /// Total terminal failures across all kinds.
    pub fn failure_total(&self) -> i64 {
        self.failures.iter().map(|f| f.count).sum()
    }

    /// True when any check reports a problem worth an operator's
    /// attention: stale workers, stuck in-flight work, ambiguous
    /// invocations, or permanent failures. In-flight work that is
    /// merely running (not stuck) is healthy, not an issue.
    pub fn has_issues(&self) -> bool {
        self.workers.stale > 0
            || self.executions.stuck > 0
            || self.ambiguous > 0
            || self.failure_total() > 0
    }
}
