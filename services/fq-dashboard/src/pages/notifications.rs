//! The notifications pane's two handlers: the list and one signal's
//! detail page.
//!
//! Same shape as every other page here — dial, read, render, and on any
//! failure the unreachable page rather than a propagated error.
//!
//! **The pane is called notifications and the resource is
//! `operator_signal`.** Both severities list here; the operator-facing
//! name is the page's, the way the nav says *health* over
//! `control.status`. What an alert is, and why it outlives the log, the
//! page says for itself.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use fq_edge::EdgeClient;
use fq_ops::surface::{
    OPERATOR_SIGNAL_SEVERITIES, OperatorSignalCounts, OperatorSignalCountsParams,
    OperatorSignalFilter, OperatorSignalKey,
};
use fq_ops::views::{OperatorSignalDetailView, OperatorSignalView};
use fq_ops::{Domain, OpId, OperatorSignalReport, ReportId};

use crate::pages::{
    CallError, Page, call, edge_error_page, edge_or_unreachable, now_ms, ok_page, unreachable_page,
    updated_line,
};
use crate::{AppState, render};

/// The pane's rendering. Named `pane` rather than `render` because
/// `crate::render` is already in scope here and one name is one thing;
/// its module header says why it is not a `render.rs` submodule.
pub(crate) mod pane;

/// How many rows the pane asks for. Well under the surface's cap: a
/// signal is by construction rare, and a screenful an operator will
/// actually read beats a page they will scroll past.
const SIGNAL_PAGE_LIMIT: u32 = 100;

/// The window the home page's notification count is taken over — the
/// "last 24h" the line says out loud.
pub(crate) const COUNT_WINDOW_MS: i64 = 86_400_000;

/// The notifications pane.
pub(crate) async fn notifications_page(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Page {
    // An unrecognised severity is dropped here rather than forwarded:
    // the daemon would refuse it (correctly — there are two), and a
    // hand-edited query string turning the whole page into an error
    // banner is a worse answer than the unfiltered pane. The filter row
    // then shows `all` as current, which is what is being rendered.
    let severity = q
        .get("severity")
        .filter(|v| OPERATOR_SIGNAL_SEVERITIES.contains(&v.as_str()))
        .cloned();
    let source = q.get("source").filter(|v| !v.is_empty()).cloned();
    let filters = pane::SignalFilters {
        severity: severity.clone(),
        source: source.clone(),
    };

    let client = match edge_or_unreachable(&state, "notifications").await {
        Ok(c) => c,
        Err(page) => return page,
    };
    let filter = match serde_json::to_value(OperatorSignalFilter {
        severity,
        source,
        since: None,
        limit: Some(SIGNAL_PAGE_LIMIT),
    }) {
        Ok(filter) => filter,
        Err(err) => return unreachable_page(&state, "notifications", &format!("encode: {err}")),
    };
    // The same split the counts make, and for the same reason: a
    // daemon that does not serve the view has no pane, which is
    // honestly an empty listing. A denied grant or a daemon error is
    // not — the empty state reads "nothing has asked for a person's
    // attention", which is a claim this page would have no basis for.
    let rows: Vec<OperatorSignalView> =
        match call(&client, OpId::List(Domain::OperatorSignal), filter).await {
            Ok(rows) => rows,
            Err(CallError::NotFound | CallError::NotRegistered(_)) => Vec::new(),
            Err(CallError::Unreachable(err)) => {
                return unreachable_page(&state, "notifications", &err);
            }
            Err(CallError::Failed(err)) => {
                return edge_error_page(&state, "notifications", &err);
            }
        };
    ok_page(
        &state,
        "notifications",
        &pane::notifications(&rows, &filters, now_ms()),
    )
}

/// One signal's detail page.
pub(crate) async fn notification_page(
    State(state): State<Arc<AppState>>,
    Path(event_id): Path<String>,
) -> Page {
    let client = match edge_or_unreachable(&state, "notification").await {
        Ok(c) => c,
        Err(page) => return page,
    };
    let signal: OperatorSignalDetailView = match call(
        &client,
        OpId::Get(Domain::OperatorSignal),
        serde_json::json!(OperatorSignalKey {
            event_id: event_id.clone()
        }),
    )
    .await
    {
        Ok(signal) => signal,
        Err(CallError::NotFound) => {
            return (
                StatusCode::NOT_FOUND,
                Html(render::live_page(
                    "notification",
                    state.refresh_secs,
                    &format!(
                        r#"<p class="muted">no operator signal with that id. <a href="/notifications">← all notifications</a></p>{}"#,
                        updated_line(now_ms())
                    ),
                )),
            );
        }
        Err(CallError::Unreachable(err)) => {
            return unreachable_page(&state, "notification", &err);
        }
        Err(CallError::Failed(err) | CallError::NotRegistered(err)) => {
            return edge_error_page(&state, "notification", &err);
        }
    };
    ok_page(
        &state,
        &format!("notification · {}", signal.kind),
        &pane::notification_detail(&signal, now_ms()),
    )
}

/// The home page's counts, over an already-dialled client. `None` is
/// **unknown** — the call did not answer, and the row says so.
///
/// Several failures, and only one of them is a zero.
/// [`CallError::NotRegistered`] is a complete answer about an older
/// daemon: it does not serve `operator_signal.counts`, so it has no
/// pane and there are genuinely no signals — zeros, and the home page
/// renders the rest of the health view rather than trading it for a
/// line. `NotFound` joins it because a report that answered so would be
/// saying the same thing about itself.
///
/// Everything else — a denied grant, a decode failure, a daemon error,
/// a connection that broke under the call — says nothing about how many
/// alerts stand. Rendering those as `0` is the worst answer available:
/// green, specific, and wrong precisely when an operator is looking at
/// the page to find out whether anything needs them. The deployed
/// failure is the first of them (a token minted before
/// `read:operator_signal` existed), which is why this is a distinction
/// and not a comment.
pub(crate) async fn signal_counts(client: &EdgeClient) -> Option<OperatorSignalCounts> {
    let since = (chrono::Utc::now() - chrono::Duration::milliseconds(COUNT_WINDOW_MS)).to_rfc3339();
    let params = serde_json::to_value(OperatorSignalCountsParams {
        notifications_since: Some(since),
    })
    .ok()?;
    match call(
        client,
        OpId::Report(ReportId::OperatorSignal(OperatorSignalReport::Counts)),
        params,
    )
    .await
    {
        Ok(counts) => Some(counts),
        Err(CallError::NotFound | CallError::NotRegistered(_)) => {
            Some(OperatorSignalCounts::default())
        }
        Err(CallError::Unreachable(_) | CallError::Failed(_)) => None,
    }
}
