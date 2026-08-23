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
// Worker
// ---------------------------------------------------------------------

/// Get identity for the Worker view.
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkerViewKey {
    pub worker_id: String,
}

/// List selection for the Worker view — the typed, schema'd filter
/// (never a query language). `fq workers list` used to pull the whole
/// roster and sieve it in the client; the selection now travels with
/// the request and the view applies it to its index.
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkerListFilter {
    /// `alive` | `stale` | `shutdown`. Absent lists the whole roster.
    #[serde(default)]
    pub status: Option<String>,
}

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

/// Where a daemon keeps its three SQLite stores, and whether they
/// exist yet. A daemon that has never run has none of them.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct StatusStores {
    pub worker_path: String,
    pub control_plane_path: String,
    pub projection_path: String,
    /// A pre-split `events.db` still on disk. Present means the daemon
    /// has not yet migrated it; absent is the healthy case.
    #[serde(default)]
    pub legacy_events_db: Option<String>,
    /// True once all three stores exist. False on a daemon that has
    /// not yet started for the first time.
    pub initialised: bool,
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
    /// Where this daemon's three stores live, and whether it has
    /// created them yet.
    ///
    /// Reported by the daemon rather than derived by the reader. The
    /// paths are the daemon's — a reader that computed them from its
    /// own config would be describing its own machine, which is right
    /// only while the two share one, and silently wrong the moment
    /// they do not.
    pub stores: StatusStores,
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
    /// Ids of the working invocations, in full — ask about one with
    /// `invocation.get`. Shorten them for display if you like; the
    /// shortened form is not an identity and nothing takes it back.
    pub working_ids: Vec<String>,
    /// In-flight invocations whose `updated_at` is older than the
    /// report's stuck threshold.
    pub stuck: i64,
    /// Ids of the stuck invocations, in full, for triage — same
    /// convention as `working_ids`.
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

/// List/Stream selection for DeadLetters — the typed, schema'd filter,
/// never a query language.
///
/// It carries exactly the narrowing `fq dead-letters list` offers
/// (`--agent`, `--limit`) and nothing invented alongside: a filter is a
/// promise the surface has to keep, so it grows when a caller needs it
/// to (P11), not when a field looks plausible.
#[derive(Default, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct DeadLetterFilter {
    /// One agent's dead letters. Absent reads every agent's.
    #[serde(default)]
    pub agent: Option<String>,
    /// Cap on one List page — the most recent N matching dead
    /// letters, and at most 500 of them (this property's `maximum`).
    /// Absent asks for the default 50.
    ///
    /// **A larger N is refused, never quietly shrunk.** So the count
    /// that comes back is always the one you asked for or the whole
    /// answer, and it reads unambiguously: fewer rows than you asked
    /// for means there are no more; exactly as many means there may
    /// be. For more than a page, narrow with `agent` — the only
    /// narrowing this filter offers — or read the same dead letters
    /// from `dead_letter.stream`, which is cursored.
    ///
    /// Ignored by Stream, which is cursored rather than paged.
    #[serde(default)]
    #[schemars(range(max = DEAD_LETTER_LIST_MAX_LIMIT))]
    pub limit: Option<u32>,
}

/// The most dead letters one List page may carry, whatever a caller
/// asks for — refused rather than quietly applied. This crate
/// *declares* the bound; the daemon's `dead_letter.list` handler is
/// what rules on a caller's `limit` against it (`fq-daemon`'s
/// `dead_letter_atom::list_limit`, which speaks `WireError` and so
/// cannot live on the shape). It is also declared on the surface as
/// this filter's `limit` maximum, so a consumer reads it off the
/// schema instead of discovering it by failing.
///
/// **The number is the edge's frame, worked backwards.** One List
/// answer is one frame, and both ends of the edge frame with
/// `LengthDelimitedCodec::new()`, whose default ceiling is 8 MiB
/// (8,388,608 bytes). A row's fixed part — one UUID, an RFC3339
/// timestamp, ten keys — is 235 bytes; the golden listing measures 319
/// and 323; and a production row measures 698: a github-watcher
/// task payload (`task`/`refs`/`constraints`/`done_criteria`/`github`,
/// 328 bytes of it) under the inline emitter's message. So 500 rows
/// leaves 8,388,608 / 500 = 16,777 bytes for each of them — twenty-four
/// times the production row, or ~16 KB of trigger payload on *every*
/// row — and a full page of production rows is 0.33 MiB, 4% of the
/// frame.
///
/// **It is smaller than the Event atom's 2,000 because the row is
/// bigger and the slack has two claimants, not one.** A dead letter
/// carries the trigger that died with it: `trigger_payload` is opaque
/// JSON the producer chose, which the wire contract says outright
/// (`docs/design/committed/trigger-wire-contract.md`: "an **opaque JSON
/// value**... Any valid JSON value is accepted"), and nothing in the
/// runtime truncates it — `fq dead-letters list` truncates for
/// *display* only, and `--json` prints it whole. `error_message` is
/// unbounded too: the inline emitter interpolates the `ExecutorError`
/// that lost the last delivery, which can be a provider's error body.
/// At 698 bytes the row is 2.4x an `EventView`'s 294, so matching that
/// atom's fourteen-fold headroom would land near 850; 500 is the round
/// number below it, and the difference is the second unbounded field's
/// margin.
///
/// The only ceiling either field already has is the broker's: a
/// publish above the server's advertised `max_payload` is refused
/// (`EventBus::publish`), which is 16 MB on the dogfood broker
/// (`ops/dogfood/infra/nats.conf`) — *above* the 8 MiB frame, so one
/// pathological row can outgrow a response all by itself. This cap
/// does not fix that, and does not pretend to; it bounds the ordinary
/// page.
///
/// What it replaces had no bound at all. Note that an oversized
/// `limit` never allocated a page that size up front: the daemon's
/// List handler scans the subject forward and keeps a sliding
/// window of at most `limit`, so the memory was however many dead
/// letters the log actually held. The scan is the whole subject either
/// way. What an unbounded `limit` bought was a response that grew with
/// retained history until the codec refused it — around 12,000
/// production-sized rows — after the scan had already been paid for.
pub const DEAD_LETTER_LIST_MAX_LIMIT: u32 = 500;

/// Stuck-work threshold: an in-flight invocation whose
/// `invocation_state.updated_at` is older than this many ms is
/// flagged "stuck" by `fq doctor`. Reuses the control-plane's
/// stale-worker value (`DEFAULT_STALE_THRESHOLD_MS = 30_000`,
/// `coordination_consumer.rs:66`) rather than inventing a third
/// hard-coded constant — an invocation that has not touched its
/// WAL row in as long as a worker has not heartbeated is the same
/// order of "not making progress" signal.
///
/// It is the daemon's choice, and the client renders it back in the
/// ">30s" line. That works because one crate holds both halves today;
/// when Phase 5 splits them, either the threshold travels in the
/// report or the client stops naming a number it did not decide.
/// How long a worker may go unheard from before the roster calls it
/// stale. A contract value, not a tuning knob: it is what `stale` means
/// on the surface, so the reader rendering it and the consumer applying
/// it have to agree.
pub const DEFAULT_STALE_THRESHOLD_MS: i64 = 30_000;

/// The `error_kind` a dead-lettered trigger is recorded under.
const DEAD_LETTER_KIND: &str = "trigger_exhausted";

/// In-flight work is "stuck" once it has not advanced for this long.
pub const DOCTOR_STUCK_THRESHOLD_MS: i64 = DEFAULT_STALE_THRESHOLD_MS;

/// Pure: assemble a [`DoctorReport`] from the already-fetched read
/// views, so it can be unit-tested without a database. The stuck
/// determination (threshold + clock-skew handling) lives in
/// `fq_runtime::views::Views::executions` — the store handle, which
/// this crate deliberately does not depend on; this builder only
/// aggregates and shortens ids for triage.
pub fn build_doctor_report(
    workers: &[crate::views::WorkerView],
    executions: &crate::views::ExecutionsView,
    ambiguous: i64,
    failures: &[crate::views::FailureView],
) -> DoctorReport {
    let mut w = DoctorWorkers::default();
    for row in workers {
        match row.status.as_str() {
            "alive" => w.alive += 1,
            "stale" => {
                w.stale += 1;
                w.stale_ids.push(row.worker_id.clone());
            }
            "shutdown" => w.shutdown += 1,
            // The control-plane only records the three statuses above;
            // an unknown value would mean a store/view drift — count it
            // as stale so it surfaces as an issue rather than vanishing.
            _ => {
                w.stale += 1;
                w.stale_ids.push(row.worker_id.clone());
            }
        }
    }

    // Full ids on the wire. They were shortened here to match the human
    // report, which reads better at 8 characters — but a shortened id is
    // not an identity: nothing accepts it back. `invocation.get` matches
    // exactly, so a caller that took one of these and asked about it got
    // NotFound, and a renderer that linked one produced a dead link.
    // Shortening is a display choice and belongs to each renderer.
    let ex = DoctorExecutions {
        in_flight: executions.in_flight,
        working: executions.working,
        working_ids: executions.working_ids.clone(),
        stuck: executions.stuck,
        stuck_ids: executions.stuck_ids.clone(),
    };

    let failures: Vec<DoctorFailure> = failures
        .iter()
        .map(|f| DoctorFailure {
            error_kind: f.error_kind.clone(),
            count: f.count,
        })
        .collect();

    let dead_letters = DoctorDeadLetters {
        exhausted_triggers: failures
            .iter()
            .filter(|f| f.error_kind == DEAD_LETTER_KIND)
            .map(|f| f.count)
            .sum(),
    };

    DoctorReport {
        workers: w,
        executions: ex,
        ambiguous,
        failures,
        dead_letters,
    }
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct InvocationResumeRequest {
    pub invocation_id: String,
    pub reason: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct InvocationResumeResponse {
    pub ok: bool,
    pub message: String,
    pub completed_call_ids: Vec<String>,
}

/// Get identity for an Event: the `event_id` the event stamps on
/// itself at construction (`Uuid::now_v7`), which is also the
/// projection index's primary key — stable, transport-independent,
/// time-ordered, and already indexed.
///
/// **Not the log sequence**, which is where this started: see the
/// module docs for the two ways a stored position comes to address
/// the wrong event, both of which happen without anybody doing
/// anything wrong.
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct EventKey {
    pub event_id: String,
}

/// Get identity for a Trigger: the `trigger_id` the runtime minted (or
/// honoured) when it took responsibility for the trigger — a UUIDv7 in
/// canonical hyphenated text, and the `triggers` table's primary key.
///
/// This is the shape `trigger.publish`'s receipt hands back, verbatim.
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct TriggerKey {
    pub trigger_id: String,
}

/// The values `InvocationListFilter::status` accepts.
///
/// The list is a contract fact, so it lives with the filter rather than
/// in either of the two places that check it: the client rejects a typo
/// before it travels, and the daemon maps the survivors onto its own
/// ownership enum — which the client cannot name and does not need to.
pub const INVOCATION_STATUS_FILTERS: [&str; 4] = ["in_flight", "ambiguous", "completed", "failed"];

/// Reject a status filter the surface does not accept, naming the ones
/// it does. Returns the value unchanged when it is one of them.
pub fn validate_invocation_status_filter(s: &str) -> Result<&str, String> {
    if INVOCATION_STATUS_FILTERS.contains(&s) {
        Ok(s)
    } else {
        Err(format!(
            "unknown status filter `{s}` — try {}",
            INVOCATION_STATUS_FILTERS.join(" | ")
        ))
    }
}
