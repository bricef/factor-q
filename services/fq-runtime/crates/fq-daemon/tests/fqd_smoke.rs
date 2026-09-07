//! `fqd` is the daemon and nothing else — but it must be the *same*
//! daemon `fqd` starts: shared code, shared behaviour. This smoke
//! proves the new binary reaches steady state and drains cleanly on
//! SIGTERM, exactly like `daemon_shutdown.rs` proves for `fqd`.

#![cfg(unix)]

use std::process::Stdio;

use fq_test_support::TestChild;
use std::time::{Duration, Instant};

fn unique_scratch() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("fqd-smoke-{}-{}", std::process::id(), nanos));
    std::fs::create_dir_all(dir.join("cache")).unwrap();
    std::fs::create_dir_all(dir.join("agents")).unwrap();
    // The edge is on by default; an ephemeral port keeps parallel
    // daemon-spawning tests from fighting over the fixed default bind.
    std::fs::write(dir.join("fq.toml"), "[edge]\nbind = \"127.0.0.1:0\"\n").unwrap();
    dir
}

#[test]
fn fqd_reaches_steady_state_and_drains_on_sigterm() {
    let server = fq_test_support::NatsServer::start();
    let scratch = unique_scratch();
    let log_path = scratch.join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let log_err = log.try_clone().expect("clone daemon log handle");

    let mut child = TestChild::builder(env!("CARGO_BIN_EXE_fqd"))
        .env("FQ_DAEMON_CONFIG", scratch.join("fq.toml"))
        .env("FQ_NATS_URL", server.url())
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_STATE_DIR", scratch.join("state"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn();

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut ready = false;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll fqd") {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            panic!("fqd exited during startup with {status:?}\n--- log ---\n{log}");
        }
        if std::fs::read_to_string(&log_path)
            .unwrap_or_default()
            .contains("Runtime ready")
        {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(ready, "fqd never reached 'Runtime ready' within 30s");

    child.signal(libc::SIGTERM).expect("kill(SIGTERM) failed");

    let status = child
        .wait_timeout(Duration::from_secs(15))
        .expect("fqd did not exit within 15s of SIGTERM");
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&scratch);

    assert!(
        status.success(),
        "expected clean exit(0) on SIGTERM, got {status:?}\n--- log ---\n{log}"
    );
    assert!(
        log.contains("Received SIGTERM, draining..."),
        "fqd did not take the drain path — is it running the shared daemon code?\n--- log ---\n{log}"
    );
    assert!(
        log.contains("edge is listening on"),
        "the edge is on by default and must reach steady state with the daemon\n--- log ---\n{log}"
    );
}

/// The Phase 1 exit criterion's "hung MCP server at boot" class, against
/// the real binary.
///
/// An agent declares a shared MCP server that accepts its stdin and
/// never answers `initialize` — `sleep`, which is exactly the shape of
/// the remote server that used to freeze `fqd` at startup with every
/// agent down. The daemon must reach `Runtime ready` anyway, within one
/// start-up deadline, and say which server it gave up on.
///
/// The manager-level fault injection lives in `fq_runtime::mcp`; this
/// test exists because "boot never blocks" is a claim about the
/// *process*, and only starting one can settle it.
#[test]
fn fqd_boots_with_a_hung_mcp_server_and_reports_it_unavailable() {
    let server = fq_test_support::NatsServer::start();
    let scratch = unique_scratch();
    // A three-second handshake deadline and no retry: the assertion is
    // about boot, and a retry loop would only add noise to the log.
    std::fs::write(
        scratch.join("fq.toml"),
        "[edge]\nbind = \"127.0.0.1:0\"\n\n\
         [mcp]\nstartup_timeout_secs = 3\nretry_initial_secs = 0\n\n\
         [providers.anthropic]\nmodels = [\"claude-haiku-4-5\"]\n\n\
         [providers.anthropic.pricing.\"claude-haiku-4-5\"]\n\
         input_per_mtok = 1.0\noutput_per_mtok = 5.0\n",
    )
    .unwrap();
    std::fs::write(
        scratch.join("agents").join("needs-wedged.md"),
        "---\nname: needs-wedged\nmodel: claude-haiku-4-5\nmcp:\n  \
         - server: wedged\n    command: sleep\n    args:\n      - \"120\"\n---\n\n\
         An agent whose MCP server never answers.\n",
    )
    .unwrap();

    let log_path = scratch.join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let log_err = log.try_clone().expect("clone daemon log handle");
    let started = Instant::now();
    let mut child = TestChild::builder(env!("CARGO_BIN_EXE_fqd"))
        .env("FQ_DAEMON_CONFIG", scratch.join("fq.toml"))
        .env("FQ_NATS_URL", server.url())
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_STATE_DIR", scratch.join("state"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn();

    let deadline = Instant::now() + Duration::from_secs(45);
    let mut ready = false;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll fqd") {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            panic!("fqd exited during startup with {status:?}\n--- log ---\n{log}");
        }
        if std::fs::read_to_string(&log_path)
            .unwrap_or_default()
            .contains("Runtime ready")
        {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let to_ready = started.elapsed();

    let _ = child.signal(libc::SIGTERM);
    let _ = child.wait_timeout(Duration::from_secs(15));
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&scratch);

    assert!(
        ready,
        "fqd never reached 'Runtime ready' with one hung MCP server\n--- log ---\n{log}"
    );
    assert!(
        to_ready < Duration::from_secs(40),
        "boot took {to_ready:?} — it must not wait past the start-up deadline\
         \n--- log ---\n{log}"
    );
    assert!(
        log.contains("MCP server unavailable") && log.contains("wedged"),
        "the daemon must name the server it gave up on\n--- log ---\n{log}"
    );
}
