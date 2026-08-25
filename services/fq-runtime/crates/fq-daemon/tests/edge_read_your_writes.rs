//! The 3c acceptance: `drop` → `get(min_seq = receipt.watermark)`
//! composes read-your-writes through the public surface alone. The
//! receipt names the drop event's sequence; the gated read waits at
//! the read horizon — EVERY consumer feeding the Invocation view's
//! fold (projection AND coordination) — so when it answers, the
//! archive row, the failed owner status, and the recent event are all
//! there. No sleeps, no polling: the horizon is the synchronisation.

#![cfg(unix)]

use std::process::{Command, Stdio};
use std::time::Duration;

use fq_ops::{Domain, OpId, Receipt};
use serde_json::json;

fn unique_scratch() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("edge-ryw-{}-{}", std::process::id(), nanos));
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

#[tokio::test]
async fn drop_then_gated_get_sees_every_effect() {
    let server = fq_test_support::NatsServer::start();
    let scratch = unique_scratch();

    let log_path = scratch.join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let log_err = log.try_clone().expect("clone log handle");
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_fqd"))
        .env("FQ_DAEMON_CONFIG", scratch.join("fq.toml"))
        .env("FQ_NATS_URL", server.url())
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_STATE_DIR", scratch.join("state"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .expect("spawn fqd");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let text = loop {
        if let Some(status) = daemon.try_wait().expect("poll fqd") {
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
    let fingerprint = parse_fingerprint(suffix_of(
        &text,
        "edge: certificate fingerprint (clients pin this): ",
    ));
    let token = {
        let mut lines = text.lines();
        lines.find(|l| l.contains("edge: admin token")).unwrap();
        lines.next().unwrap().trim().to_string()
    };
    let addr = suffix_of(&text, "- edge is listening on ").to_string();

    // A real invocation exists because its Triggered event is in the
    // log — published over the bus like any producer would.
    let bus = fq_runtime::EventBus::connect(server.url())
        .await
        .expect("connect bus");
    let invocation_id = uuid::Uuid::now_v7();
    let agent = fq_runtime::agent::AgentId::new("rw-probe").unwrap();
    let event = fq_runtime::events::Event::new(
        agent,
        invocation_id,
        fq_runtime::events::EventPayload::Triggered(fq_runtime::events::TriggeredPayload {
            trigger_id: None,
            trigger_source: fq_runtime::events::TriggerSource::Manual,
            trigger_subject: None,
            trigger_payload: json!({}),
            config_snapshot: fq_runtime::events::ConfigSnapshot {
                name: "rw-probe".to_string(),
                model: "claude-haiku-4-5".to_string(),
                system_prompt: "probe".to_string(),
                tools: vec![],
                sandbox: fq_runtime::events::SandboxSnapshot::default(),
                budget: None,
                ..Default::default()
            },
        }),
    );
    let triggered_seq = bus.publish(&event).await.expect("publish triggered");

    let client = fq_edge::EdgeClient::connect(&addr, fingerprint, &token)
        .await
        .expect("connect edge");

    // Read-your-writes for the SETUP, not only for the assertion.
    // This test used to command the drop straight after `publish`,
    // and failed roughly one run in three under a full parallel
    // `cargo test` sweep — passing every time in isolation — with
    //
    //     invocation.drop: NotFound { op: "invocation.drop",
    //                                 message: "no invocation `<uuid>`" }
    //
    // The assumption that produced it: a successful publish makes the
    // invocation actionable. It does not. What `publish` returns is
    // the event atom's durability coordinate — "this event is in the
    // log at sequence N" — not an execution receipt, and not a
    // promise that any fold has seen it. The invocation materialises
    // as a runtime entity later, when the daemon acts on the event:
    // `drop_invocation` resolves it through the projection
    // (`agent_id_for_invocation`, falling back to the control-plane
    // owner row), and both stores are written by asynchronous durable
    // JetStream consumers. Under load those consumers are
    // descheduled, the drop's lookup runs first, and NotFound is the
    // honest answer at that instant.
    //
    // So gate the setup at the coordinate publish handed back — the
    // same discipline the verification read below applies to the
    // receipt's watermark, simply applied before the command instead
    // of after. The gated read is released only when the read horizon
    // (every consumer feeding the Invocation fold) has applied
    // sequence N, after which the invocation is visible to the drop
    // by construction. A sleep or a retry loop would only make the
    // race rarer and the suite slower — it would still be a race.
    // `min_seq` is accepted on Get/List and deliberately refused on
    // commands (fq-edge/src/server.rs), so a gated read is both the
    // only way to express this ordering and the intended one.
    client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: OpId::Get(Domain::Invocation),
                version: 1,
                input: json!({"invocation_id": invocation_id.to_string()}),
                min_seq: Some(triggered_seq),
            },
        )
        .await
        .expect("rpc")
        .expect("the invocation is materialised at the Triggered event's sequence");

    // The command: drop over the public surface. Its receipt is the
    // read coordinate.
    let receipt = client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: OpId::Verb(fq_ops::VerbId::Invocation(fq_ops::Invocation::Drop)),
                version: 1,
                input: json!({
                    "invocation_id": invocation_id.to_string(),
                    "reason": "read-your-writes probe",
                }),
                min_seq: None,
            },
        )
        .await
        .expect("rpc")
        .expect("invocation.drop");
    let receipt: Receipt = serde_json::from_value(receipt.output).expect("a receipt, never state");
    let min_seq = receipt
        .watermark(Domain::Event)
        .expect("the receipt names the drop event's sequence");

    // The gated read: released only when the whole fold includes the
    // drop — owner flipped, archive written, event projected.
    let detail = client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: OpId::Get(Domain::Invocation),
                version: 1,
                input: json!({"invocation_id": invocation_id.to_string()}),
                min_seq: Some(min_seq),
            },
        )
        .await
        .expect("rpc")
        .expect("gated get");
    let detail = detail.output;
    assert_eq!(
        detail["owner"]["status"], "failed",
        "the owner flip is visible at the receipt's watermark: {detail}"
    );
    assert_eq!(
        detail["archive"]["final_phase"], "failed",
        "the archive row is visible at the receipt's watermark: {detail}"
    );
    assert!(
        detail["recent_events"]
            .as_array()
            .is_some_and(|events| events.iter().any(|e| e["event_type"]
                .as_str()
                .is_some_and(|t| t.contains("operator_recovered")))),
        "the drop event is projected at the receipt's watermark: {detail}"
    );

    // min_seq on a command is refused — receipts gate reads, not
    // writes.
    let refused = client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: fq_ops::OpId::Verb(fq_ops::VerbId::Invocation(fq_ops::Invocation::Drop)),
                version: 1,
                input: json!({"invocation_id": invocation_id.to_string()}),
                min_seq: Some(1),
            },
        )
        .await
        .expect("rpc");
    assert!(
        matches!(refused, Err(fq_edge::wire::WireError::InvalidInput { .. })),
        "min_seq on a command must refuse, got {refused:?}"
    );

    // An unknown id is NotFound — a normal outcome, not invalid input.
    let missing = client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: fq_ops::OpId::Verb(fq_ops::VerbId::Invocation(fq_ops::Invocation::Drop)),
                version: 1,
                input: json!({"invocation_id": uuid::Uuid::now_v7().to_string()}),
                min_seq: None,
            },
        )
        .await
        .expect("rpc");
    assert!(
        matches!(missing, Err(fq_edge::wire::WireError::NotFound { .. })),
        "unknown id must be NotFound, got {missing:?}"
    );

    let rc = unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
    assert_eq!(rc, 0);
    let status = daemon.wait().expect("wait");
    assert!(status.success());
    let _ = std::fs::remove_dir_all(&scratch);
}
