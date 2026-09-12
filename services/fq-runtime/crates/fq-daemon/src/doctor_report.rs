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
//! — is `fq_cli::doctor`, in the other binary. That seam is the one
//! Phase 5 split the binary along, and the report's declared shapes
//! sit on the shared side of it in [`fq_runtime::surface`] (re-exported
//! from `fq_ops::surface`, the leaf both binaries link): the
//! dashboard's health page composes this report with `control.status`,
//! so both ends name the same types.

use std::sync::Arc;

use fq_edge::wire::WireError;
use fq_ops::surface::build_doctor_report;
use fq_runtime::control_plane::coordination_consumer::DEFAULT_STALE_THRESHOLD_MS;
use fq_runtime::surface::{DoctorParams, DoctorReport};
use fq_runtime::views::Views;

/// Register `control.doctor` on the daemon's edge.
pub(crate) fn register_doctor_report(
    registry: &mut fq_edge::EdgeRegistry,
    views: Arc<Views>,
    // The daemon's own broker connection: the consumer half of the
    // report is a JetStream probe, and only this process holds one
    // (#549).
    bus: fq_runtime::EventBus,
    // What this daemon knows about itself: the stuck threshold it
    // derived from its own call deadlines (#37, shared with the sweep
    // that emits `invocation.stuck` so the report and the event cannot
    // disagree), whether a summariser durable is expected, and the live
    // state of its shared MCP servers.
    facts: &crate::operator_surface::DaemonFacts,
) -> anyhow::Result<()> {
    let stuck_after_ms = facts.stuck_after_ms;
    let summary_enabled = facts.summary_enabled;
    let mcp_servers = facts.mcp_servers.clone();
    let throttle = facts.throttle.clone();
    let agent_caps = facts.agent_caps.clone();
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
         in-flight invocation that has crossed no step boundary within `stuck_after_ms`, \
         with no tool or model call open recently enough to explain the silence. That \
         threshold is derived from this daemon's own call deadlines — twice the longest \
         a single step can legitimately take — so it is reported rather than assumed, \
         and it is not a parameter: a health report an operator can narrow is one they \
         can narrow past the problem. \
         The consumer lines are a probe of this daemon's broker at the instant of the \
         call: every durable it expects, named, with a stuck verdict for any that is \
         redelivering rather than progressing. A summariser durable is expected only \
         where `[summary]` configures one.",
    );
    registry
        .report::<DoctorParams, DoctorReport, _, _>(decl, move |_params: DoctorParams| {
            let views = views.clone();
            let bus = bus.clone();
            let mcp_servers = mcp_servers.clone();
            let throttle = throttle.clone();
            let agent_caps = agent_caps.clone();
            async move {
                let internal = |e: fq_runtime::views::ViewsError| WireError::Internal {
                    message: e.to_string(),
                };
                let now_ms = chrono::Utc::now().timestamp_millis();
                let workers = views.workers().await.map_err(internal)?;
                let executions = views
                    .executions(
                        now_ms,
                        stuck_after_ms,
                        fq_runtime::views::DEFAULT_LONG_DISPATCH_THRESHOLD_MS,
                    )
                    .await
                    .map_err(internal)?;
                // Only `.ambiguous` is read, and that count does not
                // depend on the threshold — the argument feeds
                // `recovery`'s stale-worker half, which this report gets
                // from `views.workers()` instead. It reads
                // `DEFAULT_STALE_THRESHOLD_MS` because that is what the
                // parameter means, not because anything here observes
                // it: passing the derived stuck threshold was wrong for
                // the same reason, and equally invisible.
                //
                // The stale-worker query behind it is therefore run and
                // thrown away on every `fq doctor`. Pre-existing, left
                // alone here: narrowing it is a change to `Views`, not
                // to this handler.
                let ambiguous = views
                    .recovery(now_ms, DEFAULT_STALE_THRESHOLD_MS)
                    .await
                    .map_err(internal)?
                    .ambiguous;
                let failures = views.failures().await.map_err(internal)?;
                let consumers = fq_runtime::health::probe_core_consumers(
                    &bus.jetstream(),
                    summary_enabled,
                    bus.redelivery_policy(),
                    bus.consumer_ledger(),
                )
                .await;
                Ok(build_doctor_report(
                    &workers,
                    &executions,
                    stuck_after_ms,
                    ambiguous,
                    &failures,
                    consumers,
                    fq_runtime::health::mcp_server_health(&mcp_servers),
                )
                // The throttle is live state, not a fold of the stores
                // the builder reads (#278); so is the per-agent count
                // (#718).
                .with_throttled_models(throttle.snapshot(now_ms))
                .with_agents_at_cap(agent_caps.snapshot()))
            }
        })
        .map_err(|e| anyhow::anyhow!("operator registry: {e}"))?;

    Ok(())
}

#[cfg(test)]
mod tests;
