//! The DeadLetter atom end-to-end through the authenticated edge
//! (plan Phase 4, cohort 4.2, verb 7): Get by log sequence, List
//! narrowed by the typed filter, and Stream via the real long-poll
//! `next_batch`.
//!
//! The goldens in `golden.rs` are the flip's oracle — same argv, same
//! bytes. This suite covers the surface the goldens cannot reach,
//! because `fq dead-letters list` only ever calls one of the three
//! ops: the Get whose key is the log sequence, and the Stream that
//! the atom's nature derives and no verb consumes yet. A declared op
//! nothing exercises is a declared op nothing keeps honest.

#![cfg(unix)]

use std::process::{Command, Stdio};
use std::time::Duration;

use fq_ops::{Domain, OpId};
use fq_runtime::events::{
    DEAD_LETTER_PAYLOAD_KEY, DEAD_LETTER_SOURCE_KEY, DEAD_LETTER_STREAM_SEQ_KEY,
    DEAD_LETTER_SUBJECT_KEY, Event, EventPayload, FailedPayload, FailureKind, FailurePhase,
    InvocationTotals,
};
use serde_json::json;

const AGENT: &str = "researcher";
const OTHER_AGENT: &str = "fixer";

fn unique_scratch() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("edge-dead-letter-{}-{}", std::process::id(), nanos));
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

fn failure(agent: &str, kind: FailureKind, message: &str) -> Event {
    Event::new(
        fq_runtime::AgentId::new(agent).unwrap(),
        uuid::Uuid::now_v7(),
        EventPayload::Failed(FailedPayload {
            error_kind: kind,
            error_message: message.into(),
            phase: FailurePhase::Setup,
            partial_totals: InvocationTotals::default(),
        }),
    )
}

/// A dead letter as both emitters shape it.
fn dead_letter(agent: &str, trigger_seq: u64, source: &str, payload: serde_json::Value) -> Event {
    failure(
        agent,
        FailureKind::TriggerExhausted,
        &format!("trigger exhausted after 5 deliveries (limit 5) [{source}]"),
    )
    .annotate(
        DEAD_LETTER_SUBJECT_KEY,
        json!(fq_runtime::bus::trigger_subject(agent)),
    )
    .annotate(DEAD_LETTER_PAYLOAD_KEY, payload)
    .annotate(DEAD_LETTER_STREAM_SEQ_KEY, json!(trigger_seq))
    .annotate(DEAD_LETTER_SOURCE_KEY, json!(source))
}

#[tokio::test]
async fn the_dead_letter_atom_lives_end_to_end() {
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
    let bus = fq_runtime::EventBus::connect(server.url())
        .await
        .expect("connect bus");

    let stream_op = OpId::Stream(Domain::DeadLetter);
    let all: serde_json::Value = json!({});

    // Tail-seek FIRST: from_seq = MAX with a zero wait returns an
    // empty batch and a concrete resume cursor — the gap-free seam.
    let seek = client
        .rpc
        .next_batch(
            tarpc::context::current(),
            fq_edge::NextBatchRequest {
                op: stream_op.clone(),
                version: 1,
                filter: all.clone(),
                from_seq: u64::MAX,
                max_wait_ms: 0,
            },
        )
        .await
        .expect("rpc")
        .expect("tail seek");
    assert!(seek.items.is_empty());
    assert!(seek.next_from_seq < u64::MAX, "a concrete resume cursor");

    // Published AFTER the seek, so the stream must deliver them. The
    // ordinary failure between the two dead letters shares their
    // subject and must be skipped — while still advancing the cursor.
    let first = bus
        .publish(&dead_letter(AGENT, 11, "inline", json!({"n": 1})))
        .await
        .expect("publish first dead letter");
    let ordinary = bus
        .publish(&failure(AGENT, FailureKind::RuntimeError, "ordinary"))
        .await
        .expect("publish ordinary failure");
    let second = bus
        .publish(&dead_letter(AGENT, 12, "advisory", json!({"n": 2})))
        .await
        .expect("publish second dead letter");
    let elsewhere = bus
        .publish(&dead_letter(OTHER_AGENT, 13, "inline", json!({"n": 3})))
        .await
        .expect("publish another agent's dead letter");

    let batch = client
        .rpc
        .next_batch(
            tarpc::context::current(),
            fq_edge::NextBatchRequest {
                op: stream_op.clone(),
                version: 1,
                filter: all.clone(),
                from_seq: seek.next_from_seq,
                max_wait_ms: 10_000,
            },
        )
        .await
        .expect("rpc")
        .expect("stream batch");
    let seqs: Vec<u64> = batch.items.iter().map(|i| i.seq).collect();
    assert_eq!(
        seqs,
        vec![first, second, elsewhere],
        "every agent's dead letters, and only the dead letters: {batch:?}"
    );
    assert!(
        batch.next_from_seq > ordinary,
        "the cursor advances past an ordinary failure it did not emit"
    );
    assert_eq!(batch.items[1].item["seq"], second);
    assert_eq!(
        batch.items[1].item["dead_letter"]["trigger_stream_seq"], 12,
        "the trigger sequence rides in the payload, not in the key"
    );
    assert_eq!(batch.items[1].item["dead_letter"]["source"], "advisory");

    // Get by log sequence: the identity a stream item hands back.
    let got = client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: OpId::Get(Domain::DeadLetter),
                version: 1,
                input: json!({"seq": second}),
                min_seq: None,
            },
        )
        .await
        .expect("rpc")
        .expect("dead_letter.get");
    assert_eq!(got.output["seq"], second);
    assert_eq!(got.output["dead_letter"]["agent_id"], AGENT);
    assert_eq!(
        got.output["dead_letter"]["trigger_payload"],
        json!({"n": 2})
    );

    // The sequence of an ordinary failure holds an event, not this
    // atom — a miss, not a coercion.
    let miss = client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: OpId::Get(Domain::DeadLetter),
                version: 1,
                input: json!({"seq": ordinary}),
                min_seq: None,
            },
        )
        .await
        .expect("rpc");
    assert!(
        matches!(&miss, Err(fq_edge::wire::WireError::NotFound { op, .. })
            if op == "dead_letter.get"),
        "an ordinary failure is not a dead letter; got {miss:?}"
    );

    // List, unfiltered: sequence order, every agent.
    let listed = client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: OpId::List(Domain::DeadLetter),
                version: 1,
                input: all,
                min_seq: None,
            },
        )
        .await
        .expect("rpc")
        .expect("dead_letter.list");
    let rows = listed.output.as_array().expect("a list").clone();
    assert_eq!(
        rows.iter()
            .map(|r| r["seq"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![first, second, elsewhere],
        "List answers in sequence order — the seam Stream resumes at"
    );

    // List, narrowed: the filter travels, and it is applied at the log.
    let mine = client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: OpId::List(Domain::DeadLetter),
                version: 1,
                input: json!({"agent": AGENT}),
                min_seq: None,
            },
        )
        .await
        .expect("rpc")
        .expect("dead_letter.list --agent");
    let mine = mine.output.as_array().expect("a list").clone();
    assert_eq!(
        mine.iter()
            .map(|r| r["seq"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![first, second],
        "another agent's dead letter is out of scope"
    );

    // A limit keeps the most recent page, in sequence order.
    let newest = client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: OpId::List(Domain::DeadLetter),
                version: 1,
                input: json!({"limit": 1}),
                min_seq: None,
            },
        )
        .await
        .expect("rpc")
        .expect("dead_letter.list --limit");
    let newest = newest.output.as_array().expect("a list").clone();
    assert_eq!(newest.len(), 1);
    assert_eq!(newest[0]["seq"], elsewhere, "the newest, not the first");

    // An agent id that is not a subject token is a verdict on the
    // request, not an empty answer.
    let refused = client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: OpId::List(Domain::DeadLetter),
                version: 1,
                input: json!({"agent": "not a token"}),
                min_seq: None,
            },
        )
        .await
        .expect("rpc");
    assert!(
        matches!(&refused, Err(fq_edge::wire::WireError::InvalidInput { op, .. })
            if op == "dead_letter.list"),
        "a malformed agent id must be refused, not silently matched; got {refused:?}"
    );

    // An idle long poll times out with progress, not a hang.
    let idle = client
        .rpc
        .next_batch(
            tarpc::context::current(),
            fq_edge::NextBatchRequest {
                op: stream_op,
                version: 1,
                filter: json!({}),
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
