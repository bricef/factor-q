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
//! contract is the `--via-nats` form only.
//!
//! To regenerate after an intentional output change:
//! `UPDATE_GOLDEN=1 cargo test -p fq-cli --test golden_commands` —
//! then review the diff like any other code change.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use fq_runtime::bus::EventBus;
use fq_runtime::control_plane::store::{ControlPlaneStore, OwnerStatus};
use fq_runtime::events::{
    DEAD_LETTER_PAYLOAD_KEY, DEAD_LETTER_SOURCE_KEY, DEAD_LETTER_STREAM_SEQ_KEY,
    DEAD_LETTER_SUBJECT_KEY, Event, EventPayload, FailureKind, FailurePhase, InvocationTotals,
    TriggerSource, TriggeredPayload,
};
use fq_runtime::{AgentId, ProjectionStore};
use serde_json::json;
use uuid::Uuid;

// ------------------------------------------------------------------
// Fixed identities, shared vocabulary with golden.rs.
// ------------------------------------------------------------------

/// Fixture epoch: 2026-01-02 03:04:05 UTC (same instant as golden.rs).
const BASE_MS: i64 = 1_767_323_045_000;

const INV_INFLIGHT: &str = "3a000000-0000-7000-8000-000000000003";
const AGENT_RESEARCHER: &str = "researcher";

/// A guaranteed-closed port: proves a command needs no NATS.
const NATS_CLOSED: &str = "nats://127.0.0.1:1";

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

fn snapshot_for(agent: &str) -> fq_runtime::events::ConfigSnapshot {
    fq_runtime::Agent::builder()
        .id(agent)
        .model("claude-haiku")
        .system_prompt("You are a deterministic fixture.")
        .build()
        .unwrap()
        .to_snapshot()
}

fn triggered(agent: &str, invocation: &str, seq: u32, at_ms: i64) -> Event {
    let payload = EventPayload::Triggered(TriggeredPayload {
        trigger_source: TriggerSource::Manual,
        trigger_subject: None,
        trigger_payload: serde_json::Value::String("golden fixture trigger".into()),
        config_snapshot: snapshot_for(agent),
    });
    stamp(
        Event::new(
            AgentId::new(agent).unwrap(),
            Uuid::parse_str(invocation).unwrap(),
            payload,
        ),
        seq,
        at_ms,
    )
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

// ------------------------------------------------------------------
// workers prune — the direct-store write (no NATS, proven by the
// closed port). Dry-run first against the same fixture, then the
// mutation, then the now-empty re-run: one seeded scratch exercises
// all three contract outputs in mutation order.
// ------------------------------------------------------------------

fn seed_workers(scratch: &Scratch) {
    block_on(async {
        let paths = fq_runtime::db::RuntimeDbPaths::under(scratch.dir.path());
        let cp = ControlPlaneStore::open(&paths.control_plane)
            .await
            .expect("open control plane");
        cp.register_worker("worker-alpha", "golden-host", BASE_MS)
            .await
            .unwrap();
        cp.register_worker("worker-beta", "golden-host", BASE_MS + 1_000)
            .await
            .unwrap();
        assert!(cp.mark_worker_stale("worker-alpha").await.unwrap());
    });
}

#[test]
fn golden_workers_prune() {
    let scratch = Scratch::new();
    seed_workers(&scratch);

    let (exit, stdout, stderr) = run_fq(&scratch, NATS_CLOSED, &["workers", "prune", "--dry-run"]);
    assert_eq!(exit, Some(0), "dry-run should exit 0; stderr:\n{stderr}");
    assert_golden(
        "workers_prune_dry_run_human",
        &redact(&stdout, &scratch, NATS_CLOSED, &[]),
    );

    // The dry run must not have mutated: the real prune still finds
    // worker-alpha.
    let (exit, stdout, stderr) = run_fq(&scratch, NATS_CLOSED, &["workers", "prune"]);
    assert_eq!(exit, Some(0), "prune should exit 0; stderr:\n{stderr}");
    assert_golden(
        "workers_prune_human",
        &redact(&stdout, &scratch, NATS_CLOSED, &[]),
    );

    // And pruning an already-clean store reports zero, exit 0.
    let (exit, stdout, stderr) = run_fq(&scratch, NATS_CLOSED, &["workers", "prune"]);
    assert_eq!(
        exit,
        Some(0),
        "empty prune should exit 0; stderr:\n{stderr}"
    );
    assert_golden(
        "workers_prune_empty_human",
        &redact(&stdout, &scratch, NATS_CLOSED, &[]),
    );
}

// ------------------------------------------------------------------
// reload / trigger --via-nats — fire-and-forget publishes: a live
// broker, no daemon.
// ------------------------------------------------------------------

#[test]
fn golden_reload() {
    let server = fq_test_support::NatsServer::start();
    let scratch = Scratch::new();

    let (exit, stdout, stderr) = run_fq(&scratch, server.url(), &["reload"]);
    assert_eq!(exit, Some(0), "reload should exit 0; stderr:\n{stderr}");
    assert_golden(
        "reload_human",
        &redact(&stdout, &scratch, server.url(), &[]),
    );
}

#[test]
fn golden_trigger_via_nats() {
    let server = fq_test_support::NatsServer::start();
    let scratch = Scratch::new();

    let (exit, stdout, stderr) = run_fq(
        &scratch,
        server.url(),
        &[
            "trigger",
            AGENT_RESEARCHER,
            r#"{"topic":"golden"}"#,
            "--via-nats",
        ],
    );
    assert_eq!(exit, Some(0), "trigger should exit 0; stderr:\n{stderr}");
    assert_golden(
        "trigger_via_nats_human",
        &redact(&stdout, &scratch, server.url(), &[]),
    );
}

// ------------------------------------------------------------------
// dead-letters list / requeue — JetStream-backed reads and writes.
// Each test gets a fresh broker, so stream sequences in the output
// are deterministic (a fresh trigger stream numbers from 1).
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
fn golden_dead_letters_list() {
    let server = fq_test_support::NatsServer::start();
    let scratch = Scratch::new();
    seed_dead_letters(server.url());

    let keep = [fixed_uuid(21).to_string(), fixed_uuid(22).to_string()];
    let keep: Vec<&str> = keep.iter().map(String::as_str).collect();

    let (exit, stdout, stderr) = run_fq(&scratch, server.url(), &["dead-letters", "list"]);
    assert_eq!(exit, Some(0), "list should exit 0; stderr:\n{stderr}");
    assert_golden(
        "dead_letters_list_human",
        &redact(&stdout, &scratch, server.url(), &keep),
    );

    let (exit, stdout, stderr) =
        run_fq(&scratch, server.url(), &["dead-letters", "list", "--json"]);
    assert_eq!(
        exit,
        Some(0),
        "list --json should exit 0; stderr:\n{stderr}"
    );
    assert_golden(
        "dead_letters_list_json",
        &redact(&stdout, &scratch, server.url(), &keep),
    );
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

// ------------------------------------------------------------------
// invocation drop — the operator write over the bus. The published
// event's id is minted at runtime, so it redacts to <UUID>; the
// invocation and agent identities stay byte-exact.
// ------------------------------------------------------------------

fn seed_inflight_invocation(scratch: &Scratch) {
    block_on(async {
        let paths = fq_runtime::db::RuntimeDbPaths::under(scratch.dir.path());
        let proj = ProjectionStore::open(&paths.projection)
            .await
            .expect("open projection");
        proj.insert_event(&triggered(AGENT_RESEARCHER, INV_INFLIGHT, 7, BASE_MS))
            .await
            .expect("insert event");
        let cp = ControlPlaneStore::open(&paths.control_plane)
            .await
            .expect("open control plane");
        cp.upsert_invocation_ownership(
            INV_INFLIGHT,
            AGENT_RESEARCHER,
            BASE_MS,
            OwnerStatus::InFlight,
        )
        .await
        .unwrap();
    });
}

#[test]
fn golden_invocation_drop_human() {
    let server = fq_test_support::NatsServer::start();
    let scratch = Scratch::new();
    seed_inflight_invocation(&scratch);

    let (exit, stdout, stderr) = run_fq(
        &scratch,
        server.url(),
        &[
            "invocation",
            "drop",
            INV_INFLIGHT,
            "--reason",
            "golden fixture drop",
        ],
    );
    assert_eq!(exit, Some(0), "drop should exit 0; stderr:\n{stderr}");
    assert_golden(
        "invocation_drop_human",
        &redact(&stdout, &scratch, server.url(), &[INV_INFLIGHT]),
    );
}

#[test]
fn golden_invocation_drop_json() {
    let server = fq_test_support::NatsServer::start();
    let scratch = Scratch::new();
    seed_inflight_invocation(&scratch);

    let (exit, stdout, stderr) = run_fq(
        &scratch,
        server.url(),
        &[
            "invocation",
            "drop",
            INV_INFLIGHT,
            "--reason",
            "golden fixture drop",
            "--json",
        ],
    );
    assert_eq!(
        exit,
        Some(0),
        "drop --json should exit 0; stderr:\n{stderr}"
    );
    assert_golden(
        "invocation_drop_json",
        &redact(&stdout, &scratch, server.url(), &[INV_INFLIGHT]),
    );
}

// ------------------------------------------------------------------
// down / down --now — the full daemon round-trip (the pattern from
// daemon_shutdown.rs, which owns the behavioural assertions; these
// snapshots pin the *stdout contract* only). Progress narration goes
// to stderr by design (#190) and is not snapshotted. The confirmed
// runtime id is daemon-minted, so it redacts to <UUID>.
// ------------------------------------------------------------------

#[cfg(unix)]
fn golden_down_case(golden_name: &str, down_args: &[&str]) {
    let server = fq_test_support::NatsServer::start();
    let scratch = Scratch::new();

    // The edge is on by default; an ephemeral port keeps the two
    // daemon-spawning down goldens from fighting over the fixed
    // default bind when they run in parallel.
    let daemon_config = scratch.cache().join("fq.toml");
    std::fs::write(&daemon_config, "[edge]\nbind = \"127.0.0.1:0\"\n").unwrap();

    let log_path = scratch.cache().join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let log_err = log.try_clone().expect("clone daemon log handle");
    let mut child = Command::new(env!("CARGO_BIN_EXE_fq"))
        .arg("run")
        .env("FQ_CONFIG", &daemon_config)
        .env("FQ_NATS_URL", server.url())
        .env("FQ_CACHE_DIR", scratch.cache())
        .env("FQ_AGENTS_DIR", scratch.agents())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .expect("spawn fq run");

    // Wait for steady state; fail loudly if the daemon dies on startup.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut ready = false;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll fq run") {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            panic!("daemon exited during startup with {status:?}\n--- log ---\n{log}");
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
    assert!(ready, "daemon never reached 'Runtime ready' within 30s");

    let (exit, stdout, stderr) = run_fq(&scratch, server.url(), down_args);

    // The daemon must exit on its own before the snapshot is trusted.
    let daemon_deadline = Instant::now() + Duration::from_secs(15);
    let daemon_status = loop {
        match child.try_wait().expect("poll fq run") {
            Some(status) => break status,
            None => {
                if Instant::now() >= daemon_deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("daemon did not exit within 15s of `fq {down_args:?}`");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    assert!(
        daemon_status.success(),
        "daemon exit was not clean: {daemon_status:?}"
    );
    assert_eq!(exit, Some(0), "down should exit 0; stderr:\n{stderr}");
    assert_golden(golden_name, &redact(&stdout, &scratch, server.url(), &[]));
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
