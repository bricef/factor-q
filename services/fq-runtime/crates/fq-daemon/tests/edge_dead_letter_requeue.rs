//! `dead_letter.requeue` end-to-end through the authenticated edge
//! (plan Phase 4, verb 8): the receipt walks, the second attempt is
//! refused by name, and a dead letter that names no trigger is refused
//! before anything is published.
//!
//! The goldens in `golden.rs` pin what an operator reads. This suite
//! covers what the goldens cannot: the receipt's shape as the *surface*
//! hands it back rather than as the CLI renders it, the guarantee that
//! makes this command different from the one it replaces, and the two
//! refusals — neither of which the happy-path golden can reach.
//!
//! The whole file is broker- and daemon-backed on purpose. Idempotency
//! here is a fact about a store the daemon owns and a stream it
//! publishes to; asserting it against anything less would be asserting
//! the code rather than the guarantee.

#![cfg(unix)]

use std::process::{Command, Stdio};
use std::time::Duration;

use fq_ops::{DeadLetter, Domain, OpId, Receipt, VerbId};
use fq_runtime::dead_letter::{
    DEAD_LETTER_PAYLOAD_KEY, DEAD_LETTER_SOURCE_KEY, DEAD_LETTER_STREAM_SEQ_KEY,
    DEAD_LETTER_SUBJECT_KEY, DEAD_LETTER_TRIGGER_ID_KEY,
};
use fq_runtime::events::{
    Event, EventPayload, FailedPayload, FailureKind, FailurePhase, InvocationTotals,
};
use fq_runtime::trigger::Trigger;
use serde_json::json;
use uuid::Uuid;

const AGENT: &str = "researcher";

const REQUEUE: OpId = OpId::Verb(VerbId::DeadLetter(DeadLetter::Requeue));

fn unique_scratch() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("edge-requeue-{}-{}", std::process::id(), nanos));
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

/// A dead letter as both emitters shape it. `trigger_id` is `Option`
/// because the advisory path records one only when it could read the
/// original off the trigger stream — it never invents one — and that
/// unnamed shape is a case this command has to answer for.
fn dead_letter(
    trigger_seq: u64,
    trigger_id: Option<Uuid>,
    payload: serde_json::Value,
) -> (Event, Uuid) {
    let event = Event::new(
        fq_runtime::AgentId::new(AGENT).unwrap(),
        Uuid::now_v7(),
        EventPayload::Failed(FailedPayload {
            error_kind: FailureKind::TriggerExhausted,
            error_message: "trigger exhausted after 5 deliveries (limit 5) [inline]".into(),
            phase: FailurePhase::Setup,
            partial_totals: InvocationTotals::default(),
        }),
    )
    .annotate(
        DEAD_LETTER_SUBJECT_KEY,
        json!(fq_runtime::events::subjects::trigger(AGENT)),
    )
    .annotate(DEAD_LETTER_PAYLOAD_KEY, payload)
    .annotate(DEAD_LETTER_STREAM_SEQ_KEY, json!(trigger_seq))
    .annotate(DEAD_LETTER_SOURCE_KEY, json!("inline"));
    let event = match trigger_id {
        Some(id) => event.annotate(DEAD_LETTER_TRIGGER_ID_KEY, json!(id.to_string())),
        None => event,
    };
    let event_id = event.envelope.event_id;
    (event, event_id)
}

/// A live `fqd`, an edge client pinned to its certificate, and a bus
/// connection to the same broker.
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
        let token = {
            let mut lines = text.lines();
            lines.find(|l| l.contains("edge: admin token")).unwrap();
            lines.next().unwrap().trim().to_string()
        };
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

    /// One op, with the daemon's verdict left intact — half this suite
    /// is about refusals, where the error *is* the answer under test.
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

    /// Publish a dead letter and wait for the projection to have folded
    /// it, which is what makes the trigger it names readable.
    ///
    /// The wait is on the *fact the test depends on* rather than on a
    /// sleep: the projection consumer is asynchronous, and a requeue
    /// issued before the fold has nothing to disagree with it — the
    /// command reads the log, not the fold, so it would simply pass for
    /// the wrong reason.
    async fn seed(&self, event: &Event, names: Option<Uuid>) {
        self.bus.publish(event).await.expect("publish dead letter");
        let Some(id) = names else { return };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while self
            .invoke(OpId::Get(Domain::Trigger), json!({ "trigger_id": id }))
            .await
            .is_err()
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the dead letter's trigger `{id}` never reached the projection"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The trigger `trigger.get` answers with for `id`.
    async fn trigger(&self, id: &str) -> Trigger {
        let raw = self
            .invoke(OpId::Get(Domain::Trigger), json!({ "trigger_id": id }))
            .await
            .unwrap_or_else(|e| panic!("trigger.get {id}: {e}"));
        serde_json::from_value(raw).expect("a Trigger")
    }

    fn shutdown(mut self) {
        let rc = unsafe { libc::kill(self.daemon.id() as i32, libc::SIGTERM) };
        assert_eq!(rc, 0);
        let status = self.daemon.wait().expect("wait");
        assert!(status.success());
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

/// The receipt names a trigger, the name is the key `trigger.get`
/// takes, and walking it lands on the trigger the requeue made —
/// carrying the original's payload and naming the original.
///
/// **The walk needs no watermark**, and that is asserted rather than
/// assumed: the receipt claims no position, and the Get that follows it
/// immediately still resolves, because the requeue writes the trigger's
/// permanent record before it publishes. A command that recorded the
/// requeue only through the dispatcher's later event would fail here,
/// intermittently, which is the failure mode worth pinning.
#[tokio::test]
async fn the_receipt_names_a_trigger_that_can_be_walked_to() {
    let server = fq_test_support::NatsServer::start();
    let world = World::start(server.url()).await;

    let original_id = Uuid::now_v7();
    let payload = json!({"task": "look at #12"});
    let (event, _) = dead_letter(11, Some(original_id), payload.clone());
    world.seed(&event, Some(original_id)).await;

    let receipt: Receipt = serde_json::from_value(
        world
            .invoke(REQUEUE, json!({ "agent_id": AGENT }))
            .await
            .expect("the requeue succeeds"),
    )
    .expect("a receipt");

    // Exactly one atom, in the Trigger domain — the domain of the thing
    // that was made, not of the verb's own filing.
    assert_eq!(receipt.atoms.len(), 1, "{receipt:?}");
    let named = &receipt.atoms[0];
    assert_eq!(named.domain, Domain::Trigger);
    // No watermark: publishing appends no event log position this
    // command could honestly claim, and the record is already there.
    assert!(receipt.watermarks.is_empty(), "{receipt:?}");

    // The key, handed to `trigger.get` UNCHANGED — no reshaping, no
    // unwrapping, which is the whole promise of an `AtomRef`.
    let raw = world
        .invoke(OpId::Get(Domain::Trigger), named.key.clone())
        .await
        .expect("the receipt's key is the key Get takes");
    let requeued: Trigger = serde_json::from_value(raw).expect("a Trigger");

    assert_eq!(requeued.requeued_from, Some(original_id), "{requeued:?}");
    assert_ne!(requeued.id, original_id, "a requeue is a new trigger");
    assert!(
        requeued.id > original_id,
        "and a later one, by UUIDv7 order"
    );
    assert_eq!(requeued.payload, payload, "the same work, verbatim");
    assert_eq!(
        requeued.subject.as_deref(),
        Some(fq_runtime::events::subjects::trigger(AGENT).as_str())
    );

    // The original is untouched — still gettable, still carrying no
    // lineage of its own. A requeue that had rewritten the record it
    // came from would have destroyed the evidence it depends on.
    let original = world.trigger(&original_id.to_string()).await;
    assert_eq!(original.id, original_id);
    assert_eq!(original.requeued_from, None);
    assert_eq!(original.payload, payload);

    // And it really was published: the trigger stream carries a message
    // under the new identity. Without this the whole test could pass on
    // a store write alone.
    let raw = world
        .bus
        .jetstream()
        .get_stream(fq_runtime::bus::TRIGGER_STREAM_NAME)
        .await
        .expect("trigger stream")
        .get_last_raw_message_by_subject(&fq_runtime::events::subjects::trigger(AGENT))
        .await
        .expect("the requeued trigger is on the stream");
    assert_eq!(
        fq_runtime::trigger::trigger_id_in(Some(&raw.headers)),
        Some(requeued.id),
        "the published message carries the identity the receipt named"
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&raw.payload).expect("json body"),
        payload
    );

    world.shutdown();
}

/// **Twice is refused, and the refusal names the first call's trigger.**
///
/// The whole point of the flip. It is a `Conflict` rather than an
/// `InvalidInput`, because the request was right and the work is done;
/// and it carries the id so the second caller — usually someone unsure
/// whether the first attempt landed — can go and look at what did land.
///
/// The trigger stream is checked too: a refusal that had already
/// published would have run the agent twice while reporting that it had
/// not, which is the exact harm the guarantee exists to prevent.
///
/// **Both calls name the dead letter explicitly**, and that is not
/// incidental. This daemon has no `researcher` in its registry, so the
/// trigger the first call publishes will itself exhaust its deliveries
/// and dead-letter — after which "the agent's most recent dead letter"
/// is a *different* dead letter, and a second unselected requeue would
/// legitimately succeed. The retry schedule puts that ~156s out and
/// this test runs in about one, so it is not a flake today; the
/// selector is pinned so that the property under test does not depend
/// on that arithmetic staying true.
/// (`the_selectors_name_what_they_could_not_find` covers the default.)
#[tokio::test]
async fn requeueing_the_same_dead_letter_twice_is_refused_by_name() {
    let server = fq_test_support::NatsServer::start();
    let world = World::start(server.url()).await;

    let original_id = Uuid::now_v7();
    let (event, _) = dead_letter(11, Some(original_id), json!({"n": 1}));
    world.seed(&event, Some(original_id)).await;

    let receipt: Receipt = serde_json::from_value(
        world
            .invoke(REQUEUE, json!({ "agent_id": AGENT, "trigger_seq": 11 }))
            .await
            .expect("the first requeue succeeds"),
    )
    .expect("a receipt");
    let first: fq_runtime::trigger::Trigger = serde_json::from_value(
        world
            .invoke(OpId::Get(Domain::Trigger), receipt.atoms[0].key.clone())
            .await
            .expect("get"),
    )
    .expect("a Trigger");

    let tip_before = world
        .bus
        .jetstream()
        .get_stream(fq_runtime::bus::TRIGGER_STREAM_NAME)
        .await
        .expect("trigger stream")
        .info()
        .await
        .expect("stream info")
        .state
        .last_sequence;

    let err = world
        .invoke(REQUEUE, json!({ "agent_id": AGENT, "trigger_seq": 11 }))
        .await
        .expect_err("a second requeue of the same dead letter must be refused");
    let fq_edge::wire::WireError::Conflict { op, message } = &err else {
        panic!("expected a Conflict — the request was right and the work is done; got {err:?}");
    };
    assert_eq!(op, "dead_letter.requeue");
    assert!(
        message.contains(&first.id.to_string()),
        "the refusal must name the trigger the first call made ({}); got {message}",
        first.id
    );
    assert!(
        message.contains(&original_id.to_string()),
        "…and the trigger it was asked about; got {message}"
    );
    assert!(
        message.contains("trigger.get"),
        "…and how to follow it; got {message}"
    );

    // A refused requeue is a no-op on both halves of the world. Nothing
    // reached the trigger stream — which is the harm, since a published
    // trigger runs the agent — and the record is exactly as the first
    // call left it, so the refusal did not consume, move or overwrite
    // the claim it reported.
    let tip_after = world
        .bus
        .jetstream()
        .get_stream(fq_runtime::bus::TRIGGER_STREAM_NAME)
        .await
        .expect("trigger stream")
        .info()
        .await
        .expect("stream info")
        .state
        .last_sequence;
    assert_eq!(
        tip_before, tip_after,
        "a refused requeue must publish nothing"
    );
    assert_eq!(world.trigger(&first.id.to_string()).await, first);
    assert_eq!(
        world.trigger(&original_id.to_string()).await.requeued_from,
        None,
        "and the trigger that was requeued still carries no lineage of its own"
    );

    world.shutdown();
}

/// A dead letter that names no trigger is refused with a reason of its
/// own, and nothing is published.
///
/// It is `Unlocatable` and not `InvalidInput`: the dead letter exists,
/// it lists, and the operator selected it correctly — what is missing
/// is the identity the guarantee is keyed on, and no edit to the
/// request can supply one. Requeueing it anyway would hand back the
/// guarantee's name without the guarantee.
#[tokio::test]
async fn a_dead_letter_that_names_no_trigger_is_refused() {
    let server = fq_test_support::NatsServer::start();
    let world = World::start(server.url()).await;

    let (event, event_id) = dead_letter(11, None, json!({"n": 1}));
    world.seed(&event, None).await;

    let err = world
        .invoke(REQUEUE, json!({ "agent_id": AGENT }))
        .await
        .expect_err("an unnamed dead letter cannot be requeued idempotently");
    let fq_edge::wire::WireError::Unlocatable { op, message } = &err else {
        panic!("expected an Unlocatable — the dead letter is here, its name is not; got {err:?}");
    };
    assert_eq!(op, "dead_letter.requeue");
    assert!(
        message.contains(&event_id.to_string()),
        "the refusal must name the dead letter it is about; got {message}"
    );
    for expected in ["names no trigger", "aged off", "trigger.publish"] {
        assert!(
            message.contains(expected),
            "the refusal must say `{expected}` — why, and what to do instead; got {message}"
        );
    }

    // Nothing published: the refusal happens before the reservation,
    // which happens before the publish.
    assert!(
        world
            .bus
            .jetstream()
            .get_stream(fq_runtime::bus::TRIGGER_STREAM_NAME)
            .await
            .expect("trigger stream")
            .get_last_raw_message_by_subject(&fq_runtime::events::subjects::trigger(AGENT))
            .await
            .is_err(),
        "a refused requeue must leave the trigger stream untouched"
    );

    world.shutdown();
}

/// The selectors, and the two ways they can name nothing.
///
/// Both are `NotFound` rather than an empty answer: a command has no
/// empty answer to give, and "there is no such dead letter" is a
/// perfectly normal outcome that an operator must be able to tell apart
/// from "I asked wrongly", which is what the invalid agent id gets.
#[tokio::test]
async fn the_selectors_name_what_they_could_not_find() {
    let server = fq_test_support::NatsServer::start();
    let world = World::start(server.url()).await;

    let older = Uuid::now_v7();
    let newer = Uuid::now_v7();
    let (event, _) = dead_letter(11, Some(older), json!({"n": 1}));
    world.seed(&event, Some(older)).await;
    let (event, _) = dead_letter(12, Some(newer), json!({"n": 2}));
    world.seed(&event, Some(newer)).await;

    // No selector takes the NEWEST, which is what the listing leads
    // with — a scan that kept the first match would take the other one.
    let receipt: Receipt = serde_json::from_value(
        world
            .invoke(REQUEUE, json!({ "agent_id": AGENT }))
            .await
            .expect("requeue"),
    )
    .expect("a receipt");
    let requeued: Trigger = serde_json::from_value(
        world
            .invoke(OpId::Get(Domain::Trigger), receipt.atoms[0].key.clone())
            .await
            .expect("get"),
    )
    .expect("a Trigger");
    assert_eq!(requeued.requeued_from, Some(newer));
    assert_eq!(requeued.payload, json!({"n": 2}));

    // …and the older one is still selectable by its sequence, which is
    // what makes the default a default rather than the only choice.
    let receipt: Receipt = serde_json::from_value(
        world
            .invoke(REQUEUE, json!({ "agent_id": AGENT, "trigger_seq": 11 }))
            .await
            .expect("requeue by sequence"),
    )
    .expect("a receipt");
    let requeued: Trigger = serde_json::from_value(
        world
            .invoke(OpId::Get(Domain::Trigger), receipt.atoms[0].key.clone())
            .await
            .expect("get"),
    )
    .expect("a Trigger");
    assert_eq!(requeued.requeued_from, Some(older));

    let err = world
        .invoke(REQUEUE, json!({ "agent_id": AGENT, "trigger_seq": 9999 }))
        .await
        .expect_err("a sequence no dead letter carries");
    assert!(
        matches!(&err, fq_edge::wire::WireError::NotFound { message, .. } if message.contains("9999")),
        "expected a NotFound naming the sequence; got {err:?}"
    );

    let err = world
        .invoke(REQUEUE, json!({ "agent_id": "fixer" }))
        .await
        .expect_err("an agent with no dead letters");
    assert!(
        matches!(&err, fq_edge::wire::WireError::NotFound { message, .. } if message.contains("fixer")),
        "expected a NotFound naming the agent; got {err:?}"
    );

    let err = world
        .invoke(REQUEUE, json!({ "agent_id": "not a token" }))
        .await
        .expect_err("an id the subject grammar cannot carry");
    assert!(
        matches!(&err, fq_edge::wire::WireError::InvalidInput { message, .. }
            if message.contains("not a token")),
        "expected an InvalidInput — this one IS a verdict on the request; got {err:?}"
    );

    world.shutdown();
}
