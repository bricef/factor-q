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
        .env("FQ_CLI_CONFIG", "/nonexistent/fq.toml")
        .env("FQ_AGENTS_DIR", "/nonexistent/agents")
        .env("FQ_CACHE_DIR", "/nonexistent/cache")
        .env("FQ_STATE_DIR", "/nonexistent/state")
        // The pairing store is user-side, under XDG_CONFIG_HOME, and
        // it is not covered by the four above — so without this a
        // flipped verb here would dial whatever daemon the developer
        // happens to be paired with, and pass or fail by whether one
        // was running. Every verb these tests reach is flipped now.
        .env("XDG_CONFIG_HOME", "/nonexistent/config")
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
        "doctor",
        "init",
        "trigger",
        "agent",
    ] {
        assert!(
            stdout.contains(needle),
            "expected `{needle}` in `fq --help` output; got: {stdout}"
        );
    }
}

/// `--limit -1` no longer means "no limit", and says so before it
/// costs anybody anything.
///
/// It used to be mapped to `u32::MAX` — SQLite's reading of a negative
/// LIMIT, preserved from when this query opened `projection.db`
/// itself. That read now happens in the daemon, where an unbounded
/// page is the whole projection table materialised in memory and then
/// a response frame too large to send, so the operator paid for the
/// scan and got a transport error back. There is nothing left for
/// "no limit" to mean, and pretending otherwise is the lie.
///
/// The refusal is local — no daemon, no connection, nothing allocated
/// — and it names both the cap and the way past it, so the operator's
/// next command is one edit away rather than a guess. `fq events
/// query` is the one verb where a wrong `--limit` used to be
/// expensive, which is why the check runs before the request leaves.
#[test]
fn fq_events_query_refuses_an_unbounded_limit() {
    let (exit, _stdout, stderr) = run_fq(
        // `--limit=-1`, attached: clap reads a bare `-1` as a flag.
        &["events", "query", "--limit=-1"],
        Duration::from_secs(5),
    );
    assert_eq!(exit, Some(1), "an unbounded page must be refused; {stderr}");
    for needle in [
        // What was asked for, the cap, and the two ways to get more
        // than one page.
        "-1", "2000", "--since", "tail",
    ] {
        assert!(
            stderr.contains(needle),
            "expected `{needle}` in the refusal; got: {stderr}"
        );
    }
    // Refused, not quietly turned into a page of some size the
    // operator never asked for.
    assert!(
        !stderr.contains("No events matched"),
        "an unbounded page must not be answered at all; got: {stderr}"
    );
}

/// A `--limit` too large to travel is refused, not saturated into one
/// the operator never typed.
///
/// `fq dead-letters list --limit` is a `usize` and the wire contract a
/// `u32`, so the values that cannot travel are exactly those above
/// `u32::MAX` — nothing negative can be typed for a `usize`, and clap
/// rejects the attempt itself. Those used to be clamped to `u32::MAX`,
/// on the reading that a limit past four billion asks for everything a
/// page can hold. Nothing holds everything, and `u32::MAX` is now over
/// the cap besides, so the clamp bought the operator a refusal quoting
/// a number that was never theirs.
///
/// The refusal is local — no daemon, no connection — and names the cap
/// and the narrowing, so the next command is an edit rather than a
/// guess. (Only reachable where `usize` is wider than `u32`, which is
/// every target this ships on.)
#[test]
fn fq_dead_letters_list_refuses_a_limit_too_large_to_travel() {
    let (exit, _stdout, stderr) = run_fq(
        &["dead-letters", "list", "--limit", "4294967296"],
        Duration::from_secs(5),
    );
    assert_eq!(
        exit,
        Some(1),
        "a limit that cannot travel must be refused; {stderr}"
    );
    for needle in [
        // What was asked for, the cap, and the narrowing that serves
        // more than a page.
        "4294967296",
        "500",
        "--agent",
    ] {
        assert!(
            stderr.contains(needle),
            "expected `{needle}` in the refusal; got: {stderr}"
        );
    }
    // Refused, not quietly turned into a page of some size the
    // operator never asked for.
    assert!(
        !stderr.contains("4294967295"),
        "the operator's number must not be swapped for a saturated one; got: {stderr}"
    );
}

#[test]
fn fq_drain_is_an_unrecognized_subcommand() {
    let (exit, _stdout, stderr) = run_fq(&["drain"], Duration::from_secs(5));
    assert_ne!(exit, Some(0), "removed drain subcommand should fail");
    assert!(
        stderr.contains("unrecognized subcommand"),
        "expected clap unrecognized-subcommand error; got: {stderr}"
    );
}

#[test]
fn fq_invocation_help_lists_subcommands() {
    let (exit, stdout, stderr) = run_fq(&["invocation", "--help"], Duration::from_secs(5));
    assert_eq!(
        exit,
        Some(0),
        "fq invocation --help should exit 0; stderr: {stderr}"
    );
    for needle in ["list", "show", "drop", "transcript"] {
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
fn fq_status_against_an_unreachable_daemon_fails_gracefully() {
    // The client's only dependency is the daemon's edge — it speaks to
    // no broker and opens no store — so this points at a port that is
    // reliably refused and exercises the connection-failure path.
    let mut child = Command::new(fq_binary())
        .args(["--addr", "127.0.0.1:1", "status"])
        .env("FQ_CLI_CONFIG", "/nonexistent/fq.toml")
        .env("FQ_AGENTS_DIR", "/nonexistent/agents")
        .env("FQ_CACHE_DIR", "/nonexistent/cache")
        .env("FQ_STATE_DIR", "/nonexistent/state")
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

#[test]
fn fq_invocation_transcript_help_parses() {
    let (exit, stdout, stderr) = run_fq(
        &["invocation", "transcript", "--help"],
        Duration::from_secs(5),
    );
    assert_eq!(
        exit,
        Some(0),
        "fq invocation transcript --help should exit 0; stderr: {stderr}"
    );
    // The flags the operator surface depends on are documented.
    for needle in ["--follow", "--format", "--full"] {
        assert!(
            stdout.contains(needle),
            "expected `{needle}` in transcript --help; got: {stdout}"
        );
    }
}

#[test]
fn fq_invocation_transcript_missing_db_exits_nonzero_without_panic() {
    // FQ_CACHE_DIR points at a nonexistent path, so the per-store
    // databases are absent. The command must exit non-zero with an actionable
    // error, not panic.
    let (exit, _stdout, stderr) = run_fq(
        &[
            "invocation",
            "transcript",
            "00000000-0000-0000-0000-000000000000",
        ],
        Duration::from_secs(5),
    );
    assert_eq!(
        exit,
        Some(1),
        "missing-db transcript should exit 1; stderr: {stderr}"
    );
    assert!(
        !stderr.contains("panicked"),
        "must not panic; stderr: {stderr}"
    );
}

#[test]
fn fq_doctor_help_lists_flags() {
    let (exit, stdout, stderr) = run_fq(&["doctor", "--help"], Duration::from_secs(5));
    assert_eq!(
        exit,
        Some(0),
        "fq doctor --help should exit 0; stderr: {stderr}"
    );
    for needle in ["--json", "--fail-on-issues"] {
        assert!(
            stdout.contains(needle),
            "expected `{needle}` in `fq doctor --help`; got: {stdout}"
        );
    }
}

#[test]
fn fq_doctor_without_a_daemon_exits_nonzero_without_panic() {
    // `fq doctor` asks the daemon for the health it reports on
    // (`control.doctor`), and nothing here is paired with one, so it
    // must exit non-zero with an actionable error rather than panic or
    // hang. It used to read the per-store databases directly and this
    // test withheld those instead; the failure it pins is the same
    // one, at the layer the verb now fails at.
    let (exit, _stdout, stderr) = run_fq(&["doctor"], Duration::from_secs(5));
    assert_eq!(
        exit,
        Some(1),
        "daemon-less doctor should exit 1; stderr: {stderr}"
    );
    assert!(
        !stderr.contains("panicked"),
        "must not panic; stderr: {stderr}"
    );
}
