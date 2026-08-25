//! The reports over the edge (plan Phase 4, verbs 13, 14 and 15) —
//! every one the surface declares: `cost.summary`, `cost.by_agent`,
//! `control.doctor`, `control.status`.
//!
//! What is proved here is what only the wire can prove. The
//! aggregation itself is `Views`' and is covered by its tests; the
//! doctor composite's arithmetic is covered in `doctor_report/tests.rs`.
//! What neither can reach is the three properties a *declared* report
//! has and a function call does not:
//!
//! * **A report is a privilege boundary.** Authority is Read on the
//!   report's own scope, never on its inputs — which is much of the
//!   point of putting aggregates on the surface. A token holding
//!   `read:cost` and nothing else must be able to read fleet spend
//!   while being refused the event log that spend is computed from,
//!   and must be refused `control.doctor`, which is a different scope
//!   entirely. That claim is untestable anywhere but here.
//! * **A parameter is validated, not absorbed.** An unparseable
//!   `since` is a verdict on the request, not a reason to answer over
//!   the whole history — the failure mode where a narrowing silently
//!   widens.
//! * **Absence has a shape.** `cost.by_agent` on an agent with no
//!   spend is NotFound, not an empty breakdown that a caller would
//!   render as "this agent cost nothing".

#![cfg(unix)]

use std::process::{Command, Stdio};
use std::time::Duration;

use fq_ops::{ControlReport, CostReport, Domain, OpId, ReportId};
use fq_runtime::events::{
    CostMetadata, Event, EventPayload, LlmCallOrigin, StopReason, TokenUsage,
};
use fq_runtime::{AgentId, ProjectionStore};
use serde_json::json;
use uuid::Uuid;

/// Epoch for the seeded rows: fixed, far enough in the past that
/// nothing here depends on wall-clock drift.
const BASE_MS: i64 = 1_767_323_045_000;
const INVOCATION: &str = "1c000000-0000-7000-8000-000000000001";
const AGENT: &str = "researcher";

/// The two figures the allocation rule (#466) relates: what an
/// invocation cost, and what the engine spent on its behalf.
const INVOCATION_COST: f64 = 0.0125;
const FRAMEWORK_COST: f64 = 0.0009;

fn unique_scratch() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("edge-reports-{}-{}", std::process::id(), nanos));
    std::fs::create_dir_all(dir.join("cache")).unwrap();
    std::fs::create_dir_all(dir.join("agents")).unwrap();
    std::fs::write(dir.join("fq.toml"), "[edge]\nbind = \"127.0.0.1:0\"\n").unwrap();
    dir
}

fn suffix_of<'a>(log: &'a str, prefix: &str) -> &'a str {
    log.lines()
        .find_map(|l| l.trim().strip_prefix(prefix))
        .unwrap_or_else(|| panic!("log lacks prefix {prefix:?}\n--- log ---\n{log}"))
        .trim()
}

fn parse_fingerprint(hex: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex fingerprint");
    }
    out
}

fn fixed_uuid(n: u32) -> Uuid {
    Uuid::parse_str(&format!("00000000-0000-7000-8000-0000000010{n:02}")).unwrap()
}

fn stamp(mut event: Event, seq: u32, at_ms: i64) -> Event {
    event.envelope.event_id = fixed_uuid(seq);
    event.envelope.timestamp = chrono::DateTime::from_timestamp_millis(at_ms).unwrap();
    event
}

fn cost(seq: u32, total_cost: f64, model: &str) -> CostMetadata {
    CostMetadata {
        call_id: fixed_uuid(seq),
        model: model.into(),
        input_tokens: 1_200,
        output_tokens: 340,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        input_cost: total_cost * 0.8,
        output_cost: total_cost * 0.2,
        total_cost,
        cumulative_invocation_cost: total_cost,
        cumulative_agent_cost: total_cost,
        origin: LlmCallOrigin::AgentTurn,
    }
}

/// One priced agent turn and one priced invocation summary against the
/// same invocation — the smallest world in which the allocation rule
/// says something: the agent's row carries only its own spend, the
/// reserved `summary` row carries only the engine's, and the fleet
/// total is both.
async fn seed_costs(cache: &std::path::Path) {
    let paths = fq_runtime::db::RuntimeDbPaths::under(cache);
    let proj = ProjectionStore::open(&paths.projection)
        .await
        .expect("open projection");

    let invocation = Uuid::parse_str(INVOCATION).unwrap();
    let response = stamp(
        Event::new(
            AgentId::new(AGENT).unwrap(),
            invocation,
            EventPayload::LlmResponse(fq_runtime::events::LlmResponsePayload {
                round: 0,
                call_id: fixed_uuid(2),
                content: Some("Probe reply.".into()),
                tool_calls: Vec::new(),
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 1_200,
                    output_tokens: 340,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
                origin: LlmCallOrigin::AgentTurn,
            }),
        ),
        2,
        BASE_MS + 1_000,
    )
    .with_cost(cost(2, INVOCATION_COST, "claude-haiku"));

    let summary = stamp(
        Event::new(
            AgentId::summary(),
            invocation,
            EventPayload::InvocationSummary(fq_runtime::events::InvocationSummaryPayload {
                kind: fq_runtime::events::SummaryKind::Outcome,
                summary: "Probe run finished clean.".into(),
            }),
        ),
        3,
        BASE_MS + 2_000,
    )
    .with_cost(cost(3, FRAMEWORK_COST, "cheap-model"));

    for event in [response, summary] {
        proj.insert_event(&event, None).await.expect("insert event");
    }
}

/// A running daemon with the costs above already projected, plus the
/// credentials to talk to it.
struct Daemon {
    process: std::process::Child,
    addr: String,
    fingerprint: [u8; 32],
    admin_token: String,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.process.id() as i32, libc::SIGTERM);
        }
        let _ = self.process.wait();
    }
}

async fn start_daemon(server: &fq_test_support::NatsServer) -> Daemon {
    let scratch = unique_scratch();
    seed_costs(&scratch.join("cache")).await;

    let log_path = scratch.join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let log_err = log.try_clone().expect("clone log handle");
    let mut process = Command::new(env!("CARGO_BIN_EXE_fqd"))
        .env("FQ_DAEMON_CONFIG", scratch.join("fq.toml"))
        .env("FQ_NATS_URL", server.url())
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_STATE_DIR", scratch.join("state"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .env("RUST_LOG", "off")
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .expect("spawn fqd");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let text = loop {
        if let Some(status) = process.try_wait().expect("poll fqd") {
            let text = std::fs::read_to_string(&log_path).unwrap_or_default();
            panic!("fqd exited during startup with {status:?}\n--- log ---\n{text}");
        }
        let text = std::fs::read_to_string(&log_path).unwrap_or_default();
        if text.contains("Runtime ready") {
            break text;
        }
        assert!(tokio::time::Instant::now() < deadline, "fqd never ready");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    Daemon {
        addr: suffix_of(&text, "- edge is listening on ").to_string(),
        fingerprint: parse_fingerprint(suffix_of(
            &text,
            "edge: certificate fingerprint (clients pin this): ",
        )),
        admin_token: {
            let mut lines = text.lines();
            lines.find(|l| l.contains("edge: admin token")).unwrap();
            lines.next().unwrap().trim().to_string()
        },
        process,
    }
}

async fn invoke(
    client: &fq_edge::EdgeClient,
    op: OpId,
    input: serde_json::Value,
) -> Result<serde_json::Value, fq_edge::wire::WireError> {
    client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op,
                version: 1,
                input,
                min_seq: None,
            },
        )
        .await
        .expect("rpc")
        .map(|r| r.output)
}

fn cost_summary() -> OpId {
    OpId::Report(ReportId::Cost(CostReport::Summary))
}

fn cost_by_agent() -> OpId {
    OpId::Report(ReportId::Cost(CostReport::ByAgent))
}

fn control_doctor() -> OpId {
    OpId::Report(ReportId::Control(ControlReport::Doctor))
}

fn control_status() -> OpId {
    OpId::Report(ReportId::Control(ControlReport::Status))
}

/// Close enough for money: the figures cross a JSON wire as f64, so
/// exact equality would be asserting about IEEE-754 rather than about
/// the report.
fn about(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

/// The fleet report, and the identity its declaration promises:
/// `total_cost = <per-invocation costs> + framework_cost`, with the
/// remainder named rather than merely missing.
#[tokio::test]
async fn cost_summary_reports_spend_and_names_the_unallocated_remainder() {
    let server = fq_test_support::NatsServer::start();
    let daemon = start_daemon(&server).await;
    let client =
        fq_edge::EdgeClient::connect(&daemon.addr, daemon.fingerprint, &daemon.admin_token)
            .await
            .expect("connect edge");

    let report: fq_runtime::views::CostReport = serde_json::from_value(
        invoke(&client, cost_summary(), json!({}))
            .await
            .expect("report"),
    )
    .expect("the declared output shape");

    assert!(
        about(report.total_cost, INVOCATION_COST + FRAMEWORK_COST),
        "the fleet total is both kinds of spend: {report:?}"
    );
    assert!(
        about(report.framework_cost, FRAMEWORK_COST),
        "the engine's own spend is named on the report: {report:?}"
    );
    assert!(
        about(report.total_cost - report.framework_cost, INVOCATION_COST),
        "the remainder must reconcile, not merely be absent: {report:?}"
    );

    // The rule holds per agent too, which is what makes the drill-down
    // self-explaining: the agent carries none of it, the reserved
    // `summary` row carries all of it.
    let by_id = |id: &str| {
        report
            .agents
            .iter()
            .find(|a| a.agent_id == id)
            .unwrap_or_else(|| panic!("no `{id}` row in {report:?}"))
            .clone()
    };
    assert!(about(by_id(AGENT).framework_cost, 0.0));
    assert!(about(by_id(AGENT).total_cost, INVOCATION_COST));
    let summary_row = by_id("summary");
    assert!(about(summary_row.framework_cost, summary_row.total_cost));
}

/// The drill-down, including the case that looks like missing data and
/// is not: the reserved `summary` agent has spend and no invocations
/// under it, by construction.
#[tokio::test]
async fn cost_by_agent_drills_down_and_is_not_found_when_there_is_nothing_to_drill() {
    let server = fq_test_support::NatsServer::start();
    let daemon = start_daemon(&server).await;
    let client =
        fq_edge::EdgeClient::connect(&daemon.addr, daemon.fingerprint, &daemon.admin_token)
            .await
            .expect("connect edge");

    let detail: fq_runtime::views::AgentCostDetailView = serde_json::from_value(
        invoke(&client, cost_by_agent(), json!({ "agent": AGENT }))
            .await
            .expect("report"),
    )
    .expect("the declared output shape");
    assert_eq!(detail.agent_id, AGENT);
    assert!(about(detail.totals.total_cost, INVOCATION_COST));
    assert_eq!(
        detail.invocations.len(),
        1,
        "the agent's spend is allocated to its invocation: {detail:?}"
    );

    let framework: fq_runtime::views::AgentCostDetailView = serde_json::from_value(
        invoke(&client, cost_by_agent(), json!({ "agent": "summary" }))
            .await
            .expect("report"),
    )
    .expect("the declared output shape");
    assert!(
        about(framework.totals.framework_cost, framework.totals.total_cost),
        "engine spend is the whole of the summary row: {framework:?}"
    );
    assert!(
        framework.invocations.is_empty(),
        "engine spend is charged to no invocation — an empty list here is the rule \
         showing through, not missing data: {framework:?}"
    );

    // An agent with no spend has no row of the summary to drill into,
    // so it is not found rather than a breakdown of zero — which a
    // caller would render as "this agent cost nothing".
    let err = invoke(&client, cost_by_agent(), json!({ "agent": "never-ran" }))
        .await
        .expect_err("an agent with no spend is not found");
    assert!(
        matches!(err, fq_edge::wire::WireError::NotFound { .. }),
        "got: {err:?}"
    );
}

/// A narrowing that cannot be understood must not silently widen. The
/// grammar is `views::since`'s, and the refusal quotes both the
/// spelling and the accepted set.
#[tokio::test]
async fn a_since_that_names_no_instant_is_refused_rather_than_ignored() {
    let server = fq_test_support::NatsServer::start();
    let daemon = start_daemon(&server).await;
    let client =
        fq_edge::EdgeClient::connect(&daemon.addr, daemon.fingerprint, &daemon.admin_token)
            .await
            .expect("connect edge");

    for op in [cost_summary(), cost_by_agent()] {
        let err = invoke(
            &client,
            op.clone(),
            json!({ "agent": AGENT, "since": "last tuesday" }),
        )
        .await
        .expect_err("an unparseable since is a verdict on the request");
        match err {
            fq_edge::wire::WireError::InvalidInput { op: named, message } => {
                assert_eq!(named, op.to_string());
                assert!(message.contains("last tuesday"), "got: {message}");
            }
            other => panic!("{op} should refuse, got: {other:?}"),
        }
    }

    // And a spelling it *does* understand narrows rather than refuses:
    // a bound after the seeded rows leaves nothing behind.
    let report: fq_runtime::views::CostReport = serde_json::from_value(
        invoke(&client, cost_summary(), json!({ "since": "2099-01-01" }))
            .await
            .expect("report"),
    )
    .expect("the declared output shape");
    assert!(report.agents.is_empty(), "got: {report:?}");
}

/// The composite answers, and it answers about the daemon that served
/// it — its own worker row is in the count, which is the sense in
/// which this report needs the thing it reports on.
#[tokio::test]
async fn control_doctor_answers_about_the_daemon_that_serves_it() {
    let server = fq_test_support::NatsServer::start();
    let daemon = start_daemon(&server).await;
    let client =
        fq_edge::EdgeClient::connect(&daemon.addr, daemon.fingerprint, &daemon.admin_token)
            .await
            .expect("connect edge");

    let report = invoke(&client, control_doctor(), json!({}))
        .await
        .expect("report");

    assert!(
        report["workers"]["alive"].as_i64().expect("alive count") >= 1,
        "the serving daemon registers itself, so it is in its own roster: {report}"
    );
    // The sections are all present, including the ones that are empty
    // in a healthy fixture — a report that omitted them would read as
    // "not checked" rather than "nothing found".
    for section in [
        "workers",
        "executions",
        "ambiguous",
        "failures",
        "dead_letters",
    ] {
        assert!(!report[section].is_null(), "missing {section}: {report}");
    }
    assert_eq!(report["dead_letters"]["exhausted_triggers"], 0);
    assert_eq!(
        report["failures"].as_array().expect("failures").len(),
        0,
        "nothing failed in this fixture: {report}"
    );
}

/// The machinery report answers with things only a running daemon
/// has: which build it is, the JetStream probe over the connection it
/// holds, and its own live registry. Every section is present even
/// when empty — a report that omitted them would read as "not
/// checked" rather than "nothing found".
#[tokio::test]
async fn control_status_answers_with_what_only_a_running_daemon_has() {
    let server = fq_test_support::NatsServer::start();
    let daemon = start_daemon(&server).await;
    let client =
        fq_edge::EdgeClient::connect(&daemon.addr, daemon.fingerprint, &daemon.admin_token)
            .await
            .expect("connect edge");

    let report = invoke(&client, control_status(), json!({}))
        .await
        .expect("report");

    for section in [
        "version",
        "streams",
        "registry",
        "projection_rows",
        "recovery",
    ] {
        assert!(!report[section].is_null(), "missing {section}: {report}");
    }
    assert!(
        report["version"]
            .as_str()
            .expect("a version string")
            .contains('+'),
        "the build is semver plus the commit it was built from: {report}"
    );
    // The probe reached the daemon's own streams — the client never
    // connects to the broker, so this could not be here otherwise.
    let streams = report["streams"].as_array().expect("streams");
    assert_eq!(streams.len(), 2, "both core streams are probed: {report}");
    assert!(
        streams.iter().all(|s| s
            .get("available")
            .is_some_and(|a| a["consumer"]["active"].get("name").is_some())),
        "a live daemon's streams have their durable consumers: {report}"
    );
    // This fixture's agents directory is empty, and an empty registry
    // is a zero rather than an omission.
    assert_eq!(report["registry"]["agents"], 0);
    assert_eq!(
        report["registry"]["load_errors"]
            .as_array()
            .expect("load errors")
            .len(),
        0
    );
    // The projection holds the seeded cost events, read by the daemon
    // that owns the store.
    assert!(
        report["projection_rows"].as_i64().expect("row count") >= 2,
        "the seeded events are folded: {report}"
    );
}

/// **The point of putting an aggregate on the surface.** A report's
/// authority is Read on its own scope and never on its inputs, so
/// spend is grantable without granting the event log it is computed
/// from — and `control.doctor`, being a different scope, is not
/// carried along with it.
///
/// Both directions are asserted, because a boundary that only lets
/// things through is not a boundary.
#[tokio::test]
async fn a_report_is_a_privilege_boundary_over_its_scope_not_its_inputs() {
    let server = fq_test_support::NatsServer::start();
    let daemon = start_daemon(&server).await;

    let cost_only = fq_edge::attenuate(
        &daemon.admin_token,
        &[("read".to_string(), "cost".to_string())],
    )
    .expect("attenuate to cost");
    let control_only = fq_edge::attenuate(
        &daemon.admin_token,
        &[("read".to_string(), "control".to_string())],
    )
    .expect("attenuate to control");

    let accountant = fq_edge::EdgeClient::connect(&daemon.addr, daemon.fingerprint, &cost_only)
        .await
        .expect("connect edge");
    invoke(&accountant, cost_summary(), json!({}))
        .await
        .expect("read:cost reads fleet spend");
    let denied = invoke(&accountant, OpId::List(Domain::Event), json!({}))
        .await
        .expect_err("read:cost must not carry the event log the spend is computed from");
    assert!(
        matches!(denied, fq_edge::wire::WireError::Denied { .. }),
        "got: {denied:?}"
    );
    let denied = invoke(&accountant, control_doctor(), json!({}))
        .await
        .expect_err("read:cost must not carry another domain's report");
    assert!(
        matches!(denied, fq_edge::wire::WireError::Denied { .. }),
        "got: {denied:?}"
    );

    let operator = fq_edge::EdgeClient::connect(&daemon.addr, daemon.fingerprint, &control_only)
        .await
        .expect("connect edge");
    invoke(&operator, control_doctor(), json!({}))
        .await
        .expect("read:control reads the health composite");
    let denied = invoke(&operator, cost_summary(), json!({}))
        .await
        .expect_err("read:control must not carry spend");
    assert!(
        matches!(denied, fq_edge::wire::WireError::Denied { .. }),
        "got: {denied:?}"
    );
}
