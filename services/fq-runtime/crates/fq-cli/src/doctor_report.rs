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
//! binary along, and the report's declared shapes sit on the shared
//! side of it in [`fq_runtime::surface`]: the dashboard's health page
//! composes this report with `control.status`, so both ends name the
//! same types.

use std::sync::Arc;

use fq_edge::wire::WireError;
use fq_runtime::control_plane::coordination_consumer::DEFAULT_STALE_THRESHOLD_MS;
use fq_runtime::surface::{
    DoctorDeadLetters, DoctorExecutions, DoctorFailure, DoctorParams, DoctorReport, DoctorWorkers,
};
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
