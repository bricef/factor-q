//! The Invocation view — `invocation.get` and `invocation.list`, bound
//! to the daemon's read views.
//!
//! Its own module for the reason `health_surface` is: `operator_surface`
//! is at the 800-line file cap and may only shrink, so a registration
//! that grows an argument moves out rather than pushing the file over.
//! This one grew the daemon's derived stuck threshold (#37) — the
//! detail read returns a liveness verdict, and every surface that
//! renders one has to be judged against the same number, or `fq
//! invocation show` and `fq doctor` describe the same invocation
//! differently — and then the archive sweeper's holds, which mark a
//! finished run whose archive the worker is holding back.

use std::sync::Arc;

use fq_edge::wire::WireError;
use fq_ops::health::ArchiveHold;
use fq_runtime::surface::{InvocationListFilter, InvocationViewKey};
use fq_runtime::views::{
    InvocationArchiveState, InvocationDetailView, InvocationSummaryView, Views,
};

use crate::operator_surface::DaemonFacts;

/// Register the Invocation view on the daemon's edge.
pub(crate) fn register_invocation_view(
    registry: &mut fq_edge::EdgeRegistry,
    views: Arc<Views>,
    // The derived stuck threshold, see
    // [`DaemonFacts::stuck_after_ms`], and the archive sweeper's holds,
    // see [`DaemonFacts::archive_holds`].
    facts: &DaemonFacts,
) -> anyhow::Result<()> {
    let stuck_after_ms = facts.stuck_after_ms;
    let decl = fq_ops::View::new::<
        InvocationViewKey,
        InvocationDetailView,
        InvocationSummaryView,
        InvocationListFilter,
    >(
        fq_ops::Domain::Invocation,
        "An agent invocation: the fold of its lifecycle events.",
        fq_ops::Stability::Experimental,
    );

    let get_views = views.clone();
    let get_holds = facts.archive_holds.clone();
    let list_holds = facts.archive_holds.clone();
    registry
        .view::<InvocationViewKey, InvocationDetailView, InvocationSummaryView, InvocationListFilter, _, _, _, _>(
            decl,
            move |key: InvocationViewKey| {
                let views = get_views.clone();
                let holds = get_holds.clone();
                async move {
                    let internal = |e: fq_runtime::views::ViewsError| WireError::Internal {
                        message: e.to_string(),
                    };
                    let detail = views
                        .invocation(
                            &key.invocation_id,
                            chrono::Utc::now().timestamp_millis(),
                            stuck_after_ms,
                            fq_runtime::views::DEFAULT_LONG_DISPATCH_THRESHOLD_MS,
                        )
                        .await
                        .map_err(internal)?;
                    let mut detail = detail.ok_or_else(|| WireError::NotFound {
                        op: "invocation.get".into(),
                        message: format!("no invocation `{}`", key.invocation_id),
                    })?;
                    mark_held_detail(&mut detail, &holds.snapshot());
                    Ok(detail)
                }
            },
            move |filter: InvocationListFilter| {
                let views = views.clone();
                let holds = list_holds.clone();
                async move {
                    let status = filter
                        .status
                        .as_deref()
                        .map(crate::operator_surface::parse_invocation_status_filter)
                        .transpose()
                        .map_err(|e| WireError::InvalidInput {
                            op: "invocation.list".into(),
                            message: e.to_string(),
                        })?;
                    let mut items = views
                        .invocation_index(status, filter.include_archived, filter.limit)
                        .await
                        .map_err(|e| WireError::Internal {
                            message: e.to_string(),
                        })?;
                    mark_held(&mut items, &holds.snapshot());
                    Ok(items)
                }
            },
        )
        .map_err(|e| anyhow::anyhow!("operator registry: {e}"))?;
    Ok(())
}

/// The hold on `item`'s archive, when the store says the archive is
/// `pending` and the worker's archive sweeper is holding that run's row.
///
/// The stored row says what is durably true: the run ended and its
/// archive has not reached the control plane. Why it has not is the
/// sweeper's verdict for this daemon run, kept in its memory and offered
/// again at the next start, so it is read here, where the report is
/// built, and never stored.
fn hold_for<'a>(item: &InvocationSummaryView, holds: &'a [ArchiveHold]) -> Option<&'a ArchiveHold> {
    if item.archive_state != InvocationArchiveState::Pending {
        return None;
    }
    holds
        .iter()
        .find(|hold| hold.invocation_id == item.invocation_id)
}

/// Mark the list's rows whose archive the sweeper is holding `held`.
pub(crate) fn mark_held(items: &mut [InvocationSummaryView], holds: &[ArchiveHold]) {
    for item in items {
        if hold_for(item, holds).is_some() {
            item.archive_state = InvocationArchiveState::Held;
        }
    }
}

/// The same for the detail, which also carries the hold: its reason and
/// what lifts it.
pub(crate) fn mark_held_detail(detail: &mut InvocationDetailView, holds: &[ArchiveHold]) {
    let Some(owner) = detail.owner.as_mut() else {
        return;
    };
    if let Some(hold) = hold_for(owner, holds) {
        owner.archive_state = InvocationArchiveState::Held;
        detail.archive_hold = Some(hold.clone());
    }
}

#[cfg(test)]
mod tests;
