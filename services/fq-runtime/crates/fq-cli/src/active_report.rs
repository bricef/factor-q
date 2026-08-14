//! `invocation.active`, daemon-side: what this daemon is executing at
//! the instant of the call.
//!
//! Its own module on the precedent of the other declared reports
//! (`cost_report`, `doctor_report`, `status_report`) — the assembly
//! point holds the registrations, not the registrations themselves.

use std::sync::Arc;

use fq_edge::wire::WireError;
use fq_runtime::surface::ActiveParams;
use fq_runtime::views::Views;

/// Register `invocation.active` on the daemon's edge — what this
/// daemon is executing at the instant of the call.
///
/// **A report, and the nature is the point.** A view answers as of a
/// watermark: `invocation.list` is a fold of the log, and a caller can
/// gate it on their own write. Live execution has no watermark to be
/// answered as of — a step boundary is not a position in the log, and
/// the answer changes while it is being serialised — so the model's
/// rule that a report "is not watermarked, and cannot be" is not a
/// limitation here but a description. Declaring this as a filter on
/// the Invocation view would have promised a watermark it could never
/// honour.
///
/// The rows come from the worker WAL rather than the coordination
/// ownership table. That is an implementation detail and stays out of
/// the declared description, but it is the reason the two surfaces
/// cannot be collapsed today: trigger dispatch does not populate the
/// ownership table's `in_flight` status (#50), so the WAL is the only
/// place live work is guaranteed to appear. Closing that gap would not
/// retire this report — the two questions stay different.
pub(crate) fn register_active_report(
    registry: &mut fq_edge::EdgeRegistry,
    views: Arc<Views>,
) -> anyhow::Result<()> {
    let decl = fq_ops::Report::new::<ActiveParams, Vec<fq_runtime::views::ActiveInvocationView>>(
        fq_ops::InvocationReport::Active,
        "What this daemon is executing right now: one row per running invocation, \
         longest-running first, with its open tool and model calls.",
        fq_ops::Stability::Experimental,
    )
    .description(
        "Live execution state, read at the instant of the call. Answers `what is \
         running right now` — the phase and step each invocation has reached, how \
         long since its last advance, and which tool or model calls are open on it. \
         \
         This is not the same question as listing invocations whose status is \
         in-flight, and a caller should know which they want. A listing is a fold of \
         the event log: it answers as of a position in that log, it can be gated on \
         one, and it reports what has been recorded. This reports what is happening, \
         which has no such position — the work advances while the answer is being \
         assembled, so two calls a second apart legitimately differ and neither is \
         stale. Ask the listing for history and for anything you need read-your-writes \
         on; ask this for a live picture. \
         \
         It describes one daemon — the one answering — and not a fleet. An invocation \
         another worker is driving is not here.",
    );
    registry
        .report::<ActiveParams, Vec<fq_runtime::views::ActiveInvocationView>, _, _>(
            decl,
            move |_params: ActiveParams| {
                let views = views.clone();
                async move {
                    views
                        .active_invocations(
                            chrono::Utc::now().timestamp_millis(),
                            fq_runtime::control_plane::coordination_consumer::DEFAULT_STALE_THRESHOLD_MS,
                            fq_runtime::views::DEFAULT_LONG_DISPATCH_THRESHOLD_MS,
                        )
                        .await
                        .map_err(|e| WireError::Internal {
                            message: e.to_string(),
                        })
                }
            },
        )
        .map_err(|e| anyhow::anyhow!("operator registry: {e}"))?;
    Ok(())
}
