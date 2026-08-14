//! The pages: one handler per route, each a dial, a read, and a
//! render.
//!
//! Split from `main.rs` when the edge re-point pushed that file past
//! the 800-line cap — the ratchet's remedy for a file that grows is to
//! split it, and this was already two things: the binary (its
//! arguments, its credentials, its router) and the pages it serves.
//!
//! Every handler has the same shape, and it is the crash-domain
//! contract in code: dial, read, and on any failure render
//! [`unreachable_page`] rather than propagating an error. A page that
//! cannot reach the daemon still answers the browser — with a 503 and
//! a banner saying so — because the whole point of a separate process
//! is that the operator can see the daemon is down.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use fq_edge::EdgeClient;
use fq_edge::wire::WireError;
use fq_ops::{ControlReport, CostReport as CostReportId, Domain, OpId, ReportId};
use fq_runtime::agent_view::{AgentDetailView, AgentEntryView, AgentsView};
use fq_runtime::read_service;
use fq_runtime::surface::{
    AgentListFilter, AgentViewKey, CostByAgentParams, CostSummaryParams, DoctorReport,
    EVENT_LIST_MAX_LIMIT, EventFilter, InvocationListFilter, InvocationViewKey, StatusReport,
};
use fq_runtime::views::{
    ActiveInvocationView, AgentCostDetailView, CostReport, EventView, InvocationDetailView,
    InvocationSummaryView,
};
use serde::de::DeserializeOwned;
use tarpc::context;

use crate::{AppState, render, skew};

mod transcript;
pub(crate) use transcript::{transcript_page, transcript_stream};

/// Epoch-ms clock for age rendering.
pub(crate) fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The live region's freshness line: wall-clock HH:MM:SS UTC, morphed
/// on every poll. A reader can tell at a glance that ticks are landing
/// — and a frozen time is the honest signal that they stopped.
pub(crate) fn updated_line(now_ms: i64) -> String {
    let s = now_ms / 1000 % 86_400;
    format!(
        r#"<p class="muted">updated {:02}:{:02}:{:02} UTC</p>"#,
        s / 3600,
        (s % 3600) / 60,
        s % 60
    )
}

pub(crate) type Page = (StatusCode, Html<String>);

/// Why an edge call did not produce an answer. `NotFound` is split out
/// because several pages turn it into a 404 — the request was fine,
/// the entity is not there — while everything else is a failure to
/// report on the unreachable page.
enum CallError {
    NotFound,
    Failed(String),
}

/// Dial the edge, or produce the unreachable page.
///
/// No version probe rides along. The old read service had a frozen
/// `version()` RPC for exactly that, because its wire was a
/// length-framed *binary* codec and any shape change between builds
/// surfaced as a decode failure indistinguishable from a dead daemon
/// (#154). The edge is JSON, and payloads cross it as
/// `serde_json::Value` inside a stable envelope, so a daemon that has
/// added a field is simply read by an older dashboard — the failure
/// mode the freeze existed to detect is largely gone with it. Skew is
/// still worth reporting, so the version is taken from
/// `control.status` where the health page already asks for it.
async fn edge_or_unreachable(state: &AppState, title: &str) -> Result<EdgeClient, Page> {
    EdgeClient::connect(&state.edge_addr, state.edge_fingerprint, &state.edge_token)
        .await
        .map_err(|err| unreachable_page(state, title, &format!("edge: {err}")))
}

/// One declared operation, invoked and decoded into the contract type
/// the daemon answers with. The type parameter is the whole point of
/// the shapes being shared (D-3): the daemon serialises the same
/// struct this deserialises, so a field rename is a compile error on
/// one side or the other rather than an empty page.
async fn call<T: DeserializeOwned>(
    client: &EdgeClient,
    op: OpId,
    input: serde_json::Value,
) -> Result<T, CallError> {
    match client.invoke(op, input).await {
        Err(err) => Err(CallError::Failed(format!("rpc: {err}"))),
        Ok(Err(WireError::NotFound { .. })) => Err(CallError::NotFound),
        Ok(Err(err)) => Err(CallError::Failed(err.to_string())),
        Ok(Ok(value)) => {
            serde_json::from_value(value).map_err(|err| CallError::Failed(format!("decode: {err}")))
        }
    }
}

/// Prefix the body with the skew banner when a build mismatch was
/// observed. Warn-and-continue (#168): the banner is loud, but the
/// page still renders whatever decoded.
fn with_skew_banner(state: &AppState, body: &str) -> String {
    match skew(state) {
        Some((own, daemon)) => format!("{}{}", render::skew_banner(&own, &daemon), body),
        None => body.to_string(),
    }
}

pub(crate) fn unreachable_page(state: &AppState, title: &str, error: &str) -> Page {
    let seen = match state.last_seen_ms.load(Ordering::Relaxed) {
        0 => None,
        ms => Some(ms),
    };
    // With known skew, name the likely cause: a cross-build decode
    // failure is indistinguishable from a dead daemon at this layer,
    // and "runtime unreachable" alone sends the operator hunting for
    // the wrong problem (the #154 incident).
    let error = match skew(state) {
        Some((own, daemon)) => {
            format!(
                "{error} — possibly wire mismatch from build skew (dashboard @{own}, daemon @{daemon})"
            )
        }
        None => error.to_string(),
    };
    let body = format!(
        "{}{}",
        with_skew_banner(
            state,
            &render::unreachable(&state.read_addr, &error, seen, now_ms()),
        ),
        updated_line(now_ms()),
    );
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Html(render::live_page(title, state.refresh_secs, &body)),
    )
}

pub(crate) fn ok_page(state: &AppState, title: &str, body: &str) -> Page {
    state.last_seen_ms.store(now_ms(), Ordering::Relaxed);
    let body = format!(
        "{}{}",
        with_skew_banner(state, body),
        updated_line(now_ms())
    );
    (
        StatusCode::OK,
        Html(render::live_page(title, state.refresh_secs, &body)),
    )
}

/// The health page: machinery state composed from the two Control
/// reports. `control.status` answers what the daemon is and what its
/// streams are doing; `control.doctor` answers whether anything needs
/// an operator. Two calls, one authority (`read:control`) — a report's
/// authority is Read on its own scope, so composing them costs no
/// extra grant.
///
/// What the read service served as one `HealthReport` was this same
/// data: `event_count` is `control.status`'s `projection_rows`, and
/// `executions`/`failures` are `control.doctor`'s. Nothing is dropped
/// and nothing new is asked for.
pub(crate) async fn health_page(State(state): State<Arc<AppState>>) -> Page {
    let client = match edge_or_unreachable(&state, "health").await {
        Ok(c) => c,
        Err(page) => return page,
    };
    let status: StatusReport = match call(
        &client,
        OpId::Report(ReportId::Control(ControlReport::Status)),
        serde_json::json!({}),
    )
    .await
    {
        Ok(report) => report,
        Err(CallError::NotFound) => {
            return unreachable_page(&state, "health", "control.status is not registered");
        }
        Err(CallError::Failed(err)) => return unreachable_page(&state, "health", &err),
    };
    // Recorded before the second call, so a doctor failure still
    // leaves the banner able to name both builds.
    *state.daemon_version.lock().expect("daemon_version lock") = Some(status.version.clone());

    let doctor: DoctorReport = match call(
        &client,
        OpId::Report(ReportId::Control(ControlReport::Doctor)),
        serde_json::json!({}),
    )
    .await
    {
        Ok(report) => report,
        Err(CallError::NotFound) => {
            return unreachable_page(&state, "health", "control.doctor is not registered");
        }
        Err(CallError::Failed(err)) => return unreachable_page(&state, "health", &err),
    };
    ok_page(&state, "health", &render::health(&status, &doctor))
}

/// The invocations page: the "Active now" table above the list.
///
/// **The active table is the last `ReadService` caller in the tree.**
/// It renders `ActiveInvocationView`, which is sourced from the worker
/// WAL rather than the coordination owner table — `views.rs` says why:
/// trigger dispatch does not populate the owner table's `in_flight`
/// status (#50), so the WAL is the only place live work is guaranteed
/// to appear. `invocation.list` reads the owner table, so no filter on
/// it can answer this, and no other declared operation exposes those
/// rows. Retiring the read service therefore needs a decision about
/// declaring one, which is held; until then this half of the page
/// keeps its old path and the rest of the dashboard has moved.
pub(crate) async fn invocations_page(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Page {
    let filters = render::InvocationFilters {
        include_archived: q.get("archived").is_some_and(|v| v == "1"),
        show_completed: q.get("completed").is_none_or(|v| v != "0"),
        show_failed: q.get("failed").is_none_or(|v| v != "0"),
    };
    let status = q.get("status").cloned();

    let read_client = match read_service::connect(&state.read_addr).await {
        Ok(c) => c,
        Err(err) => return unreachable_page(&state, "invocations", &format!("connect: {err}")),
    };
    let active: Vec<ActiveInvocationView> =
        match read_client.active_invocations(context::current()).await {
            Ok(Ok(active)) => active,
            Ok(Err(err)) => return unreachable_page(&state, "invocations", &err.to_string()),
            Err(err) => return unreachable_page(&state, "invocations", &format!("rpc: {err}")),
        };

    let client = match edge_or_unreachable(&state, "invocations").await {
        Ok(c) => c,
        Err(page) => return page,
    };
    let items: Vec<InvocationSummaryView> = match call(
        &client,
        OpId::List(Domain::Invocation),
        match serde_json::to_value(InvocationListFilter {
            status,
            include_archived: filters.include_archived,
            limit: INVOCATION_LIST_LIMIT,
        }) {
            Ok(value) => value,
            Err(err) => {
                return unreachable_page(&state, "invocations", &format!("encode: {err}"));
            }
        },
    )
    .await
    {
        Ok(items) => items,
        Err(CallError::NotFound) => Vec::new(),
        Err(CallError::Failed(err)) => return unreachable_page(&state, "invocations", &err),
    };
    ok_page(
        &state,
        "invocations",
        &render::invocations_page(&active, &items, filters, now_ms()),
    )
}

/// How many rows the list asks for — the number the read service's
/// `invocations` RPC was called with, kept so the page shows what it
/// always showed.
const INVOCATION_LIST_LIMIT: i64 = 100;

pub(crate) async fn invocation_page(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Page {
    let client = match edge_or_unreachable(&state, "invocation").await {
        Ok(c) => c,
        Err(page) => return page,
    };
    let detail: InvocationDetailView = match call(
        &client,
        OpId::Get(Domain::Invocation),
        serde_json::json!(InvocationViewKey {
            invocation_id: id.clone()
        }),
    )
    .await
    {
        Ok(detail) => detail,
        Err(CallError::NotFound) => {
            return (
                StatusCode::NOT_FOUND,
                Html(render::live_page(
                    "invocation",
                    state.refresh_secs,
                    &format!(
                        r#"<p class="muted">no invocation with that id.</p>{}"#,
                        updated_line(now_ms())
                    ),
                )),
            );
        }
        Err(CallError::Failed(err)) => return unreachable_page(&state, "invocation", &err),
    };
    ok_page(
        &state,
        &format!("invocation {}", &id.chars().take(8).collect::<String>()),
        &render::invocation_detail(&detail, now_ms()),
    )
}

/// The vendored datastar client (pinned v1.0.0, MIT; sha256 recorded in
/// the PR that introduced it). Served from the binary so the dashboard
/// stays fully self-contained behind its auth front — no CDN.
pub(crate) async fn datastar_js() -> impl axum::response::IntoResponse {
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/javascript"),
            (axum::http::header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("../assets/datastar.js"),
    )
}

pub(crate) async fn events_page(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Page {
    let client = match edge_or_unreachable(&state, "events").await {
        Ok(c) => c,
        Err(page) => return page,
    };
    // The page's own default, then the surface's cap. Clamping here
    // rather than forwarding an over-cap ask keeps a hand-edited
    // `?limit=` rendering a page instead of the daemon's refusal —
    // the cap is a shared constant precisely so a client can respect
    // it without discovering it by failing.
    let limit = q
        .get("limit")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(EVENT_PAGE_LIMIT)
        .min(EVENT_LIST_MAX_LIMIT);
    let filter = match serde_json::to_value(EventFilter {
        agent: q.get("agent").cloned(),
        event_type: q.get("type").cloned(),
        since: None,
        limit: Some(limit),
    }) {
        Ok(filter) => filter,
        Err(err) => return unreachable_page(&state, "events", &format!("encode: {err}")),
    };
    let rows: Vec<EventView> = match call(&client, OpId::List(Domain::Event), filter).await {
        Ok(rows) => rows,
        Err(CallError::NotFound) => Vec::new(),
        Err(CallError::Failed(err)) => return unreachable_page(&state, "events", &err),
    };
    ok_page(&state, "events", &render::events(&rows))
}

/// The events page's own default page size.
const EVENT_PAGE_LIMIT: u32 = 50;

/// RFC3339 timestamp `ms` milliseconds in the past. The projection
/// stores envelope timestamps via `.to_rfc3339()`, so this form
/// string-compares correctly against its `timestamp >= ?` bound.
fn rfc3339_ago(ms: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::milliseconds(ms)).to_rfc3339()
}

pub(crate) async fn costs_page(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Page {
    let window = render::Window::from_query(q.get("window").map(String::as_str));
    let client = match edge_or_unreachable(&state, "costs").await {
        Ok(c) => c,
        Err(page) => return page,
    };
    let since = window.since_ms().map(rfc3339_ago);
    // Hourly bars for the day window, daily otherwise.
    let hourly = window == render::Window::Day;
    let summary = |since: Option<String>, hourly_buckets: bool| {
        serde_json::to_value(CostSummaryParams {
            agent: None,
            since,
            hourly_buckets,
        })
    };
    let params = match summary(since, hourly) {
        Ok(params) => params,
        Err(err) => return unreachable_page(&state, "costs", &format!("encode: {err}")),
    };
    let report: CostReport = match call(
        &client,
        OpId::Report(ReportId::Cost(CostReportId::Summary)),
        params,
    )
    .await
    {
        Ok(report) => report,
        Err(CallError::NotFound) => {
            return unreachable_page(&state, "costs", "cost.summary is not registered");
        }
        Err(CallError::Failed(err)) => return unreachable_page(&state, "costs", &err),
    };
    // The last-24h column always reads from a day-bounded report; when
    // the page window IS the day, that's the same data — skip the call.
    let day = if window == render::Window::Day {
        report.clone()
    } else {
        let day_since = rfc3339_ago(
            render::Window::Day
                .since_ms()
                .expect("day window is bounded"),
        );
        let params = match summary(Some(day_since), false) {
            Ok(params) => params,
            Err(err) => return unreachable_page(&state, "costs", &format!("encode: {err}")),
        };
        match call(
            &client,
            OpId::Report(ReportId::Cost(CostReportId::Summary)),
            params,
        )
        .await
        {
            Ok(day) => day,
            Err(CallError::NotFound) => {
                return unreachable_page(&state, "costs", "cost.summary is not registered");
            }
            Err(CallError::Failed(err)) => return unreachable_page(&state, "costs", &err),
        }
    };
    ok_page(
        &state,
        "costs",
        &render::costs(&report, &day, window, now_ms()),
    )
}

/// How many per-invocation rows the drill-down shows; the totals row
/// carries the uncapped count ("showing N of M").
const AGENT_COST_INVOCATION_LIMIT: i64 = 50;

pub(crate) async fn agent_costs_page(
    State(state): State<Arc<AppState>>,
    Path(agent): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Page {
    let window = render::Window::from_query(q.get("window").map(String::as_str));
    let client = match edge_or_unreachable(&state, "costs").await {
        Ok(c) => c,
        Err(page) => return page,
    };
    let params = match serde_json::to_value(CostByAgentParams {
        agent: agent.clone(),
        since: window.since_ms().map(rfc3339_ago),
        invocation_limit: AGENT_COST_INVOCATION_LIMIT,
    }) {
        Ok(params) => params,
        Err(err) => return unreachable_page(&state, "costs", &format!("encode: {err}")),
    };
    let detail: AgentCostDetailView = match call(
        &client,
        OpId::Report(ReportId::Cost(CostReportId::ByAgent)),
        params,
    )
    .await
    {
        Ok(detail) => detail,
        Err(CallError::NotFound) => {
            return (
                StatusCode::NOT_FOUND,
                Html(render::live_page(
                    "costs",
                    state.refresh_secs,
                    &format!(
                        r#"<p class="muted">no cost events for that agent (in this window). <a href="/costs">← all agents</a></p>{}"#,
                        updated_line(now_ms())
                    ),
                )),
            );
        }
        Err(CallError::Failed(err)) => return unreachable_page(&state, "costs", &err),
    };
    ok_page(
        &state,
        &format!("costs · {agent}"),
        &render::agent_costs(&detail, window, now_ms()),
    )
}

/// Fold the Agent view's index rows back into the listing shape the
/// renderer takes. `agent.list` answers with one row per definition
/// file — loaded agents in id order, then the files the registry
/// rejected — because a rejected definition has no agent id to be
/// listed under; the page shows the two as separate sections.
fn agents_view(entries: Vec<AgentEntryView>) -> AgentsView {
    let mut view = AgentsView::default();
    for entry in entries {
        match entry {
            AgentEntryView::Agent(agent) => view.agents.push(agent),
            AgentEntryView::LoadError { message } => view.errors.push(message),
        }
    }
    view
}

pub(crate) async fn agents_page(State(state): State<Arc<AppState>>) -> Page {
    let client = match edge_or_unreachable(&state, "agents").await {
        Ok(c) => c,
        Err(page) => return page,
    };
    let filter = match serde_json::to_value(AgentListFilter {}) {
        Ok(filter) => filter,
        Err(err) => return unreachable_page(&state, "agents", &format!("encode: {err}")),
    };
    let entries: Vec<AgentEntryView> = match call(&client, OpId::List(Domain::Agent), filter).await
    {
        Ok(entries) => entries,
        Err(CallError::NotFound) => Vec::new(),
        Err(CallError::Failed(err)) => return unreachable_page(&state, "agents", &err),
    };
    ok_page(&state, "agents", &render::agents(&agents_view(entries)))
}

pub(crate) async fn agent_page(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Page {
    let client = match edge_or_unreachable(&state, "agent").await {
        Ok(c) => c,
        Err(page) => return page,
    };
    let detail: AgentDetailView = match call(
        &client,
        OpId::Get(Domain::Agent),
        serde_json::json!(AgentViewKey {
            agent_id: id.clone()
        }),
    )
    .await
    {
        Ok(detail) => detail,
        Err(CallError::NotFound) => {
            return (
                StatusCode::NOT_FOUND,
                Html(render::live_page(
                    "agent",
                    state.refresh_secs,
                    &format!(
                        r#"<p class="muted">no agent with that id in the registry. <a href="/agents">← all agents</a></p>{}"#,
                        updated_line(now_ms())
                    ),
                )),
            );
        }
        Err(CallError::Failed(err)) => return unreachable_page(&state, "agent", &err),
    };
    ok_page(
        &state,
        &format!("agent · {id}"),
        &render::agent_detail(&detail),
    )
}
