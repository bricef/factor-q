//! The Trigger atom end-to-end through the authenticated edge (plan
//! Phase 4, verb 6): the walk a `trigger.publish` receipt now promises.
//!
//! The receipt is the point of this suite. `trigger.publish` used to
//! answer `Receipt::empty()` because there was no `trigger.get` for an
//! `AtomRef.key` to resolve against; now there is, and the thing worth
//! proving is not that each verb works in isolation but that **the key
//! the command hands back is the key the Get takes** — fed back
//! verbatim, never re-shaped by the test. A regression to a bare
//! string, a positional key, or the trigger's stream sequence would
//! fail here rather than in a caller's code six months later.
//!
//! The suite drives a real `fqd` and a real broker, because the
//! properties it asserts span the publish, the projection consumer, and
//! the read — the three things a hand-seeded store would let drift.
//!
//! **What stands in, and why.** The one link not exercised live is the
//! worker: a trigger only gets a record when something acts on it, and
//! making that happen inside a test would mean a loaded agent
//! definition and a real provider call. So the worker's `triggered`
//! event is published onto the same bus the worker would publish it on,
//! carrying the identity the receipt just handed back — the same
//! technique `edge_dead_letter_atom` uses for its atom's source events.
//! Everything downstream of that is the daemon's own: the projection
//! consumer folds it, and the three verbs read what it wrote.

#![cfg(unix)]

use std::process::{Command, Stdio};
use std::time::Duration;

use fq_ops::{Domain, OpId, VerbId};
use fq_runtime::dead_letter::{
    DEAD_LETTER_PAYLOAD_KEY, DEAD_LETTER_SOURCE_KEY, DEAD_LETTER_SUBJECT_KEY,
    DEAD_LETTER_TRIGGER_ID_KEY,
};
use fq_runtime::events::{
    Event, EventPayload, FailedPayload, FailureKind, FailurePhase, InvocationTotals, TriggerSource,
    TriggeredPayload,
};
use serde_json::json;

const AGENT: &str = "researcher";
const OTHER_AGENT: &str = "fixer";

fn unique_scratch() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("edge-trigger-{}-{}", std::process::id(), nanos));
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

/// The `triggered` event a worker writes when it starts an invocation
/// from a trigger — the record the atom is read from, standing in for
/// the worker (see the module docs).
fn triggered(agent: &str, trigger_id: &str, payload: serde_json::Value) -> Event {
    Event::new(
        fq_runtime::AgentId::new(agent).unwrap(),
        uuid::Uuid::now_v7(),
        EventPayload::Triggered(TriggeredPayload {
            trigger_id: Some(uuid::Uuid::parse_str(trigger_id).expect("a UUID identity")),
            trigger_source: TriggerSource::Subject,
            trigger_subject: Some(fq_runtime::events::subjects::trigger(agent)),
            trigger_payload: payload,
            config_snapshot: Default::default(),
        }),
    )
}

/// A dead letter naming the trigger that died — the *other* record a
/// trigger can have, and for a trigger whose agent this daemon does not
/// hold, the only one it ever gets.
fn dead_letter(agent: &str, trigger_id: &str, payload: serde_json::Value) -> Event {
    Event::new(
        fq_runtime::AgentId::new(agent).unwrap(),
        uuid::Uuid::now_v7(),
        EventPayload::Failed(FailedPayload {
            error_kind: FailureKind::TriggerExhausted,
            error_message: "trigger exhausted after 5 deliveries (limit 5)".into(),
            phase: FailurePhase::Setup,
            partial_totals: InvocationTotals::default(),
        }),
    )
    .annotate(DEAD_LETTER_TRIGGER_ID_KEY, json!(trigger_id))
    .annotate(
        DEAD_LETTER_SUBJECT_KEY,
        json!(fq_runtime::events::subjects::trigger(agent)),
    )
    .annotate(DEAD_LETTER_PAYLOAD_KEY, payload)
    .annotate(DEAD_LETTER_SOURCE_KEY, json!("inline"))
}

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

    /// Publish a trigger through the edge and hand back the whole
    /// receipt — the value the walk starts from.
    async fn publish(&self, agent: &str, payload: serde_json::Value) -> fq_ops::Receipt {
        let raw = self
            .invoke(
                OpId::Verb(VerbId::Trigger(fq_ops::Trigger::Publish)),
                json!({"agent_id": agent, "payload": payload}),
            )
            .await
            .expect("trigger.publish");
        serde_json::from_value(raw).expect("a command answers with a receipt")
    }

    /// Stand in for the worker: publish the `triggered` event it would
    /// have written for this trigger, under the identity the receipt
    /// named. Everything from here on is the daemon's own path.
    async fn record(&self, agent: &str, key: &serde_json::Value, payload: serde_json::Value) {
        let id = key["trigger_id"]
            .as_str()
            .expect("a receipt names a string id");
        self.bus
            .publish(&triggered(agent, id, payload))
            .await
            .expect("publish the worker's record");
    }

    /// Wait for a trigger's record to reach the projection. Publishing
    /// appends nothing durable — the record appears when the runtime
    /// acts on the trigger — so a read that raced it would be asserting
    /// on the wrong state.
    async fn await_trigger(&self, key: &serde_json::Value) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            match self.invoke(OpId::Get(Domain::Trigger), key.clone()).await {
                Ok(trigger) => return trigger,
                Err(e) => {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "trigger {key} never became readable: {e:?}"
                    );
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    fn shutdown(mut self) {
        let rc = unsafe { libc::kill(self.daemon.id() as i32, libc::SIGTERM) };
        assert_eq!(rc, 0);
        let status = self.daemon.wait().expect("wait");
        assert!(status.success());
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

/// **The receipt's key is exactly what Get accepts.**
///
/// This is the promise step B adds, and it is asserted structurally:
/// the key is checked to be the `{"trigger_id": …}` object and nothing
/// else — not a bare string, not a position, not a key with extra
/// fields — and then handed to `trigger.get` *verbatim*. If a future
/// change re-shaped either side, one of the two halves fails.
///
/// The watermark is checked to be absent from `Domain::Event`, because
/// that is the trap this receipt is one edit away from: publishing
/// appends nothing to the event log, so a watermark filed under Event
/// would be a read-your-writes gate a caller could wait on forever.
#[tokio::test]
async fn a_publish_receipt_walks_to_the_trigger_it_published() {
    let server = fq_test_support::NatsServer::start();
    let world = World::start(server.url()).await;

    let payload = json!({"task": "look at #12", "refs": ["bricef/factor-q"]});
    let receipt = world.publish(AGENT, payload.clone()).await;

    assert_eq!(receipt.atoms.len(), 1, "one trigger, one reference");
    let reference = &receipt.atoms[0];
    assert_eq!(reference.domain, Domain::Trigger);
    let key = reference.key.clone();
    let fields: Vec<&String> = key
        .as_object()
        .expect("an AtomRef key is an object, not a bare id")
        .keys()
        .collect();
    assert_eq!(
        fields,
        vec!["trigger_id"],
        "the key is exactly the Get's key — a positional or ad-hoc shape is the regression"
    );
    assert!(
        uuid::Uuid::parse_str(key["trigger_id"].as_str().expect("a string id")).is_ok(),
        "the identity is a UUID, not a stream position dressed as one"
    );
    assert!(
        receipt.watermark(Domain::Event).is_none(),
        "publishing appends no event, so it must not claim an event-log position"
    );
    assert!(
        receipt.watermarks.is_empty(),
        "publish claims no position at all. Its ack is a coordinate on the *trigger* \
         stream, while a trigger becomes gettable only once the projection folds the \
         event the dispatcher emits later — a position in the event log, which does \
         not exist yet at publish time. A receipt's watermark is documented as the \
         number a caller passes as `min_seq`, so putting the ack there would have them \
         gate a read on a log the reader never consults."
    );

    // Handed back unchanged — the whole walk.
    world.record(AGENT, &key, payload.clone()).await;
    let got = world.await_trigger(&key).await;
    assert_eq!(got["id"], key["trigger_id"], "the same trigger, by name");
    assert_eq!(got["payload"], payload, "the body is kept verbatim");
    assert_eq!(got["source"], "subject");
    assert_eq!(got["subject"], "fq.trigger.researcher");

    // And it lists — index rows, with no payload on them, each
    // carrying the identity that reads the whole trigger back.
    let listed = world
        .invoke(OpId::List(Domain::Trigger), json!({"agent": AGENT}))
        .await
        .expect("trigger.list");
    let row = listed
        .as_array()
        .expect("a list")
        .iter()
        .find(|row| row["trigger_id"] == key["trigger_id"])
        .expect("the trigger just published is in its agent's listing")
        .clone();
    assert_eq!(row["agent_id"], AGENT);
    assert_eq!(row["source"], "subject");
    assert!(
        row.get("payload").is_none(),
        "LIST DOES NOT RETURN PAYLOADS — an index row is bounded; got {row}"
    );
    assert!(
        row["recorded_at"].as_str().is_some(),
        "a row says when the trigger was recorded"
    );

    world.shutdown();
}

/// **A trigger with no durable record is a named state, not a miss.**
///
/// A trigger for an agent this daemon does not hold is acked and never
/// dispatched, so nothing ever records it — the same shape as a trigger
/// that is merely still queued. Answering `NotFound` would tell an
/// operator the system had lost the trigger whose name it had just been
/// handed, so it answers `Unlocatable`, and the message names every
/// cause a primary-key lookup cannot tell apart.
///
/// A well-formed id that names nothing and a string that is not an id
/// at all are different answers, and the pair is asserted together:
/// collapsing them is exactly how "you asked wrongly" and "it is not
/// here yet" become one unhelpful reply.
#[tokio::test]
async fn an_unrecorded_trigger_is_named_rather_than_missed() {
    let server = fq_test_support::NatsServer::start();
    let world = World::start(server.url()).await;

    // No agent definition exists in this daemon's scratch directory, so
    // the dispatcher will ack and drop this and no record will follow.
    let receipt = world
        .publish("never-loaded", json!({"task": "nobody runs me"}))
        .await;
    let key = receipt.atoms[0].key.clone();

    let answer = world
        .invoke(OpId::Get(Domain::Trigger), key.clone())
        .await
        .expect_err("an unrecorded trigger is not an ordinary answer");
    let fq_edge::wire::WireError::Unlocatable { op, message } = &answer else {
        panic!("a queued trigger is real; `not found` would be a lie. got {answer:?}");
    };
    assert_eq!(op, "trigger.get");
    for cause in ["queued", "forward-only", "names nothing"] {
        assert!(
            message.contains(cause),
            "the message must name the cause `{cause}` it cannot rule out; got {message}"
        );
    }

    // A string that is not an identity at all is a verdict on the
    // request, which is a different thing to say and is said
    // differently.
    let malformed = world
        .invoke(
            OpId::Get(Domain::Trigger),
            json!({"trigger_id": "not-a-uuid"}),
        )
        .await
        .expect_err("a non-identity cannot name a trigger");
    assert!(
        matches!(&malformed, fq_edge::wire::WireError::InvalidInput { op, message }
            if op == "trigger.get" && message.contains("not-a-uuid")),
        "expected an InvalidInput naming what was written; got {malformed:?}"
    );

    world.shutdown();
}

/// A trigger that dead-lettered is still gettable — the dead letter is
/// a record of the trigger, not only of the failure, and for a trigger
/// whose agent this daemon never held it is the only record there is.
///
/// It also proves the two record types land in one population: the
/// dead-lettered trigger lists beside a published one under the same
/// filter.
#[tokio::test]
async fn a_dead_lettered_trigger_is_still_gettable() {
    let server = fq_test_support::NatsServer::start();
    let world = World::start(server.url()).await;

    let id = uuid::Uuid::now_v7().to_string();
    world
        .bus
        .publish(&dead_letter(
            OTHER_AGENT,
            &id,
            json!({"task": "the one that died"}),
        ))
        .await
        .expect("publish the dead letter");

    let key = json!({"trigger_id": id});
    let got = world.await_trigger(&key).await;
    assert_eq!(got["id"], id.as_str());
    assert_eq!(
        got["payload"],
        json!({"task": "the one that died"}),
        "the payload the dead letter carried is the trigger's own"
    );
    assert_eq!(got["subject"], "fq.trigger.fixer");

    let listed = world
        .invoke(OpId::List(Domain::Trigger), json!({"agent": OTHER_AGENT}))
        .await
        .expect("trigger.list");
    assert!(
        listed
            .as_array()
            .expect("a list")
            .iter()
            .any(|row| row["trigger_id"] == id.as_str()),
        "a dead-lettered trigger is in its agent's listing like any other"
    );

    world.shutdown();
}

/// Each declared filter axis narrows, on List and on Stream alike, and
/// the two select the same population.
///
/// `since` is asserted with a bound in the future as well as the past,
/// because an axis that is accepted and then ignored looks exactly like
/// one that works when every row is inside the window.
#[tokio::test]
async fn every_declared_axis_narrows_and_stream_agrees_with_list() {
    let server = fq_test_support::NatsServer::start();
    let world = World::start(server.url()).await;

    // Tail-seek first, so the stream must deliver what follows and the
    // assertions below are about this test's own triggers.
    let seek = world
        .client
        .rpc
        .next_batch(
            tarpc::context::current(),
            fq_edge::NextBatchRequest {
                op: OpId::Stream(Domain::Trigger),
                version: 1,
                filter: json!({}),
                from_seq: u64::MAX,
                max_wait_ms: 0,
            },
        )
        .await
        .expect("rpc")
        .expect("tail seek");
    assert!(seek.items.is_empty());
    assert!(seek.next_from_seq < u64::MAX, "a concrete resume cursor");

    let mine = world.publish(AGENT, json!({"n": 1})).await.atoms[0]
        .key
        .clone();
    let theirs = world.publish(OTHER_AGENT, json!({"n": 2})).await.atoms[0]
        .key
        .clone();
    world.record(AGENT, &mine, json!({"n": 1})).await;
    world.record(OTHER_AGENT, &theirs, json!({"n": 2})).await;
    world.await_trigger(&mine).await;
    world.await_trigger(&theirs).await;

    let ids = |listed: &serde_json::Value| {
        listed
            .as_array()
            .expect("a list")
            .iter()
            .map(|row| row["trigger_id"].clone())
            .collect::<Vec<_>>()
    };

    // `agent` narrows.
    let narrowed = world
        .invoke(OpId::List(Domain::Trigger), json!({"agent": AGENT}))
        .await
        .expect("trigger.list --agent");
    assert!(ids(&narrowed).contains(&mine["trigger_id"]));
    assert!(
        !ids(&narrowed).contains(&theirs["trigger_id"]),
        "another agent's trigger is out of scope"
    );

    // `since` narrows, in both directions.
    let future = (chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339();
    let none = world
        .invoke(OpId::List(Domain::Trigger), json!({"since": future}))
        .await
        .expect("trigger.list --since");
    assert!(
        ids(&none).is_empty(),
        "a `since` in the future selects nothing — the axis is applied, not accepted and ignored"
    );
    let past = (chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339();
    let recent = world
        .invoke(OpId::List(Domain::Trigger), json!({"since": past}))
        .await
        .expect("trigger.list --since");
    assert!(ids(&recent).contains(&mine["trigger_id"]));

    // `limit` is the caller's own bound.
    let one = world
        .invoke(OpId::List(Domain::Trigger), json!({"limit": 1}))
        .await
        .expect("trigger.list --limit");
    assert_eq!(ids(&one).len(), 1);

    // An id that cannot name any trigger is a verdict on the request,
    // not an empty listing a caller would read as "nothing happened".
    let refused = world
        .invoke(OpId::List(Domain::Trigger), json!({"agent": "not a token"}))
        .await
        .expect_err("a malformed agent id must be refused");
    assert!(
        matches!(&refused, fq_edge::wire::WireError::InvalidInput { op, .. }
            if op == "trigger.list"),
        "got {refused:?}"
    );

    // Stream: the same triggers under the same narrowing, whole.
    let batch = world
        .client
        .rpc
        .next_batch(
            tarpc::context::current(),
            fq_edge::NextBatchRequest {
                op: OpId::Stream(Domain::Trigger),
                version: 1,
                filter: json!({"agent": AGENT}),
                from_seq: seek.next_from_seq,
                max_wait_ms: 10_000,
            },
        )
        .await
        .expect("rpc")
        .expect("stream batch");
    let streamed: Vec<serde_json::Value> =
        batch.items.iter().map(|i| i.item["id"].clone()).collect();
    assert!(
        streamed.contains(&mine["trigger_id"]),
        "the stream delivers this agent's trigger: {batch:?}"
    );
    assert!(
        !streamed.contains(&theirs["trigger_id"]),
        "…and applies the same narrowing List did"
    );
    assert_eq!(
        batch.items[0].item["payload"],
        json!({"n": 1}),
        "a stream item is the whole trigger, payload included — unlike a List row"
    );

    world.shutdown();
}

/// **An oversized payload is refused, not truncated.** A trigger is
/// kept indefinitely, so what is accepted is bounded; and a shortened
/// payload would be a different task that every record then described
/// as the original one. The refusal is a verdict on the request and
/// names both numbers, so the next attempt is an edit rather than a
/// guess.
#[tokio::test]
async fn an_oversized_payload_is_refused_by_the_edge() {
    let server = fq_test_support::NatsServer::start();
    let world = World::start(server.url()).await;

    let limit = fq_runtime::trigger::MAX_TRIGGER_PAYLOAD_BYTES;
    let refused = world
        .invoke(
            OpId::Verb(VerbId::Trigger(fq_ops::Trigger::Publish)),
            json!({"agent_id": AGENT, "payload": "x".repeat(limit)}),
        )
        .await
        .expect_err("a payload over the limit must be refused");
    assert!(
        matches!(&refused, fq_edge::wire::WireError::InvalidInput { op, message }
            if op == "trigger.publish"
                && message.contains(&limit.to_string())
                && message.contains("truncated")),
        "the refusal must name the limit and say it is a refusal; got {refused:?}"
    );

    // …and one at the limit is accepted, so the number is a size a
    // publisher may actually send.
    let at_limit = world
        .publish(AGENT, json!("x".repeat(limit - 2)))
        .await
        .atoms[0]
        .key
        .clone();
    assert!(at_limit["trigger_id"].as_str().is_some());

    world.shutdown();
}
