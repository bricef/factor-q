//! The dashboard against a real daemon, over real HTTP.
//!
//! **Why this exists.** Every page the operator dashboard serves was
//! already covered — and the transcript page still spent six days
//! returning 503 on the live instance
//! ([#673](https://github.com/bricef/factor-q/issues/673)). Neither
//! existing check could see it. `dashboard-ci` drives `fn app()` with
//! `tower::oneshot` against an in-process fixture registry whose turn
//! atom answers `List` with an empty vector, so no test ever fetched a
//! transcript that had turns in it. The screenshot job renders canned
//! `fq_ops::views` structs straight through `render::*` — no
//! `EdgeClient`, no status code, no daemon, and its only assertion is
//! that at least one file was written.
//!
//! What #673 needed was the composition: a daemon holding history it
//! cannot read, an edge that answers `turn.list` with an error because
//! of it, and a dashboard that turns that error into a 503. So this
//! suite runs the real `fqd` and the real `fq-dashboard` as processes
//! and speaks HTTP/1.1 to the second one.
//!
//! **No browser.** Every page here is server-side HTML: a plain socket
//! sees the status code and the banner exactly as a browser would, and
//! `chromium --screenshot` exits 0 on a 503 anyway. The live tail is
//! SSE, which is also just bytes on a socket.
//!
//! **The legacy event.** `services/fq-runtime/crates/fq-runtime/tests/
//! corpus/events/v2/llm_request.json` is the committed shape from
//! before the message vocabulary became a tagged enum: flat
//! `{role, content, tool_calls}` messages, no `kind`. It is published
//! raw onto the stream, under the same agent as the invocation under
//! test but a *different* invocation — which is the live failure
//! exactly. `list_turns` scans the agent's whole subject from sequence
//! one and filters by invocation afterwards, so one unreadable message
//! anywhere in that agent's history failed `turn.list` for every
//! invocation of it, however recent.
//!
//! **Assertions are positive.** "Not 503, and this marker is present"
//! rather than "contains the error", so the suite goes on meaning
//! something after #673 is fixed. The one deliberately negative check
//! is the control at the end: with the daemon stopped, the transcript
//! page *must* be a 503 with the banner — the proof that these
//! assertions can fail at all.
//!
//! Gated behind the `dashboard-e2e` feature, because `cargo test -p
//! fq-daemon` (what `just runtime-ci` runs) does not build the
//! dashboard binary. `just dashboard-e2e` builds it, then runs this.

#![cfg(unix)]

use std::process::Stdio;
use std::time::Duration;

use fq_ops::surface::StatusReport;
use fq_ops::{ControlReport, Domain, OpId, ReportId};
use fq_runtime::test_support::mock_anthropic::{MockAnthropicServer, MockResponse};
use fq_test_support::TestChild;
use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// The agent everything here is seeded under. It matches the
/// `agent_id` in the committed v2 corpus event, so those bytes land in
/// this agent's history without being rewritten — the fixture is used
/// verbatim, which is the point of having one.
const AGENT: &str = "corpus-agent";

/// The model the agent definition names. Declaring an agent at all
/// arms the daemon's pricing guarantee (ADR-0004), so the config below
/// prices this model explicitly rather than reaching for the network.
const MODEL: &str = "claude-haiku-4-5";

/// The grants the dashboard's token must carry.
///
/// Copied from `REQUIRED_GRANTS` in `services/fq-dashboard/src/main.rs`
/// — the dashboard refuses to start without all six, and this crate
/// cannot link the dashboard to read them. One place here, so a drift
/// is one edit.
const DASHBOARD_GRANTS: &[&str] = &[
    "read:agent",
    "read:control",
    "read:cost",
    "read:event",
    "read:invocation",
    "read:turn",
];

/// The banner `render::unreachable` writes. Its absence is half of
/// every page assertion below.
const UNREACHABLE: &str = "runtime unreachable at";

fn unique_scratch(mock_base_url: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("dashboard-e2e-{}-{}", std::process::id(), nanos));
    std::fs::create_dir_all(dir.join("cache")).unwrap();
    let agents = dir.join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    // The registry the `/agents` pages render. Without a definition on
    // disk those two pages are empty and their markers would be
    // asserting nothing.
    std::fs::write(
        agents.join("corpus-agent.md"),
        format!(
            "---\nname: {AGENT}\nmodel: {MODEL}\nbudget: 0.05\n---\n\n\
             You are the corpus agent. This prompt exists so the agent \
             detail page has one to render.\n"
        ),
    )
    .unwrap();
    // `base_url` points the daemon's LLM client at this test's mock,
    // so the invocation below is a real dispatch that never leaves the
    // machine. The explicit price is the pricing guarantee (ADR-0004),
    // which declaring an agent arms.
    //
    // `min_seq_wait_ms` is the ceiling on the gated read below, and its
    // default is 2 s — the daemon answers `Lagging` past that, not the
    // tarpc deadline. Two seconds is a sane production default and far
    // too tight for the only load-sensitive barrier in this suite: a
    // fold that falls behind on a busy box would fail the test for a
    // reason that has nothing to do with the dashboard.
    std::fs::write(
        dir.join("fq.toml"),
        format!(
            "[edge]\nbind = \"127.0.0.1:0\"\n\
             min_seq_wait_ms = 30000\n\n\
             [providers.anthropic]\nmodels = [\"{MODEL}\"]\n\
             base_url = \"{mock_base_url}\"\n\n\
             [providers.anthropic.pricing.\"{MODEL}\"]\n\
             input_per_mtok = 1.0\noutput_per_mtok = 5.0\n"
        ),
    )
    .unwrap();
    dir
}

fn suffix_of<'a>(log: &'a str, prefix: &str) -> &'a str {
    log.lines()
        .find_map(|l| l.trim().strip_prefix(prefix))
        .unwrap_or_else(|| panic!("log lacks prefix {prefix:?}\n--- log ---\n{log}"))
        .trim()
}

fn parse_fingerprint(hex: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex fingerprint");
    }
    out
}

// === the two children ===

struct Daemon {
    process: TestChild,
    addr: String,
    fingerprint_hex: String,
    fingerprint: [u8; 32],
    admin_token: String,
    log_path: std::path::PathBuf,
}

impl Daemon {
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
    }

    /// Wait for `needle` in the daemon's log, or panic with the log.
    async fn await_log(&mut self, needle: &str, budget: Duration) -> String {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let text = self.log();
            if text.contains(needle) {
                return text;
            }
            if let Some(status) = self.process.try_wait().expect("poll fqd") {
                panic!("fqd exited ({status:?}) waiting for {needle:?}\n--- log ---\n{text}");
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "daemon log never carried {needle:?}\n--- log ---\n{text}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

async fn start_daemon(server: &fq_test_support::NatsServer, scratch: &std::path::Path) -> Daemon {
    let log_path = scratch.join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let log_err = log.try_clone().expect("clone log handle");
    // Spawned from the test body's own thread, never a `spawn_blocking`
    // worker: the guard is `PR_SET_PDEATHSIG`, which fires when the
    // spawning *thread* exits (fq-test-support/src/child.rs).
    let mut process = TestChild::builder(env!("CARGO_BIN_EXE_fqd"))
        .env("FQ_DAEMON_CONFIG", scratch.join("fq.toml"))
        .env("FQ_NATS_URL", server.url())
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_STATE_DIR", scratch.join("state"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .env("ANTHROPIC_API_KEY", "test-key-unused-by-mock")
        // JSON logs: the invocation id is read back out of them, and a
        // human-formatted line would not carry it as a field.
        .env("FQ_LOG_FORMAT", "json")
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let text = loop {
        if let Some(status) = process.try_wait().expect("poll fqd") {
            let text = std::fs::read_to_string(&log_path).unwrap_or_default();
            panic!("fqd exited during startup with {status:?}\n--- log ---\n{text}");
        }
        let text = std::fs::read_to_string(&log_path).unwrap_or_default();
        if text.contains("Runtime ready") {
            break text;
        }
        assert!(tokio::time::Instant::now() < deadline, "fqd never ready");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    let fingerprint_hex =
        suffix_of(&text, "edge: certificate fingerprint (clients pin this): ").to_string();
    Daemon {
        addr: suffix_of(&text, "- edge is listening on ").to_string(),
        fingerprint: parse_fingerprint(&fingerprint_hex),
        fingerprint_hex,
        admin_token: fq_test_support::admin_token(&scratch.join("state")),
        process,
        log_path,
    }
}

struct Dashboard {
    /// Held, never read: dropping it is what stops the dashboard.
    #[allow(dead_code)]
    process: TestChild,
    addr: String,
}

/// A free loopback port, drawn by binding `:0` and letting go. The
/// dashboard's `--bind` accepts `:0` but never prints the port it
/// resolved to, so it has to be told a concrete one; the caller retries
/// on a fresh draw if the race is lost.
fn draw_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("draw a port");
    listener.local_addr().expect("local_addr").port()
}

/// Start `fq-dashboard` against `daemon`, with an attenuated token.
///
/// The barrier is `GET /healthz`, which is liveness only — it asks the
/// daemon nothing, so it says "this process is serving" and not "the
/// runtime is up", which is the distinction the pages themselves are
/// about. Three attempts on fresh ports, because the port was drawn
/// and released rather than held.
async fn start_dashboard(
    daemon: &Daemon,
    scratch: &std::path::Path,
    token: &str,
) -> anyhow::Result<Dashboard> {
    // Sibling of the daemon binary in cargo's target dir — the same
    // idiom the CLI suites use to reach `fq`. `just dashboard-e2e`
    // builds it; nothing about `cargo test -p fq-daemon` does.
    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_fqd")).with_file_name("fq-dashboard");
    assert!(
        binary.exists(),
        "{} is missing — this suite runs the real dashboard binary, so it has to be built \
         first. Run `just dashboard-e2e`, which builds it and then runs this.",
        binary.display()
    );

    let mut last = String::new();
    for attempt in 1..=3u32 {
        let addr = format!("127.0.0.1:{}", draw_port());
        let log_path = scratch.join(format!("dashboard-{attempt}.log"));
        let log = std::fs::File::create(&log_path).expect("create dashboard log");
        let log_err = log.try_clone().expect("clone log handle");
        let mut process = TestChild::builder(&binary)
            .env("FQ_DASHBOARD_BIND", &addr)
            .env("FQ_EDGE", &daemon.addr)
            .env("FQ_EDGE_TOKEN", token)
            .env("FQ_EDGE_FINGERPRINT", &daemon.fingerprint_hex)
            // No auto-refresh under test: every read here is one this
            // test asked for.
            .env("FQ_DASHBOARD_REFRESH", "3600")
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .spawn();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = process.try_wait().expect("poll fq-dashboard") {
                last = format!(
                    "attempt {attempt} on {addr}: exited {status:?}\n--- log ---\n{}",
                    std::fs::read_to_string(&log_path).unwrap_or_default()
                );
                break;
            }
            if let Some(resp) = try_get(&addr, "/healthz", &[], Duration::from_secs(3)).await
                && resp.status == 200
                && resp.body == "ok\n"
            {
                return Ok(Dashboard { process, addr });
            }
            if tokio::time::Instant::now() >= deadline {
                last = format!(
                    "attempt {attempt} on {addr}: /healthz never answered within 30s\n\
                     --- log ---\n{}",
                    std::fs::read_to_string(&log_path).unwrap_or_default()
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    anyhow::bail!("fq-dashboard never came up in three attempts.\n{last}")
}

// === a very small HTTP/1.1 client ===

struct HttpResponse {
    status: u16,
    headers: String,
    body: String,
}

impl HttpResponse {
    /// Whether a header line `name: …value…` is present. Header names
    /// are compared lowercased; the value match is a substring, which
    /// is what every assertion here wants (`text/event-stream` inside
    /// a content type that may carry a charset).
    fn header_contains(&self, name: &str, value: &str) -> bool {
        self.headers.lines().any(|line| {
            let Some((k, v)) = line.split_once(':') else {
                return false;
            };
            k.trim().eq_ignore_ascii_case(name) && v.to_ascii_lowercase().contains(value)
        })
    }
}

/// One raw `GET`, modelled on the dashboard's own `probe()` — no HTTP
/// client crate is linked here either.
///
/// `budget` bounds the *read*, not the connect: an SSE response never
/// ends, so what arrived by the deadline is the answer. Returns `None`
/// only when the connection could not be made.
async fn try_get(
    addr: &str,
    path: &str,
    extra_headers: &[(&str, &str)],
    budget: Duration,
) -> Option<HttpResponse> {
    let mut stream = tokio::net::TcpStream::connect(addr).await.ok()?;
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (name, value) in extra_headers {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await.ok()?;

    let deadline = tokio::time::Instant::now() + budget;
    let mut raw = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let mut chunk = [0u8; 8192];
        match tokio::time::timeout(remaining, stream.read(&mut chunk)).await {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(n)) => raw.extend_from_slice(&chunk[..n]),
            Ok(Err(err)) => panic!("GET {path}: read failed: {err}"),
        }
    }

    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("GET {path}: no status in {status_line:?}"));
    Some(HttpResponse {
        status,
        headers: lines.collect::<Vec<_>>().join("\n"),
        body: body.to_string(),
    })
}

/// `try_get` for a request that must connect.
async fn get(addr: &str, path: &str, budget: Duration) -> HttpResponse {
    try_get(addr, path, &[], budget)
        .await
        .unwrap_or_else(|| panic!("GET {path}: could not connect to {addr}"))
}

/// The check every page shares: the dashboard reached the daemon.
///
/// Stated as an absence rather than a presence so it keeps its meaning
/// after #673 — a page that renders the banner has failed whatever
/// else it got right.
fn assert_reached_the_daemon(path: &str, resp: &HttpResponse) {
    assert_ne!(
        resp.status, 503,
        "GET {path} answered 503 — the dashboard could not read the daemon.\n\
         --- body ---\n{}",
        resp.body
    );
    assert!(
        !resp.body.contains(UNREACHABLE),
        "GET {path} rendered the unreachable banner (status {}).\n--- body ---\n{}",
        resp.status,
        resp.body
    );
}

fn assert_contains(path: &str, resp: &HttpResponse, needle: &str) {
    assert!(
        resp.body.contains(needle),
        "GET {path} (status {}) should contain {needle:?}.\n--- body ---\n{}",
        resp.status,
        resp.body
    );
}

// === seeding ===

/// The committed pre-#510 event, published raw onto the stream — the
/// history a long-lived instance carries across a wire break.
///
/// **Published after the daemon's projection has caught up.** Not
/// because that avoids halting the projector — it does not. The
/// projection durable subscribes to every subject and halts its loop on
/// any version it cannot read, so this message stops it either way.
/// What the ordering buys is *when*: the halt lands after the rows this
/// test needs are already folded, so everything asserted below reads a
/// projection frozen at the pre-seed horizon, which the gated read has
/// just proved is far enough. Seeding ahead of a fresh daemon instead
/// halts it at sequence one, with nothing folded at all.
///
/// The live instance was never in either state: its durable was long
/// past the v2 range, having consumed those events when the build could
/// still read them. Only the *ad-hoc rescan* — `events_from` from
/// sequence one, which is what `turn.list` does — meets them with a
/// build that cannot, and that rescan is what #673 is about and what
/// this reproduces faithfully.
async fn seed_legacy_history(server: &fq_test_support::NatsServer) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fq-runtime/tests/corpus/events/v2/llm_request.json");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
    let seeded: serde_json::Value = serde_json::from_slice(&bytes).expect("corpus event is JSON");
    assert_eq!(
        seeded["envelope"]["agent_id"], AGENT,
        "the corpus event must sit in {AGENT}'s history — `list_turns` scans \
         fq.agent.{AGENT}.> and nothing else"
    );

    let bus = fq_runtime::EventBus::connect(server.url())
        .await
        .expect("connect NATS");
    bus.jetstream()
        .publish(format!("fq.agent.{AGENT}.llm.request"), bytes.into())
        .await
        .expect("publish the v2 event")
        .await
        .expect("v2 event stored");
}

// === the suite ===

#[tokio::test(flavor = "multi_thread")]
async fn the_dashboard_serves_every_page_against_a_real_daemon() {
    let server = fq_test_support::NatsServer::start();
    // The model this run talks to. One scripted turn, terminal: the
    // agent declares its outcome and the invocation completes, which
    // is the state a transcript is usually read in.
    let mock = MockAnthropicServer::start().await;
    mock.push_response(MockResponse::tool_use(
        "toolu_dashboard_e2e",
        "report_outcome",
        json!({
            "status": "success",
            "summary": "The dashboard end-to-end run reached its terminal turn.",
        }),
        10,
        5,
    ));

    let scratch = unique_scratch(mock.base_url());
    let mut daemon = start_daemon(&server, &scratch).await;
    let invocation = run_one_invocation(&server, &mut daemon).await;

    let client =
        fq_edge::EdgeClient::connect(&daemon.addr, daemon.fingerprint, &daemon.admin_token)
            .await
            .expect("connect edge");
    // Read-your-writes before anything touches the dashboard: gate a
    // read on the last sequence this agent's subject holds, so the
    // daemon's fold is known to have taken in everything the run just
    // wrote. No sleeps anywhere below.
    let tip = fq_runtime::EventBus::connect(server.url())
        .await
        .expect("connect bus")
        .last_event_seq_matching(&format!("fq.agent.{AGENT}.>"))
        .await
        .expect("stream tip");
    gated_get(&client, &invocation, tip).await;
    let version = daemon_version(&client).await;

    // Only now: the invocation under test is folded and readable, and
    // the unreadable history goes in behind it — under the same agent,
    // under a different invocation, which is the live shape exactly.
    seed_legacy_history(&server).await;

    let grants: Vec<(String, String)> = DASHBOARD_GRANTS
        .iter()
        .map(|g| {
            let (verb, domain) = g.split_once(':').expect("grant is verb:domain");
            (verb.to_string(), domain.to_string())
        })
        .collect();
    let token = fq_edge::attenuate(&daemon.admin_token, &grants).expect("attenuate for dashboard");

    let dashboard = start_dashboard(&daemon, &scratch, &token)
        .await
        .expect("start fq-dashboard");
    let at = dashboard.addr.clone();

    check_pages(&at, &invocation, &version).await;
    check_transcript(&at, &invocation).await;
    check_datastar_negotiation(&at).await;

    // === the negative control ===
    //
    // Every assertion above is an absence of failure, which is only
    // worth something if the failure is reachable. Stop the daemon and
    // the same page must go 503 with the banner — so a suite that
    // passes has been shown capable of not passing.
    drop(client);
    daemon.process.signal(libc::SIGKILL).expect("kill fqd");
    daemon.process.wait().expect("reap fqd");

    let path = format!("/invocations/{invocation}/transcript");
    let resp = get(&at, &path, Duration::from_secs(20)).await;
    assert_eq!(
        resp.status, 503,
        "with the daemon stopped, GET {path} must be a 503.\n--- body ---\n{}",
        resp.body
    );
    assert_contains(&path, &resp, UNREACHABLE);
}

/// Drive one real invocation through the daemon and return its id.
///
/// Handed over on `fq.trigger.<agent>` — the same subject the watcher
/// and fq-cron publish on, so the daemon dispatches it itself rather
/// than the test forging the rows a dispatch writes. That matters:
/// `/invocations` is the control plane's ownership index, which only a
/// real claim populates, and the transcript is the dispatch's own
/// turns. Synthetic events on the stream produce neither.
async fn run_one_invocation(server: &fq_test_support::NatsServer, daemon: &mut Daemon) -> String {
    let nats = async_nats::connect(server.url())
        .await
        .expect("connect to test broker");
    nats.publish(
        format!("fq.trigger.{AGENT}"),
        serde_json::to_vec(&json!("render something for the dashboard"))
            .unwrap()
            .into(),
    )
    .await
    .expect("publish trigger");
    nats.flush().await.expect("flush trigger publish");

    let log = daemon
        .await_log("reducer invocation completed", Duration::from_secs(60))
        .await;
    // JSON logs, so the id is a field: parse the first line that
    // carries one rather than slicing a fixed width off a `find`, which
    // would hand back 36 bytes of whatever followed if the shape moved.
    log.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|v| {
            v.pointer("/fields/invocation_id")
                .or_else(|| v.pointer("/span/invocation_id"))
                .or_else(|| v.get("invocation_id"))
                .and_then(|id| id.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| panic!("no invocation_id in daemon log\n--- log ---\n{log}"))
}

/// A gated read: returns once the daemon's fold has taken in `min_seq`.
async fn gated_get(client: &fq_edge::EdgeClient, invocation: &str, min_seq: u64) {
    client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: OpId::Get(Domain::Invocation),
                version: 1,
                input: json!({"invocation_id": invocation}),
                min_seq: Some(min_seq),
            },
        )
        .await
        .expect("rpc")
        .expect("gated get after the run");
}

/// What `control.status` says this daemon's build is — the string the
/// health page renders, taken from the daemon itself rather than
/// reconstructed here.
async fn daemon_version(client: &fq_edge::EdgeClient) -> String {
    let report = client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: OpId::Report(ReportId::Control(ControlReport::Status)),
                version: 1,
                input: json!({}),
                min_seq: None,
            },
        )
        .await
        .expect("rpc")
        .expect("control.status");
    let status: StatusReport = serde_json::from_value(report.output).expect("decode StatusReport");
    status.version
}

/// The pages that are one read each.
async fn check_pages(at: &str, invocation: &str, version: &str) {
    let budget = Duration::from_secs(20);
    let short = &invocation[..8];
    let id = invocation;

    let resp = get(at, "/healthz", budget).await;
    assert_eq!(resp.status, 200, "GET /healthz");
    assert_eq!(resp.body, "ok\n", "GET /healthz");

    let resp = get(at, "/", budget).await;
    assert_reached_the_daemon("/", &resp);
    assert_contains("/", &resp, version);
    assert!(
        !resp.body.contains("build skew"),
        "GET / reported build skew — the daemon and the dashboard were built from \
         different stamps, so this run is not testing one tree.\n--- body ---\n{}",
        resp.body
    );

    let resp = get(at, "/invocations", budget).await;
    assert_reached_the_daemon("/invocations", &resp);
    assert_contains("/invocations", &resp, short);

    let path = format!("/invocations/{id}");
    let resp = get(at, &path, budget).await;
    assert_reached_the_daemon(&path, &resp);
    assert_contains(&path, &resp, id);
    assert_contains(&path, &resp, AGENT);

    let resp = get(at, "/events", budget).await;
    assert_reached_the_daemon("/events", &resp);
    assert_contains("/events", &resp, AGENT);

    let resp = get(at, "/costs", budget).await;
    assert_reached_the_daemon("/costs", &resp);
    assert_eq!(resp.status, 200, "GET /costs");
    // The run priced a real model call past the gated read, so this
    // page has a row and not the "no cost events recorded" body.
    assert_contains("/costs", &resp, AGENT);

    let resp = get(at, "/agents", budget).await;
    assert_reached_the_daemon("/agents", &resp);
    assert_eq!(resp.status, 200, "GET /agents");
    assert_contains("/agents", &resp, &format!(r#"href="/agents/{AGENT}""#));

    let path = format!("/agents/{AGENT}");
    let resp = get(at, &path, budget).await;
    assert_reached_the_daemon(&path, &resp);
    assert_eq!(resp.status, 200, "GET {path}");
    assert_contains(&path, &resp, r#"id="system-prompt""#);
}

/// The transcript page and its live tail — the #673 assertion.
///
/// On a build that cannot read the seeded v2 event this is where the
/// suite fails: `turn.list` errors on the whole agent's history and
/// `pages/transcript.rs` renders that as `unreachable_page`, a 503.
async fn check_transcript(at: &str, invocation: &str) {
    let path = format!("/invocations/{invocation}/transcript");
    let resp = get(at, &path, Duration::from_secs(20)).await;
    assert_reached_the_daemon(&path, &resp);
    assert_eq!(resp.status, 200, "GET {path}\n--- body ---\n{}", resp.body);
    assert_contains(&path, &resp, r#"id="turns""#);
    // `<div class="turn` and not `class="turn`. #683 renders a refused
    // `turn.list` as `<pre class="turn err">…</pre>` inside the
    // transcript's own frame, so the looser needle is present on
    // exactly the failure this suite exists to catch. Real turns are
    // the `<div>`s `render::transcript_entry_html` writes.
    let turns = resp.body.matches(r#"<div class="turn"#).count();
    assert!(
        turns >= 1,
        "GET {path} rendered no turn elements — the run's dispatch should \
         have produced at least one.\n--- body ---\n{}",
        resp.body
    );
    // The zero-turns-at-200 body. (`no transcript for that id` is the
    // 404's, which the status assertion above already excludes.)
    assert!(
        !resp.body.contains("no transcript entries."),
        "GET {path} rendered an empty transcript — `turn.list` answered \
         with no turns for a run that completed.\n--- body ---\n{}",
        resp.body
    );

    // The seam the page pinned for its own tail, read back off the
    // page rather than recomputed — asserting on the URL a browser
    // would actually open is the only version of this worth making.
    let marker = "/transcript/stream?after=";
    let seam: u64 = resp
        .body
        .split_once(marker)
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.split(['&', '\'']).next())
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            panic!(
                "GET {path} carried no live-tail seam.\n--- body ---\n{}",
                resp.body
            )
        });

    // That URL, exactly as the page pins it: one past the highest
    // sequence it rendered, so it long-polls for turns that have not
    // happened yet. What it proves is that the route is served and the
    // response is SSE — content would mean waiting out a poll for a
    // turn this finished run will never produce.
    let live = format!("/invocations/{invocation}/transcript/stream?after={seam}&full=0");
    let resp = get(at, &live, Duration::from_secs(5)).await;
    assert_eq!(resp.status, 200, "GET {live}");
    assert!(
        resp.header_contains("content-type", "text/event-stream"),
        "GET {live} should be SSE.\n--- headers ---\n{}",
        resp.headers
    );

    // And the same handler opened from the bottom of the log, where
    // the turns already are: this is the one that proves the tail
    // renders turns rather than merely holding a socket open.
    // Timeboxed either way — the stream never closes on its own.
    let replay = format!("/invocations/{invocation}/transcript/stream?after=1&full=0");
    let resp = get(at, &replay, Duration::from_secs(5)).await;
    assert_eq!(resp.status, 200, "GET {replay}");
    assert!(
        resp.header_contains("content-type", "text/event-stream"),
        "GET {replay} should be SSE.\n--- headers ---\n{}",
        resp.headers
    );
    assert_contains(&replay, &resp, "datastar-patch-elements");
    // `#turns` is the prepend target for a real turn. The stream's
    // error path also emits a patch, at `#status`, so asserting the
    // event name alone would pass on a stream that only reported a
    // failure.
    assert_contains(&replay, &resp, "selector #turns");
}

/// The datastar content negotiation: the same URL, two representations.
async fn check_datastar_negotiation(at: &str) {
    let resp = try_get(
        at,
        "/",
        &[("Datastar-Request", "true")],
        Duration::from_secs(20),
    )
    .await
    .expect("GET / with Datastar-Request");
    assert_reached_the_daemon("/ (datastar)", &resp);
    assert!(
        resp.header_contains("content-type", "text/event-stream"),
        "GET / with Datastar-Request should be SSE.\n--- headers ---\n{}",
        resp.headers
    );
    assert_contains("/ (datastar)", &resp, "selector #main");
    assert_contains("/ (datastar)", &resp, "mode inner");
}
