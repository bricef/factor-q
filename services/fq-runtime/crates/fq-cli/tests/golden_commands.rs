//! Golden-master output tests for the write/control commands —
//! completing the net that `golden.rs` opened over the reads
//! (registry+split execution plan, Phase 0).
//!
//! Same oracle discipline as `golden.rs`: drive the real binary via
//! `CARGO_BIN_EXE_fq`, snapshot stdout, byte-compare against a
//! committed golden under `tests/golden/`. Two deliberate differences,
//! both forced by these verbs *mutating* state:
//!
//! - **No shared fixture.** Every test seeds its own scratch dir and
//!   (where NATS is involved) its own private broker, so mutations
//!   cannot leak between tests and JetStream sequences are
//!   deterministic by construction (a fresh stream numbers from 1).
//! - **Runtime-minted UUIDs are redacted.** Reads render only fixture
//!   identities; commands mint fresh ones (`event_id` on a drop, the
//!   daemon's `runtime_id` on a down confirmation). [`redact`] rewrites
//!   any UUID outside the fixture set to `<UUID>`; fixture identities
//!   still compare byte-exact.
//!
//! In-process `fq trigger` is deliberately **not** snapshotted: the
//! plan schedules that mode's retirement (decision D-1), so the golden
//! contract is the `--via-nats` form only. That retirement has since
//! landed — the flag is a compatibility no-op and there is only one
//! form left — and the golden it pins is the same one.
//!
//! **Four of these verbs now need a daemon they did not need before**
//! (plan Phase 4, cohort 4.3). `reload`, `trigger` and `down` used to
//! be a bare `fq` publishing at a broker; they are declared commands on
//! the authenticated edge now, which takes a running daemon and a
//! pairing — see [`PairedDaemon`]. The harness grew; the goldens are
//! still this file's stdout contract and nothing else.
//!
//! To regenerate after an intentional output change:
//! `UPDATE_GOLDEN=1 cargo test -p fq-cli --test golden_commands` —
//! then review the diff like any other code change.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use fq_runtime::AgentId;
use fq_runtime::bus::EventBus;
use fq_runtime::dead_letter::{
    DEAD_LETTER_PAYLOAD_KEY, DEAD_LETTER_SOURCE_KEY, DEAD_LETTER_STREAM_SEQ_KEY,
    DEAD_LETTER_SUBJECT_KEY,
};
use fq_runtime::events::{Event, EventPayload, FailureKind, FailurePhase, InvocationTotals};
use serde_json::json;
use uuid::Uuid;

// ------------------------------------------------------------------
// Fixed identities, shared vocabulary with golden.rs.
// ------------------------------------------------------------------

/// Fixture epoch: 2026-01-02 03:04:05 UTC (same instant as golden.rs).
const BASE_MS: i64 = 1_767_323_045_000;

const AGENT_RESEARCHER: &str = "researcher";

fn fixed_uuid(n: u32) -> Uuid {
    Uuid::parse_str(&format!("00000000-0000-7000-8000-0000000010{n:02}")).unwrap()
}

/// Stamp determinism onto a freshly built event: fixed event id and a
/// fixed envelope timestamp (`Event::new` uses wall-clock now).
fn stamp(mut event: Event, seq: u32, at_ms: i64) -> Event {
    event.envelope.event_id = fixed_uuid(seq);
    event.envelope.timestamp = chrono::DateTime::from_timestamp_millis(at_ms).unwrap();
    event
}

/// A dead-letter event exactly as both emitters shape it (the
/// operator-module broker tests pin the emitters to this contract).
fn dead_letter_event(
    agent: &str,
    trigger_seq: u64,
    source: &str,
    payload: serde_json::Value,
    seq: u32,
    at_ms: i64,
) -> Event {
    let event = Event::new(
        AgentId::new(agent).unwrap(),
        Uuid::now_v7(),
        EventPayload::Failed(fq_runtime::events::FailedPayload {
            error_kind: FailureKind::TriggerExhausted,
            error_message: format!("trigger exhausted after 5 deliveries (limit 5) [{source}]"),
            phase: FailurePhase::Setup,
            partial_totals: InvocationTotals::default(),
        }),
    )
    .annotate(
        DEAD_LETTER_SUBJECT_KEY,
        json!(fq_runtime::bus::trigger_subject(agent)),
    )
    .annotate(DEAD_LETTER_PAYLOAD_KEY, payload)
    .annotate(DEAD_LETTER_STREAM_SEQ_KEY, json!(trigger_seq))
    .annotate(DEAD_LETTER_SOURCE_KEY, json!(source));
    stamp(event, seq, at_ms)
}

// ------------------------------------------------------------------
// Harness
// ------------------------------------------------------------------

/// Per-test scratch: cache + agents dirs, torn down by Drop.
struct Scratch {
    dir: tempfile::TempDir,
}

impl Scratch {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("scratch tempdir");
        std::fs::create_dir_all(dir.path().join("agents")).unwrap();
        Scratch { dir }
    }

    fn cache(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    /// Durable state (the edge identity, #362) — scoped to the
    /// scratch dir so a test never mints into the developer's real
    /// `$XDG_STATE_HOME/factor-q`.
    fn state(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    fn agents(&self) -> PathBuf {
        self.dir.path().join("agents")
    }
}

fn run_fq(scratch: &Scratch, nats_url: &str, args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_fq"))
        .args(args)
        .env("FQ_CONFIG", "/nonexistent/fq.toml")
        .env("FQ_AGENTS_DIR", scratch.agents())
        .env("FQ_CACHE_DIR", scratch.cache())
        .env("FQ_STATE_DIR", scratch.state())
        .env("FQ_NATS_URL", nats_url)
        .env("RUST_LOG", "off")
        .env("NO_COLOR", "1")
        .output()
        .expect("run fq binary");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// True if `s[i..i + 36]` is a UUID (8-4-4-4-12 lowercase hex).
fn is_uuid_at(bytes: &[u8], i: usize) -> bool {
    if i + 36 > bytes.len() {
        return false;
    }
    (0..36).all(|k| {
        let c = bytes[i + k];
        match k {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit() && !c.is_ascii_uppercase(),
        }
    })
}

/// Normalise environment-dependent output so goldens are stable: the
/// scratch paths, the broker URL (random port), and any UUID minted at
/// runtime. UUIDs in `keep` (fixture identities) stay byte-exact — the
/// oracle still proves the right ids are echoed back.
fn redact(raw: &str, scratch: &Scratch, nats_url: &str, keep: &[&str]) -> String {
    let cache = scratch.cache().display().to_string();
    let agents = scratch.agents().display().to_string();
    let mut out = String::with_capacity(raw.len());
    // Longest replacement first so <CACHE_DIR> never swallows the
    // agents dir nested under it.
    let replaced = raw
        .replace(&agents, "<AGENTS_DIR>")
        .replace(&cache, "<CACHE_DIR>")
        .replace(nats_url, "<NATS_URL>");
    let bytes = replaced.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if is_uuid_at(bytes, i) {
            let token = &replaced[i..i + 36];
            if keep.contains(&token) {
                out.push_str(token);
            } else {
                out.push_str("<UUID>");
            }
            i += 36;
        } else {
            // UUIDs are pure ASCII, so scanning byte-wise is safe: any
            // multi-byte char fails `is_uuid_at` and is copied whole.
            let ch = replaced[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn golden_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.golden"))
}

/// Compare redacted stdout to the committed golden. `UPDATE_GOLDEN=1`
/// regenerates instead.
fn assert_golden(name: &str, actual: &str) {
    let path = golden_path(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing golden {path:?} — run `UPDATE_GOLDEN=1 cargo test -p fq-cli \
             --test golden_commands` and commit the result"
        )
    });
    if actual != expected {
        let diff: Vec<String> = expected
            .lines()
            .zip(actual.lines())
            .enumerate()
            .filter(|(_, (e, a))| e != a)
            .map(|(i, (e, a))| format!("line {}:\n  expected: {e}\n  actual:   {a}", i + 1))
            .collect();
        panic!(
            "golden mismatch for {name} ({} vs {} lines){}\n{}\n\nIf the change is intentional: \
             UPDATE_GOLDEN=1 cargo test -p fq-cli --test golden_commands, then review the diff.",
            expected.lines().count(),
            actual.lines().count(),
            if diff.is_empty() {
                " — line count only"
            } else {
                ":"
            },
            diff.join("\n")
        );
    }
}

fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Runtime::new()
        .expect("test runtime")
        .block_on(fut)
}

// `workers prune` used to be pinned here — the one CLI write that
// opened the control-plane store directly, covered by three goldens
// against a locally-seeded fixture, run against a deliberately closed
// NATS port to prove it needed no broker. The verb is gone: reclaiming
// stale registration rows is a daemon retention sweep now, not something
// an operator has to remember to run, so there is no CLI output left to
// pin — and with it went the last golden here that ran without a broker.
// The sweep's own coverage is in `fq_runtime::control_plane::retention`.

// ------------------------------------------------------------------
// The paired-daemon harness: a live `fq run`, a client paired with its
// edge, and the scratch both share.
//
// Every verb below this line speaks the authenticated edge, so a broker
// alone is no longer enough — the daemon is what answers, and the
// client has to have been introduced to it. The pairing dance (read the
// ephemeral bind address and the once-printed admin token out of the
// daemon's own log, write a client config naming that address, `fq
// connect`) is exactly what an operator does once by hand, which is why
// the tests do it rather than reach behind the transport.
// ------------------------------------------------------------------

/// A running daemon plus a client that can talk to it.
struct PairedDaemon {
    scratch: Scratch,
    /// The pairing store's root: `fq connect` writes
    /// `$XDG_CONFIG_HOME/factor-q/connections.toml`, and pointing it at
    /// a tempdir keeps a test off the developer's real connections.
    xdg: tempfile::TempDir,
    /// The *client's* config — it names the daemon's actual bind
    /// address, which the daemon's own config could not (it asked for
    /// port 0).
    client_config: PathBuf,
    log_path: PathBuf,
    child: Option<std::process::Child>,
    server: fq_test_support::NatsServer,
}

impl PairedDaemon {
    fn start() -> Self {
        let server = fq_test_support::NatsServer::start();
        let scratch = Scratch::new();

        // An ephemeral edge port keeps daemon-spawning tests from
        // fighting over the fixed default bind when they run in
        // parallel.
        let daemon_config = scratch.cache().join("fqd.toml");
        std::fs::write(&daemon_config, "[edge]\nbind = \"127.0.0.1:0\"\n").unwrap();

        let log_path = scratch.cache().join("daemon.log");
        let log = std::fs::File::create(&log_path).expect("create daemon log");
        let log_err = log.try_clone().expect("clone daemon log handle");
        let mut child = Command::new(env!("CARGO_BIN_EXE_fq"))
            .arg("run")
            .env("FQ_CONFIG", &daemon_config)
            .env("FQ_NATS_URL", server.url())
            .env("FQ_CACHE_DIR", scratch.cache())
            .env("FQ_STATE_DIR", scratch.state())
            .env("FQ_AGENTS_DIR", scratch.agents())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .spawn()
            .expect("spawn fq run");

        // Wait for steady state; fail loudly if the daemon dies on
        // startup.
        let deadline = Instant::now() + Duration::from_secs(30);
        let text = loop {
            if let Some(status) = child.try_wait().expect("poll fq run") {
                let log = std::fs::read_to_string(&log_path).unwrap_or_default();
                panic!("daemon exited during startup with {status:?}\n--- log ---\n{log}");
            }
            let text = std::fs::read_to_string(&log_path).unwrap_or_default();
            if text.contains("Runtime ready") {
                break text;
            }
            assert!(
                Instant::now() < deadline,
                "daemon never reached 'Runtime ready' within 30s\n--- log ---\n{text}"
            );
            std::thread::sleep(Duration::from_millis(100));
        };

        let addr = text
            .lines()
            .find_map(|l| l.trim().strip_prefix("- edge is listening on "))
            .expect("edge addr in log")
            .trim()
            .to_string();
        let token = {
            let mut lines = text.lines();
            lines
                .find(|l| l.contains("edge: admin token"))
                .expect("admin token marker");
            lines.next().expect("token line").trim().to_string()
        };

        let client_config = scratch.cache().join("fq.toml");
        std::fs::write(&client_config, format!("[edge]\nbind = \"{addr}\"\n"))
            .expect("client fq.toml");

        let xdg = tempfile::tempdir().expect("xdg dir");
        let connect = Command::new(env!("CARGO_BIN_EXE_fq"))
            .args(["connect", &addr, "--token", &token])
            .env("FQ_CONFIG", &client_config)
            .env("XDG_CONFIG_HOME", xdg.path())
            .env("RUST_LOG", "off")
            .stdin(Stdio::piped())
            .output()
            .expect("run fq connect");
        assert!(
            connect.status.success(),
            "fq connect failed:\n{}",
            String::from_utf8_lossy(&connect.stderr)
        );

        PairedDaemon {
            scratch,
            xdg,
            client_config,
            log_path,
            child: Some(child),
            server,
        }
    }

    /// Run `fq` as the paired client.
    fn run_fq(&self, args: &[&str]) -> (Option<i32>, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_fq"))
            .args(args)
            .env("FQ_CONFIG", &self.client_config)
            .env("FQ_AGENTS_DIR", self.scratch.agents())
            .env("FQ_CACHE_DIR", self.scratch.cache())
            .env("FQ_STATE_DIR", self.scratch.state())
            .env("FQ_NATS_URL", self.server.url())
            .env("XDG_CONFIG_HOME", self.xdg.path())
            .env("RUST_LOG", "off")
            .env("NO_COLOR", "1")
            .output()
            .expect("run fq binary");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn redact(&self, raw: &str) -> String {
        redact(raw, &self.scratch, self.server.url(), &[])
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
    }

    /// Wait for the daemon to exit **on its own**. `None` on timeout,
    /// which the caller reports as a hung shutdown.
    fn wait_for_exit(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        let child = self.child.as_mut()?;
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait().expect("poll fq run") {
                Some(status) => {
                    self.child = None;
                    return Some(status);
                }
                None => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
}

impl Drop for PairedDaemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ------------------------------------------------------------------
// reload / trigger — commands on the edge (plan Phase 4, verbs 3 and 6).
//
// Both used to be a fire-and-forget publish from the client, snapshotted
// against a bare broker with nothing listening — a golden that passed
// whether or not anything ever received the message. They are answered
// by a daemon now, so these pin what an operator sees when the work
// actually happened.
// ------------------------------------------------------------------

#[test]
fn golden_reload() {
    let daemon = PairedDaemon::start();

    let (exit, stdout, stderr) = daemon.run_fq(&["reload"]);
    assert_eq!(exit, Some(0), "reload should exit 0; stderr:\n{stderr}");
    assert_golden("reload_human", &daemon.redact(&stdout));
    assert!(
        daemon
            .log()
            .contains("reloaded agent definitions from disk"),
        "the daemon must have actually reloaded\n--- log ---\n{}",
        daemon.log()
    );
}

#[test]
fn golden_trigger_via_nats() {
    let daemon = PairedDaemon::start();

    let (exit, stdout, stderr) = daemon.run_fq(&[
        "trigger",
        AGENT_RESEARCHER,
        r#"{"topic":"golden"}"#,
        "--via-nats",
    ]);
    assert_eq!(exit, Some(0), "trigger should exit 0; stderr:\n{stderr}");
    assert_golden("trigger_via_nats_human", &daemon.redact(&stdout));
}

/// `--via-nats` is a compatibility no-op after D-1: with the in-process
/// runner retired there is one mode left, and both spellings must reach
/// it. Pinned against the same golden the flagged form uses, so a change
/// that made them diverge would be a diff here.
#[test]
fn trigger_without_the_flag_takes_the_same_path() {
    let daemon = PairedDaemon::start();

    let (exit, stdout, stderr) =
        daemon.run_fq(&["trigger", AGENT_RESEARCHER, r#"{"topic":"golden"}"#]);
    assert_eq!(exit, Some(0), "trigger should exit 0; stderr:\n{stderr}");
    assert_golden("trigger_via_nats_human", &daemon.redact(&stdout));
}

// ------------------------------------------------------------------
// dead-letters requeue — a JetStream-backed write. A fresh broker per
// test, so stream sequences in the output are deterministic (a fresh
// trigger stream numbers from 1) — and no daemon, deliberately: a
// requeue publishes a real trigger, and a daemon would dispatch it.
//
// `dead-letters list` used to be snapshotted here beside it. It moved
// to `golden.rs`'s edge fixture with verb 7's Phase-4 flip: the
// listing is now `dead_letter.list` on the authenticated edge, which
// this harness — a bare `fq` against a broker, no daemon — cannot
// serve. The golden files are unchanged.
// ------------------------------------------------------------------

/// Two dead letters for `researcher` (older trigger seq 11, newer 12)
/// plus one ordinary failure that must be excluded from the listing.
fn seed_dead_letters(nats_url: &str) {
    block_on(async {
        let bus = EventBus::connect(nats_url).await.expect("connect NATS");
        bus.publish(&dead_letter_event(
            AGENT_RESEARCHER,
            11,
            "inline",
            json!({"n": 1}),
            21,
            BASE_MS,
        ))
        .await
        .unwrap();
        bus.publish(&dead_letter_event(
            AGENT_RESEARCHER,
            12,
            "advisory",
            json!({"n": 2}),
            22,
            BASE_MS + 1_000,
        ))
        .await
        .unwrap();
        bus.publish(&stamp(
            Event::new(
                AgentId::new(AGENT_RESEARCHER).unwrap(),
                Uuid::now_v7(),
                EventPayload::Failed(fq_runtime::events::FailedPayload {
                    error_kind: FailureKind::RuntimeError,
                    error_message: "ordinary failure".into(),
                    phase: FailurePhase::Setup,
                    partial_totals: InvocationTotals::default(),
                }),
            ),
            23,
            BASE_MS + 2_000,
        ))
        .await
        .unwrap();
    });
}

#[test]
fn golden_dead_letters_requeue_human() {
    let server = fq_test_support::NatsServer::start();
    let scratch = Scratch::new();
    seed_dead_letters(server.url());

    let keep = [fixed_uuid(22).to_string()];
    let keep: Vec<&str> = keep.iter().map(String::as_str).collect();

    // No --trigger-seq: selects the newest dead letter (seq 12). The
    // fresh trigger is the first message on this broker's trigger
    // stream, so the echoed new seq is deterministically 1.
    let (exit, stdout, stderr) = run_fq(
        &scratch,
        server.url(),
        &["dead-letters", "requeue", AGENT_RESEARCHER],
    );
    assert_eq!(exit, Some(0), "requeue should exit 0; stderr:\n{stderr}");
    assert_golden(
        "dead_letters_requeue_human",
        &redact(&stdout, &scratch, server.url(), &keep),
    );
}

#[test]
fn golden_dead_letters_requeue_json() {
    let server = fq_test_support::NatsServer::start();
    let scratch = Scratch::new();
    seed_dead_letters(server.url());

    let keep = [fixed_uuid(22).to_string()];
    let keep: Vec<&str> = keep.iter().map(String::as_str).collect();

    let (exit, stdout, stderr) = run_fq(
        &scratch,
        server.url(),
        &["dead-letters", "requeue", AGENT_RESEARCHER, "--json"],
    );
    assert_eq!(
        exit,
        Some(0),
        "requeue --json should exit 0; stderr:\n{stderr}"
    );
    assert_golden(
        "dead_letters_requeue_json",
        &redact(&stdout, &scratch, server.url(), &keep),
    );
}

// The `invocation drop` goldens used to live here. They moved to
// `golden.rs`'s edge fixture with the verb's Phase-4 flip: dropping is
// now an `invocation.drop` command on the authenticated edge, which
// this harness — a bare `fq` over seeded stores, no daemon — cannot
// serve. The golden files are unchanged.

// ------------------------------------------------------------------
// down / down --now — the full daemon round-trip (the pattern from
// daemon_shutdown.rs, which owns the behavioural assertions; these
// snapshots pin the *stdout contract* only). Progress narration goes
// to stderr by design (#190) and is not snapshotted.
//
// The confirmation changed shape with the flip (plan Phase 4, verb 4):
// it used to be the daemon's own `fq.system.shutdown` event, read off a
// client subscription, which is where the runtime id and clean flag in
// the old golden came from. There is no subscription left — the point
// of the verb is that the process stops — so what is confirmed now is
// the daemon's edge going away, and the mode is the only thing the
// client still knows.
// ------------------------------------------------------------------

#[cfg(unix)]
fn golden_down_case(golden_name: &str, down_args: &[&str]) {
    let mut daemon = PairedDaemon::start();

    let (exit, stdout, stderr) = daemon.run_fq(down_args);

    // The daemon must exit on its own before the snapshot is trusted.
    let status = daemon
        .wait_for_exit(Duration::from_secs(15))
        .unwrap_or_else(|| panic!("daemon did not exit within 15s of `fq {down_args:?}`"));
    assert!(status.success(), "daemon exit was not clean: {status:?}");
    assert_eq!(exit, Some(0), "down should exit 0; stderr:\n{stderr}");
    assert_golden(golden_name, &daemon.redact(&stdout));
}

#[cfg(unix)]
#[test]
fn golden_down() {
    golden_down_case("down_human", &["down"]);
}

#[cfg(unix)]
#[test]
fn golden_down_now() {
    golden_down_case("down_now_human", &["down", "--now"]);
}
