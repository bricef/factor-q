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
//!
//! It also covers what the surface *declares* about List, not only
//! what List does: the cap on a page is read back off the published
//! operation the way a consumer would read it, so a declaration that
//! drifted from the daemon's behaviour fails from either side.

#![cfg(unix)]

use std::process::{Command, Stdio};
use std::time::Duration;

use fq_ops::{Domain, OpId};
use fq_runtime::dead_letter::{
    DEAD_LETTER_PAYLOAD_KEY, DEAD_LETTER_SOURCE_KEY, DEAD_LETTER_STREAM_SEQ_KEY,
    DEAD_LETTER_SUBJECT_KEY,
};
use fq_runtime::events::{
    Event, EventPayload, FailedPayload, FailureKind, FailurePhase, InvocationTotals,
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
        json!(fq_runtime::events::subjects::trigger(agent)),
    )
    .annotate(DEAD_LETTER_PAYLOAD_KEY, payload)
    .annotate(DEAD_LETTER_STREAM_SEQ_KEY, json!(trigger_seq))
    .annotate(DEAD_LETTER_SOURCE_KEY, json!(source))
}

/// A live `fqd`, an edge client pinned to its certificate, and a bus
/// connection to the same broker — the fixture every test in this file
/// drives.
///
/// Extracted rather than copied when the second test arrived: two
/// daemons started by two hand-written copies of the same forty lines
/// is how the copies drift, and one of them would have been the one
/// that stopped proving anything.
struct World {
    scratch: std::path::PathBuf,
    daemon: std::process::Child,
    client: fq_edge::EdgeClient,
    bus: fq_runtime::EventBus,
}

impl World {
    async fn start(nats_url: &str) -> World {
        let scratch = unique_scratch();
        let log_path = scratch.join("daemon.log");
        let log = std::fs::File::create(&log_path).expect("create daemon log");
        let log_err = log.try_clone().expect("clone log handle");
        let mut daemon = Command::new(env!("CARGO_BIN_EXE_fqd"))
            .env("FQ_DAEMON_CONFIG", scratch.join("fq.toml"))
            .env("FQ_NATS_URL", nats_url)
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
        let token = fq_test_support::admin_token(&scratch.join("state"));
        let addr = suffix_of(&text, "- edge is listening on ").to_string();

        let client = fq_edge::EdgeClient::connect(&addr, fingerprint, &token)
            .await
            .expect("connect edge");
        let bus = fq_runtime::EventBus::connect(nats_url)
            .await
            .expect("connect bus");
        World {
            scratch,
            daemon,
            client,
            bus,
        }
    }

    /// One read op, with the daemon's verdict left intact — for the
    /// requests it is supposed to refuse, where the error *is* the
    /// answer under test.
    async fn invoke(
        &self,
        op: OpId,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, fq_edge::wire::WireError> {
        self.client
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
            .map(|response| response.output)
    }

    fn shutdown(mut self) {
        let rc = unsafe { libc::kill(self.daemon.id() as i32, libc::SIGTERM) };
        assert_eq!(rc, 0);
        let status = self.daemon.wait().expect("wait");
        assert!(status.success());
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

#[tokio::test]
async fn the_dead_letter_atom_lives_end_to_end() {
    let server = fq_test_support::NatsServer::start();
    let world = World::start(server.url()).await;
    let client = &world.client;
    let bus = &world.bus;

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

    world.shutdown();
}

/// The agent this file's cap test publishes under — its own, so the
/// row counts below are exact rather than "at least".
const CAPPED_AGENT: &str = "capped";

/// The largest page `dead_letter.list` will serve, as a consumer reads
/// it: off the declared surface, not out of the source.
fn declared_list_cap(surface: &serde_json::Value) -> u64 {
    surface
        .as_array()
        .expect("the surface describes itself as a list of entries")
        .iter()
        .filter_map(|entry| entry.get("atom"))
        .find(|atom| atom["domain"] == "dead_letter")
        .expect("the DeadLetter atom is on the surface")["filter_schema"]["properties"]["limit"]
        ["maximum"]
        .as_u64()
        .expect("the declared filter says how large a page may be")
}

async fn page(world: &World, limit: u64) -> Result<serde_json::Value, fq_edge::wire::WireError> {
    world
        .invoke(
            OpId::List(Domain::DeadLetter),
            json!({"agent": CAPPED_AGENT, "limit": limit}),
        )
        .await
}

async fn rows(world: &World, limit: u64) -> usize {
    page(world, limit)
        .await
        .unwrap_or_else(|e| panic!("dead_letter.list must serve a page of {limit}; got {e:?}"))
        .as_array()
        .expect("a list of dead letters")
        .len()
}

/// **A List page is bounded, the bound is declared, and an ask above
/// it is refused rather than quietly shortened.**
///
/// `dead_letter.list` served whatever `limit` it was handed, and the
/// CLI made that worse by saturating anything past `u32::MAX` into
/// "everything a page can hold" — a page that holds everything does
/// not exist. One List answer is one frame, the edge's codec stops at
/// 8 MiB, and a dead letter is a fat row: it carries the trigger
/// payload that died with it, opaque JSON nothing truncates. So a
/// large enough listing was a scan of the whole subject followed by a
/// response too large to encode, and the operator paid for the scan.
///
/// Clamping would have fixed the frame and broken the answer. List
/// hands back a bare array — no envelope, no cursor, nowhere to say
/// "there is more" — so a page the daemon shortened is
/// indistinguishable from a listing that ended, and on *this* listing
/// that reads as "nothing else fell on the floor".
///
/// So the three properties that make a cap honest, asserted end to
/// end:
///
/// 1. **The cap is on the surface.** It is read here off
///    `List(Operation)`, the way a consumer would, rather than
///    hardcoded — and since the refusal and the served page are both
///    measured against that number, a declared cap that disagreed with
///    the enforced one fails this test from either side.
/// 2. **Above it is a refusal**, naming the cap and the cursored read
///    that serves more than a page, so the caller's next request is an
///    edit rather than a guess.
/// 3. **Below it the page is the caller's own number** — which is what
///    makes a row count readable at all: fewer rows than you asked for
///    means there are no more.
#[tokio::test]
async fn a_list_page_is_bounded_by_a_cap_the_surface_declares() {
    let server = fq_test_support::NatsServer::start();
    let world = World::start(server.url()).await;

    let cap = declared_list_cap(
        &world
            .invoke(OpId::List(Domain::Operation), json!({}))
            .await
            .expect("operation.list"),
    );

    // A corpus only this test can have published: its own agent, which
    // the filter narrows to. The daemon mints events of its own
    // throughout and none of them is a dead letter (that needs a
    // trigger to actually exhaust), but the assertions here are exact
    // counts rather than set membership, so the window admits nothing
    // else by construction.
    const CORPUS: u64 = 3;
    for i in 0..CORPUS {
        world
            .bus
            .publish(&dead_letter(
                CAPPED_AGENT,
                100 + i,
                "inline",
                json!({"n": i}),
            ))
            .await
            .expect("publish a dead letter");
    }

    let err = page(&world, cap + 1)
        .await
        .expect_err("a page over the cap must be refused, not served short");
    assert!(
        matches!(&err, fq_edge::wire::WireError::InvalidInput { op, message }
            if op == "dead_letter.list"
                && message.contains(&cap.to_string())
                && message.contains("dead_letter.stream")),
        "the refusal must name the cap and the cursored read; got {err:?}"
    );

    // The declared cap is servable, not one the daemon trips over —
    // and asking for it over a three-row corpus answers with three, so
    // "fewer rows than I asked for" is a complete listing rather than
    // a shortened one.
    assert_eq!(
        rows(&world, cap).await,
        CORPUS as usize,
        "a page at the declared cap must be served, and must not be padded to it"
    );

    // Under the cap, `limit` is the caller's own bound in both
    // directions: never rounded up to the cap, never rounded down to
    // some number the daemon preferred.
    assert_eq!(
        rows(&world, CORPUS + 5).await,
        CORPUS as usize,
        "asking for more than exists must answer with everything"
    );
    assert_eq!(
        rows(&world, CORPUS - 1).await,
        (CORPUS - 1) as usize,
        "asking for fewer than exists must answer with exactly that many"
    );

    world.shutdown();
}
