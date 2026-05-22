//! Subprocess smoke tests for the `fq` binary. Catches the
//! egregious binary-level regressions (CLI arg parser
//! breakage, missing imports in fq-cli, panic-on-startup)
//! that in-process tests can't see.
//!
//! Each test invokes the binary via `CARGO_BIN_EXE_fq` so
//! cargo builds it as a test fixture (no `cargo run` needed
//! at test time). Tests do NOT need NATS to be running —
//! they exercise the binary's command surface, not the live
//! runtime.

use std::process::Command;
use std::time::Duration;

/// Path to the binary that cargo built for this test crate.
fn fq_binary() -> &'static str {
    env!("CARGO_BIN_EXE_fq")
}

/// Run `fq` with the given args; return (exit_code, stdout, stderr).
/// Times out after `timeout` to avoid a hung child hanging the test
/// run. Bogus paths in the env keep this hermetic — we never read
/// the user's real fq.toml.
fn run_fq(args: &[&str], timeout: Duration) -> (Option<i32>, String, String) {
    let mut child = Command::new(fq_binary())
        .args(args)
        // Force-resolve to non-existent paths so tests don't
        // pick up the developer's real config / cache.
        .env("FQ_CONFIG", "/nonexistent/fq.toml")
        .env("FQ_AGENTS_DIR", "/nonexistent/agents")
        .env("FQ_CACHE_DIR", "/nonexistent/cache")
        // Quiet logging so stderr stays readable.
        .env("RUST_LOG", "off")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn fq binary");

    // Poll for exit with a deadline.
    let deadline = std::time::Instant::now() + timeout;
    let exit_status = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break status,
            None => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    panic!("fq {args:?} did not exit within {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    use std::io::Read;
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_string(&mut stdout);
    }
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut stderr);
    }
    (exit_status.code(), stdout, stderr)
}

#[test]
fn fq_help_lists_expected_subcommands() {
    let (exit, stdout, stderr) = run_fq(&["--help"], Duration::from_secs(5));
    assert_eq!(exit, Some(0), "fq --help should exit 0; stderr: {stderr}");
    // clap routes --help to stdout; sanity-check the
    // subcommands the operator surface depends on are
    // listed.
    for needle in [
        "invocation",
        "workers",
        "status",
        "init",
        "run",
        "trigger",
        "agent",
    ] {
        assert!(
            stdout.contains(needle),
            "expected `{needle}` in `fq --help` output; got: {stdout}"
        );
    }
}

#[test]
fn fq_invocation_help_lists_subcommands() {
    let (exit, stdout, stderr) = run_fq(&["invocation", "--help"], Duration::from_secs(5));
    assert_eq!(
        exit,
        Some(0),
        "fq invocation --help should exit 0; stderr: {stderr}"
    );
    for needle in ["list", "show", "drop"] {
        assert!(
            stdout.contains(needle),
            "expected `{needle}` in `fq invocation --help`; got: {stdout}"
        );
    }
}

#[test]
fn fq_workers_help_lists_subcommands() {
    let (exit, stdout, stderr) = run_fq(&["workers", "--help"], Duration::from_secs(5));
    assert_eq!(
        exit,
        Some(0),
        "fq workers --help should exit 0; stderr: {stderr}"
    );
    for needle in ["list", "show"] {
        assert!(
            stdout.contains(needle),
            "expected `{needle}` in `fq workers --help`; got: {stdout}"
        );
    }
}

#[test]
fn fq_status_against_bogus_nats_fails_gracefully() {
    // Use a NATS URL that's guaranteed not to be listening
    // so the binary's connection-failure path is exercised.
    // 127.0.0.1:1 is reliably refused on most systems.
    let mut child = Command::new(fq_binary())
        .args(["--nats-url", "nats://127.0.0.1:1", "status"])
        .env("FQ_CONFIG", "/nonexistent/fq.toml")
        .env("FQ_AGENTS_DIR", "/nonexistent/agents")
        .env("FQ_CACHE_DIR", "/nonexistent/cache")
        .env("RUST_LOG", "off")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn fq");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(s) => break s,
            None => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    panic!("fq status did not exit within 10s on bogus NATS");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    };
    // Either exit code is acceptable as long as it's not a
    // panic / segfault: graceful exit 0 with "✗ failed"
    // text, OR exit 1 with an anyhow-style error on stderr.
    // The point is "the binary didn't crash."
    let code = status.code();
    assert!(
        code == Some(0) || code == Some(1),
        "fq status exited with unexpected code {code:?}"
    );
}
