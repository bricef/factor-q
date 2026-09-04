//! `fq-dashboard` — the operator dashboard: a standalone BFF binary
//! with its own crash domain (#105 layer 3). It holds only a client to
//! the daemon and an HTTP server; it cannot touch runtime state and
//! cannot take the runtime down. If the daemon is unreachable it
//! renders "runtime unreachable, last seen Ns ago" rather than
//! breaking; if this process dies, the daemon never notices.
//!
//! **It reads over the authenticated edge, as a second principal.**
//! Every page invokes declared operations with a token
//! attenuated to six read grants — `agent`, `control`, `cost`,
//! `event`, `invocation`, `turn` — so a compromised dashboard can read
//! exactly what it renders and command nothing. The token is minted
//! offline from the admin token (`fq token attenuate`) and reaches
//! this process through its environment; the daemon's certificate
//! fingerprint is pinned the same way. Both are required: with no
//! token there is nothing to fail closed *to*, so startup refuses
//! rather than running unauthenticated.
//!
//! Deliberately naive (v0, per the plan): each browser request dials
//! the daemon fresh (localhost TCP — microseconds, and it doubles
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

use std::sync::Arc;
use std::sync::atomic::AtomicI64;

use anyhow::Context as _;
use axum::Router;
use axum::routing::get;
use clap::Parser;

mod fixtures;
mod pages;
mod render;

use pages::{
    agent_costs_page, agent_page, agents_page, costs_page, datastar_js, events_page, health_page,
    invocation_page, invocations_page, transcript_page, transcript_stream,
};

/// This build's git SHA (stamped by build.rs). Compared against the
/// daemon's build, which `control.status` carries, and printed by
/// `--version` as `fq-dashboard <sha>` (watcher-style) so deploy.sh
/// can verify bundle coherence.
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
    /// Address of the daemon's authenticated edge (`[edge] bind` in
    /// the daemon's `fqd.toml`). Note that its default is the same port
    /// this process serves on, so a host running both must move one of
    /// them.
    #[arg(long, default_value = "127.0.0.1:9472", env = "FQ_EDGE")]
    edge: String,
    /// The dashboard's capability token — an attenuated admin token
    /// granting only the six reads it renders. Mint with
    /// `fq token attenuate` (see the startup refusal for the exact
    /// invocation). Required.
    #[arg(long, env = "FQ_EDGE_TOKEN")]
    edge_token: Option<String>,
    /// SHA-256 of the daemon's certificate, hex — the pin. The daemon
    /// prints it when it provisions its identity and keeps it in
    /// `<state>/edge/fingerprint`. Required.
    #[arg(long, env = "FQ_EDGE_FINGERPRINT")]
    edge_fingerprint: Option<String>,
    /// Browser auto-refresh interval, in seconds.
    #[arg(long, default_value_t = 5, env = "FQ_DASHBOARD_REFRESH")]
    refresh: u64,
}

/// The grants the dashboard's token must carry — one per domain it
/// reads, verb `read`, and nothing else. Named here because the
/// startup refusal prints the `fq token attenuate` line that mints
/// them, and an operator reading that line should be able to check it
/// against the pages it is for:
///
/// | grant | pages |
/// |---|---|
/// | `read:agent` | `/agents`, `/agents/{id}` |
/// | `read:control` | `/` (health) |
/// | `read:cost` | `/costs`, `/costs/{agent}` |
/// | `read:event` | `/events` |
/// | `read:invocation` | `/invocations`, `/invocations/{id}` |
/// | `read:turn` | the transcript page and its live tail |
///
/// Deliberately not `read:*`, which would additionally grant `worker`,
/// `dead_letter`, `trigger` and `operation` — four domains no page
/// here renders.
const REQUIRED_GRANTS: &[&str] = &[
    "read:agent",
    "read:control",
    "read:cost",
    "read:event",
    "read:invocation",
    "read:turn",
];

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
/// so this is the two target addresses, the credentials the edge one
/// needs, the refresh knob, and the last-successful-read timestamp for
/// the unreachable banner.
pub(crate) struct AppState {
    pub(crate) edge_addr: String,
    pub(crate) edge_fingerprint: [u8; 32],
    pub(crate) edge_token: String,
    pub(crate) refresh_secs: u64,
    /// Epoch ms of the last successful read; 0 = never.
    pub(crate) last_seen_ms: AtomicI64,
    /// The daemon's version string as last observed on `control.status`
    /// (#168). `None` until the health page has been served once — no
    /// other page asks for it, because no other page needs the rest of
    /// that report and it is not free (a JetStream probe, a row count
    /// and a recovery fold). Kept across connect failures: "last
    /// observed" is honest context for the unreachable page.
    pub(crate) daemon_version: std::sync::Mutex<Option<String>>,
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
pub(crate) fn skew(state: &AppState) -> Option<(String, String)> {
    let guard = state.daemon_version.lock().expect("daemon_version lock");
    let daemon_sha = sha_suffix(guard.as_deref()?).to_string();
    (daemon_sha != OWN_SHA).then(|| (OWN_SHA.to_string(), daemon_sha))
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

/// What an operator has to run to mint the token this process needs.
/// Printed by the startup refusal, so the fix is a copy-paste rather
/// than a documentation hunt.
fn mint_instructions(edge: &str) -> String {
    let grants = REQUIRED_GRANTS
        .iter()
        .map(|g| format!("--grant {g}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "mint one from the admin token, offline:\n  \
         fq token attenuate --addr {edge} {grants}\n\
         The result authorises reads on exactly the domains this dashboard renders \
         and nothing else — do not pass the admin token itself."
    )
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

    // Same posture as the daemon it fronts: never off-box.
    let bind: std::net::SocketAddr = args
        .bind
        .parse()
        .with_context(|| format!("invalid bind address `{}`", args.bind))?;
    anyhow::ensure!(
        bind.ip().is_loopback(),
        "dashboard bind `{}` is not loopback — serve on localhost and reach it via an SSH tunnel",
        args.bind
    );

    // Fail closed. There is no unauthenticated mode to fall back to —
    // the edge refuses a connection without a token — so a missing
    // credential is a startup error naming the fix, not a process that
    // runs and renders "unreachable" on every page.
    let edge_token = args
        .edge_token
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no edge token: set FQ_EDGE_TOKEN (or pass --edge-token).\n{}",
                mint_instructions(&args.edge)
            )
        })?;
    let edge_fingerprint = args
        .edge_fingerprint
        .filter(|f| !f.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no edge fingerprint: set FQ_EDGE_FINGERPRINT (or pass --edge-fingerprint).\n\
                 It is the SHA-256 of the daemon's certificate, which the daemon printed \
                 when it provisioned its identity (the `edge: certificate fingerprint` line) \
                 and keeps in <state>/edge/fingerprint."
            )
        })?;
    let edge_fingerprint = fq_edge::parse_fingerprint_hex(&edge_fingerprint)
        .context("FQ_EDGE_FINGERPRINT is not a certificate fingerprint")?;

    let state = Arc::new(AppState {
        edge_addr: args.edge.clone(),
        edge_fingerprint,
        edge_token,
        refresh_secs: args.refresh,
        last_seen_ms: AtomicI64::new(0),
        daemon_version: std::sync::Mutex::new(None),
    });

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind {bind}"))?;
    tracing::info!(
        "fq-dashboard serving http://{bind} over the edge at {}",
        args.edge
    );
    axum::serve(listener, app(state)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::http::StatusCode;
    use fq_edge::EdgeRegistry;
    use fq_edge::auth::EdgeIdentity;
    use fq_edge::wire::WireError;
    use fq_ops::Domain;
    use fq_ops::agent_view::{AgentDetailView, AgentEntryView, AgentSummaryView};
    use fq_ops::surface::{
        AgentListFilter, AgentViewKey, CostByAgentParams, CostSummaryParams, DoctorReport,
        EventFilter, InvocationListFilter, InvocationViewKey, StatusReport, TurnFilter,
    };
    use fq_ops::views::{
        ActiveInvocationView, AgentCostDetailView, CostReport, EventView, InvocationDetailView,
        InvocationSummaryView,
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::pages::unreachable_page;

    async fn body_string(resp: axum::response::Response) -> String {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// One loaded agent, so the agents pages exercise the full wire
    /// path: list row, detail with a system prompt.
    /// Keys for the two atoms the dashboard lists but never Gets.
    /// The real ones are still private to `fq-cli` — one consumer, no
    /// shared definition — and a declaration needs *a* key type, so
    /// these stand in for the half of the surface this test does not
    /// exercise.
    #[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
    struct EventKey {
        event_id: String,
    }

    #[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
    struct TurnKey {
        seq: u64,
    }

    fn probe_summary() -> AgentSummaryView {
        AgentSummaryView {
            agent_id: "probe".to_string(),
            model: "claude-haiku-4-5".to_string(),
            budget: None,
            trigger: None,
            tool_count: 1,
            prompt_bytes: 17,
            path: "agents/probe.md".to_string(),
        }
    }

    fn probe_detail() -> AgentDetailView {
        AgentDetailView {
            agent_id: "probe".to_string(),
            model: "claude-haiku-4-5".to_string(),
            system_prompt: "You are a probe.".to_string(),
            tools: vec!["exec".to_string()],
            mcp_servers: Vec::new(),
            budget: None,
            max_iterations: None,
            effort: None,
            trigger: None,
            path: "agents/probe.md".to_string(),
        }
    }

    /// The declared surface the dashboard actually calls, bound to
    /// handlers that answer what an idle daemon answers: empty
    /// listings, one loaded agent, and a `control.status` naming this
    /// build.
    ///
    /// The **real** declared shapes are on both sides of this wire —
    /// the filter the dashboard serialises is deserialised here by the
    /// same struct — so the test fails if a field is renamed on one
    /// side. What it does not re-implement is the daemon's handlers;
    /// those have their own suites (`golden.rs`, `operator_surface.rs`)
    /// and reproducing them here would be a second copy free to drift
    /// from the first.
    fn fixture_registry(version: &str) -> EdgeRegistry {
        use fq_ops::surface::{ActiveParams, DoctorParams, StatusParams};
        use fq_ops::turn::TurnState;

        let mut registry = EdgeRegistry::new();
        registry
            .view::<InvocationViewKey, InvocationDetailView, InvocationSummaryView, InvocationListFilter, _, _, _, _>(
                fq_ops::View::new::<InvocationViewKey, InvocationDetailView, InvocationSummaryView, InvocationListFilter>(
                    Domain::Invocation, "invocations", fq_ops::Stability::Experimental),
                |key: InvocationViewKey| async move {
                    Err::<InvocationDetailView, _>(WireError::NotFound {
                        op: "invocation.get".into(),
                        message: format!("no invocation `{}`", key.invocation_id),
                    })
                },
                |_filter: InvocationListFilter| async move {
                    Ok(Vec::<InvocationSummaryView>::new())
                },
            )
            .expect("register invocation view");
        registry
            .view::<AgentViewKey, AgentDetailView, AgentEntryView, AgentListFilter, _, _, _, _>(
                fq_ops::View::new::<AgentViewKey, AgentDetailView, AgentEntryView, AgentListFilter>(
                    Domain::Agent,
                    "agents",
                    fq_ops::Stability::Experimental,
                ),
                |key: AgentViewKey| async move {
                    if key.agent_id == "probe" {
                        Ok(probe_detail())
                    } else {
                        Err(WireError::NotFound {
                            op: "agent.get".into(),
                            message: format!("no agent `{}`", key.agent_id),
                        })
                    }
                },
                |_filter: AgentListFilter| async move {
                    Ok(vec![AgentEntryView::Agent(probe_summary())])
                },
            )
            .expect("register agent view");
        registry
            .atom::<EventKey, EventView, EventView, EventFilter, _, _, _, _, _, _>(
                fq_ops::Atom::new::<EventKey, EventView, EventFilter>(
                    Domain::Event,
                    "events",
                    fq_ops::Stability::Experimental,
                ),
                |_key| async move {
                    Err::<EventView, _>(WireError::NotFound {
                        op: "event.get".into(),
                        message: "no event".into(),
                    })
                },
                |_filter: EventFilter| async move { Ok(Vec::<EventView>::new()) },
                |_filter: EventFilter, _from, _wait| async move {
                    Ok(fq_edge::StreamBatch {
                        items: Vec::new(),
                        next_from_seq: 0,
                    })
                },
            )
            .expect("register event atom");
        registry
            .atom::<TurnKey, TurnState, TurnState, TurnFilter, _, _, _, _, _, _>(
                fq_ops::Atom::new::<TurnKey, TurnState, TurnFilter>(
                    Domain::Turn,
                    "turns",
                    fq_ops::Stability::Experimental,
                ),
                |_key| async move {
                    Err::<TurnState, _>(WireError::NotFound {
                        op: "turn.get".into(),
                        message: "no turn".into(),
                    })
                },
                |_filter: TurnFilter| async move { Ok(Vec::<TurnState>::new()) },
                |_filter: TurnFilter, _from, _wait| async move {
                    Ok(fq_edge::StreamBatch {
                        items: Vec::new(),
                        next_from_seq: 0,
                    })
                },
            )
            .expect("register turn atom");
        registry
            .report::<CostSummaryParams, CostReport, _, _>(
                fq_ops::Report::new::<CostSummaryParams, CostReport>(
                    fq_ops::CostReport::Summary,
                    "fleet spend",
                    fq_ops::Stability::Experimental,
                ),
                |_params: CostSummaryParams| async move { Ok(CostReport::default()) },
            )
            .expect("register cost.summary");
        registry
            .report::<CostByAgentParams, AgentCostDetailView, _, _>(
                fq_ops::Report::new::<CostByAgentParams, AgentCostDetailView>(
                    fq_ops::CostReport::ByAgent,
                    "one agent's spend",
                    fq_ops::Stability::Experimental,
                ),
                |params: CostByAgentParams| async move {
                    Err::<AgentCostDetailView, _>(WireError::NotFound {
                        op: "cost.by_agent".into(),
                        message: format!("no cost events for `{}`", params.agent),
                    })
                },
            )
            .expect("register cost.by_agent");
        let version = version.to_string();
        registry
            .report::<StatusParams, StatusReport, _, _>(
                fq_ops::Report::new::<StatusParams, StatusReport>(
                    fq_ops::ControlReport::Status,
                    "machinery state",
                    fq_ops::Stability::Experimental,
                ),
                move |_params: StatusParams| {
                    let version = version.clone();
                    async move {
                        let mut report = crate::fixtures::status_report();
                        report.version = version;
                        Ok(report)
                    }
                },
            )
            .expect("register control.status");
        registry
            .report::<ActiveParams, Vec<ActiveInvocationView>, _, _>(
                fq_ops::Report::new::<ActiveParams, Vec<ActiveInvocationView>>(
                    fq_ops::InvocationReport::Active,
                    "live work",
                    fq_ops::Stability::Experimental,
                ),
                |_params: ActiveParams| async move { Ok(crate::fixtures::active_rows()) },
            )
            .expect("register invocation.active");
        registry
            .report::<DoctorParams, DoctorReport, _, _>(
                fq_ops::Report::new::<DoctorParams, DoctorReport>(
                    fq_ops::ControlReport::Doctor,
                    "health composite",
                    fq_ops::Stability::Experimental,
                ),
                |_params: DoctorParams| async move { Ok(crate::fixtures::doctor_report()) },
            )
            .expect("register control.doctor");
        registry
    }

    struct TestEdge {
        addr: String,
        fingerprint: [u8; 32],
        token: String,
    }

    /// Serve the fixture surface, handing back a token attenuated to
    /// exactly [`REQUIRED_GRANTS`] — the credential the deployed
    /// dashboard holds, not the admin token it was minted from. Every
    /// page below therefore proves its grant is sufficient, and
    /// [`a_token_missing_a_grant_is_denied`] proves the set is not
    /// merely generous.
    async fn spawn_edge(version: &str) -> TestEdge {
        spawn_edge_with(version, REQUIRED_GRANTS).await
    }

    async fn spawn_edge_with(version: &str, grants: &[&str]) -> TestEdge {
        let identity = EdgeIdentity::provision().unwrap();
        let fingerprint = identity.fingerprint();
        let admin = identity.mint_admin_token().unwrap();
        let grants: Vec<(String, String)> = grants
            .iter()
            .map(|g| {
                let (verb, domain) = g.split_once(':').expect("grant is verb:domain");
                (verb.to_string(), domain.to_string())
            })
            .collect();
        let token = fq_edge::attenuate(&admin, &grants).unwrap();
        let registry = Arc::new(fixture_registry(version));
        let (addr, serving) = fq_edge::bind("127.0.0.1:0", &identity, registry)
            .await
            .unwrap();
        tokio::spawn(serving);
        TestEdge {
            addr: addr.to_string(),
            fingerprint,
            token,
        }
    }

    /// A port with nothing listening — the unreachable case.
    fn dead_addr() -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().to_string()
    }

    fn state_for(edge: &TestEdge) -> Arc<AppState> {
        Arc::new(AppState {
            edge_addr: edge.addr.clone(),
            edge_fingerprint: edge.fingerprint,
            edge_token: edge.token.clone(),
            refresh_secs: 5,
            last_seen_ms: AtomicI64::new(0),
            daemon_version: std::sync::Mutex::new(None),
        })
    }

    /// Spin a real edge and drive the router end to end with oneshot
    /// requests — the BFF's full path minus a real browser, now over
    /// the authenticated surface and an attenuated token.
    #[tokio::test]
    async fn pages_render_against_a_live_edge() {
        let edge = spawn_edge(&format!("0.1.0+{OWN_SHA}")).await;
        let app = app(state_for(&edge));

        // The health page composes both Control reports.
        let resp = app
            .clone()
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let html = body_string(resp).await;
        assert!(html.contains(OWN_SHA), "version rendered: {html}");
        assert!(html.contains("reachable"));
        // `projection_rows` lands where `event_count` used to.
        assert!(
            html.contains("64016") || html.contains("64,016"),
            "got: {html}"
        );
        // …and the doctor's half of the page arrived with it.
        assert!(html.contains("2 in-flight (1 working"), "got: {html}");
        assert!(
            !html.contains("build skew"),
            "matched builds must not banner: {html}"
        );

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

        // Transcript of an unknown id: 404 through the empty turn list.
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
        // selector (two calls collapse to one on the day window).
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

        // The drill-down 404s through the wire's NotFound path.
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

        // Unknown agent: 404 through the wire's NotFound path.
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

    /// The invocations page carries both reads — the live report above
    /// the folded listing — over one connection and one grant.
    #[tokio::test]
    async fn the_invocations_page_carries_the_live_table_and_the_listing() {
        let edge = spawn_edge(&format!("0.1.0+{OWN_SHA}")).await;
        let resp = app(state_for(&edge))
            .oneshot(Request::get("/invocations").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let html = body_string(resp).await;
        // The report answered with a live row, so the section exists…
        assert!(html.contains("Active now"), "got: {html}");
        assert!(html.contains("019f5b3f"), "live row rendered: {html}");
        // …and the empty listing below it still says so.
        assert!(html.contains("no invocations"), "got: {html}");
    }

    /// The attenuation is load-bearing, not decorative: drop one grant
    /// and the page it serves is refused by the daemon. The dashboard
    /// still renders — a denial is a failure to read, not a crash.
    #[tokio::test]
    async fn a_token_missing_a_grant_is_denied() {
        let grants: Vec<&str> = REQUIRED_GRANTS
            .iter()
            .copied()
            .filter(|g| *g != "read:cost")
            .collect();
        let edge = spawn_edge_with(&format!("0.1.0+{OWN_SHA}"), &grants).await;
        let app = app(state_for(&edge));

        let resp = app
            .clone()
            .oneshot(Request::get("/costs").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let html = body_string(resp).await;
        assert!(html.contains("denied"), "the refusal is shown: {html}");

        // …and the grants it does hold still work, so this is a
        // per-operation check rather than a broken connection.
        let resp = app
            .oneshot(Request::get("/agents").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// The crash-domain contract: with no daemon listening, every page
    /// renders the unreachable banner as a 503 — never a panic, never
    /// a broken page.
    #[tokio::test]
    async fn unreachable_runtime_renders_banner() {
        let dead = dead_addr();
        let edge = TestEdge {
            addr: dead.clone(),
            fingerprint: [0u8; 32],
            token: "not-a-token".to_string(),
        };
        let resp = app(state_for(&edge))
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let html = body_string(resp).await;
        assert!(html.contains("runtime unreachable"), "got: {html}");
        assert!(html.contains("never seen"), "got: {html}");
        // No skew has ever been observed — the page must not claim any
        // (#168: unknown is not mismatch).
        assert!(!html.contains("build skew"), "got: {html}");

        // A poll tick during an outage morphs the unreachable body in —
        // the page goes loud within one interval instead of freezing.
        let resp = app(state_for(&edge))
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
    }

    /// Build skew (#168): a daemon from a different build trips the
    /// banner naming both SHAs — but the page still renders whatever
    /// decoded (warn-and-continue), and the unreachable page names
    /// skew as the likely cause once observed.
    #[tokio::test]
    async fn mismatched_builds_banner_but_still_render() {
        let edge = spawn_edge("0.9.9+deadbeefcafe").await;
        let state = state_for(&edge);
        let app = app(state.clone());

        let resp = app
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

        // With skew observed, a read failure names the likely cause
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

    /// The mint line the startup refusal prints must name every grant
    /// the pages need — an operator who copies it gets a token that
    /// works, or this test is what tells us it would not have.
    #[test]
    fn the_refusal_names_every_grant_it_needs() {
        let text = mint_instructions("127.0.0.1:9472");
        for grant in REQUIRED_GRANTS {
            assert!(text.contains(grant), "{grant} missing from: {text}");
        }
        assert!(text.contains("fq token attenuate"), "got: {text}");
    }
}
