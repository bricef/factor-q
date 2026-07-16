//! `fq-dashboard` — the operator dashboard: a standalone BFF binary
//! with its own crash domain (#105 layer 3). It holds only a tarpc
//! client to the runtime's read service and an HTTP server; it cannot
//! touch runtime state and cannot take the runtime down. If the daemon
//! is unreachable it renders "runtime unreachable, last seen Ns ago"
//! rather than breaking; if this process dies, the daemon never
//! notices.
//!
//! Deliberately naive (v0, per the plan): each browser request dials
//! the read service fresh (localhost TCP — microseconds, and it doubles
//! as reconnect logic) and renders server-side HTML. Liveness is a
//! datastar poll (the vendored client, no framework): every page's
//! `#main` region re-fetches its own URL each tick and the response —
//! negotiated via the `Datastar-Request` header — is a single-event
//! SSE patch morphed in place, so open folds, scroll position, and
//! text selection survive (the old whole-page `<meta refresh>` reset
//! them every 5s). No-JS browsers keep the full-page refresh via
//! `<noscript>`. Zero CORS (the browser only ever talks to this
//! process). Localhost-only: the operator reaches it via SSH tunnel,
//! and the bind refuses anything else.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use anyhow::Context as _;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::get;
use clap::Parser;
use fq_runtime::read_service::{self, ReadServiceClient};
use tarpc::context;

mod fixtures;
mod render;

/// This build's git SHA (stamped by build.rs). Compared against the
/// daemon's over the frozen `ReadService::version` probe (#168), and
/// printed by `--version` as `fq-dashboard <sha>` (watcher-style) so
/// deploy.sh can verify bundle coherence.
const OWN_SHA: &str = env!("FQ_GIT_SHA");

#[derive(Parser)]
#[command(
    name = "fq-dashboard",
    about = "factor-q operator dashboard (read-only)",
    version = OWN_SHA
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// Loopback address to serve the dashboard on. Reached via SSH
    /// tunnel; a non-loopback bind is refused.
    #[arg(long, default_value = "127.0.0.1:9472", env = "FQ_DASHBOARD_BIND")]
    bind: String,
    /// Address of the runtime's read service (`[read_service]` in
    /// fq.toml).
    #[arg(long, default_value = "127.0.0.1:9471", env = "FQ_READ_SERVICE")]
    read_service: String,
    /// Browser auto-refresh interval, in seconds.
    #[arg(long, default_value_t = 5, env = "FQ_DASHBOARD_REFRESH")]
    refresh: u64,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Render every page from canned, deterministic fixture data into a
    /// directory of static HTML files — the input for the screenshot
    /// pipeline (scripts/dashboard-screenshots.sh). Needs no daemon and
    /// no broker; a visual diff of the output means the rendering
    /// changed, never the clock.
    RenderFixtures {
        /// Output directory for the .html files.
        #[arg(long, default_value = "dashboard-fixtures")]
        out: std::path::PathBuf,
    },
}

/// Shared per-process state. No connection is held — see module doc —
/// so this is just the target address, the refresh knob, and the
/// last-successful-read timestamp for the unreachable banner.
struct AppState {
    read_addr: String,
    refresh_secs: u64,
    /// Epoch ms of the last successful RPC; 0 = never.
    last_seen_ms: AtomicI64,
    /// The daemon's version string as last observed over the frozen
    /// `ReadService::version` probe (#168). `None` until first
    /// observed, or when the probe fails (daemon predates the RPC) —
    /// in which case no skew is claimed. Kept across connect failures:
    /// "last observed" is honest context for the unreachable page.
    daemon_version: std::sync::Mutex<Option<String>>,
}

/// The SHA half of a `semver+sha` version string; the whole string
/// when there is no `+` (defensive — both sides emit the suffix form).
fn sha_suffix(version: &str) -> &str {
    version.rsplit_once('+').map_or(version, |(_, sha)| sha)
}

/// `Some((own_sha, daemon_sha))` when the last-observed daemon build
/// differs from this binary's — the build-skew signal (#168). Compares
/// SHAs, not full version strings, so a semver difference between the
/// two workspaces cannot false-positive.
fn skew(state: &AppState) -> Option<(String, String)> {
    let guard = state.daemon_version.lock().expect("daemon_version lock");
    let daemon_sha = sha_suffix(guard.as_deref()?).to_string();
    (daemon_sha != OWN_SHA).then(|| (OWN_SHA.to_string(), daemon_sha))
}

/// Epoch-ms clock for age rendering.
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The live region's freshness line: wall-clock HH:MM:SS UTC, morphed
/// on every poll. A reader can tell at a glance that ticks are landing
/// — and a frozen time is the honest signal that they stopped.
fn updated_line(now_ms: i64) -> String {
    let s = now_ms / 1000 % 86_400;
    format!(
        r#"<p class="muted">updated {:02}:{:02}:{:02} UTC</p>"#,
        s / 3600,
        (s % 3600) / 60,
        s % 60
    )
}

type Page = (StatusCode, Html<String>);

/// Dial the read service, or produce the unreachable page. On a
/// successful dial, also runs the build-skew probe (#168): the frozen
/// `version()` RPC decodes across any build pair, so it works exactly
/// when the shape-carrying RPCs might not. A probe failure records
/// "unknown" (`None`) rather than skew — a daemon predating the RPC
/// must not trip a false banner.
async fn client_or_unreachable(state: &AppState, title: &str) -> Result<ReadServiceClient, Page> {
    match read_service::connect(&state.read_addr).await {
        Ok(c) => {
            let observed = c.version(context::current()).await.ok();
            *state.daemon_version.lock().expect("daemon_version lock") = observed;
            Ok(c)
        }
        Err(err) => Err(unreachable_page(state, title, &format!("connect: {err}"))),
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

fn unreachable_page(state: &AppState, title: &str, error: &str) -> Page {
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

fn ok_page(state: &AppState, title: &str, body: &str) -> Page {
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

async fn health_page(State(state): State<Arc<AppState>>) -> Page {
    let client = match client_or_unreachable(&state, "health").await {
        Ok(c) => c,
        Err(page) => return page,
    };
    match client.health(context::current()).await {
        Ok(Ok(report)) => ok_page(&state, "health", &render::health(&report)),
        Ok(Err(err)) => unreachable_page(&state, "health", &err.to_string()),
        Err(err) => unreachable_page(&state, "health", &format!("rpc: {err}")),
    }
}

async fn invocations_page(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Page {
    let client = match client_or_unreachable(&state, "invocations").await {
        Ok(c) => c,
        Err(page) => return page,
    };
    let filters = render::InvocationFilters {
        include_archived: q.get("archived").is_some_and(|v| v == "1"),
        show_completed: q.get("completed").is_none_or(|v| v != "0"),
        show_failed: q.get("failed").is_none_or(|v| v != "0"),
    };
    let status = q.get("status").cloned();
    let active = match client.active_invocations(context::current()).await {
        Ok(Ok(active)) => active,
        Ok(Err(err)) => return unreachable_page(&state, "invocations", &err.to_string()),
        Err(err) => return unreachable_page(&state, "invocations", &format!("rpc: {err}")),
    };
    match client
        .invocations(context::current(), status, filters.include_archived, 100)
        .await
    {
        Ok(Ok(items)) => ok_page(
            &state,
            "invocations",
            &render::invocations_page(&active, &items, filters, now_ms()),
        ),
        Ok(Err(err)) => unreachable_page(&state, "invocations", &err.to_string()),
        Err(err) => unreachable_page(&state, "invocations", &format!("rpc: {err}")),
    }
}

async fn invocation_page(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Page {
    let client = match client_or_unreachable(&state, "invocation").await {
        Ok(c) => c,
        Err(page) => return page,
    };
    match client.invocation(context::current(), id.clone()).await {
        Ok(Ok(Some(detail))) => ok_page(
            &state,
            &format!("invocation {}", &id.chars().take(8).collect::<String>()),
            &render::invocation_detail(&detail, now_ms()),
        ),
        Ok(Ok(None)) => (
            StatusCode::NOT_FOUND,
            Html(render::live_page(
                "invocation",
                state.refresh_secs,
                &format!(
                    r#"<p class="muted">no invocation with that id.</p>{}"#,
                    updated_line(now_ms())
                ),
            )),
        ),
        Ok(Err(err)) => unreachable_page(&state, "invocation", &err.to_string()),
        Err(err) => unreachable_page(&state, "invocation", &format!("rpc: {err}")),
    }
}

/// The vendored datastar client (pinned v1.0.0, MIT; sha256 recorded in
/// the PR that introduced it). Served from the binary so the dashboard
/// stays fully self-contained behind its auth front — no CDN.
async fn datastar_js() -> impl axum::response::IntoResponse {
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/javascript"),
            (axum::http::header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("../assets/datastar.js"),
    )
}

/// The transcript's live tail: an SSE stream of datastar element
/// patches. Polls `transcript_since` (cursor-indexed, microsecond WAL
/// reads) every second and forwards only NEW entries as appends into
/// `#turns`; when the run's Outcome arrives it patches `#status` and
/// closes the stream. tarpc has no server-streaming, so poll-and-forward
/// is the tarpc-shaped bridge (design discussion on #105).
async fn transcript_stream(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> axum::response::sse::Sse<
    impl futures::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use datastar::prelude::{ElementPatchMode, PatchElements};

    fn status_error(msg: &str) -> Event {
        PatchElements::new(format!(
            r#"<p id="status" class="bad">stream error — {} (reload to retry)</p>"#,
            render::esc(msg)
        ))
        .write_as_axum_sse_event()
    }

    struct Poll {
        client: Option<ReadServiceClient>,
        addr: String,
        id: String,
        cursor: u64,
        truncate: Option<u64>,
        queue: std::collections::VecDeque<Event>,
        done: bool,
    }

    let full = q.get("full").is_some_and(|v| v == "1");
    let init = Poll {
        client: None,
        addr: state.read_addr.clone(),
        id,
        cursor: q.get("after").and_then(|v| v.parse().ok()).unwrap_or(0),
        truncate: if full {
            None
        } else {
            Some(fq_runtime::transcript::DEFAULT_TRUNCATE_BYTES as u64)
        },
        queue: std::collections::VecDeque::new(),
        done: false,
    };

    let stream = futures::stream::unfold(init, |mut s| async move {
        loop {
            if let Some(event) = s.queue.pop_front() {
                return Some((Ok(event), s));
            }
            if s.done {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;

            if s.client.is_none() {
                match read_service::connect(&s.addr).await {
                    Ok(c) => s.client = Some(c),
                    Err(err) => {
                        s.queue.push_back(status_error(&format!("connect: {err}")));
                        s.done = true;
                        continue;
                    }
                }
            }
            let call = s
                .client
                .as_ref()
                .expect("client dialled above")
                .transcript_since(context::current(), s.id.clone(), s.cursor, s.truncate)
                .await;
            match call {
                Ok(Ok(Some((json, next)))) => {
                    s.cursor = next;
                    let entries: Vec<fq_runtime::transcript::TranscriptEntry> =
                        serde_json::from_str(&json).unwrap_or_default();
                    // #turns is a column-reverse panel (newest-first
                    // DOM): PREPENDING in chronological order lands
                    // each newer entry at the visual bottom, and the
                    // panel's scroll stays pinned there.
                    for entry in &entries {
                        s.queue.push_back(
                            PatchElements::new(render::transcript_entry_html(entry, now_ms()))
                                .selector("#turns")
                                .mode(ElementPatchMode::Prepend)
                                .write_as_axum_sse_event(),
                        );
                    }
                    if let Some(phase) = render::transcript_outcome(&entries) {
                        s.queue.push_back(
                            PatchElements::new(render::transcript_status_html(Some(phase)))
                                .write_as_axum_sse_event(),
                        );
                        s.done = true;
                    }
                }
                // No transcript yet — keep polling; it may appear.
                Ok(Ok(None)) => {}
                Ok(Err(err)) => {
                    s.queue.push_back(status_error(&err.to_string()));
                    s.done = true;
                }
                Err(err) => {
                    s.queue.push_back(status_error(&format!("rpc: {err}")));
                    s.done = true;
                }
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn transcript_page(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Page {
    let full = q.get("full").is_some_and(|v| v == "1");
    let truncate = if full {
        None
    } else {
        Some(fq_runtime::transcript::DEFAULT_TRUNCATE_BYTES as u64)
    };
    let client = match client_or_unreachable(&state, "transcript").await {
        Ok(c) => c,
        Err(page) => return page,
    };
    match client
        .transcript(context::current(), id.clone(), truncate)
        .await
    {
        Ok(Ok(Some(json))) => {
            // The wire carries the canonical JSON shape (see
            // ReadService::transcript); decode with the shared type.
            let entries: Vec<fq_runtime::transcript::TranscriptEntry> =
                match serde_json::from_str(&json) {
                    Ok(entries) => entries,
                    Err(err) => {
                        return unreachable_page(&state, "transcript", &format!("decode: {err}"));
                    }
                };
            let title = format!("transcript {}", &id.chars().take(8).collect::<String>());
            // Best-effort: the one-line summary (#216) rides the
            // invocation detail view. A failure here must not take the
            // transcript down — the page renders without the line.
            let summary = match client.invocation(context::current(), id.clone()).await {
                Ok(Ok(Some(detail))) => detail.summary,
                _ => None,
            };
            let mut body = render::transcript(&entries, now_ms(), full, &id, summary.as_deref());
            let live = render::transcript_outcome(&entries).is_none();
            // Live runs stream: datastar opens the SSE tail from the
            // snapshot's cursor and appends turns in place — no page
            // reloads, no scroll resets. Finished runs render static.
            // No-JS browsers fall back to the <noscript> meta-refresh.
            let extra_head = if live {
                r#"<script type="module" src="/assets/datastar.js"></script>"#
            } else {
                ""
            };
            if live {
                body.push_str(&format!(
                    r#"<div data-on-load="@get('/invocations/{}/transcript/stream?after={}&full={}')"></div>"#,
                    render::esc(&id),
                    entries.len(),
                    u8::from(full),
                ));
            }
            state.last_seen_ms.store(now_ms(), Ordering::Relaxed);
            (
                StatusCode::OK,
                Html(render::page_opts(
                    &title,
                    None,
                    extra_head,
                    &with_skew_banner(&state, &body),
                )),
            )
        }
        Ok(Ok(None)) => (
            StatusCode::NOT_FOUND,
            Html(render::page(
                "transcript",
                state.refresh_secs,
                r#"<p class="muted">no transcript for that id (no dispatch rows recorded).</p>"#,
            )),
        ),
        Ok(Err(err)) => unreachable_page(&state, "transcript", &err.to_string()),
        Err(err) => unreachable_page(&state, "transcript", &format!("rpc: {err}")),
    }
}

async fn events_page(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Page {
    let client = match client_or_unreachable(&state, "events").await {
        Ok(c) => c,
        Err(page) => return page,
    };
    let agent = q.get("agent").cloned();
    let event_type = q.get("type").cloned();
    let limit = q
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(50_i64);
    match client
        .events(context::current(), agent, event_type, None, limit)
        .await
    {
        Ok(Ok(rows)) => ok_page(&state, "events", &render::events(&rows)),
        Ok(Err(err)) => unreachable_page(&state, "events", &err.to_string()),
        Err(err) => unreachable_page(&state, "events", &format!("rpc: {err}")),
    }
}

/// RFC3339 timestamp `ms` milliseconds in the past. The projection
/// stores envelope timestamps via `.to_rfc3339()`, so this form
/// string-compares correctly against its `timestamp >= ?` bound.
fn rfc3339_ago(ms: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::milliseconds(ms)).to_rfc3339()
}

async fn costs_page(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Page {
    let window = render::Window::from_query(q.get("window").map(String::as_str));
    let client = match client_or_unreachable(&state, "costs").await {
        Ok(c) => c,
        Err(page) => return page,
    };
    let since = window.since_ms().map(rfc3339_ago);
    let report = match client.costs(context::current(), None, since).await {
        Ok(Ok(report)) => report,
        Ok(Err(err)) => return unreachable_page(&state, "costs", &err.to_string()),
        Err(err) => return unreachable_page(&state, "costs", &format!("rpc: {err}")),
    };
    // The last-24h column always reads from a day-bounded report; when
    // the page window IS the day, that's the same data — skip the RPC.
    let day = if window == render::Window::Day {
        report.clone()
    } else {
        let day_since = rfc3339_ago(
            render::Window::Day
                .since_ms()
                .expect("day window is bounded"),
        );
        match client
            .costs(context::current(), None, Some(day_since))
            .await
        {
            Ok(Ok(day)) => day,
            Ok(Err(err)) => return unreachable_page(&state, "costs", &err.to_string()),
            Err(err) => return unreachable_page(&state, "costs", &format!("rpc: {err}")),
        }
    };
    ok_page(&state, "costs", &render::costs(&report, &day, window))
}

/// How many per-invocation rows the drill-down shows; the totals row
/// carries the uncapped count ("showing N of M").
const AGENT_COST_INVOCATION_LIMIT: i64 = 50;

async fn agent_costs_page(
    State(state): State<Arc<AppState>>,
    Path(agent): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Page {
    let window = render::Window::from_query(q.get("window").map(String::as_str));
    let client = match client_or_unreachable(&state, "costs").await {
        Ok(c) => c,
        Err(page) => return page,
    };
    let since = window.since_ms().map(rfc3339_ago);
    match client
        .agent_costs(
            context::current(),
            agent.clone(),
            since,
            AGENT_COST_INVOCATION_LIMIT,
        )
        .await
    {
        Ok(Ok(Some(detail))) => ok_page(
            &state,
            &format!("costs · {agent}"),
            &render::agent_costs(&detail, window, now_ms()),
        ),
        Ok(Ok(None)) => (
            StatusCode::NOT_FOUND,
            Html(render::live_page(
                "costs",
                state.refresh_secs,
                &format!(
                    r#"<p class="muted">no cost events for that agent (in this window). <a href="/costs">← all agents</a></p>{}"#,
                    updated_line(now_ms())
                ),
            )),
        ),
        Ok(Err(err)) => unreachable_page(&state, "costs", &err.to_string()),
        Err(err) => unreachable_page(&state, "costs", &format!("rpc: {err}")),
    }
}

async fn agents_page(State(state): State<Arc<AppState>>) -> Page {
    let client = match client_or_unreachable(&state, "agents").await {
        Ok(c) => c,
        Err(page) => return page,
    };
    match client.agents(context::current()).await {
        Ok(Ok(view)) => ok_page(&state, "agents", &render::agents(&view)),
        Ok(Err(err)) => unreachable_page(&state, "agents", &err.to_string()),
        Err(err) => unreachable_page(&state, "agents", &format!("rpc: {err}")),
    }
}

async fn agent_page(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Page {
    let client = match client_or_unreachable(&state, "agent").await {
        Ok(c) => c,
        Err(page) => return page,
    };
    match client.agent(context::current(), id.clone()).await {
        Ok(Ok(Some(detail))) => ok_page(
            &state,
            &format!("agent · {id}"),
            &render::agent_detail(&detail),
        ),
        Ok(Ok(None)) => (
            StatusCode::NOT_FOUND,
            Html(render::live_page(
                "agent",
                state.refresh_secs,
                &format!(
                    r#"<p class="muted">no agent with that id in the registry. <a href="/agents">← all agents</a></p>{}"#,
                    updated_line(now_ms())
                ),
            )),
        ),
        Ok(Err(err)) => unreachable_page(&state, "agent", &err.to_string()),
        Err(err) => unreachable_page(&state, "agent", &format!("rpc: {err}")),
    }
}

/// Datastar content negotiation: the vendored client stamps a
/// `Datastar-Request` header on every `@get`, so the same URL serves
/// two representations — a full HTML page for navigations, and for
/// poll ticks a single-event SSE patch that morphs the `#main` region
/// in place (mode `inner`, so the region's own `data-on-interval`
/// attribute is never touched). Same handlers, same render path, same
/// bytes; the reload disappears, so open folds, scroll position, and
/// text selection survive the tick.
///
/// Pass-through cases: requests without the header; non-HTML responses
/// (the transcript's own SSE stream, the vendored asset); and any HTML
/// page without a live region (the transcript page).
async fn datastar_negotiation(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let is_datastar = req.headers().contains_key("datastar-request");
    let resp = next.run(req).await;
    if !is_datastar {
        return resp;
    }
    let is_html = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html"));
    if !is_html {
        return resp;
    }

    let (parts, body) = resp.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(_) => return axum::response::Response::from_parts(parts, axum::body::Body::empty()),
    };
    let html = String::from_utf8_lossy(&bytes);
    let Some(inner) = extract_main_inner(&html) else {
        // No live region — hand back the full page unchanged.
        return axum::response::Response::from_parts(parts, axum::body::Body::from(bytes));
    };

    use datastar::prelude::{ElementPatchMode, PatchElements};
    let event = PatchElements::new(inner)
        .selector("#main")
        .mode(ElementPatchMode::Inner)
        .write_as_axum_sse_event();
    axum::response::sse::Sse::new(futures::stream::once(async move {
        Ok::<_, std::convert::Infallible>(event)
    }))
    .into_response()
}

/// The inner HTML of the `#main` live region. The shell is our own
/// deterministic template ([`render::live_page`]): exactly one
/// `<div id="main" …>` whose closing `</div>` is the document's last —
/// nothing but `</body></html>` follows it.
fn extract_main_inner(html: &str) -> Option<String> {
    let start = html.find(r#"<div id="main""#)?;
    let open_end = start + html[start..].find('>')? + 1;
    let end = html.rfind("</div>")?;
    (end >= open_end).then(|| html[open_end..end].to_string())
}

/// Build the router — separated from `main` so tests drive it with
/// `tower::ServiceExt::oneshot`.
fn app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(health_page))
        .route("/invocations", get(invocations_page))
        .route("/invocations/{id}", get(invocation_page))
        .route("/invocations/{id}/transcript", get(transcript_page))
        .route(
            "/invocations/{id}/transcript/stream",
            get(transcript_stream),
        )
        .route("/assets/datastar.js", get(datastar_js))
        .route("/events", get(events_page))
        .route("/costs", get(costs_page))
        .route("/costs/{agent}", get(agent_costs_page))
        .route("/agents", get(agents_page))
        .route("/agents/{id}", get(agent_page))
        .layer(axum::middleware::from_fn(datastar_negotiation))
        .with_state(state)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    if let Some(Command::RenderFixtures { out }) = args.command {
        for name in fixtures::write_all(&out)? {
            println!("{}", out.join(format!("{name}.html")).display());
        }
        return Ok(());
    }

    // Same posture as the read service it fronts: never off-box.
    let bind: std::net::SocketAddr = args
        .bind
        .parse()
        .with_context(|| format!("invalid bind address `{}`", args.bind))?;
    anyhow::ensure!(
        bind.ip().is_loopback(),
        "dashboard bind `{}` is not loopback — serve on localhost and reach it via an SSH tunnel",
        args.bind
    );

    let state = Arc::new(AppState {
        read_addr: args.read_service.clone(),
        refresh_secs: args.refresh,
        last_seen_ms: AtomicI64::new(0),
        daemon_version: std::sync::Mutex::new(None),
    });

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind {bind}"))?;
    tracing::info!(
        "fq-dashboard serving http://{bind} over read service {}",
        args.read_service
    );
    axum::serve(listener, app(state)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use fq_runtime::control_plane::projection::ProjectionStore;
    use fq_runtime::control_plane::store::ControlPlaneStore;
    use fq_runtime::views::Views;
    use fq_runtime::worker::store::WorkerStore;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn body_string(resp: axum::response::Response) -> String {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn state_for(addr: &str) -> Arc<AppState> {
        Arc::new(AppState {
            read_addr: addr.to_string(),
            refresh_secs: 5,
            last_seen_ms: AtomicI64::new(0),
            daemon_version: std::sync::Mutex::new(None),
        })
    }

    /// Spin a real read service over a seeded temp DB; drive the router
    /// end to end with oneshot requests — the BFF's full path minus a
    /// real browser.
    #[tokio::test]
    async fn pages_render_against_a_live_read_service() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let cp = ControlPlaneStore::open(&path).await.unwrap();
            cp.register_worker("w1", "localhost", 100).await.unwrap();
            let _ws = WorkerStore::open(&path).await.unwrap();
            let _proj = ProjectionStore::open(&path).await.unwrap();
        }
        let views = Arc::new(Views::open(&path).await.unwrap());
        let js = async_nats_lazy().await;
        // A registry with one real parsed definition, so the agents
        // pages exercise the full wire path.
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("probe.md"),
            "---\nname: probe\nmodel: claude-haiku-4-5\ntools:\n  - exec\n---\n\nYou are a probe.\n",
        )
        .unwrap();
        let registry = fq_runtime::AgentRegistry::load_from_directory(&agents_dir, None).unwrap();
        let registry = Arc::new(tokio::sync::RwLock::new(Arc::new(registry)));
        // A version whose SHA half matches this binary's: the matched-
        // builds case — the skew banner must NOT appear anywhere below.
        let (addr, serving) = read_service::bind(
            "127.0.0.1:0",
            views,
            js,
            std::time::Duration::from_millis(100),
            format!("0.1.0+{OWN_SHA}"),
            registry,
        )
        .await
        .unwrap();
        tokio::spawn(serving);

        let app = app(state_for(&addr.to_string()));

        // The health page reaches the probe (lazy client → per-stream
        // Unavailable after the 100ms timeout) and the DB counts.
        let resp = app
            .clone()
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let html = body_string(resp).await;
        assert!(html.contains(OWN_SHA), "version rendered: {html}");
        assert!(html.contains("reachable"));
        assert!(
            !html.contains("build skew"),
            "matched builds must not banner: {html}"
        );

        let resp = app
            .clone()
            .oneshot(Request::get("/invocations").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let html = body_string(resp).await;
        assert!(html.contains("no invocations"));
        // Nothing in flight → the active section must not exist at all.
        assert!(!html.contains("Active now"), "got: {html}");

        let resp = app
            .clone()
            .oneshot(
                Request::get("/invocations/no-such-id")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Transcript of an unknown id: 404 through the wire's None path.
        let resp = app
            .clone()
            .oneshot(
                Request::get("/invocations/no-such-id/transcript")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let resp = app
            .clone()
            .oneshot(Request::get("/costs").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(body_string(resp).await.contains("no cost events"));

        // A bounded window renders the same empty page plus the
        // selector (two RPCs collapse to one on the day window).
        let resp = app
            .clone()
            .oneshot(
                Request::get("/costs?window=24h")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let html = body_string(resp).await;
        assert!(html.contains("no cost events"), "got: {html}");
        assert!(html.contains("<b>24h</b>"), "got: {html}");

        // The drill-down 404s through the wire's None path for an
        // agent with no cost events.
        let resp = app
            .clone()
            .oneshot(
                Request::get("/costs/no-such-agent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // The agents list serves the registry over the wire, and each
        // definition links to its detail page.
        let resp = app
            .clone()
            .oneshot(Request::get("/agents").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let html = body_string(resp).await;
        assert!(
            html.contains(r#"<a href="/agents/probe">probe</a>"#),
            "got: {html}"
        );

        // The detail page carries the collapsed system prompt.
        let resp = app
            .clone()
            .oneshot(Request::get("/agents/probe").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let html = body_string(resp).await;
        assert!(
            html.contains(
                r#"<details id="system-prompt" data-preserve-attr="open"><summary>system prompt ("#
            ),
            "got: {html}"
        );
        assert!(html.contains("You are a probe."), "got: {html}");

        // Unknown agent: 404 through the wire's None path.
        let resp = app
            .clone()
            .oneshot(
                Request::get("/agents/no-such-agent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let resp = app
            .clone()
            .oneshot(Request::get("/events").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Datastar negotiation: the same URL with the header the
        // vendored client stamps returns a single-event SSE morph of
        // #main — same content, no page shell.
        let resp = app
            .clone()
            .oneshot(
                Request::get("/")
                    .header("Datastar-Request", "true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(ct.starts_with("text/event-stream"), "got: {ct}");
        let sse = body_string(resp).await;
        assert!(sse.contains("datastar-patch-elements"), "got: {sse}");
        assert!(sse.contains("selector #main"), "got: {sse}");
        assert!(sse.contains("mode inner"), "got: {sse}");
        assert!(sse.contains("reachable"), "got: {sse}");
        assert!(sse.contains("updated "), "freshness line: {sse}");
        assert!(
            !sse.contains("<!doctype html>"),
            "fragment must not carry the shell: {sse}"
        );

        // The full page carries the poll wiring and the no-JS fallback.
        let resp = app
            .clone()
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let html = body_string(resp).await;
        assert!(
            html.contains("data-on-interval__duration.5s"),
            "got: {html}"
        );
        assert!(html.contains("/assets/datastar.js"), "got: {html}");
        assert!(html.contains("<noscript>"), "got: {html}");

        // Non-HTML responses pass through untouched even with the header.
        let resp = app
            .oneshot(
                Request::get("/assets/datastar.js")
                    .header("Datastar-Request", "true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let ct = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(ct.starts_with("text/javascript"), "got: {ct}");
    }

    /// The crash-domain contract: with no read service listening, every
    /// page renders the unreachable banner as a 503 — never a panic,
    /// never a broken page.
    #[tokio::test]
    async fn unreachable_runtime_renders_banner() {
        // Bind-then-drop to get a port with nothing listening.
        let dead = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().to_string()
        };
        let app = app(state_for(&dead));
        let resp = app
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let html = body_string(resp).await;
        assert!(html.contains("runtime unreachable"), "got: {html}");
        assert!(html.contains("never seen"), "got: {html}");

        // A poll tick during an outage morphs the unreachable body in —
        // the page goes loud within one interval instead of freezing.
        let resp = super::app(state_for(&dead))
            .oneshot(
                Request::get("/")
                    .header("Datastar-Request", "true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let sse = body_string(resp).await;
        assert!(sse.contains("datastar-patch-elements"), "got: {sse}");
        assert!(sse.contains("runtime unreachable"), "got: {sse}");
        // No skew has ever been observed — the page must not claim any
        // (#168: unknown is not mismatch).
        assert!(!html.contains("build skew"), "got: {html}");
    }

    /// Build skew (#168): a daemon from a different build trips the
    /// banner naming both SHAs — but the page still renders whatever
    /// decoded (warn-and-continue), and the unreachable page names
    /// skew as the likely cause once observed.
    #[tokio::test]
    async fn mismatched_builds_banner_but_still_render() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let _cp = ControlPlaneStore::open(&path).await.unwrap();
            let _ws = WorkerStore::open(&path).await.unwrap();
            let _proj = ProjectionStore::open(&path).await.unwrap();
        }
        let views = Arc::new(Views::open(&path).await.unwrap());
        let js = async_nats_lazy().await;
        let registry = Arc::new(tokio::sync::RwLock::new(Arc::new(
            fq_runtime::AgentRegistry::new(),
        )));
        let (addr, serving) = read_service::bind(
            "127.0.0.1:0",
            views,
            js,
            std::time::Duration::from_millis(100),
            "0.9.9+deadbeefcafe".to_string(),
            registry,
        )
        .await
        .unwrap();
        tokio::spawn(serving);

        let state = state_for(&addr.to_string());
        let app = app(state.clone());

        let resp = app
            .clone()
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        // Warn-and-continue: the page is a 200 with data AND the banner.
        assert_eq!(resp.status(), StatusCode::OK);
        let html = body_string(resp).await;
        assert!(html.contains("build skew"), "banner missing: {html}");
        assert!(html.contains("deadbeefcafe"), "daemon sha named: {html}");
        assert!(html.contains(OWN_SHA), "own sha named: {html}");
        assert!(html.contains("reachable"), "data still rendered: {html}");

        // With skew observed, an RPC failure names the likely cause
        // instead of a bare "runtime unreachable" (the #154 incident).
        let page = unreachable_page(&state, "health", "rpc: decode failed");
        assert!(
            page.1.0.contains("wire mismatch from build skew"),
            "got: {}",
            page.1.0
        );
    }

    #[test]
    fn extract_main_inner_finds_the_live_region() {
        let html = render::live_page("t", 5, "<p>hello</p><details open></details>");
        assert_eq!(
            extract_main_inner(&html).as_deref(),
            Some("<p>hello</p><details open></details>")
        );
        // Pages without a live region (the transcript) pass through.
        assert_eq!(extract_main_inner("<html><body>x</body></html>"), None);
    }

    #[test]
    fn sha_suffix_takes_the_suffix_or_the_whole() {
        assert_eq!(sha_suffix("0.1.0+abc123"), "abc123");
        assert_eq!(sha_suffix("bare-sha"), "bare-sha");
        assert_eq!(sha_suffix("1.0+with+plus"), "plus");
    }

    /// A jetstream context over a lazily-connecting client — the probe
    /// target is never dialled successfully in these tests; health falls
    /// back to Unavailable rows via the probe timeout.
    async fn async_nats_lazy() -> async_nats::jetstream::Context {
        async_nats::jetstream::new(
            async_nats::connect_with_options(
                "nats://127.0.0.1:1",
                async_nats::ConnectOptions::new().retry_on_initial_connect(),
            )
            .await
            .expect("lazy client"),
        )
    }
}
