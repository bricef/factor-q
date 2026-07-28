//! Golden-master output tests for every DB-backed read command (#261).
//!
//! A deterministic fixture store (fixed UUIDs, fixed timestamps) is
//! seeded once per test process; each test drives the real binary via
//! `CARGO_BIN_EXE_fq` against it and compares stdout to a committed
//! golden file under `tests/golden/`.
//!
//! These snapshots are the acceptance oracle for the Views read-path
//! refactor: a behavioural change in any read command's output is a
//! hard diff here, never a silent drift.
//!
//! Volatile output (age/duration strings computed from wall-clock now,
//! the tempdir path, the test broker's random port) is normalised
//! before comparison — see [`redact`]. Everything else must be
//! byte-identical.
//!
//! To regenerate after an intentional output change:
//! `UPDATE_GOLDEN=1 cargo test -p fq-cli --test golden` — then review
//! the diff like any other code change.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use fq_runtime::control_plane::store::{ControlPlaneStore, InvocationArchiveRow, OwnerStatus};
use fq_runtime::events::{
    CostMetadata, Event, EventPayload, FailureKind, FailurePhase, InvocationTotals, LlmCallOrigin,
    Message, MessageRole, StopReason, TokenUsage, TriggerSource, TriggeredPayload,
};
use fq_runtime::llm::ChatResponse;
use fq_runtime::worker::InvocationStateRow;
use fq_runtime::{AgentId, ProjectionStore, WorkerStore};
use uuid::Uuid;

// ------------------------------------------------------------------
// Fixed identities. Everything the fixture writes is derived from
// these constants so rendered output is stable across runs and
// machines.
// ------------------------------------------------------------------

/// Fixture epoch: 2026-01-02 03:04:05 UTC, far enough in the past that
/// "is it stale/stuck?" classifications are stable, recent enough that
/// nothing overflows a duration formatter.
const BASE_MS: i64 = 1_767_323_045_000;

const INV_COMPLETED: &str = "1c000000-0000-7000-8000-000000000001";
const INV_FAILED: &str = "2f000000-0000-7000-8000-000000000002";
const INV_INFLIGHT: &str = "3a000000-0000-7000-8000-000000000003";
const INV_ARCHIVED: &str = "4e000000-0000-7000-8000-000000000004";

const AGENT_RESEARCHER: &str = "researcher";
const AGENT_FIXER: &str = "fixer";

fn fixed_uuid(n: u32) -> Uuid {
    Uuid::parse_str(&format!("00000000-0000-7000-8000-0000000010{n:02}")).unwrap()
}

fn inv(id: &str) -> Uuid {
    Uuid::parse_str(id).unwrap()
}

// ------------------------------------------------------------------
// Fixture seeding
// ------------------------------------------------------------------

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
        Event::new(AgentId::new(agent).unwrap(), inv(invocation), payload),
        seq,
        at_ms,
    )
}

fn cost(call: u32, total: f64, cumulative: f64) -> CostMetadata {
    CostMetadata {
        call_id: fixed_uuid(call),
        model: "claude-haiku".into(),
        input_tokens: 1_200,
        output_tokens: 340,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        input_cost: total * 0.7,
        output_cost: total * 0.3,
        total_cost: total,
        cumulative_invocation_cost: cumulative,
        cumulative_agent_cost: cumulative,
        origin: LlmCallOrigin::AgentTurn,
    }
}

fn llm_response(agent: &str, invocation: &str, seq: u32, at_ms: i64, total_cost: f64) -> Event {
    let payload = EventPayload::LlmResponse(fq_runtime::events::LlmResponsePayload {
        round: 0,
        call_id: fixed_uuid(seq),
        content: Some("Fixture assistant reply.".into()),
        tool_calls: Vec::new(),
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 1_200,
            output_tokens: 340,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
        origin: LlmCallOrigin::AgentTurn,
    });
    stamp(
        Event::new(AgentId::new(agent).unwrap(), inv(invocation), payload),
        seq,
        at_ms,
    )
    .with_cost(cost(seq, total_cost, total_cost))
}

fn state_row(
    invocation: &str,
    agent: &str,
    phase: &str,
    started_at: i64,
    terminal_at: Option<i64>,
) -> InvocationStateRow {
    InvocationStateRow {
        invocation_id: invocation.to_string(),
        agent_id: agent.to_string(),
        schema_version: 1,
        phase: phase.to_string(),
        state_blob: b"{}".to_vec(),
        step_index: 2,
        started_at,
        updated_at: terminal_at.unwrap_or(started_at + 1_000),
        terminal_at,
        workspace_ref: None,
        archive_status: None,
        archive_published_at: None,
        trigger_source: Some("manual".into()),
        trigger_subject: None,
        trigger_payload: Some("\"golden fixture trigger\"".into()),
    }
}

async fn seed(dir: &Path) {
    seed_at(dir, BASE_MS).await
}

async fn seed_at(dir: &Path, base_ms: i64) {
    let paths = fq_runtime::db::RuntimeDbPaths::under(dir);
    let proj = ProjectionStore::open(&paths.projection)
        .await
        .expect("open projection");

    // Projection events: two invocations for `researcher`, two for
    // `fixer`, with per-call costs on the LLM responses.
    for event in [
        triggered(AGENT_RESEARCHER, INV_COMPLETED, 1, base_ms),
        llm_response(AGENT_RESEARCHER, INV_COMPLETED, 2, base_ms + 1_000, 0.0125),
        stamp(
            Event::new(
                AgentId::new(AGENT_RESEARCHER).unwrap(),
                inv(INV_COMPLETED),
                EventPayload::Completed(fq_runtime::events::CompletedPayload {
                    task_status: Default::default(),
                    result_summary: Some("Fixture complete.".into()),
                    total_llm_calls: 2,
                    total_tool_calls: 1,
                    total_cost: 0.0125,
                    total_duration_ms: 5_000,
                }),
            ),
            3,
            base_ms + 5_000,
        ),
        triggered(AGENT_FIXER, INV_FAILED, 4, base_ms + 10_000),
        llm_response(AGENT_FIXER, INV_FAILED, 5, base_ms + 11_000, 0.0031),
        stamp(
            Event::new(
                AgentId::new(AGENT_FIXER).unwrap(),
                inv(INV_FAILED),
                EventPayload::Failed(fq_runtime::events::FailedPayload {
                    error_kind: FailureKind::ToolError,
                    error_message: "fixture tool exploded".into(),
                    phase: FailurePhase::ToolResult,
                    partial_totals: InvocationTotals {
                        total_llm_calls: 1,
                        total_tool_calls: 1,
                        total_cost: 0.0031,
                        total_duration_ms: 2_000,
                        ..Default::default()
                    },
                }),
            ),
            6,
            base_ms + 12_000,
        ),
        triggered(AGENT_RESEARCHER, INV_INFLIGHT, 7, base_ms + 20_000),
        triggered(AGENT_FIXER, INV_ARCHIVED, 8, base_ms + 30_000),
    ] {
        proj.insert_event(&event).await.expect("insert event");
    }

    // Worker WAL: a full llm+tool transcript for INV_COMPLETED, an open
    // (dispatched, uncompleted) LLM call for INV_INFLIGHT, and terminal
    // state for INV_FAILED.
    let worker = WorkerStore::open(&paths.worker)
        .await
        .expect("open worker store");
    worker
        .upsert_invocation_state(&state_row(
            INV_COMPLETED,
            AGENT_RESEARCHER,
            "completed",
            base_ms,
            Some(base_ms + 5_000),
        ))
        .await
        .unwrap();
    worker
        .upsert_invocation_state(&state_row(
            INV_FAILED,
            AGENT_FIXER,
            "failed",
            base_ms + 10_000,
            Some(base_ms + 12_000),
        ))
        .await
        .unwrap();
    worker
        .upsert_invocation_state(&state_row(
            INV_INFLIGHT,
            AGENT_RESEARCHER,
            "awaiting_model",
            base_ms + 20_000,
            None,
        ))
        .await
        .unwrap();

    let request_payload = serde_json::json!({
        "messages": [
            Message {
                role: MessageRole::System,
                content: Some("You are a deterministic fixture.".into()),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            Message {
                role: MessageRole::User,
                content: Some("Summarise the fixture, then read a file.".into()),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
        ]
    })
    .to_string();
    let first_response = serde_json::to_string(&ChatResponse {
        content: Some("Reading the fixture file first.".into()),
        tool_calls: vec![fq_runtime::events::MessageToolCall {
            tool_call_id: fq_runtime::events::ToolCallId::new("tc-1").unwrap(),
            tool_name: "read_file".into(),
            parameters: serde_json::json!({"path": "fixture.txt"}),
        }],
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage {
            input_tokens: 1_200,
            output_tokens: 340,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    })
    .unwrap();
    let second_response = serde_json::to_string(&ChatResponse {
        content: Some("The fixture file says: deterministic.".into()),
        tool_calls: Vec::new(),
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 1_400,
            output_tokens: 120,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    })
    .unwrap();

    worker
        .write_llm_intent(
            INV_COMPLETED,
            "req-1",
            "claude-haiku",
            &request_payload,
            base_ms,
        )
        .await
        .unwrap();
    worker
        .write_llm_dispatched(INV_COMPLETED, "req-1", base_ms + 100)
        .await
        .unwrap();
    worker
        .write_llm_completed(
            INV_COMPLETED,
            "req-1",
            &first_response,
            false,
            0.0125,
            base_ms + 1_000,
        )
        .await
        .unwrap();
    worker
        .write_tool_intent(
            INV_COMPLETED,
            "tc-1",
            "read_file",
            "{\"path\":\"fixture.txt\"}",
            base_ms + 1_500,
        )
        .await
        .unwrap();
    worker
        .write_tool_dispatched(INV_COMPLETED, "tc-1", base_ms + 1_600)
        .await
        .unwrap();
    worker
        .write_tool_completed(
            INV_COMPLETED,
            "tc-1",
            "{\"bytes\":42,\"content\":\"deterministic\"}",
            false,
            base_ms + 2_000,
        )
        .await
        .unwrap();
    worker
        .write_llm_intent(
            INV_COMPLETED,
            "req-2",
            "claude-haiku",
            &request_payload,
            base_ms + 3_000,
        )
        .await
        .unwrap();
    worker
        .write_llm_dispatched(INV_COMPLETED, "req-2", base_ms + 3_100)
        .await
        .unwrap();
    worker
        .write_llm_completed(
            INV_COMPLETED,
            "req-2",
            &second_response,
            false,
            0.0125,
            base_ms + 4_000,
        )
        .await
        .unwrap();

    // The in-flight invocation has an open dispatch (intent+dispatched,
    // never completed).
    worker
        .write_llm_intent(
            INV_INFLIGHT,
            "req-open",
            "claude-haiku",
            &request_payload,
            base_ms + 21_000,
        )
        .await
        .unwrap();
    worker
        .write_llm_dispatched(INV_INFLIGHT, "req-open", base_ms + 21_100)
        .await
        .unwrap();

    // Control plane: workers in each lifecycle state, ownership rows,
    // and one archived invocation with no surviving worker state.
    let cp = ControlPlaneStore::open(&paths.control_plane)
        .await
        .expect("open control plane");
    cp.register_worker("worker-alpha", "golden-host", base_ms)
        .await
        .unwrap();
    cp.register_worker("worker-beta", "golden-host", base_ms + 1_000)
        .await
        .unwrap();
    cp.register_worker("worker-omega", "golden-host", base_ms + 2_000)
        .await
        .unwrap();
    assert!(cp.mark_worker_stale("worker-alpha").await.unwrap());
    cp.mark_worker_shutdown("worker-omega").await.unwrap();

    for (invocation, agent, status, at) in [
        (
            INV_COMPLETED,
            AGENT_RESEARCHER,
            OwnerStatus::Completed,
            base_ms + 5_000,
        ),
        (
            INV_FAILED,
            AGENT_FIXER,
            OwnerStatus::Failed,
            base_ms + 12_000,
        ),
        (
            INV_INFLIGHT,
            AGENT_RESEARCHER,
            OwnerStatus::InFlight,
            base_ms + 20_000,
        ),
        (
            INV_ARCHIVED,
            AGENT_FIXER,
            OwnerStatus::Completed,
            base_ms + 31_000,
        ),
    ] {
        cp.upsert_invocation_ownership(invocation, agent, at, status)
            .await
            .unwrap();
    }

    cp.insert_archive(&InvocationArchiveRow {
        invocation_id: INV_ARCHIVED.to_string(),
        agent_id: AGENT_FIXER.to_string(),
        final_phase: "completed".to_string(),
        final_state_blob: b"{}".to_vec(),
        started_at: base_ms + 30_000,
        terminal_at: base_ms + 31_000,
        archived_at: base_ms + 31_500,
    })
    .await
    .unwrap();
}

// ------------------------------------------------------------------
// Harness
// ------------------------------------------------------------------

struct Fixture {
    dir: tempfile::TempDir,
}

fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let dir = tempfile::tempdir().expect("fixture tempdir");
        tokio::runtime::Runtime::new()
            .expect("fixture runtime")
            .block_on(seed(dir.path()));
        Fixture { dir }
    })
}

enum Nats {
    /// A live private broker, for the commands that bail without NATS
    /// (`status`). The guard is owned by the calling test rather than cached
    /// in a `static`, and that is load-bearing: `NatsServer` spawns the broker
    /// with `PR_SET_PDEATHSIG`, which Linux delivers when the spawning
    /// *thread* exits, not the process. libtest runs each `#[test]` on its own
    /// thread, so a shared `static` broker is killed the moment whichever test
    /// happened to initialise it returns, leaving later tests holding a handle
    /// to a dead server. `fq-test-support` states the constraint outright —
    /// "every shape here starts the server from a thread that outlives the
    /// guard" — and a `static` is the one shape that inverts it. Starting per
    /// test costs one extra spawn and matches every other broker call site in
    /// the tree.
    Live(fq_test_support::NatsServer),
    /// A guaranteed-closed port: proves the command needs no NATS.
    Closed,
}

fn run_fq(args: &[&str], nats: &Nats) -> (Option<i32>, String, String) {
    let fixture = fixture();
    let nats_url = match nats {
        Nats::Live(server) => server.url().to_string(),
        Nats::Closed => "nats://127.0.0.1:1".to_string(),
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_fq"))
        .args(args)
        .env("FQ_CONFIG", "/nonexistent/fq.toml")
        .env("FQ_AGENTS_DIR", "/nonexistent/agents")
        .env("FQ_CACHE_DIR", fixture.dir.path())
        .env("FQ_NATS_URL", &nats_url)
        .env("RUST_LOG", "off")
        .env("NO_COLOR", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn fq binary");

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break status,
            None => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    panic!("fq {args:?} did not exit within 30s");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    };
    use std::io::Read;
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_string(&mut stdout);
    }
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut stderr);
    }
    (status.code(), stdout, stderr)
}

/// Collapse every maximal ASCII-digit run in `line` to a single `#`.
fn collapse_digits(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_run = false;
    for c in line.chars() {
        if c.is_ascii_digit() {
            if !in_run {
                out.push('#');
                in_run = true;
            }
        } else {
            in_run = false;
            out.push(c);
        }
    }
    out
}

/// Normalise environment-dependent output so goldens are stable:
/// the fixture dir path, the broker URL (random port), and — on lines
/// containing any of `volatile_markers` — wall-clock-derived numbers.
fn redact(raw: &str, nats: &Nats, volatile_markers: &[&str]) -> String {
    let fixture_path = fixture().dir.path().display().to_string();
    let nats_url = match nats {
        Nats::Live(server) => server.url().to_string(),
        Nats::Closed => "nats://127.0.0.1:1".to_string(),
    };
    raw.lines()
        .map(|line| {
            let line = line.replace(&fixture_path, "<CACHE_DIR>");
            let line = line.replace(&nats_url, "<NATS_URL>");
            if volatile_markers.iter().any(|m| line.contains(m)) {
                collapse_digits(&line)
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn golden_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.golden"))
}

/// Run one read command and compare its redacted stdout to the
/// committed golden. `UPDATE_GOLDEN=1` regenerates instead.
fn check_golden(name: &str, args: &[&str], nats: Nats, volatile_markers: &[&str]) {
    let (exit, stdout, stderr) = run_fq(args, &nats);
    assert_eq!(
        exit,
        Some(0),
        "fq {args:?} should exit 0; stderr:\n{stderr}"
    );
    let actual = redact(&stdout, &nats, volatile_markers);
    compare_golden(name, &actual);
}

fn compare_golden(name: &str, actual: &str) {
    let path = golden_path(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing golden {path:?} — run `UPDATE_GOLDEN=1 cargo test -p fq-cli --test golden` \
             and commit the result"
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
             UPDATE_GOLDEN=1 cargo test -p fq-cli --test golden, then review the diff.",
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

// ------------------------------------------------------------------
// The snapshots: every DB-backed read command, human + JSON.
// ------------------------------------------------------------------

/// A leftover v1 single-file layout is a migration hint, not a read:
/// pin the refusal message so read commands never silently open (or
/// worse, recreate) the legacy file.
#[test]
fn legacy_single_file_layout_is_a_hint_not_a_read() {
    // `costs` still reads the local stores; the invocation verbs no
    // longer do (they speak the edge — Phase 3b), so the legacy-layout
    // hint is pinned on a verb where a local read still happens.
    let dir = tempfile::tempdir().expect("legacy tempdir");
    std::fs::write(dir.path().join("events.db"), b"not a real db").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_fq"))
        .args(["costs"])
        .env("FQ_CONFIG", "/nonexistent/fq.toml")
        .env("FQ_AGENTS_DIR", "/nonexistent/agents")
        .env("FQ_CACHE_DIR", dir.path())
        .env("FQ_NATS_URL", "nats://127.0.0.1:1")
        .env("RUST_LOG", "off")
        .output()
        .expect("run fq binary");
    let exit = out.status.code();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(exit, Some(0), "a legacy layout must not read");
    assert!(
        stdout.is_empty(),
        "fatal error must not pollute stdout; got:\n{stdout}"
    );
    assert!(
        stderr.contains("legacy single-file database"),
        "stderr should carry the migration hint even with RUST_LOG=off; got:\n{stderr}"
    );
}

/// The flipped verbs' unpaired error is operator guidance, not a
/// stack trace: no stored connection means "run fq connect", stated.
#[test]
fn flipped_verb_without_a_pairing_says_how_to_pair() {
    let xdg = tempfile::tempdir().expect("xdg dir");
    let out = Command::new(env!("CARGO_BIN_EXE_fq"))
        .args(["invocation", "list"])
        .env("FQ_CONFIG", "/nonexistent/fq.toml")
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("RUST_LOG", "off")
        .output()
        .expect("run fq binary");
    assert_ne!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("fq connect"),
        "unpaired flipped verb must point at `fq connect`; got:\n{stderr}"
    );
}

#[test]
fn golden_status_human() {
    check_golden(
        "status_human",
        &["status"],
        Nats::Live(fq_test_support::test_nats()),
        &["connection:", "rows:"],
    );
}

#[test]
fn golden_status_json() {
    check_golden(
        "status_json",
        &["status", "--json"],
        Nats::Live(fq_test_support::test_nats()),
        &[],
    );
}

#[test]
fn golden_doctor_human() {
    check_golden("doctor_human", &["doctor"], Nats::Closed, &["for "]);
}

#[test]
fn golden_doctor_json() {
    check_golden("doctor_json", &["doctor", "--json"], Nats::Closed, &[]);
}

#[test]
fn golden_costs_human() {
    check_golden("costs_human", &["costs"], Nats::Closed, &[]);
}

#[test]
fn golden_costs_json() {
    check_golden("costs_json", &["costs", "--json"], Nats::Closed, &[]);
}

#[test]
fn golden_events_query_human() {
    check_golden(
        "events_query_human",
        &["events", "query"],
        Nats::Closed,
        &[],
    );
}

#[test]
fn golden_events_query_json() {
    check_golden(
        "events_query_json",
        &["events", "query", "--json"],
        Nats::Closed,
        &[],
    );
}

// ------------------------------------------------------------------
// The edge-backed goldens (plan Phase 3b): the flipped verbs run the
// same argv against the same golden files — through a live daemon and
// the authenticated edge instead of local SQLite. Each test copies
// the seeded fixture (the daemon opens the stores RW), starts its own
// broker and daemon, pairs once via `fq connect`, and compares the
// unchanged golden: the flip proven byte-identical.
// ------------------------------------------------------------------

/// The conversation of [`INV_COMPLETED`], as the event log records it:
/// the two assistant turns and the tool result between them, stamped at
/// the same instants the WAL fixture uses so the rendered transcript is
/// the same timeline either way.
///
/// This is the transcript fixture's substrate. The WAL seed above
/// describes the same run from the worker's side (it is what
/// `invocation.get` still mines the opening prompt from); these are the
/// facts the Turn atom is folded from, and `fq invocation transcript`
/// now reads both.
fn conversation_events() -> Vec<Event> {
    let agent = AgentId::new(AGENT_RESEARCHER).unwrap();
    let call = || fq_runtime::events::ToolCallId::new("tc-1").unwrap();
    vec![
        stamp(
            Event::new(
                agent.clone(),
                inv(INV_COMPLETED),
                EventPayload::LlmResponse(fq_runtime::events::LlmResponsePayload {
                    round: 1,
                    call_id: fixed_uuid(20),
                    content: Some("Reading the fixture file first.".into()),
                    tool_calls: vec![fq_runtime::events::MessageToolCall {
                        tool_call_id: call(),
                        tool_name: "read_file".into(),
                        parameters: serde_json::json!({"path": "fixture.txt"}),
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: TokenUsage {
                        input_tokens: 1_200,
                        output_tokens: 340,
                        cache_read_tokens: 0,
                        cache_write_tokens: 0,
                    },
                    origin: LlmCallOrigin::AgentTurn,
                }),
            ),
            20,
            BASE_MS,
        )
        .with_cost(cost(20, 0.0125, 0.0125)),
        stamp(
            Event::new(
                agent.clone(),
                inv(INV_COMPLETED),
                EventPayload::ToolResult(fq_runtime::events::ToolResultPayload {
                    round: 1,
                    tool_name: "read_file".into(),
                    tool_call_id: call(),
                    output: "{\"bytes\":42,\"content\":\"deterministic\"}".into(),
                    is_error: false,
                    error_kind: None,
                    duration_ms: 500,
                }),
            ),
            21,
            BASE_MS + 1_500,
        ),
        stamp(
            Event::new(
                agent,
                inv(INV_COMPLETED),
                EventPayload::LlmResponse(fq_runtime::events::LlmResponsePayload {
                    round: 2,
                    call_id: fixed_uuid(22),
                    content: Some("The fixture file says: deterministic.".into()),
                    tool_calls: Vec::new(),
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage {
                        input_tokens: 1_400,
                        output_tokens: 120,
                        cache_read_tokens: 0,
                        cache_write_tokens: 0,
                    },
                    origin: LlmCallOrigin::AgentTurn,
                }),
            ),
            22,
            BASE_MS + 3_000,
        )
        .with_cost(cost(22, 0.0125, 0.0125)),
    ]
}

struct EdgeFixture {
    daemon: Option<std::process::Child>,
    dir: tempfile::TempDir,
    xdg: tempfile::TempDir,
    client_config: std::path::PathBuf,
    broker: fq_test_support::NatsServer,
}

impl EdgeFixture {
    /// The plain fixture: stores seeded, event log empty. Every golden
    /// but the transcript ones renders folds, which the stores already
    /// hold.
    fn start() -> Self {
        // A private seed at the SAME fixed base as the shared fixture
        // (the JSON goldens embed its literal timestamps), then worker
        // heartbeats alone freshened to now: the live daemon's
        // stale-worker sweep runs at startup, and ancient heartbeats
        // would (correctly) reclassify in-flight work as ambiguous —
        // an artifact of fixture age, not of the transport under
        // test. No flipped golden renders a heartbeat, so the
        // freshening is invisible to the outputs.
        let dir = tempfile::tempdir().expect("edge fixture dir");
        tokio::runtime::Runtime::new()
            .expect("edge fixture runtime")
            .block_on(async {
                seed_at(dir.path(), BASE_MS).await;
                let paths = fq_runtime::db::RuntimeDbPaths::under(dir.path());
                let cp = ControlPlaneStore::open(&paths.control_plane)
                    .await
                    .expect("open control plane");
                // The in-flight row's OWNER is the worker named after
                // its agent; give it a live registration so the
                // daemon's recovery sees a live owner and leaves the
                // in-flight work alone. Registrations aren't rendered
                // by the flipped goldens.
                let now = chrono::Utc::now().timestamp_millis();
                for worker in ["researcher", "fixer"] {
                    cp.register_worker(worker, "golden-host", now)
                        .await
                        .expect("register owner worker");
                    cp.heartbeat_worker(worker, now)
                        .await
                        .expect("freshen owner heartbeat");
                }
            });
        std::fs::create_dir_all(dir.path().join("agents")).expect("agents dir");

        let broker = fq_test_support::NatsServer::start();
        let daemon_config = dir.path().join("fqd.toml");
        std::fs::write(&daemon_config, "[edge]\nbind = \"127.0.0.1:0\"\n").expect("fqd.toml");
        let log_path = dir.path().join("daemon.log");
        let log = std::fs::File::create(&log_path).expect("daemon log");
        let log_err = log.try_clone().expect("log handle");
        let mut daemon = Command::new(env!("CARGO_BIN_EXE_fqd"))
            .env("FQ_CONFIG", &daemon_config)
            .env("FQ_NATS_URL", broker.url())
            .env("FQ_CACHE_DIR", dir.path())
            .env("FQ_AGENTS_DIR", dir.path().join("agents"))
            .env("RUST_LOG", "off")
            .env("NO_COLOR", "1")
            .stdout(std::process::Stdio::from(log))
            .stderr(std::process::Stdio::from(log_err))
            .spawn()
            .expect("spawn fqd");

        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let text = loop {
            if let Some(status) = daemon.try_wait().expect("poll fqd") {
                let text = std::fs::read_to_string(&log_path).unwrap_or_default();
                panic!("fqd exited during startup with {status:?}\n--- log ---\n{text}");
            }
            let text = std::fs::read_to_string(&log_path).unwrap_or_default();
            if text.contains("Runtime ready") {
                break text;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fqd never reached 'Runtime ready'\n--- log ---\n{text}"
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

        // The daemon's startup recovery has (correctly) marked the
        // seeded in-flight row ambiguous: at boot, in-flight work
        // owned by a previous life cannot still be running. The
        // goldens capture a world where that work IS live — so
        // re-assert the row once that recovery has run; the fresh
        // owner heartbeat above keeps the periodic sweep off it for
        // the test's lifetime.
        //
        // "Runtime ready" is NOT a barrier for this: startup recovery
        // is detached, and the ambiguity is applied asynchronously by
        // the coordination consumer reacting to an
        // `invocation.ambiguous` event. Re-asserting on the strength
        // of the log line alone races the daemon — if our write lands
        // first, recovery overwrites it and the goldens see Ambiguous
        // instead of InFlight (#395). So wait for the transition we
        // are compensating for to be OBSERVED, then undo it.
        tokio::runtime::Runtime::new()
            .expect("post-boot runtime")
            .block_on(async {
                let paths = fq_runtime::db::RuntimeDbPaths::under(dir.path());
                let cp = ControlPlaneStore::open(&paths.control_plane)
                    .await
                    .expect("reopen control plane");

                let deadline = std::time::Instant::now() + Duration::from_secs(30);
                loop {
                    let owner = cp
                        .get_invocation_owner(INV_INFLIGHT)
                        .await
                        .expect("read in-flight owner");
                    if owner.map(|o| o.status) == Some(OwnerStatus::Ambiguous) {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "startup recovery never marked {INV_INFLIGHT} ambiguous — the fixture \
                         compensates for that transition, so it must observe it first (#395)"
                    );
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }

                cp.upsert_invocation_ownership(
                    INV_INFLIGHT,
                    AGENT_RESEARCHER,
                    BASE_MS + 20_000,
                    OwnerStatus::InFlight,
                )
                .await
                .expect("re-assert in-flight row");
            });

        // The client's config names the daemon's actual address as
        // the default edge, so the flipped verbs dial it unchanged.
        let client_config = dir.path().join("fq.toml");
        std::fs::write(&client_config, format!("[edge]\nbind = \"{addr}\"\n"))
            .expect("client fq.toml");

        // Pair once: non-interactive TOFU auto-pins with a notice.
        let xdg = tempfile::tempdir().expect("xdg dir");
        let connect = Command::new(env!("CARGO_BIN_EXE_fq"))
            .args(["connect", &addr, "--token", &token])
            .env("FQ_CONFIG", &client_config)
            .env("XDG_CONFIG_HOME", xdg.path())
            .env("RUST_LOG", "off")
            .stdin(std::process::Stdio::piped())
            .output()
            .expect("run fq connect");
        assert!(
            connect.status.success(),
            "fq connect failed:\n{}",
            String::from_utf8_lossy(&connect.stderr)
        );

        EdgeFixture {
            daemon: Some(daemon),
            dir,
            xdg,
            client_config,
            broker,
        }
    }

    /// The transcript fixture: the plain fixture plus the invocation's
    /// conversation on the event log.
    ///
    /// Published after the daemon is ready, because the daemon is what
    /// provisions the event stream — a publish before that has nothing
    /// to land in. Only the transcript goldens use this: the turns are
    /// projected as they land, so the other flipped goldens
    /// (`invocation show`'s recent-events block) would see a different
    /// world for no reason.
    fn start_with_conversation() -> Self {
        let fixture = Self::start();
        tokio::runtime::Runtime::new()
            .expect("conversation runtime")
            .block_on(async {
                let bus = fq_runtime::EventBus::connect(fixture.broker.url())
                    .await
                    .expect("connect bus");
                for event in conversation_events() {
                    bus.publish(&event).await.expect("publish conversation");
                }
            });
        fixture
    }

    fn run_fq(&self, args: &[&str]) -> (Option<i32>, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_fq"))
            .args(args)
            .env("FQ_CONFIG", &self.client_config)
            .env("FQ_CACHE_DIR", self.dir.path())
            .env("XDG_CONFIG_HOME", self.xdg.path())
            .env("RUST_LOG", "off")
            .env("NO_COLOR", "1")
            .output()
            .expect("run fq");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

impl Drop for EdgeFixture {
    fn drop(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            unsafe {
                libc::kill(daemon.id() as i32, libc::SIGTERM);
            }
            let _ = daemon.wait();
        }
    }
}

/// The edge-transport variant of [`check_golden`]: same argv, same
/// golden file, the data travels the authenticated edge.
fn check_golden_edge(name: &str, args: &[&str], volatile_markers: &[&str]) {
    check_golden_on(EdgeFixture::start(), name, args, volatile_markers);
}

fn check_golden_on(fixture: EdgeFixture, name: &str, args: &[&str], volatile_markers: &[&str]) {
    let (exit, stdout, stderr) = fixture.run_fq(args);
    assert_eq!(
        exit,
        Some(0),
        "fq {args:?} over the edge should exit 0; stderr:\n{stderr}"
    );
    let actual = redact(&stdout, &Nats::Closed, volatile_markers);
    compare_golden(name, &actual);
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

/// Rewrite every runtime-minted UUID to `<UUID>`. Ids in `keep`
/// (fixture identities) stay byte-exact, so the oracle still proves
/// the right invocation was echoed back. Only the mutating goldens
/// need this: the reads render fixture identities alone.
fn redact_uuids(raw: &str, keep: &[&str]) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < bytes.len() {
        if is_uuid_at(bytes, i) {
            let token = &raw[i..i + 36];
            out.push_str(if keep.contains(&token) {
                token
            } else {
                "<UUID>"
            });
            i += 36;
        } else {
            // UUIDs are pure ASCII, so scanning byte-wise is safe: any
            // multi-byte char fails `is_uuid_at` and is copied whole.
            let ch = raw[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// The drop goldens' harness (plan Phase 4, verb 18). Unlike the read
/// goldens this one MUTATES, so it takes a private fixture — daemon,
/// broker and stores all its own — and unlike them it needs a daemon
/// at all: dropping is now an `invocation.drop` command on the edge,
/// and the identities it prints come back from the gated read that
/// follows. The event id is minted per run, so it redacts to `<UUID>`.
fn check_golden_drop(name: &str, args: &[&str]) {
    let fixture = EdgeFixture::start();
    let (exit, stdout, stderr) = fixture.run_fq(args);
    assert_eq!(
        exit,
        Some(0),
        "fq {args:?} over the edge should exit 0; stderr:\n{stderr}"
    );
    let actual = redact_uuids(&redact(&stdout, &Nats::Closed, &[]), &[INV_INFLIGHT]);
    compare_golden(name, &actual);
}

#[test]
fn golden_invocation_list_human() {
    check_golden_edge("invocation_list_human", &["invocation", "list"], &["ago"]);
}

#[test]
fn golden_invocation_list_json() {
    check_golden_edge(
        "invocation_list_json",
        &["invocation", "list", "--json"],
        &[],
    );
}

#[test]
fn golden_invocation_show_human() {
    check_golden_edge(
        "invocation_show_human",
        &["invocation", "show", INV_COMPLETED],
        &["ago"],
    );
}

#[test]
fn golden_invocation_show_json() {
    check_golden_edge(
        "invocation_show_json",
        &["invocation", "show", INV_COMPLETED, "--json"],
        &[],
    );
}

// The transcript goldens (plan Phase 4, verb 20): the same argv and
// the same golden files, now composed from `turn.list` (the Turn atom,
// folded from the event log) with the opening prompt from
// `invocation.get`. Byte-identical output across a change of substrate
// is the whole claim, so the files below are untouched.

// The drop goldens (plan Phase 4, verb 18): the same argv and the
// same golden files, now a command on the edge instead of a control
// request plus a client-side store write. They moved here from
// `golden_commands.rs` because that harness runs `fq` against seeded
// stores with no daemon, and a drop without a daemon is exactly what
// this flip retires. Byte-identical output across the change of
// substrate is the whole claim, so the files below are untouched.

#[test]
fn golden_invocation_drop_human() {
    check_golden_drop(
        "invocation_drop_human",
        &[
            "invocation",
            "drop",
            INV_INFLIGHT,
            "--reason",
            "golden fixture drop",
        ],
    );
}

#[test]
fn golden_invocation_drop_json() {
    check_golden_drop(
        "invocation_drop_json",
        &[
            "invocation",
            "drop",
            INV_INFLIGHT,
            "--reason",
            "golden fixture drop",
            "--json",
        ],
    );
}

#[test]
fn golden_transcript_human() {
    check_golden_on(
        EdgeFixture::start_with_conversation(),
        "transcript_human",
        &["invocation", "transcript", INV_COMPLETED],
        &[],
    );
}

#[test]
fn golden_transcript_full_human() {
    check_golden_on(
        EdgeFixture::start_with_conversation(),
        "transcript_full_human",
        &["invocation", "transcript", INV_COMPLETED, "--full"],
        &[],
    );
}

#[test]
fn golden_transcript_json() {
    check_golden_on(
        EdgeFixture::start_with_conversation(),
        "transcript_json",
        &["invocation", "transcript", INV_COMPLETED, "--json"],
        &[],
    );
}

#[test]
fn golden_workers_list_human() {
    check_golden(
        "workers_list_human",
        &["workers", "list"],
        Nats::Closed,
        &["ago", "age"],
    );
}

#[test]
fn golden_workers_list_json() {
    check_golden(
        "workers_list_json",
        &["workers", "list", "--json"],
        Nats::Closed,
        &[],
    );
}

#[test]
fn golden_workers_show_human() {
    check_golden(
        "workers_show_human",
        &["workers", "show", "worker-alpha"],
        Nats::Closed,
        &["ago", "age"],
    );
}

#[test]
fn golden_workers_show_json() {
    check_golden(
        "workers_show_json",
        &["workers", "show", "worker-alpha", "--json"],
        Nats::Closed,
        &[],
    );
}
