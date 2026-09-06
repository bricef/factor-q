//! `control.doctor`: its declared output, and the pure fold that
//! assembles one.
//!
//! Its own module because it is the surface's one *composite* — a fold
//! of several folds, where everything else in `surface` is a key, a
//! filter or a single report's shape. It is also the piece that grows:
//! each new class of thing a daemon can be unhealthy about adds a
//! field here and a block to the client's rendering, and it took the
//! parent file past its cap when consumers and MCP servers arrived in
//! the same week.
//!
//! The shapes stay `fq_ops::surface::*` by re-export: two binaries name
//! them, and a declared type that moved path would be a wire change
//! dressed as a refactor.

use serde::{Deserialize, Serialize};

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
    /// How long an invocation may go without crossing a step boundary
    /// before the two counts above call it stuck, in milliseconds.
    ///
    /// Reported rather than known, because it is *derived* from this
    /// daemon's call deadlines — twice one worst-case step — and not a
    /// constant a reader could quote. Two daemons configured
    /// differently disagree about the same silent invocation, and both
    /// are right; a client that assumed a number would tell an operator
    /// the wrong thing about the daemon in front of them.
    ///
    /// It is not a parameter. A health report an operator can narrow is
    /// one they can narrow past the problem.
    pub stuck_after_ms: i64,
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
    /// Every durable consumer this daemon expects, and how each one is
    /// doing. The Phase 1 exit criterion, verbatim: "`fq doctor`
    /// reports every consumer". Before this the report was a fold of
    /// the stores alone, so a control-plane consumer stuck redelivering
    /// — the wedge finding B4 describes — left every check green.
    ///
    /// A daemon that cannot be asked answers nothing at all, which is
    /// why this is a plain list rather than an optional one: reaching
    /// the report means the daemon probed its own broker.
    #[serde(default)]
    pub consumers: Vec<crate::health::ConsumerHealth>,
    /// Every shared MCP server a loaded agent declares, and whether it
    /// is up. An unavailable server is not a daemon-level failure —
    /// boot carries on without it — so it is exactly the kind of
    /// standing degradation only a health report surfaces: the agents
    /// that need it are refused at dispatch, and nothing else says
    /// why.
    #[serde(default)]
    pub mcp_servers: Vec<crate::health::McpServerHealth>,
}

impl DoctorReport {
    /// Total terminal failures across all kinds.
    pub fn failure_total(&self) -> i64 {
        self.failures.iter().map(|f| f.count).sum()
    }

    /// Consumers an operator should act on — missing, unreadable, or
    /// stuck redelivering.
    pub fn faulty_consumers(&self) -> impl Iterator<Item = &crate::health::ConsumerHealth> {
        self.consumers.iter().filter(|c| c.is_fault())
    }

    /// MCP servers an operator should act on — the unavailable ones.
    pub fn unavailable_mcp_servers(&self) -> impl Iterator<Item = &crate::health::McpServerHealth> {
        self.mcp_servers.iter().filter(|s| s.is_fault())
    }

    /// True when any check reports a problem worth an operator's
    /// attention: stale workers, stuck in-flight work, ambiguous
    /// invocations, permanent failures, or a consumer that has stopped
    /// making progress. In-flight work that is merely running (not
    /// stuck) is healthy, not an issue, and so is a consumer that is
    /// merely behind.
    pub fn has_issues(&self) -> bool {
        self.workers.stale > 0
            || self.executions.stuck > 0
            || self.ambiguous > 0
            || self.failure_total() > 0
            || self.faulty_consumers().next().is_some()
            || self.unavailable_mcp_servers().next().is_some()
    }
}

/// The `error_kind` a dead-lettered trigger is recorded under.
const DEAD_LETTER_KIND: &str = "trigger_exhausted";

/// Pure: assemble a [`DoctorReport`] from the already-fetched read
/// views, so it can be unit-tested without a database. The stuck
/// determination (threshold + clock-skew handling) lives in
/// `fq_runtime::control_plane::liveness` — behind the store handles,
/// which this crate deliberately does not depend on; this builder only
/// aggregates and carries the threshold through so the client can say
/// what the verdict meant.
pub fn build_doctor_report(
    workers: &[crate::views::WorkerView],
    executions: &crate::views::ExecutionsView,
    stuck_after_ms: i64,
    ambiguous: i64,
    failures: &[crate::views::FailureView],
    consumers: Vec<crate::health::ConsumerHealth>,
    mcp_servers: Vec<crate::health::McpServerHealth>,
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
        stuck_after_ms,
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
        consumers,
        mcp_servers,
    }
}
