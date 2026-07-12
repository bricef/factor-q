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
//! as reconnect logic), renders static HTML, and the browser refreshes
//! via `<meta refresh>`. Zero client-side JS, zero framework, zero CORS
//! (the browser only ever talks to this process). Localhost-only: the
//! operator reaches it via SSH tunnel, and the bind refuses anything
//! else.

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

mod render;

#[derive(Parser)]
#[command(
    name = "fq-dashboard",
    about = "factor-q operator dashboard (read-only)"
)]
struct Args {
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

/// Shared per-process state. No connection is held — see module doc —
/// so this is just the target address, the refresh knob, and the
/// last-successful-read timestamp for the unreachable banner.
struct AppState {
    read_addr: String,
    refresh_secs: u64,
    /// Epoch ms of the last successful RPC; 0 = never.
    last_seen_ms: AtomicI64,
}

/// `std`-only epoch-ms clock (the dashboard has no chrono dependency).
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

type Page = (StatusCode, Html<String>);

/// Dial the read service, or produce the unreachable page.
async fn client_or_unreachable(state: &AppState, title: &str) -> Result<ReadServiceClient, Page> {
    match read_service::connect(&state.read_addr).await {
        Ok(c) => Ok(c),
        Err(err) => Err(unreachable_page(state, title, &format!("connect: {err}"))),
    }
}

fn unreachable_page(state: &AppState, title: &str, error: &str) -> Page {
    let seen = match state.last_seen_ms.load(Ordering::Relaxed) {
        0 => None,
        ms => Some(ms),
    };
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Html(render::page(
            title,
            state.refresh_secs,
            &render::unreachable(&state.read_addr, error, seen, now_ms()),
        )),
    )
}

fn ok_page(state: &AppState, title: &str, body: &str) -> Page {
    state.last_seen_ms.store(now_ms(), Ordering::Relaxed);
    (
        StatusCode::OK,
        Html(render::page(title, state.refresh_secs, body)),
    )
}

async fn health_page(State(state): State<Arc<AppState>>) -> Page {
    let client = match client_or_unreachable(&state, "health").await {
        Ok(c) => c,
        Err(page) => return page,
    };
    match client.health(context::current()).await {
        Ok(Ok(report)) => ok_page(&state, "health", &render::health(&report, now_ms())),
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
    let include_archived = q.get("archived").is_some_and(|v| v == "1");
    let status = q.get("status").cloned();
    match client
        .invocations(context::current(), status, include_archived, 100)
        .await
    {
        Ok(Ok(items)) => ok_page(
            &state,
            "invocations",
            &render::invocations(&items, include_archived),
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
            Html(render::page(
                "invocation",
                state.refresh_secs,
                r#"<p class="muted">no invocation with that id.</p>"#,
            )),
        ),
        Ok(Err(err)) => unreachable_page(&state, "invocation", &err.to_string()),
        Err(err) => unreachable_page(&state, "invocation", &format!("rpc: {err}")),
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

async fn costs_page(State(state): State<Arc<AppState>>) -> Page {
    let client = match client_or_unreachable(&state, "costs").await {
        Ok(c) => c,
        Err(page) => return page,
    };
    match client.costs(context::current(), None, None).await {
        Ok(Ok(report)) => ok_page(&state, "costs", &render::costs(&report)),
        Ok(Err(err)) => unreachable_page(&state, "costs", &err.to_string()),
        Err(err) => unreachable_page(&state, "costs", &format!("rpc: {err}")),
    }
}

/// Build the router — separated from `main` so tests drive it with
/// `tower::ServiceExt::oneshot`.
fn app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(health_page))
        .route("/invocations", get(invocations_page))
        .route("/invocations/:id", get(invocation_page))
        .route("/events", get(events_page))
        .route("/costs", get(costs_page))
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
        let (addr, serving) = read_service::bind(
            "127.0.0.1:0",
            views,
            js,
            std::time::Duration::from_millis(100),
            "dash-test".to_string(),
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
        assert!(html.contains("dash-test"), "version rendered: {html}");
        assert!(html.contains("reachable"));

        let resp = app
            .clone()
            .oneshot(Request::get("/invocations").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(body_string(resp).await.contains("no invocations"));

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

        let resp = app
            .clone()
            .oneshot(Request::get("/costs").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(body_string(resp).await.contains("no cost events"));

        let resp = app
            .oneshot(Request::get("/events").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
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
