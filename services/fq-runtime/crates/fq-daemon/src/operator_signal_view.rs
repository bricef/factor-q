//! The OperatorSignal view: what a component of the daemon said an
//! operator should look at, on the operator surface — Get by the
//! producing event's identity, List over the recent window, and a
//! counts report for the line a dashboard's home page opens with.
//!
//! **A view, not an atom, and the difference is where the fact lives.**
//! An atom is the fact itself; an operator signal's fact is the
//! `operator_signal` *event*, which is already on this surface as the
//! Event atom. What is registered here is the projection's fold of
//! those events — one row per signal, indexed on the two things a
//! person triages by — which is a view in exactly the sense Invocation
//! is one. That also settles the stream: tailing signals is
//! `event.stream` narrowed to the event type, so declaring a second
//! stream here would be two cursors over one log, free to disagree.
//!
//! **The whole signal is in the row, and that is not an optimisation.**
//! Get answers from the projection rather than hopping to the log,
//! because an alert is kept indefinitely while the log keeps thirty
//! days: a detail page that read the particulars back out of the log
//! would go blank on precisely the signals the index exists to
//! preserve. The row therefore carries `detail` and the references
//! verbatim, the way a trigger's row carries its payload.
//!
//! Its own module rather than more of `operator_surface.rs`: that file
//! is the daemon's assembly point and is near its size budget.

use std::sync::Arc;

use fq_edge::wire::WireError;
use fq_ops::surface::{
    OPERATOR_SIGNAL_LIST_MAX_LIMIT, OPERATOR_SIGNAL_SEVERITIES, OperatorSignalCounts,
    OperatorSignalCountsParams, OperatorSignalFilter, OperatorSignalKey,
};
use fq_ops::views::{OperatorSignalDetailView, OperatorSignalView};
use fq_runtime::views::Views;

/// Default List page. Matches the other operator listings an
/// operator reads a screenful of at a time.
const OPERATOR_SIGNAL_LIST_DEFAULT_LIMIT: u32 = 50;

/// The page size a filter asks List for: the caller's own number,
/// checked against the cap, or the default when they named none.
///
/// **Over the cap is a refusal, not a shorter page** — the discipline
/// every list on this surface follows, for the reason the Event atom
/// states at length: List answers with a bare array, so a page the
/// daemon silently shortened is indistinguishable from a listing that
/// ended.
fn list_limit(filter: &OperatorSignalFilter) -> Result<u32, WireError> {
    let Some(limit) = filter.limit else {
        return Ok(OPERATOR_SIGNAL_LIST_DEFAULT_LIMIT);
    };
    if limit > OPERATOR_SIGNAL_LIST_MAX_LIMIT {
        return Err(WireError::InvalidInput {
            op: "operator_signal.list".into(),
            message: format!(
                "limit {limit} is over the {OPERATOR_SIGNAL_LIST_MAX_LIMIT}-row cap on one \
                 List page — ask for {OPERATOR_SIGNAL_LIST_MAX_LIMIT} or fewer. The cap is \
                 not applied silently because a shortened page and a complete one are the \
                 same answer to look at. For more than a page, narrow with `severity`, \
                 `source` or `since`."
            ),
        });
    }
    Ok(limit)
}

/// The severity narrowing, validated at the edge.
///
/// There are exactly two severities, so a value that is neither is a
/// verdict on the request rather than a listing with nothing in it: an
/// operator who mistypes `alerts` must not be told there are none.
fn severity_filter(filter: &OperatorSignalFilter) -> Result<Option<&str>, WireError> {
    let Some(severity) = filter.severity.as_deref() else {
        return Ok(None);
    };
    if !OPERATOR_SIGNAL_SEVERITIES.contains(&severity) {
        return Err(WireError::InvalidInput {
            op: "operator_signal.list".into(),
            message: format!(
                "unknown severity `{severity}` — try {}",
                OPERATOR_SIGNAL_SEVERITIES.join(" | ")
            ),
        });
    }
    Ok(Some(severity))
}

/// Narrow by an instant, in the grammar `fq costs --since` and
/// `event.list` already take — an operator who copies an argument from
/// one read to another must not discover that they disagree.
fn since_as_stored(since: Option<&str>, op: &str) -> Result<Option<String>, WireError> {
    since
        .map(|s| {
            fq_runtime::views::since::instant(s)
                .map(|t| t.to_rfc3339())
                .map_err(|e| WireError::InvalidInput {
                    op: op.to_string(),
                    message: format!("since {e}"),
                })
        })
        .transpose()
}

fn internal(e: fq_runtime::views::ViewsError) -> WireError {
    WireError::Internal {
        message: e.to_string(),
    }
}

/// Register the OperatorSignal view and its counts report.
pub(crate) fn register_operator_signal_view(
    registry: &mut fq_edge::EdgeRegistry,
    views: Arc<Views>,
) -> anyhow::Result<()> {
    let decl = fq_ops::View::new::<
        OperatorSignalKey,
        OperatorSignalDetailView,
        OperatorSignalView,
        OperatorSignalFilter,
    >(
        fq_ops::Domain::OperatorSignal,
        "A thing a component of the daemon said an operator should look at.",
        fq_ops::Stability::Experimental,
    )
    .description(
        "Two severities, and the difference is a human's hours. A `notification` is \
         handled during normal hours and looked at by the operator; an `alert` reaches \
         the operator out of hours and names something the system cannot recover from \
         on its own. `kind` is a dotted name whose FIRST SEGMENT IS THE SOURCE — \
         `pricing.change_refused` is the `pricing` component saying it — so the two \
         cannot disagree; `source` on the filter narrows by that segment. The kind \
         vocabulary is a registry that grows without a schema change, so a kind this \
         build has never heard of is a value to show, not an error. \
         Get answers with the whole signal: its structured `detail` exactly as the \
         producer sent it, the references it points at, and the ids of the signals \
         either side of it FROM THE SAME SOURCE, in the same order List renders. List \
         answers with index rows — identity, time, severity, source, kind and the one \
         summary line — most recent `limit` first. Every row's `event_id` reads the \
         whole signal back through Get, and always resolves: the row is the signal, so \
         there is no payload behind it for retention to take away. \
         AN ALERT IS OPEN UNTIL A LATER SIGNAL CLOSES IT. A signal may name the one it \
         closes, by event id, in `resolves`; every row here carries that and also \
         `resolved_by`, the first later signal that named it. An alert whose \
         `resolved_by` is null is one of the ones `operator_signal.counts` calls open. \
         A resolution is a relation between two signals on one log and not an \
         acknowledgement: a resolved alert is history, not something a person marked \
         read, and it stays listed. \
         RETENTION IS NOT THE SAME FOR THE TWO SEVERITIES. Notifications age out with \
         the event log they were folded from, on the daemon's `retention_days` window. \
         ALERTS ARE NEVER SWEPT and have no expiry: the record that a person had to \
         intervene outlives the log it arrived on, so a listing narrowed to alerts \
         reaches back past everything else here. \
         There is no stream. These are folds of `operator_signal` events, and tailing \
         them is `event.stream` narrowed to that event type — one cursor over one log \
         rather than two that could disagree. \
         `event_id` is the identity of the producing event, so the same string reads \
         the raw envelope back through `event.get` for as long as the log holds it.",
    );

    let get_views = views.clone();
    let list_views = views.clone();
    registry
        .view::<OperatorSignalKey, OperatorSignalDetailView, OperatorSignalView, OperatorSignalFilter, _, _, _, _>(
            decl,
            move |key: OperatorSignalKey| {
                let views = get_views.clone();
                async move {
                    views
                        .operator_signal(&key.event_id)
                        .await
                        .map_err(internal)?
                        .ok_or_else(|| WireError::NotFound {
                            op: "operator_signal.get".into(),
                            message: format!("no operator signal `{}`", key.event_id),
                        })
                }
            },
            move |filter: OperatorSignalFilter| {
                let views = list_views.clone();
                async move {
                    let severity = severity_filter(&filter)?;
                    let limit = list_limit(&filter)?;
                    let since =
                        since_as_stored(filter.since.as_deref(), "operator_signal.list")?;
                    views
                        .operator_signals(
                            severity,
                            filter.source.as_deref(),
                            since.as_deref(),
                            i64::from(limit),
                        )
                        .await
                        .map_err(internal)
                }
            },
        )
        .map_err(|e| anyhow::anyhow!("operator registry: {e}"))?;

    register_counts_report(registry, views)
}

/// `operator_signal.counts` — the two numbers a dashboard's home line
/// says out loud.
///
/// A report rather than a filter on List, because a **count** is not a
/// page: a listing is capped, and a full page and a saturated one look
/// the same, so counting rows off a listing would give the home line a
/// number that quietly stops growing. Same shape and same reasoning as
/// `cost.summary`, which folds the same projection into totals.
fn register_counts_report(
    registry: &mut fq_edge::EdgeRegistry,
    views: Arc<Views>,
) -> anyhow::Result<()> {
    let decl = fq_ops::Report::new::<OperatorSignalCountsParams, OperatorSignalCounts>(
        fq_ops::OperatorSignalReport::Counts,
        "How many notifications landed inside a window, and how many alerts are still \
         open.",
        fq_ops::Stability::Experimental,
    )
    .description(
        "Two counts, and only one of them takes a window. `notifications_since` bounds \
         the notification count; the alert count is bounded by RESOLUTION instead, \
         because alerts are never swept and counting them inside a window would answer a \
         different question. AN ALERT IS OPEN UNTIL A LATER SIGNAL RESOLVES IT — names \
         its `event_id` in that signal's `resolves` — so `open_alerts` is a fold over \
         the log rather than a row count, and it falls when the component that raised \
         the alert says the condition has passed. It is not an unread count: this build \
         has no acknowledgement, and a resolved alert stays on the record and stays \
         listable. Both counts are reads of the same index `operator_signal.list` pages, \
         so a count and a listing narrowed the same way agree.",
    );
    registry
        .report::<OperatorSignalCountsParams, OperatorSignalCounts, _, _>(
            decl,
            move |params: OperatorSignalCountsParams| {
                let views = views.clone();
                async move {
                    let since = since_as_stored(
                        params.notifications_since.as_deref(),
                        "operator_signal.counts",
                    )?;
                    views
                        .operator_signal_counts(since.as_deref())
                        .await
                        .map_err(internal)
                }
            },
        )
        .map_err(|e| anyhow::anyhow!("operator registry: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests;
