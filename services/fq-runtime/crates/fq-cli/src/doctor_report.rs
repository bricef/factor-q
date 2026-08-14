//! `control.doctor`, daemon-side (plan Phase 4, verb 15): the
//! durable-execution health composite — worker liveness, in-flight and
//! stuck work, ambiguous invocations, permanent failures and dead
//! letters, in one report.
//!
//! **A machinery report, and the stretch is declared rather than
//! hidden.** The domain model's definition of a report — a named,
//! typed computation over resources — is a caution against reports
//! that are Gets on a pretend-resource. `control.doctor` is not one:
//! the Control synthetic has no Get (ADR-0006 Appendix D), so there is
//! no read here to duplicate or disguise. What it is is a fold of
//! several folds, scoped to `Control` for authority, which is the same
//! character `control.status` has and the model admits for exactly
//! that reason.
//!
//! **The four sub-reads are internal to the composite, not report
//! inputs** — the per-method call the plan left open. `workers`,
//! `executions`, `recovery` and `failures` stay private reads this
//! handler makes with system authority, and none of them becomes
//! surface. Three reasons, in order of weight:
//!
//! * A report's authority is Read on **its own scope**, not on its
//!   inputs. `control.doctor` is grantable to an operator who cannot
//!   list workers, and would be whether or not the sub-reads were
//!   declared — so declaring them buys the composite nothing and costs
//!   four names against the P11 curation gate ("few by design").
//! * They are not promises anyone asked for. `recovery` is consulted
//!   for one integer (`ambiguous`); `executions` is only meaningful
//!   paired with the thresholds this module chooses; `event_count` —
//!   the fourth name the plan listed — is not read here at all, it
//!   belongs to `fq status` and travels with verb 14. Declaring an
//!   intermediate because a composite happens to compute it is how a
//!   surface grows a second read mechanism by accident.
//! * The one sub-read that *is* already surface — the worker roster,
//!   as `worker.list` — is deliberately not routed through the edge
//!   here. A report handler calling its own daemon's edge would buy a
//!   second authority check on a call that has already been
//!   authorised, and a second serialisation of rows it is about to
//!   reduce to three integers.
//!
//! The client half — the verb, its exit code, and the human rendering
//! — is [`crate::doctor`]. That seam is the one Phase 5 splits the
//! binary along.

use std::sync::Arc;

use fq_edge::wire::WireError;
use fq_runtime::control_plane::coordination_consumer::DEFAULT_STALE_THRESHOLD_MS;
use fq_runtime::views::Views;

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
pub(crate) const DOCTOR_STUCK_THRESHOLD_MS: i64 = DEFAULT_STALE_THRESHOLD_MS;

/// The projection's failure-kind string for a dead-lettered trigger —
/// `FailureKind::TriggerExhausted` serialized with the wire vocabulary.
const DEAD_LETTER_KIND: &str = "trigger_exhausted";

/// The typed parameters of `control.doctor`. Empty, and declared
/// anyway: every check runs, because a health report an operator can
/// narrow is one they can narrow past the problem. This is where a
/// future option — an override for [`DOCTOR_STUCK_THRESHOLD_MS`] —
/// would appear.
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct DoctorParams {}

/// Worker liveness counts plus the ids of any stale workers so
/// the operator can act without a second `fq workers list` call.
#[derive(
    serde::Serialize, serde::Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Default,
)]
pub(crate) struct DoctorWorkers {
    pub(crate) alive: i64,
    pub(crate) stale: i64,
    pub(crate) shutdown: i64,
    /// Worker ids currently past the stale threshold.
    pub(crate) stale_ids: Vec<String>,
}

/// In-flight / current-execution view, read from the worker-local
/// `invocation_state` table (the reliable live view — the CP owner
/// table's `in_flight` status is not populated by trigger dispatch
/// yet; see issue #50).
#[derive(
    serde::Serialize, serde::Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Default,
)]
pub(crate) struct DoctorExecutions {
    pub(crate) in_flight: i64,
    /// In-flight invocations with a fresh open dispatch (tool or LLM) —
    /// actively working, however silent their WAL row.
    pub(crate) working: i64,
    /// Short ids of the working invocations, same convention as
    /// `stuck_ids`.
    pub(crate) working_ids: Vec<String>,
    /// In-flight invocations whose `updated_at` is older than
    /// [`DOCTOR_STUCK_THRESHOLD_MS`].
    pub(crate) stuck: i64,
    /// Short ids of the stuck invocations, for triage.
    pub(crate) stuck_ids: Vec<String>,
}

/// Availability of the dead-letter section.
/// Dead-lettered triggers: transient pre-WAL failures that
/// exhausted the trigger consumer's delivery bound. The dispatcher
/// consumes the exhausted trigger and emits a terminal `failed` event
/// with kind [`DEAD_LETTER_KIND`]; this counts that bucket, so the
/// report needs no extra query. The event's annotations carry the
/// trigger subject and payload for requeue/diagnosis.
#[derive(
    serde::Serialize, serde::Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq,
)]
pub(crate) struct DoctorDeadLetters {
    pub(crate) exhausted_triggers: i64,
}

/// The full doctor report — `control.doctor`'s declared output, and
/// what `fq doctor --json` prints verbatim. Built by the pure
/// [`build_doctor_report`] so the checks are unit-testable without a
/// live DB.
#[derive(
    serde::Serialize, serde::Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq,
)]
pub(crate) struct DoctorReport {
    pub(crate) workers: DoctorWorkers,
    pub(crate) executions: DoctorExecutions,
    /// Ambiguous invocations needing operator triage (CP owner
    /// table, `status='ambiguous'`).
    pub(crate) ambiguous: i64,
    /// Terminal failures grouped by `FailureKind` (from the
    /// projection `events` table, `event_type='failed'`).
    pub(crate) failures: Vec<DoctorFailure>,
    pub(crate) dead_letters: DoctorDeadLetters,
}

/// One failure-kind bucket in the report. Mirrors
/// [`fq_runtime::views::FailureView`] but owns its data so the report
/// is a self-contained serialisable value.
#[derive(
    serde::Serialize, serde::Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq,
)]
pub(crate) struct DoctorFailure {
    pub(crate) error_kind: String,
    pub(crate) count: i64,
}

impl DoctorReport {
    /// Total terminal failures across all kinds.
    pub(crate) fn failure_total(&self) -> i64 {
        self.failures.iter().map(|f| f.count).sum()
    }

    /// True when any check reports a problem worth an operator's
    /// attention: stale workers, stuck in-flight work, ambiguous
    /// invocations, or permanent failures. In-flight work that is
    /// merely running (not stuck) is healthy, not an issue.
    pub(crate) fn has_issues(&self) -> bool {
        self.workers.stale > 0
            || self.executions.stuck > 0
            || self.ambiguous > 0
            || self.failure_total() > 0
    }
}

/// Pure: assemble a [`DoctorReport`] from the already-fetched read
/// views, so it can be unit-tested without a database. The stuck
/// determination (threshold + clock-skew handling) lives in
/// [`fq_runtime::views::Views::executions`]; this builder only
/// aggregates and shortens ids for triage.
pub(crate) fn build_doctor_report(
    workers: &[fq_runtime::views::WorkerView],
    executions: &fq_runtime::views::ExecutionsView,
    ambiguous: i64,
    failures: &[fq_runtime::views::FailureView],
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

    // Short ids (8 chars) for triage, matching the human report.
    let short = |ids: &[String]| -> Vec<String> {
        ids.iter().map(|id| id.chars().take(8).collect()).collect()
    };
    let ex = DoctorExecutions {
        in_flight: executions.in_flight,
        working: executions.working,
        working_ids: short(&executions.working_ids),
        stuck: executions.stuck,
        stuck_ids: short(&executions.stuck_ids),
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

/// Register `control.doctor` on the daemon's edge.
pub(crate) fn register_doctor_report(
    registry: &mut fq_edge::EdgeRegistry,
    views: Arc<Views>,
) -> anyhow::Result<()> {
    let decl = fq_ops::Report::new::<DoctorParams, DoctorReport>(
        fq_ops::ControlReport::Doctor,
        "Durable-execution health in one report: workers, current work, ambiguity, \
         failures, dead letters.",
        fq_ops::Stability::Experimental,
    )
    .description(
        "A composite: the four checks are read together, at one instant, by the daemon \
         that owns the work being reported on — which is also the cost of asking. This \
         report cannot describe a daemon that is not running, and the absence of an \
         answer is itself the finding rather than a transport failure to retry. \
         The checks that count as *issues* are stale workers, stuck in-flight work, \
         ambiguous invocations and permanent failures; in-flight work that is merely \
         running is healthy, and the dead-letter line is informational. `stuck` means an \
         in-flight invocation whose WAL row has not advanced within the same threshold \
         that makes a worker stale — not hearing from either for that long is the same \
         order of signal. The threshold is the daemon's and is not a parameter: a health \
         report an operator can narrow is one they can narrow past the problem.",
    );
    registry
        .report::<DoctorParams, DoctorReport, _, _>(decl, move |_params: DoctorParams| {
            let views = views.clone();
            async move {
                let internal = |e: fq_runtime::views::ViewsError| WireError::Internal {
                    message: e.to_string(),
                };
                let now_ms = chrono::Utc::now().timestamp_millis();
                let workers = views.workers().await.map_err(internal)?;
                let executions = views
                    .executions(
                        now_ms,
                        DOCTOR_STUCK_THRESHOLD_MS,
                        fq_runtime::views::DEFAULT_LONG_DISPATCH_THRESHOLD_MS,
                    )
                    .await
                    .map_err(internal)?;
                let ambiguous = views
                    .recovery(now_ms, DOCTOR_STUCK_THRESHOLD_MS)
                    .await
                    .map_err(internal)?
                    .ambiguous;
                let failures = views.failures().await.map_err(internal)?;
                Ok(build_doctor_report(
                    &workers,
                    &executions,
                    ambiguous,
                    &failures,
                ))
            }
        })
        .map_err(|e| anyhow::anyhow!("operator registry: {e}"))?;

    Ok(())
}

#[cfg(test)]
mod tests;
