//! `control.projection_rebuild` end-to-end through the authenticated
//! edge: refused without `confirm`, and with it the daemon drops and
//! re-derives its projection under a running consumer — the
//! cost-bearing rows it was seeded with survive, and `control.status`
//! reports the rebuild.
//!
//! The store-level guarantees (what a rebuild keeps, what a bump does)
//! are proved in `fq-runtime`; this suite covers what only the wire can
//! prove — that the verb is served, that its refusal is a verdict on
//! the request, and that the report carries the record afterwards.

#![cfg(unix)]

use std::process::Stdio;
use std::time::Duration;

use fq_ops::surface::StatusReport;
use fq_ops::{Control, ControlReport, CostReport, OpId, ReportId, VerbId};
use fq_runtime::events::{
    CostMetadata, Event, EventPayload, LlmCallOrigin, StopReason, TokenUsage,
};
use fq_runtime::{AgentId, ProjectionStore};
use fq_test_support::TestChild;
use serde_json::json;
use uuid::Uuid;

const BASE_MS: i64 = 1_767_323_045_000;
const INVOCATION: &str = "1c000000-0000-7000-8000-000000000001";
const AGENT: &str = "researcher";
const INVOCATION_COST: f64 = 0.0125;

fn unique_scratch() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("edge-rebuild-{}-{}", std::process::id(), nanos));
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
    Uuid::parse_str(&format!("00000000-0000-7000-8000-0000000020{n:02}")).unwrap()
}

/// A priced call seeded straight into the projection — never on the
/// stream, so nothing can bring it back but the rebuild carrying it
/// across.
async fn seed_cost_row(cache: &std::path::Path) {
    let paths = fq_runtime::db::RuntimeDbPaths::under(cache);
    let proj = ProjectionStore::open(&paths.projection)
        .await
        .expect("open projection");
    let invocation = Uuid::parse_str(INVOCATION).unwrap();
    let mut event = Event::new(
        AgentId::new(AGENT).unwrap(),
        invocation,
        EventPayload::LlmResponse(fq_runtime::events::LlmResponsePayload {
            parts: fq_runtime::events::assistant_parts(Some("Probe reply.".into()), Vec::new()),
            round: 0,
            call_id: fixed_uuid(2),
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 1_200,
                output_tokens: 340,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: None,
            },
            origin: LlmCallOrigin::AgentTurn,
        }),
    )
    .with_cost(CostMetadata {
        call_id: fixed_uuid(2),
        model: "claude-haiku".into(),
        input_tokens: 1_200,
        output_tokens: 340,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        input_cost: INVOCATION_COST * 0.8,
        output_cost: INVOCATION_COST * 0.2,
        total_cost: INVOCATION_COST,
        cumulative_invocation_cost: INVOCATION_COST,
        cumulative_agent_cost: INVOCATION_COST,
        origin: LlmCallOrigin::AgentTurn,
        reasoning_tokens: None,
        reported_cost: None,
    });
    event.envelope.event_id = fixed_uuid(2);
    event.envelope.timestamp = chrono::DateTime::from_timestamp_millis(BASE_MS).unwrap();
    proj.insert_event(&event, None).await.expect("insert event");
}

struct Daemon {
    /// Held, never read: dropping it is what stops the daemon.
    #[allow(dead_code)]
    process: TestChild,
    addr: String,
    fingerprint: [u8; 32],
    admin_token: String,
}

async fn start_daemon(server: &fq_test_support::NatsServer) -> Daemon {
    let scratch = unique_scratch();
    seed_cost_row(&scratch.join("cache")).await;

    let log_path = scratch.join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let log_err = log.try_clone().expect("clone log handle");
    let mut process = TestChild::builder(env!("CARGO_BIN_EXE_fqd"))
        .env("FQ_DAEMON_CONFIG", scratch.join("fq.toml"))
        .env("FQ_NATS_URL", server.url())
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_STATE_DIR", scratch.join("state"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .env("RUST_LOG", "off")
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn();

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
        admin_token: fq_test_support::admin_token(&scratch.join("state")),
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

fn projection_rebuild() -> OpId {
    OpId::Verb(VerbId::Control(Control::ProjectionRebuild))
}

fn control_status() -> OpId {
    OpId::Report(ReportId::Control(ControlReport::Status))
}

fn cost_summary() -> OpId {
    OpId::Report(ReportId::Cost(CostReport::Summary))
}

fn about(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

async fn fleet_total(client: &fq_edge::EdgeClient) -> f64 {
    let summary = invoke(client, cost_summary(), json!({}))
        .await
        .expect("cost.summary");
    summary["total_cost"]
        .as_f64()
        .unwrap_or_else(|| panic!("no fleet total in {summary}"))
}

#[tokio::test]
async fn projection_rebuild_over_the_edge() {
    let server = fq_test_support::NatsServer::start();
    let daemon = start_daemon(&server).await;
    let client =
        fq_edge::EdgeClient::connect(&daemon.addr, daemon.fingerprint, &daemon.admin_token)
            .await
            .expect("edge client");

    // The seeded spend is there before, and no rebuild is on record.
    assert!(about(fleet_total(&client).await, INVOCATION_COST));
    let before: StatusReport = serde_json::from_value(
        invoke(&client, control_status(), json!({}))
            .await
            .expect("control.status"),
    )
    .unwrap();
    assert!(
        before.projection_rebuild.is_none(),
        "a daemon whose projection was never rebuilt reports none: {:?}",
        before.projection_rebuild
    );

    // Without `confirm`: a verdict on the request, and nothing happens.
    let refused = invoke(&client, projection_rebuild(), json!({}))
        .await
        .expect_err("a bare rebuild is refused");
    assert!(
        matches!(&refused, fq_edge::wire::WireError::InvalidInput { op, message }
            if op == "control.projection_rebuild" && message.contains("confirm")),
        "got {refused:?}"
    );
    let unchanged: StatusReport = serde_json::from_value(
        invoke(&client, control_status(), json!({}))
            .await
            .expect("control.status"),
    )
    .unwrap();
    assert!(
        unchanged.projection_rebuild.is_none(),
        "a refused rebuild rebuilds nothing"
    );

    // With it: the daemon rebuilds under its running consumer and
    // answers with an empty receipt.
    let receipt = invoke(
        &client,
        projection_rebuild(),
        json!({ "confirm": true, "reason": "edge test" }),
    )
    .await
    .expect("control.projection_rebuild");
    let receipt: fq_ops::Receipt = serde_json::from_value(receipt).unwrap();
    assert!(
        receipt.atoms.is_empty(),
        "a rebuild appends no atom: {receipt:?}"
    );

    // The record is on the report, the seeded spend was carried across
    // (it was never on the stream, so nothing else could bring it
    // back), and the daemon is still serving.
    let after: StatusReport = serde_json::from_value(
        invoke(&client, control_status(), json!({}))
            .await
            .expect("control.status"),
    )
    .unwrap();
    let rebuild = after
        .projection_rebuild
        .expect("the rebuild is on the status report");
    assert!(rebuild.reason.contains("edge test"), "{rebuild:?}");
    assert!(
        !rebuild.consumer_reset_pending,
        "the durable was reset before the answer"
    );
    assert!(rebuild.target_seq.is_some(), "{rebuild:?}");
    assert_eq!(rebuild.from_version, None);
    assert!(
        about(fleet_total(&client).await, INVOCATION_COST),
        "spend survives a rebuild"
    );

    // The replay catches up: the daemon's own startup events are back
    // in the projection and the record reports complete.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let status: StatusReport = serde_json::from_value(
            invoke(&client, control_status(), json!({}))
                .await
                .expect("control.status"),
        )
        .unwrap();
        let rebuild = status.projection_rebuild.expect("still on record");
        if !rebuild.in_progress && status.projection_rows >= 2 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the replay never completed: {rebuild:?} rows={}",
            status.projection_rows
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
