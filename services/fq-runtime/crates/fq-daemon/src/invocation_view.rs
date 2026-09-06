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
//! differently.

use std::sync::Arc;

use fq_edge::wire::WireError;
use fq_runtime::surface::{InvocationListFilter, InvocationViewKey};
use fq_runtime::views::Views;

/// Register the Invocation view on the daemon's edge.
pub(crate) fn register_invocation_view(
    registry: &mut fq_edge::EdgeRegistry,
    views: Arc<Views>,
    // The daemon's derived stuck threshold — see
    // [`crate::operator_surface::DaemonFacts::stuck_after_ms`].
    stuck_after_ms: i64,
) -> anyhow::Result<()> {
    let decl = fq_ops::View::new::<
        InvocationViewKey,
        fq_runtime::views::InvocationDetailView,
        fq_runtime::views::InvocationSummaryView,
        InvocationListFilter,
    >(
        fq_ops::Domain::Invocation,
        "An agent invocation: the fold of its lifecycle events.",
        fq_ops::Stability::Experimental,
    );

    let get_views = views.clone();
    registry
        .view::<InvocationViewKey, fq_runtime::views::InvocationDetailView, fq_runtime::views::InvocationSummaryView, InvocationListFilter, _, _, _, _>(
            decl,
            move |key: InvocationViewKey| {
                let views = get_views.clone();
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
                    detail.ok_or_else(|| WireError::NotFound {
                        op: "invocation.get".into(),
                        message: format!("no invocation `{}`", key.invocation_id),
                    })
                }
            },
            move |filter: InvocationListFilter| {
                let views = views.clone();
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
                    views
                        .invocation_index(status, filter.include_archived, filter.limit)
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
