//! End-to-end tests for `fq invocation resume` (#373): the operator
//! recovery path for ambiguous invocations, exercised against a real
//! spawned daemon, a private broker, and a scripted mock LLM.
//!
//! The acceptance scenario is the one the feature exists for: an
//! invocation is SIGKILLed mid-`builtin__exec` (the WAL freezes with a
//! `dispatched` row and no `completed`), the restarted daemon
//! classifies it Ambiguous, the operator resumes it, and the
//! invocation completes under its own steam. The mock LLM doubles as
//! the oracle for the injection contract: the post-resume model
//! request must carry the synthetic interrupted-result notice — the
//! disclosure is *conversation content*, so it is asserted at the
//! wire, not inferred from logs.
//!
//! SIGKILL (not SIGTERM/SIGINT) is load-bearing: any graceful path
//! would drain or complete the dispatch and the invocation would
//! never be Ambiguous. This is the crash the recovery taxonomy's
//! third category exists for.
//!
//! Isolation follows daemon_shutdown.rs: every test spawns its own
//! nats-server (#233) and its own daemon over a scratch config, so
//! tests run in parallel with no shared broker and no locks.

#![cfg(unix)]

use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use fq_runtime::test_support::mock_anthropic::{MockAnthropicServer, MockResponse};
use serde_json::json;

fn fq_binary() -> &'static str {
    env!("CARGO_BIN_EXE_fq")
}

/// Scratch layout for one test: config, agents, cache, and a
/// workspace dir the agent's sandbox permits. Unique per test run so
/// parallel tests never collide.
struct Scratch {
    root: std::path::PathBuf,
    /// Held so `fq.toml` can be rewritten in full once the daemon
    /// reports the edge address it actually bound — see
    /// [`Scratch::pin_edge_bind`].
    mock_base_url: String,
}

impl Scratch {
    fn new(tag: &str, mock_base_url: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("fq-resume-{tag}-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(root.join("cache")).unwrap();
        std::fs::create_dir_all(root.join("agents")).unwrap();
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        std::fs::create_dir_all(root.join("xdg")).unwrap();

        // The agent under test: one exec tool, sandboxed to the
        // scratch workspace. The mock decides what it "asks" for; the
        // definition only has to permit it.
        std::fs::write(
            root.join("agents").join("resume-probe.md"),
            format!(
                "---\nname: resume-probe\nmodel: claude-haiku-4-5\ntools:\n  - builtin__exec\n\
                 sandbox:\n  exec_cwd:\n    - {}\nbudget: 1.00\n---\n\n\
                 Test probe agent. Run the command you are told to run.\n",
                root.join("workspace").display()
            ),
        )
        .unwrap();

        let scratch = Self {
            root,
            mock_base_url: mock_base_url.to_string(),
        };

        // Port 0: the kernel picks, the daemon reports what it got, and
        // the first `Daemon::pair` writes that back here
        // (`pin_edge_bind`).
        //
        // The shape this replaces bound a socket to find a free port,
        // dropped it, and wrote the number into this file for the
        // daemon to bind seconds later. The wrong assumption is that a
        // port observed free stays free: across that gap nothing owns
        // it, so it belongs to whoever asks next. The four tests in
        // this file run in parallel and reach this constructor within
        // milliseconds of each other, and every other suite standing up
        // its own daemon, broker and mock is drawing from the same
        // ephemeral range at the same time, so two draws can return the
        // same number. It only bites under load, because only under
        // load do the starts overlap.
        //
        // What it costs is a daemon that dies on `fq_edge::bind` before
        // it ever prints `Runtime ready` (`fq-cli/src/lib.rs` — the
        // bind is fatal and precedes the banner), so the test that lost
        // the race reports "daemon exited while waiting for
        // \"Runtime ready\"" and the operator-visible reason — someone
        // else has the port — appears nowhere in it. `:0` has no gap in
        // which to lose the port, so the first start cannot race at
        // all.
        scratch.write_config("127.0.0.1:0");
        scratch
    }

    fn path(&self, rel: &str) -> std::path::PathBuf {
        self.root.join(rel)
    }

    /// Write `fq.toml` with `edge_bind` as the edge address.
    ///
    /// The pricing guarantee (#62) requires the model declared; the
    /// haiku name resolves in the LiteLLM table. `base_url` points the
    /// daemon's LLM client at this test's mock server.
    fn write_config(&self, edge_bind: &str) {
        let mock_base_url = &self.mock_base_url;
        std::fs::write(
            self.path("fq.toml"),
            format!(
                "[edge]\nbind = \"{edge_bind}\"\n\n\
                 [providers.anthropic]\nmodels = [\"claude-haiku-4-5\"]\nbase_url = \"{mock_base_url}\"\n"
            ),
        )
        .unwrap();
    }

    /// Freeze the address the first daemon actually bound, so every
    /// later start of this scratch lands on the same one.
    ///
    /// The pin itself is load-bearing and always was: these tests
    /// restart the daemon, and the client pairing has to survive the
    /// restart — same address, same persisted identity, same token.
    /// Client verbs dial `[edge] bind` out of this file while `fq
    /// connect` stores the pairing under the address it is handed, so
    /// the two have to name the same place; leaving `:0` here would
    /// hand every restart a fresh port and break the pairing under
    /// test. What was wrong before was the *timing*, not the pinning —
    /// an address chosen before anything owned it is a bet, whereas the
    /// one the daemon is already listening on is a fact.
    ///
    /// The daemon reads this file once at startup and nothing rereads
    /// it, so rewriting it under a live daemon is safe.
    fn pin_edge_bind(&self, addr: &str) {
        self.write_config(addr);
    }
}

/// Run an `fq` CLI verb against this test's daemon/state, returning
/// the completed output. Never panics on non-zero exit — the error
/// matrix asserts on failures deliberately.
fn run_fq(scratch: &Scratch, nats_url: &str, args: &[&str]) -> Output {
    Command::new(fq_binary())
        .args(args)
        .env("FQ_CONFIG", scratch.path("fq.toml"))
        .env("FQ_NATS_URL", nats_url)
        .env("FQ_CACHE_DIR", scratch.path("cache"))
        .env("FQ_STATE_DIR", scratch.path("state"))
        .env("FQ_AGENTS_DIR", scratch.path("agents"))
        .env("XDG_CONFIG_HOME", scratch.path("xdg"))
        .env("ANTHROPIC_API_KEY", "test-key-unused-by-mock")
        .output()
        .expect("run fq CLI")
}

struct Daemon {
    child: std::process::Child,
    log_path: std::path::PathBuf,
}

impl Daemon {
    fn spawn(scratch: &Scratch, nats_url: &str, log_name: &str) -> Self {
        let log_path = scratch.path(log_name);
        let log = std::fs::File::create(&log_path).expect("create daemon log");
        let log_err = log.try_clone().expect("clone log handle");
        let child = Command::new(fq_binary())
            .arg("run")
            .env("FQ_CONFIG", scratch.path("fq.toml"))
            .env("FQ_NATS_URL", nats_url)
            .env("FQ_CACHE_DIR", scratch.path("cache"))
            .env("FQ_STATE_DIR", scratch.path("state"))
            .env("FQ_AGENTS_DIR", scratch.path("agents"))
            .env("ANTHROPIC_API_KEY", "test-key-unused-by-mock")
            // JSON logs: single-line, no ANSI — the id extraction and
            // needle waits parse this, not the human format.
            .env("FQ_LOG_FORMAT", "json")
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .spawn()
            .expect("spawn fq run");
        Self { child, log_path }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
    }

    /// Pair the CLI with this daemon's edge (idempotent): the flipped
    /// read verbs (`invocation list`/`show`) speak the authenticated
    /// edge, so tests that verify through them need the stored
    /// pairing. Only the FIRST daemon of a scratch prints the admin
    /// token; later daemons reuse the persisted identity, and the
    /// address pinned here keeps the stored pairing valid across
    /// restarts.
    ///
    /// Both values come out of the daemon log because neither exists
    /// anywhere else — the kernel chose the address, and the token is
    /// minted and printed exactly once — and both are matched by
    /// content, never by line number. (`tests/smoke/smoke.sh` reads the
    /// same two values from the same output for the same reasons.)
    fn pair(&self, scratch: &Scratch) {
        if scratch
            .path("xdg")
            .join("factor-q/connections.toml")
            .exists()
        {
            return;
        }
        let log = self.log();
        let addr = log
            .lines()
            .find_map(|l| l.trim().strip_prefix("- edge is listening on "))
            .expect("edge addr in daemon log")
            .trim()
            .to_string();
        // First daemon of this scratch, so `fq.toml` still says `:0`.
        // Record what the kernel actually handed it before any client
        // verb — or the restarted daemon — reads `[edge] bind` back out
        // of that file.
        scratch.pin_edge_bind(&addr);
        let token = admin_token_from_log(&log).expect("admin token in first daemon log");
        let out = Command::new(fq_binary())
            .args(["connect", &addr, "--token", &token])
            .env("FQ_CONFIG", scratch.path("fq.toml"))
            .env("XDG_CONFIG_HOME", scratch.path("xdg"))
            .stdin(std::process::Stdio::piped())
            .output()
            .expect("run fq connect");
        assert!(
            out.status.success(),
            "fq connect failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Poll the daemon log until `needle` appears. Panics (with the
    /// full log) if the daemon exits or the deadline passes first —
    /// a hung wait must fail loudly, not sit out the suite timeout.
    async fn await_log(&mut self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().expect("poll daemon") {
                panic!(
                    "daemon exited ({status:?}) while waiting for {needle:?}\n--- log ---\n{}",
                    self.log()
                );
            }
            if self.log().contains(needle) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!(
            "daemon log never contained {needle:?} within {timeout:?}\n--- log ---\n{}",
            self.log()
        );
    }

    /// The crash under test: SIGKILL, no grace of any kind.
    fn sigkill(&mut self) {
        let rc = unsafe { libc::kill(self.child.id() as i32, libc::SIGKILL) };
        assert_eq!(rc, 0, "kill(SIGKILL) failed");
        let _ = self.child.wait();
    }

    fn stop(&mut self) {
        let _ = unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if self.child.try_wait().expect("poll daemon").is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
    }
}

/// Extract the admin token from a first-run daemon log: from the
/// banner, scan forward for the first line whose *shape* is a token.
///
/// The shape is the whole point. The obvious reading — "the line after
/// the marker" — assumes the banner and the token it introduces are
/// adjacent in the file, and nothing makes that true. `Daemon::spawn`
/// points the child's stdout and stderr at one file: the banner is a
/// `println!` on stdout, every startup tracing event is a separate
/// write to stderr, and the banner is emitted mid-startup while a dozen
/// tasks are announcing themselves. Nothing orders the two streams, so
/// a tracing line can land in the gap. `lines.next()` then hands `fq
/// connect` a log event as a credential; the daemon fails to parse it
/// and answers "token rejected", which the CLI dresses up as an
/// identity that may have been rotated — an error describing something
/// that did not happen, and a diagnosis cycle away from the truth
/// (#454).
///
/// A biscuit is one unbroken run of base64url alone on its line,
/// comfortably over 40 characters. No tracing line can wear that shape:
/// JSON records carry braces, quotes and colons, and the human format
/// carries spaces and timestamps — all outside the charset. So the scan
/// walks past whatever landed in the gap and stops on the token however
/// the two streams interleaved.
///
/// `None` rather than a best-effort line is deliberate: an extraction
/// that cannot find a token must fail here, where the reason is
/// legible, instead of forwarding a non-credential and letting the
/// daemon's rejection explain it wrongly.
fn admin_token_from_log(log: &str) -> Option<String> {
    let mut lines = log.lines();
    lines.find(|l| l.contains("edge: admin token"))?;
    lines
        .map(str::trim)
        .find(|l| {
            l.len() >= 40
                && l.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '='))
        })
        .map(str::to_string)
}

/// The #454 flake in miniature, without a daemon: the shape scan has to
/// survive tracing output landing between the banner and the token, and
/// it has to stay anchored on the banner. This is the piece most likely
/// to regress silently — the positional read came back clean from every
/// one of a few hundred local scratches before CI caught it once.
#[test]
fn admin_token_read_by_shape_not_by_position() {
    // Biscuit shape: base64url, no internal whitespace, long.
    let token =
        "Ep0BCjMKCXByaW5jaXBhbAoFYWRtaW4YAyIKCggIChIDGIAIEiQIABIgQmFzZTY0VXJsX3J1bi13aXRoLXBhZD0=";
    let token_line = format!("  {token}");
    let banner = "edge: admin token (printed once; store it securely):";
    let fingerprint = "edge: certificate fingerprint (clients pin this): \
                       0adfe5f1c0ffee00deadbeef1234567890abcdef1234567890abcdef12345678";
    // The two formats the daemon's tracing can take. Neither is
    // token-shaped; the hex fingerprint above is long enough but rides
    // on a line with punctuation, so it is not either.
    let json_line = r#"{"timestamp":"2026-07-24T10:11:12.493056Z","level":"INFO","fields":{"message":"trigger dispatcher starting"}}"#;
    let text_line = "2026-07-24T10:11:12.494128Z  INFO fq_cli: advisory watch starting";

    // Adjacent — nothing landed in the gap. The common case, and the
    // only one the positional read ever handled.
    let clean = [fingerprint, banner, token_line.as_str(), "Runtime ready."].join("\n");
    assert_eq!(admin_token_from_log(&clean).as_deref(), Some(token));

    // Interleaved. Position picks `json_line` and the pairing fails
    // with a message about identity rotation; shape picks the token.
    let interleaved = [
        fingerprint,
        banner,
        json_line,
        text_line,
        "",
        token_line.as_str(),
        "Runtime ready.",
    ]
    .join("\n");
    assert_eq!(admin_token_from_log(&interleaved).as_deref(), Some(token));

    // Nothing token-shaped after the banner: `None`, so the caller
    // panics here rather than handing a log line to `fq connect`.
    let truncated = [banner, json_line, text_line].join("\n");
    assert!(admin_token_from_log(&truncated).is_none());

    // A restarted daemon reuses its persisted identity and prints no
    // banner at all. The scan starts from the marker, so it must not
    // fall back to the first base64-ish line in the file.
    let restarted = ["edge: identity loaded from state", token_line.as_str()].join("\n");
    assert!(admin_token_from_log(&restarted).is_none());
}

/// Extract the invocation id from the daemon log — the single
/// invocation each scenario creates. Reading it from the log (rather
/// than list output) keeps the assertion surface on the operator
/// verbs themselves.
fn invocation_id_from_log(log: &str) -> String {
    let marker = "\"invocation_id\":\"";
    let start = log.find(marker).expect("no invocation_id in daemon log") + marker.len();
    log[start..start + 36].to_string()
}

/// Drive one invocation into the Ambiguous state: trigger the probe
/// agent, let the mock hand it a long-running exec, SIGKILL the
/// daemon mid-dispatch, restart, and wait for recovery to classify.
/// Returns the restarted daemon and the invocation id.
async fn crash_into_ambiguous(
    scratch: &Scratch,
    nats_url: &str,
    mock: &MockAnthropicServer,
) -> (Daemon, String) {
    // Turn 1: the model asks for a sleep long enough that the kill
    // always lands mid-dispatch.
    mock.push_response(MockResponse::tool_use(
        "toolu_probe_1",
        "builtin__exec",
        json!({
            "command": ["sleep", "300"],
            "cwd": scratch.path("workspace"),
        }),
        10,
        5,
    ));

    let mut daemon = Daemon::spawn(scratch, nats_url, "daemon-first.log");
    daemon
        .await_log("Runtime ready", Duration::from_secs(30))
        .await;
    daemon.pair(scratch);

    // Hand the invocation to the DAEMON over the trigger wire — the
    // same `fq.trigger.<agent>` subject the watcher and fq-cron
    // publish on. (`fq trigger` the CLI verb runs the agent
    // in-process instead, which is exactly not this test.)
    let nats = async_nats::connect(nats_url)
        .await
        .expect("connect to test broker");
    nats.publish(
        "fq.trigger.resume-probe",
        serde_json::to_vec(&json!("run the probe")).unwrap().into(),
    )
    .await
    .expect("publish trigger");
    nats.flush().await.expect("flush trigger publish");

    // The dispatched WAL row is written when the tool is handed off —
    // this log line is the runner announcing exactly that handoff.
    daemon
        .await_log(
            "model produced tool calls; dispatching",
            Duration::from_secs(30),
        )
        .await;
    // Give the exec child a beat to actually spawn so the kill lands
    // squarely inside the dispatch, not on its doorstep.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let invocation_id = invocation_id_from_log(&daemon.log());

    // While the tool is genuinely RUNNING, resume must refuse: the
    // WAL shape (dispatched-without-completed) is identical to a
    // crash, and only the owning worker's liveness tells them apart.
    // Injecting under a live worker would put two drivers on one
    // invocation.
    let live = run_fq(scratch, nats_url, &["invocation", "resume", &invocation_id]);
    assert!(
        !live.status.success(),
        "resume of a LIVE invocation must be rejected"
    );
    let live_msg = format!(
        "{}{}",
        String::from_utf8_lossy(&live.stdout),
        String::from_utf8_lossy(&live.stderr)
    );
    assert!(
        live_msg.contains("executing on this daemon"),
        "live rejection should say the invocation is currently executing:\n{live_msg}"
    );

    daemon.sigkill();

    let mut restarted = Daemon::spawn(scratch, nats_url, "daemon-second.log");
    restarted
        .await_log("Runtime ready", Duration::from_secs(30))
        .await;
    restarted.pair(scratch);

    // Wait for the classification on the operator's own surface — the
    // same view a human triaging the crash would read.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let list = run_fq(scratch, nats_url, &["invocation", "list"]);
        if String::from_utf8_lossy(&list.stdout).contains("ambiguous") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "invocation never classified ambiguous after crash+restart\n--- daemon log ---\n{}",
            restarted.log()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    (restarted, invocation_id)
}

/// Resume, retrying while the just-crashed worker still reads
/// `alive`: ambiguous classification lands before the staleness
/// sweep flips the dead worker, so an operator (and this test)
/// resuming promptly may need one beat before the guard releases.
async fn resume_released(scratch: &Scratch, nats_url: &str, invocation_id: &str) -> Output {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let out = run_fq(
            scratch,
            nats_url,
            &[
                "invocation",
                "resume",
                invocation_id,
                "--reason",
                "e2e test",
            ],
        );
        let msg = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if out.status.success()
            || !msg.contains("executing on this daemon")
            || Instant::now() >= deadline
        {
            return out;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The #373 acceptance scenario: crash → Ambiguous → resume →
/// completes under its own steam, with the injected disclosure
/// asserted on the wire.
#[tokio::test(flavor = "multi_thread")]
async fn resume_recovers_ambiguous_invocation_end_to_end() {
    let server = fq_test_support::NatsServer::start();
    let nats_url = server.url().to_string();
    let mock = MockAnthropicServer::start().await;
    let scratch = Scratch::new("e2e", mock.base_url());

    let (mut daemon, invocation_id) = crash_into_ambiguous(&scratch, &nats_url, &mock).await;

    // Turn 2 (post-resume): the model, told its exec was interrupted,
    // declares the outcome — `report_outcome` is the terminal
    // declaration (a bare text turn would just be asked for another
    // turn), and the harness completes the invocation on it.
    mock.push_response(MockResponse::tool_use(
        "toolu_probe_2",
        "report_outcome",
        json!({
            "status": "success",
            "summary": "Verified the interrupted exec left no partial state; done.",
        }),
        10,
        5,
    ));

    let resume = resume_released(&scratch, &nats_url, &invocation_id).await;
    assert!(
        resume.status.success(),
        "resume failed on an ambiguous invocation: {}\n{}",
        String::from_utf8_lossy(&resume.stdout),
        String::from_utf8_lossy(&resume.stderr)
    );

    daemon
        .await_log("reducer invocation completed", Duration::from_secs(60))
        .await;

    // The wire oracle: the second model request must carry the
    // synthetic interrupted result — as a tool_result tied to the
    // stuck call id, with the disclosure text intact. This is the
    // injection contract; logs proving it happened are not enough.
    let requests = mock.received_requests();
    assert_eq!(
        requests.len(),
        2,
        "expected exactly two model calls (crash turn + post-resume turn)"
    );
    let second = requests[1].to_string();
    assert!(
        second.contains("interrupted by a runtime crash"),
        "post-resume model request lacks the interrupted-result notice:\n{second}"
    );
    assert!(
        second.contains("toolu_probe_1"),
        "injected result is not tied to the stuck tool_use id:\n{second}"
    );

    // The audit trail: operator_resumed is on the record for this
    // invocation, and the terminal state is completed — via the
    // product's own operator surfaces, not the store.
    let show = run_fq(&scratch, &nats_url, &["invocation", "show", &invocation_id]);
    let show_text = String::from_utf8_lossy(&show.stdout).to_string();
    assert!(
        show_text.contains("operator_resumed"),
        "invocation show lacks the operator_resumed audit event:\n{show_text}"
    );
    assert!(
        show_text.contains("completed"),
        "invocation did not reach completed after resume:\n{show_text}"
    );

    // The same event through the query surface (#373 acceptance):
    // `fq events query` reads the projection without NATS.
    let events = run_fq(
        &scratch,
        &nats_url,
        &[
            "events",
            "query",
            "--agent",
            "resume-probe",
            "--limit",
            "100",
        ],
    );
    assert!(
        String::from_utf8_lossy(&events.stdout).contains("operator_resumed"),
        "fq events query does not surface invocation.operator_resumed:\n{}",
        String::from_utf8_lossy(&events.stdout)
    );

    // Resuming a completed invocation must be a clean, explanatory
    // error — not a second injection.
    let again = run_fq(
        &scratch,
        &nats_url,
        &["invocation", "resume", &invocation_id],
    );
    assert!(
        !again.status.success(),
        "second resume of a completed invocation must fail"
    );

    daemon.stop();
    mock.shutdown().await;
}

/// The operator-error matrix: unknown ids and terminal states must be
/// rejected with distinct errors and zero side effects — resume is
/// precondition-gated, unlike drop's kill-switch (#107 lesson).
#[tokio::test(flavor = "multi_thread")]
async fn resume_rejects_unknown_and_dropped_invocations() {
    let server = fq_test_support::NatsServer::start();
    let nats_url = server.url().to_string();
    let mock = MockAnthropicServer::start().await;
    let scratch = Scratch::new("matrix", mock.base_url());

    let (mut daemon, invocation_id) = crash_into_ambiguous(&scratch, &nats_url, &mock).await;

    // Unknown id: rejected, and the message says so.
    let unknown = run_fq(
        &scratch,
        &nats_url,
        &[
            "invocation",
            "resume",
            "00000000-0000-7000-8000-000000000000",
        ],
    );
    assert!(
        !unknown.status.success(),
        "resume of an unknown id must fail"
    );

    // Drop wins: once the operator has issued the terminal transition,
    // resume must refuse — the no-downgrade contract seen from the
    // other side.
    let drop_out = run_fq(
        &scratch,
        &nats_url,
        &[
            "invocation",
            "drop",
            &invocation_id,
            "--reason",
            "matrix test",
        ],
    );
    assert!(
        drop_out.status.success(),
        "drop of an ambiguous invocation failed: {}",
        String::from_utf8_lossy(&drop_out.stderr)
    );

    // `drop` publishes; the coordination consumer applies. Wait for the
    // decision to be OBSERVABLE before asserting resume refuses it —
    // otherwise this races the consumer and passes only when it happens
    // to be keeping up. (That the window exists at all is its own bug,
    // filed separately: a resume issued inside it is accepted.)
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let show = run_fq(&scratch, &nats_url, &["invocation", "show", &invocation_id]);
        let text = String::from_utf8_lossy(&show.stdout).to_string();
        if text
            .lines()
            .any(|l| l.trim_start().starts_with("status:") && l.contains("failed"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "drop never became observable:\n{text}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let resume_dropped = run_fq(
        &scratch,
        &nats_url,
        &["invocation", "resume", &invocation_id],
    );
    assert!(
        !resume_dropped.status.success(),
        "resume after drop must fail — operator terminal decisions are final"
    );
    let msg = format!(
        "{}{}",
        String::from_utf8_lossy(&resume_dropped.stdout),
        String::from_utf8_lossy(&resume_dropped.stderr)
    );
    assert!(
        !msg.contains("panicked"),
        "resume-after-drop must be a clean error, not a crash:\n{msg}"
    );

    // No injection happened: the mock never saw a second model call.
    assert_eq!(
        mock.received_requests().len(),
        1,
        "rejected resumes must not reach the model"
    );

    daemon.stop();

    // Daemon down: the verb must fail cleanly and promptly, telling
    // the operator there is no daemon — not hang and not panic.
    let down = run_fq(
        &scratch,
        &nats_url,
        &["invocation", "resume", &invocation_id],
    );
    assert!(
        !down.status.success(),
        "resume with no daemon running must fail"
    );
    let down_msg = format!(
        "{}{}",
        String::from_utf8_lossy(&down.stdout),
        String::from_utf8_lossy(&down.stderr)
    );
    assert!(
        !down_msg.contains("panicked"),
        "daemon-down resume must be a clean error:\n{down_msg}"
    );

    // The #374 resurrection regression: restart after the drop. The
    // startup reconciliation must close the worker row from the
    // authoritative owner status BEFORE recovery classifies, so the
    // dropped invocation neither re-reports ambiguous nor renders a
    // live execution — on any restart, forever.
    let mut third = Daemon::spawn(&scratch, &nats_url, "daemon-third.log");
    third
        .await_log("Runtime ready", Duration::from_secs(30))
        .await;
    third.pair(&scratch);
    let list = run_fq(&scratch, &nats_url, &["invocation", "list"]);
    let list_text = String::from_utf8_lossy(&list.stdout).to_string();
    assert!(
        !list_text.contains("ambiguous"),
        "dropped invocation resurrected as ambiguous after restart:\n{list_text}"
    );
    assert!(
        list_text.contains("failed"),
        "dropped invocation lost its terminal status after restart:\n{list_text}"
    );
    let show = run_fq(&scratch, &nats_url, &["invocation", "show", &invocation_id]);
    let show_text = String::from_utf8_lossy(&show.stdout).to_string();
    assert!(
        !show_text.contains("Live execution"),
        "dropped invocation still renders a live execution after restart:\n{show_text}"
    );
    third.stop();

    mock.shutdown().await;
}

/// A failed post-injection re-drive must FAIL the invocation cleanly
/// (terminal, visible, re-resume rejected) — never leave it limbo.
/// The injection itself still succeeds and is durable; the verb
/// reports it; the model failure surfaces in the daemon log; and the
/// terminal state closes the recovery loop without another operator
/// action being possible or needed. (Byte-identical replay of the
/// injected result is pinned at the runner seam:
/// `injected_interrupted_result_reaches_replay_byte_identical`.)
#[tokio::test(flavor = "multi_thread")]
async fn failed_redrive_fails_the_invocation_cleanly() {
    let server = fq_test_support::NatsServer::start();
    let nats_url = server.url().to_string();
    let mock = MockAnthropicServer::start().await;
    let scratch = Scratch::new("replay", mock.base_url());

    let (mut daemon, invocation_id) = crash_into_ambiguous(&scratch, &nats_url, &mock).await;

    // Nothing queued: the detached re-drive exhausts its model retries.
    // The verb still succeeds — it reports the INJECTION, which is
    // already durable by the time it replies.
    let resume = resume_released(&scratch, &nats_url, &invocation_id).await;
    assert!(
        resume.status.success(),
        "resume (the injection) should succeed even though the model is down: {}",
        String::from_utf8_lossy(&resume.stderr)
    );
    daemon
        .await_log("operator resume failed", Duration::from_secs(60))
        .await;

    // The invocation lands terminal-failed on the operator surface —
    // not limbo, not ambiguous, not silently gone.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let list = run_fq(&scratch, &nats_url, &["invocation", "list"]);
        if String::from_utf8_lossy(&list.stdout).contains("failed") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "invocation never reached failed after the re-drive died\n--- daemon log ---\n{}",
            daemon.log()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Terminal means terminal: no second resume.
    let again = run_fq(
        &scratch,
        &nats_url,
        &["invocation", "resume", &invocation_id],
    );
    assert!(
        !again.status.success(),
        "resume of a terminally-failed invocation must be rejected"
    );

    daemon.stop();
    mock.shutdown().await;
}

/// #107 live-drop contract: the runner is the liveness authority, so a bare
/// drop is side-effect free while active; explicit --live lets the current
/// tool finish (matching drain semantics), then stops before the next step.
#[tokio::test(flavor = "multi_thread")]
async fn live_drop_requires_opt_in_halts_and_stays_terminal_after_restart() {
    let server = fq_test_support::NatsServer::start();
    let nats_url = server.url().to_string();
    let mock = MockAnthropicServer::start().await;
    let scratch = Scratch::new("live-drop", mock.base_url());

    mock.push_response(MockResponse::tool_use(
        "toolu_live_drop",
        "builtin__exec",
        json!({
            "command": ["sleep", "5"],
            "cwd": scratch.path("workspace"),
        }),
        10,
        5,
    ));
    // Queued but must NEVER be consumed: if the halt really preempts at
    // the boundary, the invocation never asks for another turn. Without
    // this the mock would be exhausted and the invocation would die on
    // its own — making "it stopped" indistinguishable from "it ran out".
    mock.push_response(MockResponse::text(
        "this turn must never be requested",
        10,
        5,
    ));

    let mut daemon = Daemon::spawn(&scratch, &nats_url, "daemon-live-drop.log");
    daemon
        .await_log("Runtime ready", Duration::from_secs(30))
        .await;
    daemon.pair(&scratch);
    let nats = async_nats::connect(&nats_url)
        .await
        .expect("connect broker");
    nats.publish(
        "fq.trigger.resume-probe",
        serde_json::to_vec(&json!("run the probe")).unwrap().into(),
    )
    .await
    .expect("publish trigger");
    nats.flush().await.expect("flush trigger");
    daemon
        .await_log(
            "model produced tool calls; dispatching",
            Duration::from_secs(30),
        )
        .await;
    let invocation_id = invocation_id_from_log(&daemon.log());

    let bare = run_fq(&scratch, &nats_url, &["invocation", "drop", &invocation_id]);
    assert!(!bare.status.success(), "bare live drop must be rejected");
    let bare_msg = format!(
        "{}{}",
        String::from_utf8_lossy(&bare.stdout),
        String::from_utf8_lossy(&bare.stderr)
    );
    assert!(bare_msg.contains("currently running"), "{bare_msg}");
    assert!(bare_msg.contains("--live"), "{bare_msg}");
    assert!(
        !run_fq(&scratch, &nats_url, &["invocation", "show", &invocation_id])
            .stdout
            .windows("operator_recovered".len())
            .any(|w| w == b"operator_recovered"),
        "rejected bare drop published a terminal audit event"
    );

    let forced = run_fq(
        &scratch,
        &nats_url,
        &[
            "invocation",
            "drop",
            &invocation_id,
            "--live",
            "--reason",
            "test halt",
        ],
    );
    assert!(
        forced.status.success(),
        "forced live drop failed: {}",
        String::from_utf8_lossy(&forced.stderr)
    );
    daemon
        .await_log(
            "operator halt — stopping invocation at step boundary",
            Duration::from_secs(30),
        )
        .await;

    // The preemption proof: the second response is still sitting in the
    // queue, so only the opening turn was ever requested.
    assert_eq!(
        mock.received_requests().len(),
        1,
        "the halt did not preempt — the invocation consumed another model turn"
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let show = run_fq(&scratch, &nats_url, &["invocation", "show", &invocation_id]);
        let text = String::from_utf8_lossy(&show.stdout).to_string();
        // The status LINE, not merely the word somewhere in the output
        // (`archived:` and event rows carry it too).
        if text
            .lines()
            .any(|l| l.trim_start().starts_with("status:") && l.contains("failed"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "live drop never landed terminal:\n{text}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    daemon.stop();
    let mut restarted = Daemon::spawn(&scratch, &nats_url, "daemon-live-drop-restart.log");
    restarted
        .await_log("Runtime ready", Duration::from_secs(30))
        .await;
    restarted.pair(&scratch);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        !restarted.log().contains("resuming invocation"),
        "dropped invocation resurrected after restart:\n{}",
        restarted.log()
    );
    let resume = run_fq(
        &scratch,
        &nats_url,
        &["invocation", "resume", &invocation_id],
    );
    assert!(!resume.status.success(), "terminal live-drop was resumable");

    restarted.stop();
    mock.shutdown().await;
}
