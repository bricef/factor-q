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

use fq_runtime::control_plane::store::{
    ControlPlaneStore, InvocationArchiveRow, OwnerStatus, WorkerStatus,
};
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

/// The invocation's opening messages: the agent's system prompt and
/// the user message its trigger became. One definition seeded into
/// both records of that one fact — the WAL's first
/// `llm_dispatch.request_payload` (which the read service's transcript
/// mines) and the `llm.request` event (which the Turn fold folds into
/// the opening prompt turn). If the two ever disagree the fixture is
/// lying, not the code.
fn opening_messages() -> Vec<Message> {
    vec![
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

/// A call that ended in `llm.failure` (#447), in the shape that
/// bills: a 200 with nothing in it, so the provider's usage is
/// recoverable and priced onto the envelope exactly like a response.
/// The fixture carries one so the read commands are pinned against a
/// *failing* call, not only a succeeding one — the coverage whose
/// absence let the missing event ship.
fn llm_failure(agent: &str, invocation: &str, seq: u32, at_ms: i64, total_cost: f64) -> Event {
    let payload = EventPayload::LlmFailure(fq_runtime::events::LlmFailurePayload {
        round: 2,
        call_id: fixed_uuid(seq),
        model: "claude-haiku".into(),
        error_kind: fq_runtime::events::LlmErrorKind::EmptyResponse,
        error_message: "model returned an empty response (no content, no tool calls)".into(),
        duration_ms: 900,
        usage: Some(TokenUsage {
            input_tokens: 1_200,
            output_tokens: 340,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        }),
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
        llm_failure(AGENT_FIXER, INV_FAILED, 9, base_ms + 11_500, 0.0007),
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
        proj.insert_event(&event, None).await.expect("insert event");
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

    let request_payload = serde_json::json!({ "messages": opening_messages() }).to_string();
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
        .env("FQ_STATE_DIR", fixture.dir.path().join("state"))
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
        .env("FQ_STATE_DIR", dir.path().join("state"))
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
/// Every newly flipped verb joins this list — `agent list` (verb 9) is
/// the one whose old self needed no daemon at all.
#[test]
fn flipped_verb_without_a_pairing_says_how_to_pair() {
    for verb in [
        ["invocation", "list"],
        ["agent", "list"],
        ["events", "tail"],
        ["dead-letters", "list"],
    ] {
        let xdg = tempfile::tempdir().expect("xdg dir");
        let out = Command::new(env!("CARGO_BIN_EXE_fq"))
            .args(verb)
            .env("FQ_CONFIG", "/nonexistent/fq.toml")
            .env("XDG_CONFIG_HOME", xdg.path())
            .env("RUST_LOG", "off")
            .output()
            .expect("run fq binary");
        assert_ne!(out.status.code(), Some(0), "{verb:?} must fail unpaired");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("fq connect"),
            "unpaired {verb:?} must point at `fq connect`; got:\n{stderr}"
        );
    }
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

// REVIEWED GOLDEN CHANGE (plan Phase 4, verb 12) — the second in this
// migration, and the same correction as the first (see the roster
// rationale at `golden_workers_list_human`). It is not a golden
// weakened to make a flip pass.
//
// The previous expectation was nine events and nothing else. That
// listing could only be produced by reading the projection with the
// daemon absent — and for THIS surface that is not merely an unusual
// state, it is a contradictory one: the daemon is what publishes
// events and what projects them, so a store holding events that no
// daemon ever touched describes a world in which nothing could have
// produced them. The projection consumer runs with `filter_subjects:
// Vec::new()` and indexes every subject, so a real `fq events query`
// has ALWAYS shown the daemon's own events — `system_startup`,
// `system_recovery`, and whatever its boot recovery had to say about
// the seeded in-flight work.
//
// Production output does not change with this flip. What changed is
// that the TEST now runs a daemon, so its world finally contains what
// production's always did. The nine seeded rows are still here, still
// in order, with every timestamp, agent, event type, cost and
// invocation byte-identical to before; the daemon's rows join them,
// newest first, and only the values a running daemon minted are
// redacted — by value, not by field list (see
// `redact_event_index_json`). A regression that turned a seeded
// timestamp into `now`, or dropped the log position off a projected
// row, fails here rather than being absorbed.
//
// The rows are also the Event atom's INDEX rows now, not events: no
// payload, plus the `event_id` that reads one back whole through
// `event.get`. The log position is deliberately absent — it is an
// internal locator `event.get` resolves through, not something a
// caller holds. That contract is declared on the atom itself, and
// `edge_event_tail::a_list_row_walks_to_its_whole_event` executes the
// walk.
#[test]
fn golden_events_query_human() {
    check_golden_events("events_query_human", &["events", "query"]);
}

#[test]
fn golden_events_query_json() {
    check_golden_events("events_query_json", &["events", "query", "--json"]);
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
        // The opening `llm.request`, published (in the real runner)
        // immediately before the provider call that answers it — so it
        // precedes round 1's response in the log, carries the same
        // instant the WAL row's `intent_at` records, and is where the
        // transcript's prompt turn comes from.
        stamp(
            Event::new(
                agent.clone(),
                inv(INV_COMPLETED),
                EventPayload::LlmRequest(fq_runtime::events::LlmRequestPayload {
                    call_id: fixed_uuid(20),
                    model: "claude-haiku".into(),
                    messages: opening_messages(),
                    tools_available: Vec::new(),
                    request_params: fq_runtime::events::RequestParams {
                        effort: None,
                        temperature: None,
                        max_tokens: Some(4096),
                    },
                    origin: LlmCallOrigin::AgentTurn,
                }),
            ),
            19,
            BASE_MS,
        ),
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
    /// The plain fixture: stores seeded, event log empty, agent
    /// directory empty. Every golden but the transcript ones renders
    /// folds, which the stores already hold.
    fn start() -> Self {
        Self::start_with_agents(&[])
    }

    /// The fixture with agent definitions on disk before the daemon
    /// boots, so the daemon's live registry holds them (plan Phase 4,
    /// verb 9). Definitions are `(file name, contents)`; a file that
    /// fails to parse is left in deliberately, because the load-error
    /// path is part of `fq agent list`'s contract.
    ///
    /// Declaring agents forces the daemon's pricing guarantee
    /// (ADR-0004) to have something to check, so the config gains a
    /// provider declaring their model with an explicit price. The
    /// override keeps the fixture hermetic: coverage is satisfied from
    /// config, never from the network fetch that `PricingTable::load`
    /// attempts.
    fn start_with_agents(definitions: &[(&str, &str)]) -> Self {
        // A private seed at the SAME fixed base as the shared fixture
        // (the JSON goldens embed its literal timestamps). Registration
        // times are pinned; only heartbeats are freshened, and that
        // happens after the daemon is up (see `freshen_live_workers`) —
        // the live daemon's stale-worker sweep runs at startup, and an
        // ancient heartbeat on the in-flight row's owner would
        // (correctly) reclassify live work as ambiguous.
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
                // in-flight work alone. `fq workers list` renders these
                // rows, so their registration times are pinned like
                // every other seeded timestamp.
                for (offset, worker) in [(10_000, "fixer"), (11_000, "researcher")] {
                    cp.register_worker(worker, "golden-host", BASE_MS + offset)
                        .await
                        .expect("register owner worker");
                }
            });
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(&agents_dir).expect("agents dir");
        for (name, contents) in definitions {
            std::fs::write(agents_dir.join(name), contents).expect("agent definition");
        }

        let broker = fq_test_support::NatsServer::start();
        let daemon_config = dir.path().join("fqd.toml");
        let pricing = if definitions.is_empty() {
            String::new()
        } else {
            format!(
                "\n[providers.anthropic]\nmodels = [\"{FIXTURE_AGENT_MODEL}\"]\n\n\
                 [providers.anthropic.pricing.\"{FIXTURE_AGENT_MODEL}\"]\n\
                 input_per_mtok = 1.0\noutput_per_mtok = 5.0\n"
            )
        };
        std::fs::write(
            &daemon_config,
            format!("[edge]\nbind = \"127.0.0.1:0\"\n{pricing}"),
        )
        .expect("fqd.toml");
        let log_path = dir.path().join("daemon.log");
        let log = std::fs::File::create(&log_path).expect("daemon log");
        let log_err = log.try_clone().expect("log handle");
        let mut daemon = Command::new(env!("CARGO_BIN_EXE_fqd"))
            .env("FQ_CONFIG", &daemon_config)
            .env("FQ_NATS_URL", broker.url())
            .env("FQ_CACHE_DIR", dir.path())
            .env("FQ_STATE_DIR", dir.path().join("state"))
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

                // The workers that should read `alive` get their
                // heartbeat here — after the daemon's startup sweep,
                // and moments before the verb under test runs — rather
                // than before the daemon was spawned. A worker is alive
                // only as long as its heartbeat is inside the 30s
                // threshold, and fixture startup (two 30s deadlines,
                // a broker, a daemon boot, a pairing) can outlast that
                // under a loaded parallel run. Freshening last shrinks
                // the exposure to the pairing plus one process spawn.
                //
                // `worker-beta` is freshened with them deliberately:
                // seeded `alive` with a 2026-01-02 heartbeat, it is a
                // contradiction the sweep exists to repair, and a
                // fixture should not ask a golden to pin a state the
                // system is actively correcting.
                //
                // Let the sweep finish repairing first, though. It
                // publishes one `worker.orphaned` per alive→stale
                // transition it wins, and freshening mid-sweep steals
                // transitions from it — so how many of those events
                // exist would be decided by a race, and `fq events
                // query` renders them (plan Phase 4, verb 12). Waiting
                // for every ancient-heartbeat row to have been promoted
                // makes the count a fact of the fixture (`worker-beta`,
                // `fixer`, `researcher`; `worker-alpha` is seeded stale
                // and `worker-omega` shutdown, and the sweep only
                // promotes alive rows). The freshening below then puts
                // all three back to `alive`, which is what the roster
                // golden pins — and now pins as an outcome rather than
                // as a race won.
                let threshold =
                    fq_runtime::control_plane::coordination_consumer::DEFAULT_STALE_THRESHOLD_MS;
                let deadline = std::time::Instant::now() + Duration::from_secs(30);
                loop {
                    let now_ms = chrono::Utc::now().timestamp_millis();
                    let pending: Vec<String> = cp
                        .list_workers()
                        .await
                        .expect("read worker roster")
                        .into_iter()
                        .filter(|w| {
                            w.status == WorkerStatus::Alive
                                && now_ms - w.last_heartbeat >= threshold
                        })
                        .map(|w| w.worker_id)
                        .collect();
                    if pending.is_empty() {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the daemon's stale sweep never promoted {pending:?}; the fixture \
                         freshens after it so the orphan events are counted, not raced"
                    );
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }

                for worker in ["fixer", "researcher", "worker-beta"] {
                    cp.heartbeat_worker(worker, chrono::Utc::now().timestamp_millis())
                        .await
                        .expect("freshen live worker heartbeat");
                }
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
        Self::start_with_events(conversation_events())
    }

    /// [`start_with_conversation`](Self::start_with_conversation) over
    /// an arbitrary event list — the seam a negative control needs, so
    /// a test can withhold one event and watch what stops rendering.
    fn start_with_events(events: Vec<Event>) -> Self {
        let fixture = Self::start();
        tokio::runtime::Runtime::new()
            .expect("conversation runtime")
            .block_on(async {
                let bus = fq_runtime::EventBus::connect(fixture.broker.url())
                    .await
                    .expect("connect bus");
                for event in &events {
                    bus.publish(event).await.expect("publish conversation");
                }
            });
        fixture
    }

    fn run_fq(&self, args: &[&str]) -> (Option<i32>, String, String) {
        self.run_fq_with_agents_dir(args, &self.agents_dir())
    }

    fn agents_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("agents")
    }

    /// Stop the daemon and wait for it, leaving the pairing and the
    /// stores behind — a client that is configured and paired, talking
    /// to nothing.
    fn stop_daemon(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            unsafe {
                libc::kill(daemon.id() as i32, libc::SIGTERM);
            }
            let _ = daemon.wait();
        }
    }

    /// The client's own agents directory is named explicitly, and by
    /// default it is the very directory the daemon loaded from. That
    /// is the A/B this fixture exists to make honest for verb 9:
    /// nothing about the harness changes across the flip, so a golden
    /// that held when `fq agent list` read the disk and still holds
    /// when it reads the daemon's registry is comparing like with
    /// like. Point it somewhere else and the two answers separate —
    /// which is exactly what [`agent_list_reads_the_daemon_not_the_client_disk`]
    /// asserts.
    fn run_fq_with_agents_dir(
        &self,
        args: &[&str],
        agents_dir: &std::path::Path,
    ) -> (Option<i32>, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_fq"))
            .args(args)
            .env("FQ_CONFIG", &self.client_config)
            .env("FQ_CACHE_DIR", self.dir.path())
            .env("FQ_STATE_DIR", self.dir.path().join("state"))
            .env("FQ_AGENTS_DIR", agents_dir)
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

/// Epoch-ms at or after this are a runtime clock read, not one of the
/// fixture's pinned constants: a full day past the fixture epoch, so
/// no seeded offset can reach it and no real `now` can fall short.
const WALL_CLOCK_FLOOR_MS: i64 = BASE_MS + 86_400_000;

/// The host every seeded worker registers under. The daemon's own row
/// carries `local_host_label()` instead — `$HOSTNAME` when the
/// environment exports one — so it is machine-dependent.
const FIXTURE_HOST: &str = "golden-host";

/// The worker roster's volatile fields, redacted **by field** so that
/// which rows exist, in what order, with which status and in-flight
/// count all stay pinned — that set is the answer under test.
///
/// Three things in a live daemon's roster are minted at run time: the
/// daemon's own worker id (its `runtime_id`), the host it reports, and
/// every wall-clock-derived time — the heartbeat ages the human table
/// renders, and the epoch-ms fields of the rows whose heartbeat is
/// current. Line-level digit collapsing would take the in-flight
/// counts with them, so each field is replaced on its own.
fn redact_worker_roster(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for line in redact_uuids(raw, &[]).lines() {
        out.push_str(&redact_worker_line(line));
        out.push('\n');
    }
    out
}

fn redact_worker_line(line: &str) -> String {
    // JSON form: one field per line, so the field name is the key.
    if let Some((key, value)) = json_field(line) {
        let redacted = match key {
            "host" if value.trim_matches('"') != FIXTURE_HOST => Some("\"<HOST>\"".to_string()),
            "registered_at_ms" | "last_heartbeat_ms"
                if value
                    .parse::<i64>()
                    .is_ok_and(|ms| ms >= WALL_CLOCK_FLOOR_MS) =>
            {
                Some("<MS>".to_string())
            }
            _ => None,
        };
        if let Some(redacted) = redacted {
            let prefix = line.len() - line.trim_start().len();
            let comma = if line.ends_with(',') { "," } else { "" };
            return format!("{}\"{key}\": {redacted}{comma}", " ".repeat(prefix));
        }
        return line.to_string();
    }

    // Human form: `worker status hb-age in-flight host`, each field but
    // the last left-padded to a fixed width. Only a numeric age is
    // wall-clock-derived — `stale` and `future` are verdicts and stay.
    let fields: Vec<(usize, &str)> = field_spans(line);
    if fields.len() < 5 || !["alive", "stale", "shutdown"].contains(&fields[1].1) {
        return line.to_string();
    }
    let mut line = line.to_string();
    let (host_at, host) = fields[4];
    if host != FIXTURE_HOST {
        line.replace_range(host_at..host_at + host.len(), "<HOST>");
    }
    let (age_at, age) = fields[2];
    if age.ends_with(['s', 'm', 'h']) && age[..age.len() - 1].parse::<u64>().is_ok() {
        // Replace the whole 10-wide column, not just the token: `0s`
        // and `10s` differ in length, and the padding after them is
        // what keeps the row byte-stable across runs.
        let width = 10.min(line.len() - age_at);
        line.replace_range(age_at..age_at + width, &format!("{:<width$}", "<AGE>"));
    }
    // Last, because widening the first field shifts every span after
    // it: a redacted worker id is 30 columns shorter than the UUID it
    // replaced, and an unpadded row would read as a rendering bug in
    // `fq workers list` rather than as a redaction.
    let (id_at, id) = fields[0];
    if id == "<UUID>" {
        line.replace_range(id_at..id_at + id.len(), &format!("{id:<28}"));
    }
    line
}

/// `("key", "value")` for a `  "key": value[,]` line, else `None`.
fn json_field(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.trim().split_once(": ")?;
    Some((
        key.strip_prefix('"')?.strip_suffix('"')?,
        value.trim_end_matches(','),
    ))
}

/// Every whitespace-delimited field with its byte offset.
fn field_spans(line: &str) -> Vec<(usize, &str)> {
    let mut spans = Vec::new();
    let mut at = 0;
    while let Some(start) = line[at..].find(|c: char| !c.is_whitespace()) {
        let from = at + start;
        let field = line[from..]
            .split_whitespace()
            .next()
            .expect("non-space run");
        spans.push((from, field));
        at = from + field.len();
    }
    spans
}

/// The wall-clock floor as the event index writes its timestamps —
/// the same rule and the same instant as [`WALL_CLOCK_FLOOR_MS`], in
/// the units this surface renders. RFC3339 with a fixed `+00:00`
/// offset compares lexically in instant order, which is also why the
/// projection can range-scan the column.
fn wall_clock_floor_rfc3339() -> String {
    chrono::DateTime::from_timestamp_millis(WALL_CLOCK_FLOOR_MS)
        .expect("floor is a valid instant")
        .to_rfc3339()
}

/// Every identity the events fixture pins, so [`redact_uuids`] can
/// tell a seeded row's ids from a running daemon's. Derived from the
/// fixture's own constants rather than listed again: a new seeded
/// event joins this set by being seeded.
fn seeded_identities() -> Vec<String> {
    let mut keep: Vec<String> = (1..=99).map(|n| fixed_uuid(n).to_string()).collect();
    keep.extend(
        [INV_COMPLETED, INV_FAILED, INV_INFLIGHT, INV_ARCHIVED]
            .iter()
            .map(|id| id.to_string()),
    );
    keep
}

/// The event index's volatile values, redacted **by value**: every
/// deterministic figure in the answer stays literal, and only values a
/// running daemon minted are replaced.
///
/// Three kinds, and each rule is a property of the value rather than a
/// field name — so a regression that turned a seeded value into a
/// runtime one still fails here rather than being absorbed:
///
/// * **UUIDs** — every id outside the fixture's own set
///   ([`seeded_identities`]) is runtime-minted. The daemon's system
///   events correlate on its runtime id, so that is their invocation.
/// * **Timestamps** — at or after the wall-clock floor (a full day
///   past the fixture epoch), a clock read rather than a seeded
///   constant.
/// * **Log positions** — the fixture seeds its index rows directly, so
///   they have none and render `null`; every row a daemon projected
///   carries the position it was delivered at. A present `seq` is
///   therefore a runtime value, and a `null` one is the fixture
///   saying, accurately, that no log ever carried that row.
///
/// Every rule is a property of one value, so this walks lines rather
/// than re-serialising a parsed document: the golden is the verb's own
/// rendering, field order included, and a round trip through
/// `serde_json::Value` would re-sort it.
fn redact_event_index_json(raw: &str) -> String {
    let keep = seeded_identities();
    let keep: Vec<&str> = keep.iter().map(String::as_str).collect();
    let floor = wall_clock_floor_rfc3339();
    redact_uuids(raw, &keep)
        .lines()
        .map(|line| {
            let Some((key, value)) = json_field(line) else {
                return line.to_string();
            };
            let redacted = match key {
                "timestamp" if *value.trim_matches('"') >= *floor => "\"<TIME>\"",
                "seq" if value != "null" => "\"<SEQ>\"",
                _ => return line.to_string(),
            };
            let indent = line.len() - line.trim_start().len();
            let comma = if line.ends_with(',') { "," } else { "" };
            format!("{}\"{key}\": {redacted}{comma}", " ".repeat(indent))
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// [`redact_event_index_json`]'s rules applied to the human table,
/// where a row is `timestamp agent event cost invocation` and the
/// invocation is its first 8 characters — too short for
/// [`redact_uuids`] to recognise, so the row's own timestamp is what
/// says whether that prefix was minted at run time.
fn redact_event_index_human(raw: &str) -> String {
    let floor = wall_clock_floor_rfc3339();
    let seeded: Vec<String> = seeded_identities()
        .iter()
        .map(|id| id.chars().take(8).collect())
        .collect();
    raw.lines()
        .map(|line| {
            let fields = field_spans(line);
            // The timestamp column is rendered to second precision.
            let runtime = fields
                .first()
                .is_some_and(|(_, ts)| ts.len() == 19 && *ts >= &floor[..19]);
            if !runtime {
                return line.to_string();
            }
            let mut line = line.to_string();
            // Invocation last-but-one in the replace order: the
            // timestamp column is fixed-width, so rewriting it first
            // would shift the span the invocation was found at.
            if let Some((at, inv)) = fields.get(4)
                && !seeded.iter().any(|s| s == inv)
            {
                line.replace_range(at..&(at + inv.len()), "<INV>");
            }
            // The whole 20-wide column, not just the token: the
            // padding after it is what keeps the row byte-stable.
            let (at, _) = fields[0];
            let width = 20.min(line.len() - at);
            line.replace_range(at..at + width, &format!("{:<width$}", "<TIME>"));
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// The event-index goldens' harness (plan Phase 4, verb 12): a live
/// daemon, and the answer redacted by value.
fn check_golden_events(name: &str, args: &[&str]) {
    let fixture = EdgeFixture::start();
    let (exit, stdout, stderr) = fixture.run_fq(args);
    assert_eq!(
        exit,
        Some(0),
        "fq {args:?} over the edge should exit 0; stderr:\n{stderr}"
    );
    let plain = redact(&stdout, &Nats::Closed, &[]);
    let actual = if args.contains(&"--json") {
        redact_event_index_json(&plain)
    } else {
        redact_event_index_human(&plain)
    };
    compare_golden(name, &actual);
}

/// The roster goldens' harness: [`check_golden_edge`] with the
/// worker-specific field redaction (plan Phase 4, verb 21).
fn check_golden_roster(name: &str, args: &[&str]) {
    let fixture = EdgeFixture::start();
    let (exit, stdout, stderr) = fixture.run_fq(args);
    assert_eq!(
        exit,
        Some(0),
        "fq {args:?} over the edge should exit 0; stderr:\n{stderr}"
    );
    let actual = redact_worker_roster(&redact(&stdout, &Nats::Closed, &[]));
    compare_golden(name, &actual);
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

/// The negative control for "the opening prompt is a Turn".
///
/// The transcript's prompt block is folded from the invocation's
/// opening `llm.request` event and from nothing else. Publish the same
/// conversation with that one event withheld and the prompt must
/// vanish — while every entry after it renders unchanged, so this is
/// the prompt going missing, not the transcript.
///
/// What makes it a control rather than a tautology: the fixture's
/// worker WAL still holds `req-1`'s stored `request_payload`, carrying
/// the very system/user pair the goldens print (see `seed_at`). That
/// is the record the deleted `invocation.get{with_prompt}` path read.
/// If any WAL-backed read were still supplying the prompt, it would
/// render here regardless of the event log. It does not.
#[test]
fn the_transcripts_prompt_comes_from_the_llm_request_event() {
    let present = EdgeFixture::start_with_conversation();
    let (exit, stdout, stderr) = present.run_fq(&["invocation", "transcript", INV_COMPLETED]);
    assert_eq!(exit, Some(0), "stderr:\n{stderr}");
    assert!(
        stdout.contains("── prompt ──") && stdout.contains("You are a deterministic fixture."),
        "with the llm.request event, the prompt renders; got:\n{stdout}"
    );

    let withheld = EdgeFixture::start_with_events(
        conversation_events()
            .into_iter()
            .filter(|e| !matches!(e.payload, EventPayload::LlmRequest(_)))
            .collect(),
    );
    let (exit, stdout, stderr) = withheld.run_fq(&["invocation", "transcript", INV_COMPLETED]);
    assert_eq!(exit, Some(0), "stderr:\n{stderr}");
    assert!(
        !stdout.contains("── prompt ──"),
        "no llm.request event, no prompt block; got:\n{stdout}"
    );
    assert!(
        !stdout.contains("You are a deterministic fixture."),
        "the WAL still holds this prompt — nothing may read it back; got:\n{stdout}"
    );
    assert!(
        stdout.contains("Reading the fixture file first.")
            && stdout.contains("── tool result: read_file ──"),
        "the rest of the conversation is untouched; got:\n{stdout}"
    );
}

// REVIEWED GOLDEN CHANGE (plan Phase 4, verb 21) — the one in this
// migration, and here is why it is not a golden weakened to make a
// flip pass.
//
// The previous expectation was three workers, one of them `alive`
// with a heartbeat from 2026-01-02. That roster was wrong twice over:
//
//   * It described a store no daemon had ever touched. The daemon
//     self-registers its own worker row at startup (`run_daemon`, and
//     that has always been true) — so an operator running `fq workers
//     list` against a live system has ALWAYS seen that row. The old
//     golden could only be produced by reading the store with the
//     daemon absent, which is not a state operators read from.
//   * `alive` with a stale heartbeat is a contradiction the system
//     actively repairs: the coordination consumer's sweep promotes
//     such a row to `stale` on sight (first tick immediately, then
//     every 10s). The fixture now freshens that worker's heartbeat
//     instead, so the row is self-consistent rather than pinned
//     mid-repair.
//
// Production output does not change with this flip. What changed is
// that the TEST now runs a daemon, so its world finally contains what
// production's always did. The listing still covers all three
// statuses — `alive` (the daemon's own row, both owner workers,
// `worker-beta`), `stale` (`worker-alpha`), `shutdown`
// (`worker-omega`) — and the row set, its order, every status and
// every in-flight count stay pinned; only runtime-minted identity and
// wall-clock time are redacted, by field.
#[test]
fn golden_workers_list_human() {
    check_golden_roster("workers_list_human", &["workers", "list"]);
}

#[test]
fn golden_workers_list_json() {
    check_golden_roster("workers_list_json", &["workers", "list", "--json"]);
}

#[test]
fn golden_workers_show_human() {
    check_golden_edge(
        "workers_show_human",
        &["workers", "show", "worker-alpha"],
        &["ago", "age"],
    );
}

#[test]
fn golden_workers_show_json() {
    check_golden_edge(
        "workers_show_json",
        &["workers", "show", "worker-alpha", "--json"],
        &[],
    );
}

// ------------------------------------------------------------------
// The Agent view goldens (plan Phase 4, verb 9).
//
// Verb 9 shipped with no goldens at all, so these are written FIRST,
// against the CLI's own disk read, and only then is the verb moved
// onto the daemon's live registry. Without that order "byte-identical"
// is a claim no test can refute.
// ------------------------------------------------------------------

/// The model the fixture's definitions name. Declared in the daemon's
/// config with an explicit price so the ADR-0004 pricing guarantee is
/// satisfied offline. Spelled out again inside each definition below —
/// they are `&'static str` and cannot interpolate it — so the daemon
/// refusing to start is what a mismatch looks like.
const FIXTURE_AGENT_MODEL: &str = "claude-haiku-4-5";

/// Two definitions the registry loads and one it rejects. The reject
/// is a plain markdown file with no frontmatter — the most ordinary
/// way an agents directory acquires one (someone drops a note in it) —
/// and its error text is fixed, so it belongs in a golden.
fn agent_definitions() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "researcher.md",
            "---\nname: researcher\nmodel: claude-haiku-4-5\ntools:\n  - builtin__exec\n\
             budget: 1.00\n---\n\nYou research.\n",
        ),
        (
            "fixer.md",
            "---\nname: fixer\nmodel: claude-haiku-4-5\ntools:\n  - builtin__exec\n  \
             - builtin__file_read\n---\n\nYou fix.\n",
        ),
        ("notes.md", "# Scratch notes\n\nNot an agent definition.\n"),
    ]
}

/// Run `fq agent list` against a fixture whose daemon loaded
/// `definitions`, with the agents directory redacted (it is a
/// tempdir, and the rendered rows name it in every `path=`).
fn check_golden_agents(name: &str, args: &[&str], definitions: &[(&str, &str)]) {
    let fixture = EdgeFixture::start_with_agents(definitions);
    let (exit, stdout, stderr) = fixture.run_fq(args);
    assert_eq!(
        exit,
        Some(0),
        "fq {args:?} should exit 0; stderr:\n{stderr}"
    );
    let agents_dir = fixture.agents_dir().display().to_string();
    compare_golden(name, &stdout.replace(&agents_dir, "<AGENTS_DIR>"));
}

// REVIEWED GOLDEN CHANGE (plan Phase 4, verb 9) — one line, and only
// after both goldens were first captured against the old disk read.
//
// Every agent row and the whole error block are byte-identical across
// the flip. What changed is the provenance line, because the old one
// named a directory: `Loaded 2 agent(s) from <dir>:` and `No agents
// found in <dir>`. There is no directory in the daemon's answer, and
// printing the CLIENT's configured path over rows the DAEMON computed
// would reintroduce exactly the skew this flip removes — the client's
// path can name a directory the daemon never read. The rows carry
// `path=` each, so the "which file?" question is still answered, per
// row, by the daemon.
#[test]
fn golden_agent_list_human() {
    check_golden_agents("agent_list_human", &["agent", "list"], &agent_definitions());
}

/// An agents directory with nothing in it.
#[test]
fn golden_agent_list_empty_human() {
    check_golden_agents("agent_list_empty_human", &["agent", "list"], &[]);
}

/// The skew, gone by construction: the client's own agents directory
/// holds a different definition entirely, and the listing reports the
/// daemon's registry regardless. Before the flip this test would have
/// printed `client-only` — that disagreement, between what an operator
/// reads and what the daemon would run, is what verb 9 was moved for.
#[test]
fn agent_list_reads_the_daemon_not_the_client_disk() {
    let fixture = EdgeFixture::start_with_agents(&agent_definitions());
    let elsewhere = fixture.dir.path().join("client-agents");
    std::fs::create_dir_all(&elsewhere).expect("client agents dir");
    std::fs::write(
        elsewhere.join("client-only.md"),
        "---\nname: client-only\nmodel: claude-haiku-4-5\n---\n\nOnly on the client's disk.\n",
    )
    .expect("client-only definition");

    let (exit, stdout, stderr) = fixture.run_fq_with_agents_dir(&["agent", "list"], &elsewhere);
    assert_eq!(exit, Some(0), "stderr:\n{stderr}");
    assert!(
        !stdout.contains("client-only"),
        "the client's own directory must not be read; got:\n{stdout}"
    );
    for daemon_side in ["researcher", "fixer", "notes.md"] {
        assert!(
            stdout.contains(daemon_side),
            "the daemon's registry must be what is listed ({daemon_side}); got:\n{stdout}"
        );
    }
}

/// What the flip costs, stated: the listing now needs the daemon that
/// holds the registry, so "I could not ask" is exit 1 with an
/// operator-facing reason — never an empty listing, which would read
/// as "this daemon has no agents".
///
/// This test replaces a golden that pinned `Agent directory <dir> does
/// not exist (resolved: <dir>)`. That message described the CLI
/// reading its own configured path, and there is no longer a path to
/// read; it was captured before the flip (its golden is in this
/// commit's history) and is answered here by the behaviour that took
/// its place.
#[test]
fn agent_list_without_a_daemon_reports_why() {
    let mut fixture = EdgeFixture::start_with_agents(&agent_definitions());
    // Paired first, so this is not the unpaired path — the client has
    // a connection and the daemon behind it is gone.
    let (exit, stdout, _) = fixture.run_fq(&["agent", "list"]);
    assert_eq!(
        exit,
        Some(0),
        "the fixture pairs and lists before the daemon is stopped"
    );
    assert!(stdout.contains("researcher"), "got: {stdout}");

    fixture.stop_daemon();
    let (exit, stdout, stderr) = fixture.run_fq(&["agent", "list"]);
    assert_eq!(
        exit,
        Some(1),
        "a daemon-less listing must fail loudly; stdout:\n{stdout}"
    );
    assert!(
        stdout.is_empty(),
        "a failed listing must not print a partial answer; got:\n{stdout}"
    );
    assert!(
        stderr.contains("could not reach the edge at") && stderr.contains("Connection refused"),
        "the failure must name the edge it could not reach; got:\n{stderr}"
    );
}

// ------------------------------------------------------------------
// The DeadLetter atom goldens (plan Phase 4, verb 7).
//
// The same argv and the same golden files, now `dead_letter.list` on
// the authenticated edge instead of the CLI running its own ephemeral
// scan against the bus. They moved here from `golden_commands.rs`
// because that harness drives `fq` against a broker with no daemon,
// which is exactly what this flip retires. Byte-identical output
// across the change of substrate is the whole claim, so the files are
// untouched.
//
// A live daemon does NOT change the answer, and that is worth stating
// because it is where two earlier flips found their goldens pinning an
// impossible world. A dead letter is a `Failed` event with
// `error_kind = TriggerExhausted`, published only by the dispatcher's
// inline emitter or by the advisory watcher — both of which need a
// trigger to actually exhaust its deliveries. The fixture publishes no
// triggers, so none of the daemon's own events (startup, recovery,
// worker registration, heartbeats) is one: the roster grew when `fq
// workers list` gained a daemon, and this listing does not.
// ------------------------------------------------------------------

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
    use fq_runtime::events::{
        DEAD_LETTER_PAYLOAD_KEY, DEAD_LETTER_SOURCE_KEY, DEAD_LETTER_STREAM_SEQ_KEY,
        DEAD_LETTER_SUBJECT_KEY,
    };
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
        serde_json::json!(fq_runtime::bus::trigger_subject(agent)),
    )
    .annotate(DEAD_LETTER_PAYLOAD_KEY, payload)
    .annotate(DEAD_LETTER_STREAM_SEQ_KEY, serde_json::json!(trigger_seq))
    .annotate(DEAD_LETTER_SOURCE_KEY, serde_json::json!(source));
    stamp(event, seq, at_ms)
}

/// Two dead letters for `researcher` (older trigger seq 11, newer 12)
/// plus one ordinary failure. The third is the negative control: it
/// lands on the very same `fq.agent.researcher.failed` subject the
/// atom reads, so only the `error_kind` predicate keeps it out of the
/// listing.
fn dead_letter_events() -> Vec<Event> {
    vec![
        dead_letter_event(
            AGENT_RESEARCHER,
            11,
            "inline",
            serde_json::json!({"n": 1}),
            21,
            BASE_MS,
        ),
        dead_letter_event(
            AGENT_RESEARCHER,
            12,
            "advisory",
            serde_json::json!({"n": 2}),
            22,
            BASE_MS + 1_000,
        ),
        stamp(
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
        ),
    ]
}

#[test]
fn golden_dead_letters_list_human() {
    check_golden_on(
        EdgeFixture::start_with_events(dead_letter_events()),
        "dead_letters_list_human",
        &["dead-letters", "list"],
        &[],
    );
}

#[test]
fn golden_dead_letters_list_json() {
    check_golden_on(
        EdgeFixture::start_with_events(dead_letter_events()),
        "dead_letters_list_json",
        &["dead-letters", "list", "--json"],
        &[],
    );
}

/// The narrowing the verb offers, now applied at the log rather than
/// after the fact — and the proof that it is applied at all: the
/// fixture's dead letters belong to `researcher`, so `--agent fixer`
/// must come back empty rather than listing them.
///
/// Not a golden, because "no dead letters" is one line that two very
/// different bugs produce (a filter that excludes everything, and a
/// read that found nothing); the positive case above is the oracle,
/// and this is the discrimination it needs beside it.
#[test]
fn the_agent_filter_narrows_the_listing() {
    let fixture = EdgeFixture::start_with_events(dead_letter_events());

    let (exit, stdout, stderr) =
        fixture.run_fq(&["dead-letters", "list", "--agent", AGENT_RESEARCHER]);
    assert_eq!(exit, Some(0), "stderr:\n{stderr}");
    assert!(
        stdout.contains("2 dead-lettered trigger(s)"),
        "the agent's own dead letters still list; got:\n{stdout}"
    );

    let (exit, stdout, stderr) = fixture.run_fq(&["dead-letters", "list", "--agent", AGENT_FIXER]);
    assert_eq!(exit, Some(0), "stderr:\n{stderr}");
    assert_eq!(
        stdout, "No dead-lettered triggers.\n",
        "another agent's listing must be empty; got:\n{stdout}"
    );
}

/// `--limit` is the atom's, not the renderer's: asking for one row
/// must return the NEWEST one, because the listing leads with it. A
/// limit applied to the wrong end of the scan would still print one
/// row, and would print the wrong one.
#[test]
fn a_limit_keeps_the_newest_rows() {
    let fixture = EdgeFixture::start_with_events(dead_letter_events());
    let (exit, stdout, stderr) = fixture.run_fq(&["dead-letters", "list", "--limit", "1"]);
    assert_eq!(exit, Some(0), "stderr:\n{stderr}");
    assert!(
        stdout.contains("1 dead-lettered trigger(s)") && stdout.contains("seq=12 via advisory"),
        "one row, and it is the newest; got:\n{stdout}"
    );
    assert!(
        !stdout.contains("seq=11"),
        "the older dead letter must not survive the limit; got:\n{stdout}"
    );
}
