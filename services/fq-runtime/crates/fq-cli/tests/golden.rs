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

async fn seed(db_path: &Path) {
    let proj = ProjectionStore::open(db_path)
        .await
        .expect("open projection");

    // Projection events: two invocations for `researcher`, two for
    // `fixer`, with per-call costs on the LLM responses.
    for event in [
        triggered(AGENT_RESEARCHER, INV_COMPLETED, 1, BASE_MS),
        llm_response(AGENT_RESEARCHER, INV_COMPLETED, 2, BASE_MS + 1_000, 0.0125),
        stamp(
            Event::new(
                AgentId::new(AGENT_RESEARCHER).unwrap(),
                inv(INV_COMPLETED),
                EventPayload::Completed(fq_runtime::events::CompletedPayload {
                    result_summary: Some("Fixture complete.".into()),
                    total_llm_calls: 2,
                    total_tool_calls: 1,
                    total_cost: 0.0125,
                    total_duration_ms: 5_000,
                }),
            ),
            3,
            BASE_MS + 5_000,
        ),
        triggered(AGENT_FIXER, INV_FAILED, 4, BASE_MS + 10_000),
        llm_response(AGENT_FIXER, INV_FAILED, 5, BASE_MS + 11_000, 0.0031),
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
            BASE_MS + 12_000,
        ),
        triggered(AGENT_RESEARCHER, INV_INFLIGHT, 7, BASE_MS + 20_000),
        triggered(AGENT_FIXER, INV_ARCHIVED, 8, BASE_MS + 30_000),
    ] {
        proj.insert_event(&event).await.expect("insert event");
    }

    // Worker WAL: a full llm+tool transcript for INV_COMPLETED, an open
    // (dispatched, uncompleted) LLM call for INV_INFLIGHT, and terminal
    // state for INV_FAILED.
    let worker = WorkerStore::open(db_path).await.expect("open worker store");
    worker
        .upsert_invocation_state(&state_row(
            INV_COMPLETED,
            AGENT_RESEARCHER,
            "completed",
            BASE_MS,
            Some(BASE_MS + 5_000),
        ))
        .await
        .unwrap();
    worker
        .upsert_invocation_state(&state_row(
            INV_FAILED,
            AGENT_FIXER,
            "failed",
            BASE_MS + 10_000,
            Some(BASE_MS + 12_000),
        ))
        .await
        .unwrap();
    worker
        .upsert_invocation_state(&state_row(
            INV_INFLIGHT,
            AGENT_RESEARCHER,
            "awaiting_model",
            BASE_MS + 20_000,
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
            BASE_MS,
        )
        .await
        .unwrap();
    worker
        .write_llm_dispatched(INV_COMPLETED, "req-1", BASE_MS + 100)
        .await
        .unwrap();
    worker
        .write_llm_completed(
            INV_COMPLETED,
            "req-1",
            &first_response,
            false,
            0.0125,
            BASE_MS + 1_000,
        )
        .await
        .unwrap();
    worker
        .write_tool_intent(
            INV_COMPLETED,
            "tc-1",
            "read_file",
            "{\"path\":\"fixture.txt\"}",
            BASE_MS + 1_500,
        )
        .await
        .unwrap();
    worker
        .write_tool_dispatched(INV_COMPLETED, "tc-1", BASE_MS + 1_600)
        .await
        .unwrap();
    worker
        .write_tool_completed(
            INV_COMPLETED,
            "tc-1",
            "{\"bytes\":42,\"content\":\"deterministic\"}",
            false,
            BASE_MS + 2_000,
        )
        .await
        .unwrap();
    worker
        .write_llm_intent(
            INV_COMPLETED,
            "req-2",
            "claude-haiku",
            &request_payload,
            BASE_MS + 3_000,
        )
        .await
        .unwrap();
    worker
        .write_llm_dispatched(INV_COMPLETED, "req-2", BASE_MS + 3_100)
        .await
        .unwrap();
    worker
        .write_llm_completed(
            INV_COMPLETED,
            "req-2",
            &second_response,
            false,
            0.0125,
            BASE_MS + 4_000,
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
            BASE_MS + 21_000,
        )
        .await
        .unwrap();
    worker
        .write_llm_dispatched(INV_INFLIGHT, "req-open", BASE_MS + 21_100)
        .await
        .unwrap();

    // Control plane: workers in each lifecycle state, ownership rows,
    // and one archived invocation with no surviving worker state.
    let cp = ControlPlaneStore::open(db_path)
        .await
        .expect("open control plane");
    cp.register_worker("worker-alpha", "golden-host", BASE_MS)
        .await
        .unwrap();
    cp.register_worker("worker-beta", "golden-host", BASE_MS + 1_000)
        .await
        .unwrap();
    cp.register_worker("worker-omega", "golden-host", BASE_MS + 2_000)
        .await
        .unwrap();
    assert!(cp.mark_worker_stale("worker-alpha").await.unwrap());
    cp.mark_worker_shutdown("worker-omega").await.unwrap();

    for (invocation, agent, status, at) in [
        (
            INV_COMPLETED,
            AGENT_RESEARCHER,
            OwnerStatus::Completed,
            BASE_MS + 5_000,
        ),
        (
            INV_FAILED,
            AGENT_FIXER,
            OwnerStatus::Failed,
            BASE_MS + 12_000,
        ),
        (
            INV_INFLIGHT,
            AGENT_RESEARCHER,
            OwnerStatus::InFlight,
            BASE_MS + 20_000,
        ),
        (
            INV_ARCHIVED,
            AGENT_FIXER,
            OwnerStatus::Completed,
            BASE_MS + 31_000,
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
        started_at: BASE_MS + 30_000,
        terminal_at: BASE_MS + 31_000,
        archived_at: BASE_MS + 31_500,
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
        let db_path = dir.path().join("events.db");
        tokio::runtime::Runtime::new()
            .expect("fixture runtime")
            .block_on(seed(&db_path));
        Fixture { dir }
    })
}

/// The private test broker, started once for the commands that need a
/// live NATS (`status` bails without one). Everything else gets a
/// closed-port URL so no daemon can leak in.
fn broker() -> &'static fq_test_support::NatsServer {
    static BROKER: OnceLock<fq_test_support::NatsServer> = OnceLock::new();
    BROKER.get_or_init(fq_test_support::test_nats)
}

enum Nats {
    /// A live private broker (commands that bail without NATS).
    Live,
    /// A guaranteed-closed port: proves the command needs no NATS.
    Closed,
}

fn run_fq(args: &[&str], nats: &Nats) -> (Option<i32>, String, String) {
    let fixture = fixture();
    let nats_url = match nats {
        Nats::Live => broker().url().to_string(),
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
        Nats::Live => broker().url().to_string(),
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

    let path = golden_path(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &actual).unwrap();
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

#[test]
fn golden_status_human() {
    check_golden(
        "status_human",
        &["status"],
        Nats::Live,
        &["connection:", "rows:"],
    );
}

#[test]
fn golden_status_json() {
    check_golden("status_json", &["status", "--json"], Nats::Live, &[]);
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

#[test]
fn golden_invocation_list_human() {
    check_golden(
        "invocation_list_human",
        &["invocation", "list"],
        Nats::Closed,
        &["ago"],
    );
}

#[test]
fn golden_invocation_list_json() {
    check_golden(
        "invocation_list_json",
        &["invocation", "list", "--json"],
        Nats::Closed,
        &[],
    );
}

#[test]
fn golden_invocation_show_human() {
    check_golden(
        "invocation_show_human",
        &["invocation", "show", INV_COMPLETED],
        Nats::Closed,
        &["ago"],
    );
}

#[test]
fn golden_invocation_show_json() {
    check_golden(
        "invocation_show_json",
        &["invocation", "show", INV_COMPLETED, "--json"],
        Nats::Closed,
        &[],
    );
}

#[test]
fn golden_transcript_human() {
    check_golden(
        "transcript_human",
        &["invocation", "transcript", INV_COMPLETED],
        Nats::Closed,
        &[],
    );
}

#[test]
fn golden_transcript_full_human() {
    check_golden(
        "transcript_full_human",
        &["invocation", "transcript", INV_COMPLETED, "--full"],
        Nats::Closed,
        &[],
    );
}

#[test]
fn golden_transcript_json() {
    check_golden(
        "transcript_json",
        &[
            "invocation",
            "transcript",
            INV_COMPLETED,
            "--format",
            "json",
        ],
        Nats::Closed,
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
