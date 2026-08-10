//! The 3d acceptance: the Turn atom lives end-to-end through the
//! authenticated edge — Get by log sequence, List with full payloads,
//! and Stream via the real long-poll `next_batch`: tail-seek with
//! `from_seq = u64::MAX`, items carrying their sequences, the cursor
//! advancing past non-matching events, and the tool-result join
//! (name, parameters, `initiating_turn`) intact across the wire.

#![cfg(unix)]

use std::process::{Command, Stdio};
use std::time::Duration;

use fq_ops::{Domain, OpId};
use serde_json::json;

fn unique_scratch() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("edge-turn-{}-{}", std::process::id(), nanos));
    std::fs::create_dir_all(dir.join("cache")).unwrap();
    std::fs::create_dir_all(dir.join("agents")).unwrap();
    std::fs::write(dir.join("fq.toml"), "[edge]\nbind = \"127.0.0.1:0\"\n").unwrap();
    dir
}

fn suffix_of<'a>(log: &'a str, prefix: &str) -> &'a str {
    log.lines()
        .find_map(|l| l.trim().strip_prefix(prefix))
        .unwrap_or_else(|| panic!("log lacks prefix {prefix:?}"))
        .trim()
}

fn parse_fingerprint(hex: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex fingerprint");
    }
    out
}

fn assistant_with_call(
    agent: &fq_runtime::agent::AgentId,
    invocation: uuid::Uuid,
    call_id: &str,
) -> fq_runtime::events::Event {
    fq_runtime::events::Event::new(
        agent.clone(),
        invocation,
        fq_runtime::events::EventPayload::LlmResponse(fq_runtime::events::LlmResponsePayload {
            round: 1,
            call_id: uuid::Uuid::now_v7(),
            content: Some("reading the file".into()),
            tool_calls: vec![fq_runtime::events::MessageToolCall {
                tool_call_id: fq_runtime::events::ToolCallId::new(call_id).unwrap(),
                tool_name: "read_file".into(),
                parameters: json!({"path": "fixture.txt"}),
            }],
            stop_reason: fq_runtime::events::StopReason::ToolUse,
            usage: fq_runtime::events::TokenUsage::default(),
            origin: Default::default(),
        }),
    )
}

fn tool_result(
    agent: &fq_runtime::agent::AgentId,
    invocation: uuid::Uuid,
    call_id: &str,
) -> fq_runtime::events::Event {
    fq_runtime::events::Event::new(
        agent.clone(),
        invocation,
        fq_runtime::events::EventPayload::ToolResult(fq_runtime::events::ToolResultPayload {
            round: 1,
            tool_name: "read_file".into(),
            tool_call_id: fq_runtime::events::ToolCallId::new(call_id).unwrap(),
            output: "deterministic".into(),
            is_error: false,
            error_kind: None,
            duration_ms: 3,
        }),
    )
}

#[tokio::test]
async fn the_turn_atom_lives_end_to_end() {
    let server = fq_test_support::NatsServer::start();
    let scratch = unique_scratch();

    let log_path = scratch.join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let log_err = log.try_clone().expect("clone log handle");
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_fqd"))
        .env("FQ_CONFIG", scratch.join("fq.toml"))
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
            panic!("fqd exited during startup with {status:?}");
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

    let client = fq_edge::EdgeClient::connect(&addr, fingerprint, &token)
        .await
        .expect("connect edge");

    // Tail-seek FIRST: from_seq = MAX with a zero wait returns an
    // empty batch and a concrete resume cursor — the gap-free seam.
    let stream_op = OpId::Stream(Domain::Turn);
    let bus = fq_runtime::EventBus::connect(server.url())
        .await
        .expect("connect bus");
    let agent = fq_runtime::agent::AgentId::new("turn-probe").unwrap();
    let invocation = uuid::Uuid::now_v7();
    // The invocation must resolve to its agent (list/stream look the
    // subject up): one Triggered event establishes it.
    let t_seq = bus
        .publish(&fq_runtime::events::Event::new(
            agent.clone(),
            invocation,
            fq_runtime::events::EventPayload::Triggered(fq_runtime::events::TriggeredPayload {
                trigger_id: None,
                trigger_source: fq_runtime::events::TriggerSource::Manual,
                trigger_subject: None,
                trigger_payload: json!({}),
                config_snapshot: fq_runtime::events::ConfigSnapshot {
                    name: "turn-probe".into(),
                    model: "claude-haiku-4-5".into(),
                    system_prompt: "probe".into(),
                    tools: vec![],
                    sandbox: fq_runtime::events::SandboxSnapshot::default(),
                    budget: None,
                    ..Default::default()
                },
            }),
        ))
        .await
        .expect("publish triggered");
    // Gate on the fold including the Triggered event before touching
    // the turn surface — the 3a/3c composition doing its day job.
    client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: OpId::Get(Domain::Invocation),
                version: 1,
                input: json!({"invocation_id": invocation.to_string()}),
                min_seq: Some(t_seq),
            },
        )
        .await
        .expect("rpc")
        .expect("gated get after trigger");

    let filter = json!({"invocation_id": invocation.to_string()});
    let seek = client
        .rpc
        .next_batch(
            tarpc::context::current(),
            fq_edge::NextBatchRequest {
                op: stream_op.clone(),
                version: 1,
                filter: filter.clone(),
                from_seq: u64::MAX,
                max_wait_ms: 0,
            },
        )
        .await
        .expect("rpc")
        .expect("tail seek");
    assert!(seek.items.is_empty());
    assert!(seek.next_from_seq < u64::MAX, "a concrete resume cursor");

    // Publish the turns AFTER the seek: the stream must deliver them.
    let a_seq = bus
        .publish(&assistant_with_call(&agent, invocation, "tc-1"))
        .await
        .expect("publish assistant");
    let r_seq = bus
        .publish(&tool_result(&agent, invocation, "tc-1"))
        .await
        .expect("publish result");

    let batch = client
        .rpc
        .next_batch(
            tarpc::context::current(),
            fq_edge::NextBatchRequest {
                op: stream_op.clone(),
                version: 1,
                filter: filter.clone(),
                from_seq: seek.next_from_seq,
                max_wait_ms: 10_000,
            },
        )
        .await
        .expect("rpc")
        .expect("stream batch");
    assert_eq!(batch.items.len(), 2, "both turns arrive: {batch:?}");
    assert_eq!(batch.items[0].seq, a_seq);
    assert_eq!(batch.items[1].seq, r_seq);
    assert!(batch.next_from_seq > r_seq);
    let result_turn = &batch.items[1].item;
    assert_eq!(result_turn["round"], 1);
    assert_eq!(result_turn["initiating_turn"], a_seq);
    assert_eq!(result_turn["action"]["tool_name"], "read_file");
    assert_eq!(
        result_turn["action"]["parameters"],
        json!({"path": "fixture.txt"}),
        "the join carries the call's parameters across the wire"
    );

    // Get by log sequence: the lone-turn path.
    let got = client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: OpId::Get(Domain::Turn),
                version: 1,
                input: json!({"seq": a_seq}),
                min_seq: None,
            },
        )
        .await
        .expect("rpc")
        .expect("turn.get");
    assert_eq!(got.output["seq"], a_seq);
    assert_eq!(got.output["action"]["kind"], "assistant");

    // List with full payloads.
    let listed = client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: OpId::List(Domain::Turn),
                version: 1,
                input: filter.clone(),
                min_seq: None,
            },
        )
        .await
        .expect("rpc")
        .expect("turn.list");
    let listed = listed.output.as_array().unwrap().clone();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0]["invocation_id"], invocation.to_string());
    assert!(listed[0]["action"]["content"].is_string(), "full payloads");

    // An idle long poll times out with progress, not a hang.
    let idle = client
        .rpc
        .next_batch(
            tarpc::context::current(),
            fq_edge::NextBatchRequest {
                op: stream_op,
                version: 1,
                filter,
                from_seq: batch.next_from_seq,
                max_wait_ms: 200,
            },
        )
        .await
        .expect("rpc")
        .expect("idle poll");
    assert!(idle.items.is_empty());
    assert!(idle.next_from_seq >= batch.next_from_seq);

    let rc = unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
    assert_eq!(rc, 0);
    let status = daemon.wait().expect("wait");
    assert!(status.success());
    let _ = std::fs::remove_dir_all(&scratch);
}
