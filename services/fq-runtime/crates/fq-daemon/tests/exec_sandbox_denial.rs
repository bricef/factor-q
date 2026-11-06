//! The exec sandbox, end to end through a spawned daemon: an agent
//! granted `builtin__exec` only inside one directory is handed an exec
//! call whose `cwd` is somewhere else, and the runtime must refuse it.
//!
//! The call comes from a scripted mock LLM, not a real model. This
//! test used to live in the live smoke suite (`tests/smoke/smoke.sh`),
//! where it could only run if gpt-4o-mini chose to make the forbidden
//! call — and on 2026-11-01 it didn't: it predicted the refusal and
//! reported failure without calling the tool, so the run had nothing
//! to check. The property under test is the sandbox's, and the model
//! contributes nothing to it but that chance of not asking.
//!
//! What this covers that the unit and reducer tests do not: the
//! `sandbox.exec_cwd` an agent *file* declares reaching the exec tool
//! inside the real `fqd`, with the refusal then recorded where an
//! operator reads it. `exec.rs` proves the tool refuses a hand-built
//! sandbox; `tool_sandbox_violation_surfaces_on_reducer_path` proves
//! the runner's classification, for `file_read`. Neither loads an
//! agent definition or runs the daemon.
//!
//! What it does not cover: containment. The exec sandbox is a `cwd`
//! gate — a command run from an allowed `cwd` can still write anywhere
//! the daemon's user can, which `exec.rs` documents among the gaps
//! that only OS-level isolation closes (ADR-0010).
//!
//! Isolation follows invocation_resume.rs: the test spawns its own
//! nats-server (#233), its own mock and its own daemon over a scratch
//! config.

#![cfg(unix)]

use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use fq_runtime::test_support::mock_anthropic::{MockAnthropicServer, MockResponse};
use fq_test_support::TestChild;
use serde_json::{Value, json};

const AGENT: &str = "sandbox-probe";
const DENIED_CALL_ID: &str = "toolu_sandbox_1";
/// What the denied command would create — in `outside/`, if it ran.
const MARKER: &str = "marker";

fn fqd_binary() -> &'static str {
    env!("CARGO_BIN_EXE_fqd")
}

fn fq_client_binary() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_fqd")).with_file_name("fq")
}

/// Scratch layout: config, agents, cache, state, and the two
/// directories the scenario is about — `allowed/`, the agent's only
/// `exec_cwd`, and `outside/`, where the mock asks to run.
struct Scratch {
    root: std::path::PathBuf,
    dir: Option<tempfile::TempDir>,
    mock_base_url: String,
}

impl Scratch {
    fn new(mock_base_url: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix(&format!("fq-exec-sandbox-{}-", std::process::id()))
            .tempdir_in(std::env::temp_dir())
            .expect("create scratch directory");
        let root = dir.path().to_path_buf();
        for sub in ["cache", "agents", "xdg", "allowed", "outside"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        std::fs::write(
            root.join("agents").join(format!("{AGENT}.md")),
            format!(
                "---\nname: {AGENT}\nmodel: claude-haiku-4-5\ntools:\n  - builtin__exec\n\
                 sandbox:\n  exec_cwd:\n    - {}\nbudget: 1.00\n---\n\n\
                 Test probe agent. Run the command you are told to run.\n",
                root.join("allowed").display()
            ),
        )
        .unwrap();
        let scratch = Self {
            root,
            dir: Some(dir),
            mock_base_url: mock_base_url.to_string(),
        };
        // Port 0, then pinned to what the daemon bound — see
        // invocation_resume.rs's `Scratch::new` for why a port picked
        // in advance is a race.
        scratch.write_config("127.0.0.1:0");
        scratch
    }

    fn path(&self, rel: &str) -> std::path::PathBuf {
        self.root.join(rel)
    }

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
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let path = self.dir.take().expect("scratch guard present").keep();
            eprintln!("test failed; scratch preserved at {}", path.display());
        }
    }
}

/// Run an `fq` CLI verb against this test's daemon, returning stdout.
/// Panics with both streams on a non-zero exit: every verb here is
/// expected to succeed.
fn run_fq(scratch: &Scratch, nats_url: &str, args: &[&str]) -> String {
    let out: Output = Command::new(fq_client_binary())
        .args(args)
        .env("FQ_CLI_CONFIG", scratch.path("fq.toml"))
        .env("FQ_NATS_URL", nats_url)
        .env("FQ_CACHE_DIR", scratch.path("cache"))
        .env("FQ_STATE_DIR", scratch.path("state"))
        .env("FQ_AGENTS_DIR", scratch.path("agents"))
        .env("XDG_CONFIG_HOME", scratch.path("xdg"))
        .env("ANTHROPIC_API_KEY", "test-key-unused-by-mock")
        .output()
        .expect("run fq CLI");
    assert!(
        out.status.success(),
        "fq {args:?} failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

struct Daemon {
    child: TestChild,
    log_path: std::path::PathBuf,
}

impl Daemon {
    fn spawn(scratch: &Scratch, nats_url: &str) -> Self {
        let log_path = scratch.path("daemon.log");
        let log = std::fs::File::create(&log_path).expect("create daemon log");
        let log_err = log.try_clone().expect("clone log handle");
        let child = TestChild::builder(fqd_binary())
            .env("FQ_DAEMON_CONFIG", scratch.path("fq.toml"))
            .env("FQ_NATS_URL", nats_url)
            .env("FQ_CACHE_DIR", scratch.path("cache"))
            .env("FQ_STATE_DIR", scratch.path("state"))
            .env("FQ_AGENTS_DIR", scratch.path("agents"))
            .env("ANTHROPIC_API_KEY", "test-key-unused-by-mock")
            .env("FQ_LOG_FORMAT", "json")
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .spawn();
        Self { child, log_path }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
    }

    /// Poll the daemon log until `needle` appears; panic with the log
    /// if the daemon exits or the deadline passes first.
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

    /// Pair the CLI with this daemon's edge, which the read verbs
    /// below speak. The address exists only in the log (the kernel
    /// chose it); token and fingerprint come from the files the daemon
    /// wrote beside its identity (#545).
    fn pair(&self, scratch: &Scratch) {
        let log = self.log();
        let addr = log
            .lines()
            .find_map(|l| l.trim().strip_prefix("- edge is listening on "))
            .expect("edge addr in daemon log")
            .trim()
            .to_string();
        scratch.write_config(&addr);
        let token = fq_test_support::admin_token(&scratch.path("state"));
        let fingerprint = fq_test_support::edge_fingerprint(&scratch.path("state"));
        let out = Command::new(fq_client_binary())
            .args([
                "connect",
                &addr,
                "--token",
                &token,
                "--fingerprint",
                &fingerprint,
            ])
            .env("FQ_CLI_CONFIG", scratch.path("fq.toml"))
            .env("XDG_CONFIG_HOME", scratch.path("xdg"))
            .stdin(Stdio::piped())
            .output()
            .expect("run fq connect");
        assert!(
            out.status.success(),
            "fq connect failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn stop(&mut self) {
        let _ = self.child.signal(libc::SIGTERM);
        let _ = self.child.wait_timeout(Duration::from_secs(15));
    }
}

/// Every content block of every message in a recorded model request.
fn content_blocks(request: &Value) -> impl Iterator<Item = &Value> {
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["content"].as_array())
        .flatten()
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_outside_exec_cwd_is_refused_and_recorded() {
    let server = fq_test_support::NatsServer::start();
    let nats_url = server.url().to_string();
    let mock = MockAnthropicServer::start().await;
    let scratch = Scratch::new(mock.base_url());
    let outside = scratch.path("outside");

    // Turn 1: the forbidden call — a write, so "did it run" has an
    // answer on disk. Turn 2: the agent, shown the refusal, gives up;
    // `report_outcome` is the terminal declaration.
    mock.push_response(MockResponse::tool_use(
        DENIED_CALL_ID,
        "builtin__exec",
        json!({"command": ["touch", MARKER], "cwd": outside}),
        10,
        5,
    ));
    mock.push_response(MockResponse::tool_use(
        "toolu_sandbox_2",
        fq_runtime::tools::REPORT_OUTCOME_CANONICAL_NAME,
        json!({"status": "blocked", "summary": "The exec sandbox refused that directory."}),
        10,
        5,
    ));

    let mut daemon = Daemon::spawn(&scratch, &nats_url);
    daemon
        .await_log("- edge is listening on ", Duration::from_secs(30))
        .await;
    daemon.pair(&scratch);

    let nats = async_nats::connect(&nats_url)
        .await
        .expect("connect to test broker");
    nats.publish(
        format!("fq.trigger.{AGENT}"),
        serde_json::to_vec(&json!("touch the marker"))
            .unwrap()
            .into(),
    )
    .await
    .expect("publish trigger");
    nats.flush().await.expect("flush trigger publish");

    daemon
        .await_log("reducer invocation completed", Duration::from_secs(60))
        .await;

    // The command never ran.
    assert!(
        !outside.join(MARKER).exists(),
        "the denied exec ran: {} exists",
        outside.join(MARKER).display()
    );

    // The model was shown the refusal: its second request answers the
    // denied call with a tool result, rather than the invocation dying
    // on it.
    let requests = mock.received_requests();
    assert_eq!(
        requests.len(),
        2,
        "expected the denied turn and the outcome turn"
    );
    assert!(
        content_blocks(&requests[1])
            .any(|b| b["type"] == "tool_result" && b["tool_use_id"] == DENIED_CALL_ID),
        "the second model request carries no tool_result for {DENIED_CALL_ID}:\n{}",
        requests[1]
    );

    // The structured marker, where an operator reads it. Polled: the
    // events come through the projection, which a NATS consumer fills
    // on its own schedule, and the completion log line says nothing
    // about whether it has caught up.
    let deadline = Instant::now() + Duration::from_secs(30);
    let tool_results: Vec<Value> = loop {
        let out = run_fq(
            &scratch,
            &nats_url,
            &[
                "events",
                "query",
                "--agent",
                AGENT,
                "--event-type",
                "tool_result",
                "--json",
            ],
        );
        let rows: Vec<Value> = serde_json::from_str(&out).expect("events query --json parses");
        if !rows.is_empty() {
            break rows;
        }
        assert!(
            Instant::now() < deadline,
            "no tool_result event projected within 30s\n--- daemon log ---\n{}",
            daemon.log()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(
        tool_results.len(),
        1,
        "one tool call, one result: {tool_results:#?}"
    );
    // A cwd outside `exec_cwd` is a sandbox refusal. `permission_denied`
    // is the other denial — a tool the agent was never granted — and is
    // not interchangeable with this one.
    assert_eq!(
        tool_results[0]["error_kind"], "sandbox_violation",
        "the denied exec is not recorded as a sandbox violation: {:#}",
        tool_results[0]
    );

    // A refusal is an answer, not a wedge: the invocation completes.
    let invocations: Vec<Value> = serde_json::from_str(&run_fq(
        &scratch,
        &nats_url,
        &["invocation", "list", "--json", "--include-archived"],
    ))
    .expect("invocation list --json parses");
    let ours: Vec<&Value> = invocations
        .iter()
        .filter(|i| i["agent_id"] == AGENT)
        .collect();
    assert_eq!(
        ours.len(),
        1,
        "one trigger, one invocation: {invocations:#?}"
    );
    assert_eq!(ours[0]["status"], "completed", "{:#}", ours[0]);

    daemon.stop();
    mock.shutdown().await;
}
