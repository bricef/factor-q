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
//!
//! **Requeue takes the verdict itself** (issue 1059), and the second half
//! of the file is that: before anything is reserved or published, the
//! daemon asks the records the dead-letter watch asks. A start on record
//! is refused whatever the request says; nothing on record proceeds,
//! marked `start_unchecked` or not; and when it cannot tell, the request
//! is refused unless it says the operator has checked
//! (`checked_not_started`). The records are the daemon's own claim
//! store, written here as a dispatcher would have left them, and the one
//! state only a live dispatcher can hold, a copy of the trigger still
//! handling its claim, is made with a real run held behind the worker cap.

#![cfg(unix)]

use std::process::Stdio;

use fq_test_support::TestChild;
use std::time::Duration;

use fq_ops::{DeadLetter, Domain, OpId, Receipt, VerbId};
use fq_runtime::bus::TRIGGER_STREAM_NAME;
use fq_runtime::control_plane::store::{ClaimState, TriggerClaimRow};
use fq_runtime::control_plane::{OwnerStatus, TriggerKey};
use fq_runtime::dead_letter::{
    DEAD_LETTER_PAYLOAD_KEY, DEAD_LETTER_SOURCE_KEY, DEAD_LETTER_START_UNCHECKED_KEY,
    DEAD_LETTER_STREAM_SEQ_KEY, DEAD_LETTER_SUBJECT_KEY, DEAD_LETTER_TRIGGER_ID_KEY,
};
use fq_runtime::events::{
    Event, EventPayload, FailedPayload, FailureKind, FailurePhase, InvocationTotals,
};
use fq_runtime::test_support::fault::MockFault;
use fq_runtime::test_support::mock_anthropic::MockAnthropicServer;
use fq_runtime::trigger::Trigger;
use fq_runtime::{ControlPlaneStore, ProjectionStore, RuntimeDbPaths};
use serde_json::json;
use uuid::Uuid;

const AGENT: &str = "researcher";

const REQUEUE: OpId = OpId::Verb(VerbId::DeadLetter(DeadLetter::Requeue));

/// The claimant on the claims this suite writes by hand: a daemon that
/// has exited, as one that claimed a trigger before a restart would be.
const GONE_WORKER: &str = "daemon-that-exited";

/// The agent the running-world test registers, and its model.
const RUNNER: &str = "held-runner";
const MODEL: &str = "claude-haiku-4-5";

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
    dead_letter_for(AGENT, trigger_seq, trigger_id, payload)
}

/// Mark a dead letter as the watch marks one it wrote without being able
/// to check whether the trigger's run started.
fn unchecked(event: Event, reason: &str, invocation_id: Option<&str>) -> Event {
    event.annotate(
        DEAD_LETTER_START_UNCHECKED_KEY,
        json!({ "reason": reason, "invocation_id": invocation_id }),
    )
}

/// [`dead_letter`], for `agent`.
fn dead_letter_for(
    agent: &str,
    trigger_seq: u64,
    trigger_id: Option<Uuid>,
    payload: serde_json::Value,
) -> (Event, Uuid) {
    let event = Event::new(
        fq_runtime::AgentId::new(agent).unwrap(),
        Uuid::now_v7(),
        EventPayload::Failed(FailedPayload {
            error_kind: FailureKind::TriggerExhausted,
            error_message: "trigger exhausted after 5 deliveries (limit 5) [inline]".into(),
            phase: FailurePhase::Setup,
            partial_totals: InvocationTotals::default(),
            detail: None,
        }),
    )
    .annotate(
        DEAD_LETTER_SUBJECT_KEY,
        json!(fq_runtime::events::subjects::trigger(agent)),
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
    daemon: TestChild,
    client: fq_edge::EdgeClient,
    bus: fq_runtime::EventBus,
}

impl World {
    async fn start(nats_url: &str) -> World {
        Self::start_in(unique_scratch(), nats_url).await
    }

    /// A daemon that runs [`RUNNER`] on the mock model at `model_url`.
    async fn start_running(nats_url: &str, model_url: &str) -> World {
        let scratch = unique_scratch();
        std::fs::write(
            scratch.join("agents").join(format!("{RUNNER}.md")),
            format!("---\nname: {RUNNER}\nmodel: {MODEL}\nbudget: 1.00\n---\n\nAnswer.\n"),
        )
        .unwrap();
        // The model is declared with its price, so the daemon's pricing
        // guarantee is met from config rather than from a network fetch.
        std::fs::write(
            scratch.join("fq.toml"),
            format!(
                "[edge]\nbind = \"127.0.0.1:0\"\n\n\
                 [providers.anthropic]\nmodels = [\"{MODEL}\"]\nbase_url = \"{model_url}\"\n\n\
                 [providers.anthropic.pricing.\"{MODEL}\"]\n\
                 input_per_mtok = 1.0\noutput_per_mtok = 5.0\n"
            ),
        )
        .unwrap();
        Self::start_in(scratch, nats_url).await
    }

    async fn start_in(scratch: std::path::PathBuf, nats_url: &str) -> World {
        let log_path = scratch.join("daemon.log");
        let log = std::fs::File::create(&log_path).expect("create daemon log");
        let log_err = log.try_clone().expect("clone log handle");
        let mut daemon = TestChild::builder(env!("CARGO_BIN_EXE_fqd"))
            .env("FQ_DAEMON_CONFIG", scratch.join("fq.toml"))
            .env("FQ_NATS_URL", nats_url)
            .env("FQ_CACHE_DIR", scratch.join("cache"))
            .env("FQ_STATE_DIR", scratch.join("state"))
            .env("FQ_AGENTS_DIR", scratch.join("agents"))
            .env("ANTHROPIC_API_KEY", "test-key-unused-by-mock")
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .spawn();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let text = loop {
            if let Some(status) = daemon.try_wait().expect("poll fqd") {
                panic!("fqd exited during startup with {status:?}");
            }
            let text = std::fs::read_to_string(&log_path).unwrap_or_default();
            if text.contains("- edge is listening on ") {
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
        self.daemon.signal(libc::SIGTERM).expect("kill(SIGTERM)");
        let status = self.daemon.wait().expect("wait");
        assert!(status.success());
        let _ = std::fs::remove_dir_all(&self.scratch);
    }

    /// Stop a daemon that is holding a run open: a drain would wait for
    /// the run's held model call, so it is killed, and nothing is
    /// asserted about how it exits.
    fn kill(mut self) {
        self.daemon.signal(libc::SIGKILL).expect("kill(SIGKILL)");
        let _ = self.daemon.wait();
        let _ = std::fs::remove_dir_all(&self.scratch);
    }

    /// Wait until the claim on the trigger at `stream_seq` satisfies
    /// `done`, and return it.
    async fn claim_until(
        &self,
        stream_seq: u64,
        what: &str,
        done: impl Fn(&TriggerClaimRow) -> bool,
    ) -> TriggerClaimRow {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(row) = self.claim_row(stream_seq).await
                && done(&row)
            {
                return row;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the claim on trigger sequence {stream_seq} never became {what}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The daemon's three stores.
    fn stores(&self) -> RuntimeDbPaths {
        RuntimeDbPaths::under(&self.scratch.join("cache"))
    }

    /// A second handle on the daemon's claim store, as a test writes to
    /// it: the dispatcher's records, made by hand.
    async fn control_plane(&self) -> ControlPlaneStore {
        ControlPlaneStore::open(&self.stores().control_plane)
            .await
            .expect("open the daemon's control plane")
    }

    /// The claim table's key for the trigger at `stream_seq` of the
    /// trigger stream as the daemon connected to it.
    fn claim_key(&self, stream_seq: u64) -> TriggerKey<'static> {
        TriggerKey {
            stream: TRIGGER_STREAM_NAME,
            stream_epoch: self.bus.trigger_stream_epoch(),
            stream_seq,
        }
    }

    /// The claim a dispatcher that has since exited left on the trigger
    /// at `stream_seq`, naming `invocation`: `claimed`, or marked durably
    /// started when `started`.
    async fn claim(&self, stream_seq: u64, invocation: &str, started: bool) {
        let cp = self.control_plane().await;
        let key = self.claim_key(stream_seq);
        cp.claim_trigger(
            key,
            GONE_WORKER,
            invocation,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .expect("claim");
        if started {
            cp.mark_trigger_started(key, invocation)
                .await
                .expect("mark started");
        }
    }

    /// The claim on the trigger at `stream_seq`, as the store holds it now.
    async fn claim_row(&self, stream_seq: u64) -> Option<TriggerClaimRow> {
        self.control_plane()
            .await
            .trigger_claim(self.claim_key(stream_seq))
            .await
            .expect("read the claim")
    }

    /// The trigger stream's last sequence: a refused requeue must leave
    /// it where it was.
    async fn trigger_tip(&self) -> u64 {
        self.bus
            .jetstream()
            .get_stream(TRIGGER_STREAM_NAME)
            .await
            .expect("trigger stream")
            .info()
            .await
            .expect("stream info")
            .state
            .last_sequence
    }

    /// The trigger a requeue of `original` reserved, if one did. A refused
    /// requeue must leave none.
    async fn requeue_of(&self, original: Uuid) -> Option<Uuid> {
        ProjectionStore::open_read_only(&self.stores().projection)
            .await
            .expect("open the projection")
            .requeue_of(&original.to_string())
            .await
            .expect("read the requeue reservation")
    }

    /// Ask for a requeue that must be refused, and check it left nothing
    /// behind: no trigger published, no reservation of `original`, and,
    /// when it could not tell, the claim at the requested sequence as it
    /// was. (A start found in the records may mark the claim; that refusal
    /// says so, and the tests that reach it check the mark.)
    async fn refused(&self, input: serde_json::Value, original: Uuid) -> fq_edge::wire::WireError {
        let tip = self.trigger_tip().await;
        let claim = |input: &serde_json::Value| {
            let seq = input["trigger_seq"].as_u64();
            async move {
                match seq {
                    Some(seq) => format!(
                        "{:?}",
                        self.control_plane()
                            .await
                            .trigger_claim(self.claim_key(seq))
                            .await
                    ),
                    None => String::new(),
                }
            }
        };
        let claim_before = claim(&input).await;
        let err = self
            .invoke(REQUEUE, input.clone())
            .await
            .expect_err(&format!("{input} must be refused"));
        assert_eq!(
            self.trigger_tip().await,
            tip,
            "a refused requeue must publish nothing; it answered {err:?}"
        );
        assert_eq!(
            self.requeue_of(original).await,
            None,
            "a refused requeue must reserve nothing; it answered {err:?}"
        );
        if matches!(err, fq_edge::wire::WireError::InvalidInput { .. }) {
            assert_eq!(
                claim(&input).await,
                claim_before,
                "a requeue refused for want of a verdict must leave the claim as it was"
            );
        }
        err
    }

    /// A row in the daemon's worker WAL for `invocation`, in flight: the
    /// record a run's first WAL write leaves.
    async fn wal_row(&self, invocation: &str, agent: &str) {
        let now = chrono::Utc::now().timestamp_millis();
        fq_runtime::WorkerStore::open(&self.stores().worker)
            .await
            .expect("open the daemon's worker store")
            .upsert_invocation_state(&fq_runtime::worker::InvocationStateRow {
                invocation_id: invocation.to_string(),
                agent_id: agent.to_string(),
                schema_version: 1,
                phase: "running".to_string(),
                state_blob: b"{}".to_vec(),
                step_index: 0,
                started_at: now,
                updated_at: now,
                terminal_at: None,
                workspace_ref: None,
                archive_status: None,
                archive_published_at: None,
                trigger_source: Some("subject".to_string()),
                trigger_subject: None,
                trigger_payload: None,
                terminal_outcome: None,
                terminal_published_at: None,
            })
            .await
            .expect("write the WAL row");
    }

    /// An ownership row for `invocation`, completed: the record an
    /// archived run leaves, which nothing sweeps.
    async fn owner_row(&self, invocation: &str) {
        self.control_plane()
            .await
            .upsert_invocation_ownership(
                invocation,
                GONE_WORKER,
                chrono::Utc::now().timestamp_millis(),
                OwnerStatus::Completed,
            )
            .await
            .expect("the run's ownership row");
    }

    /// Ask for a requeue that must go through, and return the trigger it
    /// made.
    async fn requeued(&self, input: serde_json::Value) -> Trigger {
        let receipt: Receipt = serde_json::from_value(
            self.invoke(REQUEUE, input.clone())
                .await
                .unwrap_or_else(|e| panic!("{input} must be requeued; got {e:?}")),
        )
        .expect("a receipt");
        self.trigger(
            receipt.atoms[0].key["trigger_id"]
                .as_str()
                .expect("the receipt names a trigger"),
        )
        .await
    }
}

/// A refusal because the trigger's run has a start on record: a
/// `Conflict` naming the run, the dead letter and its sequence.
fn assert_started(err: &fq_edge::wire::WireError, invocation: &str, event: Uuid, seq: u64) {
    let fq_edge::wire::WireError::Conflict { op, message } = err else {
        panic!("a start on record is a Conflict: the work ran; got {err:?}");
    };
    assert_eq!(op, "dead_letter.requeue");
    for expected in [
        invocation.to_string(),
        format!("fq invocation show {invocation}"),
        event.to_string(),
        format!("trigger sequence {seq}"),
        "start on record".to_string(),
    ] {
        assert!(
            message.contains(&expected),
            "the refusal must say `{expected}`; got {message}"
        );
    }
}

/// A refusal because the daemon cannot tell whether the run started: an
/// `InvalidInput` (the request can say the operator checked), naming the
/// dead letter, its sequence, why, and the override.
fn assert_cannot_tell(err: &fq_edge::wire::WireError, why: &str, event: Uuid, seq: u64) {
    let fq_edge::wire::WireError::InvalidInput { op, message } = err else {
        panic!("cannot tell is an InvalidInput: the override is the edit; got {err:?}");
    };
    assert_eq!(op, "dead_letter.requeue");
    for expected in [
        why.to_string(),
        event.to_string(),
        format!("trigger sequence {seq}"),
        "--checked-not-started".to_string(),
        "checked_not_started".to_string(),
    ] {
        assert!(
            message.contains(&expected),
            "the refusal must say `{expected}`; got {message}"
        );
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
        raw.headers
            .get(async_nats::header::NATS_MESSAGE_ID)
            .map(|value| value.as_str()),
        raw.headers
            .get(fq_ops::trigger::TRIGGER_ID_HEADER)
            .map(|value| value.as_str()),
        "broker dedup and trigger identity use the same id"
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

// ------------------------------------------------------------------
// Requeue takes the verdict itself (issue 1059).
// ------------------------------------------------------------------

/// **A start on record is refused, whatever the request says.** The dead
/// letter is one the watch marked unchecked, and its trigger's claim is
/// marked durably started: the run started after the watch gave up
/// waiting. Requeueing it would run the work twice, so the refusal is a
/// `Conflict` naming the run, and `checked_not_started` does not get past
/// it: an operator's word is not evidence against the record.
///
/// Then the retention sweep deletes the started claim, as it does once a
/// claim is older than `[state] trigger_claim_retention_days`. The marked
/// dead letter now has no claim to consult, which is "cannot tell": an
/// absent row is a claim given back before any start or one swept after
/// a start, and the two look the same. Refused without the override,
/// through with it, and the requeue that goes through is also the proof
/// that none of the refusals before it left a reservation behind.
#[tokio::test]
async fn an_unchecked_dead_letter_whose_run_started_is_refused_whatever_the_flag() {
    let server = fq_test_support::NatsServer::start();
    let world = World::start(server.url()).await;

    let original = Uuid::now_v7();
    let invocation = Uuid::now_v7().to_string();
    let (event, event_id) = dead_letter(21, Some(original), json!({"n": 21}));
    let event = unchecked(
        event,
        "a copy of the trigger is still handling its claim",
        Some(&invocation),
    );
    world.seed(&event, Some(original)).await;
    world.claim(21, &invocation, true).await;

    for input in [
        json!({ "agent_id": AGENT, "trigger_seq": 21 }),
        json!({ "agent_id": AGENT, "trigger_seq": 21, "checked_not_started": true }),
    ] {
        let err = world.refused(input, original).await;
        assert_started(&err, &invocation, event_id, 21);
    }

    let swept = world
        .control_plane()
        .await
        .sweep_started_trigger_claims(i64::MAX)
        .await
        .expect("sweep");
    assert_eq!(swept, 1, "the started claim is the one swept");
    let err = world
        .refused(json!({ "agent_id": AGENT, "trigger_seq": 21 }), original)
        .await;
    assert_cannot_tell(&err, "no claim names its delivery", event_id, 21);
    assert!(
        err.to_string()
            .contains(&format!("fq invocation show {invocation}")),
        "the refusal names the run the mark names, to check; got {err}"
    );
    let requeued = world
        .requeued(json!({ "agent_id": AGENT, "trigger_seq": 21, "checked_not_started": true }))
        .await;
    assert_eq!(requeued.requeued_from, Some(original));

    world.shutdown();
}

/// **An unmarked dead letter is guarded too.** It was written with
/// nothing of the run on record, and the run started after it was
/// written, by a copy its writer did not see (a replay of the stream,
/// say). The claim says so now, and the claim is what requeue asks: it is
/// refused, with or without the override, naming the run. A check of the
/// `start_unchecked` mark alone would have let this one through.
#[tokio::test]
async fn an_unmarked_dead_letter_whose_run_started_after_it_was_written_is_refused() {
    let server = fq_test_support::NatsServer::start();
    let world = World::start(server.url()).await;

    let original = Uuid::now_v7();
    let (event, event_id) = dead_letter(31, Some(original), json!({"n": 31}));
    world.seed(&event, Some(original)).await;
    let invocation = Uuid::now_v7().to_string();
    world.claim(31, &invocation, true).await;

    for input in [
        json!({ "agent_id": AGENT, "trigger_seq": 31 }),
        json!({ "agent_id": AGENT, "trigger_seq": 31, "checked_not_started": true }),
    ] {
        let err = world.refused(input, original).await;
        assert_started(&err, &invocation, event_id, 31);
    }

    world.shutdown();
}

/// A start found in the run's records, rather than on its claim, is
/// refused the same way: the claim still reads `claimed`, and the control
/// plane's ownership table has the run. The claim is then marked started,
/// as the check marks it for the dead-letter watch and as startup recovery
/// would, so a redelivered or replayed copy of the trigger meets a marked
/// claim and is dropped. That write is the one thing a refusal changes,
/// and the refusal says it made it.
#[tokio::test]
async fn a_start_found_in_the_records_is_refused_and_marks_the_claim() {
    let server = fq_test_support::NatsServer::start();
    let world = World::start(server.url()).await;

    let original = Uuid::now_v7();
    let (event, event_id) = dead_letter(32, Some(original), json!({"n": 32}));
    world.seed(&event, Some(original)).await;
    let invocation = Uuid::now_v7().to_string();
    world.claim(32, &invocation, false).await;
    world
        .control_plane()
        .await
        .upsert_invocation_ownership(
            &invocation,
            GONE_WORKER,
            chrono::Utc::now().timestamp_millis(),
            OwnerStatus::Completed,
        )
        .await
        .expect("the run's ownership row");

    let err = world
        .refused(json!({ "agent_id": AGENT, "trigger_seq": 32 }), original)
        .await;
    assert_started(&err, &invocation, event_id, 32);
    assert!(
        err.to_string().contains("marked started now"),
        "the refusal says it marked the claim; got {err}"
    );
    assert!(
        world
            .claim_row(32)
            .await
            .expect("the claim is still there")
            .state
            == ClaimState::DurablyStarted,
        "the claim is marked started"
    );

    world.shutdown();
}

/// **Nothing on record proceeds, marked or not.** The watch marked this
/// dead letter because a copy of the trigger was still handling its claim
/// when it gave up; that copy has since let go without starting the run.
/// The claim still reads `claimed`, no copy is handling it, and neither
/// the worker's WAL nor the ownership table has the run. The daemon makes
/// the check the guide used to ask an operator to make by hand, finds
/// nothing, and requeues with no flag.
#[tokio::test]
async fn an_unchecked_dead_letter_with_nothing_on_record_is_requeued_without_the_flag() {
    let server = fq_test_support::NatsServer::start();
    let world = World::start(server.url()).await;

    let original = Uuid::now_v7();
    let invocation = Uuid::now_v7().to_string();
    let (event, _) = dead_letter(41, Some(original), json!({"n": 41}));
    let event = unchecked(
        event,
        "a copy of the trigger is still handling its claim",
        Some(&invocation),
    );
    world.seed(&event, Some(original)).await;
    world.claim(41, &invocation, false).await;

    let requeued = world
        .requeued(json!({ "agent_id": AGENT, "trigger_seq": 41 }))
        .await;
    assert_eq!(requeued.requeued_from, Some(original));
    assert_eq!(requeued.payload, json!({"n": 41}));

    world.shutdown();
}

/// **No sequence given.** Requeue then takes the agent's newest dead
/// letter, and a refusal of it says which one it took: its sequence, its
/// event, and that it was the newest, so an operator who did not choose
/// can see what was chosen. The newest is marked unchecked and no claim
/// names its delivery: cannot tell. The older one, unmarked, is not the
/// one refused, and the message does not name it.
#[tokio::test]
async fn with_no_sequence_a_refusal_names_the_dead_letter_it_took() {
    let server = fq_test_support::NatsServer::start();
    let world = World::start(server.url()).await;

    let older = Uuid::now_v7();
    let (event, older_event) = dead_letter(51, Some(older), json!({"n": 51}));
    world.seed(&event, Some(older)).await;
    let newer = Uuid::now_v7();
    let invocation = Uuid::now_v7().to_string();
    let (event, newer_event) = dead_letter(52, Some(newer), json!({"n": 52}));
    let event = unchecked(
        event,
        "the trigger's claim could not be read: database is locked",
        Some(&invocation),
    );
    world.seed(&event, Some(newer)).await;

    let err = world.refused(json!({ "agent_id": AGENT }), newer).await;
    assert_cannot_tell(&err, "no claim names its delivery", newer_event, 52);
    assert!(
        err.to_string().contains("newest dead letter"),
        "the refusal says it took the newest; got {err}"
    );
    assert!(
        !err.to_string().contains(&older_event.to_string()),
        "the refusal is about the newest alone; got {err}"
    );

    // **The override names its dead letter.** It is the operator's word
    // about one dead letter's run, and with no sequence the request takes
    // whichever is the newest when it arrives, which need not be the one
    // they checked: refused, before anything is selected.
    let err = world
        .refused(
            json!({ "agent_id": AGENT, "checked_not_started": true }),
            newer,
        )
        .await;
    let fq_edge::wire::WireError::InvalidInput { op, message } = &err else {
        panic!("the override without a sequence is an InvalidInput; got {err:?}");
    };
    assert_eq!(op, "dead_letter.requeue");
    for expected in ["checked_not_started", "trigger_seq", "--trigger-seq"] {
        assert!(
            message.contains(expected),
            "the refusal must say `{expected}`; got {message}"
        );
    }

    let requeued = world
        .requeued(json!({ "agent_id": AGENT, "trigger_seq": 52, "checked_not_started": true }))
        .await;
    assert_eq!(requeued.requeued_from, Some(newer));

    world.shutdown();
}

/// **A start under the run the mark names is a start on record.** The
/// watch wrote this dead letter naming the run X its claim named; the copy
/// it was waiting for then started X, which finished and was archived, so
/// the ownership table holds X, and that row is never swept. When requeue
/// has no claim of this stream to consult, it does not stop at "cannot
/// tell": it asks the records by X's own id, and a start found there is
/// refused, with or without the override. Two ways to have no claim: the
/// claim was swept (sequence 81), and the dead letter predates the trigger
/// stream the daemon now consumes, after the stream was recreated and the
/// daemon restarted (sequence 82).
#[tokio::test]
async fn a_start_under_the_run_the_mark_names_is_refused_when_no_claim_can_be_consulted() {
    let server = fq_test_support::NatsServer::start();
    let world = World::start(server.url()).await;
    let before_this_stream = chrono::DateTime::from_timestamp_nanos(
        world.bus.trigger_stream_epoch() - 3_600 * 1_000_000_000,
    );

    for (seq, earlier_stream) in [(81, false), (82, true)] {
        let original = Uuid::now_v7();
        let run = Uuid::now_v7().to_string();
        let (event, event_id) = dead_letter(seq, Some(original), json!({ "n": seq }));
        let mut event = unchecked(
            event,
            "a copy of the trigger is still handling its claim",
            Some(&run),
        );
        if earlier_stream {
            event.envelope.timestamp = before_this_stream;
        }
        world.seed(&event, Some(original)).await;
        world.owner_row(&run).await;

        for input in [
            json!({ "agent_id": AGENT, "trigger_seq": seq }),
            json!({ "agent_id": AGENT, "trigger_seq": seq, "checked_not_started": true }),
        ] {
            let err = world.refused(input, original).await;
            assert_started(&err, &run, event_id, seq);
            assert!(
                err.to_string()
                    .contains("on the control plane's record as completed"),
                "the refusal says where the start is; got {err}"
            );
        }
    }

    world.shutdown();
}

/// **The claims cannot be read, and the WAL holds the run the mark
/// names.** The claim that would name the run cannot be read, so the
/// check cannot decide; the dead letter's mark names X, and the worker's
/// WAL holds X in flight. A start found by X's own id is a start on
/// record: refused, with or without the override.
#[tokio::test]
async fn when_the_claims_cannot_be_read_a_start_under_the_run_the_mark_names_is_refused() {
    let server = fq_test_support::NatsServer::start();
    let world = World::start(server.url()).await;

    let original = Uuid::now_v7();
    let run = Uuid::now_v7().to_string();
    let (event, event_id) = dead_letter(91, Some(original), json!({"n": 91}));
    let event = unchecked(
        event,
        "a copy of the trigger is still handling its claim",
        Some(&run),
    );
    world.seed(&event, Some(original)).await;
    world.wal_row(&run, AGENT).await;

    let store = sqlx::SqlitePool::connect(&format!(
        "sqlite://{}",
        world.stores().control_plane.display()
    ))
    .await
    .expect("open the claim store");
    sqlx::query("ALTER TABLE trigger_claim RENAME TO trigger_claim_out_of_reach")
        .execute(&store)
        .await
        .expect("hide the claim table");

    for input in [
        json!({ "agent_id": AGENT, "trigger_seq": 91 }),
        json!({ "agent_id": AGENT, "trigger_seq": 91, "checked_not_started": true }),
    ] {
        let err = world.refused(input, original).await;
        assert_started(&err, &run, event_id, 91);
        assert!(
            err.to_string().contains("in flight in the worker's WAL"),
            "the refusal says where the start is; got {err}"
        );
    }

    sqlx::query("ALTER TABLE trigger_claim_out_of_reach RENAME TO trigger_claim")
        .execute(&store)
        .await
        .expect("restore the claim table");
    world.shutdown();
}

/// **Which records, by what key.** A claim is keyed on the trigger
/// stream's incarnation as well as the sequence, and a dead letter carries
/// only the sequence. One written before the stream was last created names
/// a sequence of the earlier stream, and this stream's claim at the same
/// sequence is another trigger's: here, one whose run started. It is not
/// consulted. A marked dead letter from the earlier stream is then "cannot
/// tell", its own claim not there to consult, and not refused on the other
/// trigger's start; an unmarked one proceeds, as one whose claim is gone
/// does.
#[tokio::test]
async fn a_dead_letter_from_an_earlier_stream_is_not_judged_by_this_streams_claims() {
    let server = fq_test_support::NatsServer::start();
    let world = World::start(server.url()).await;

    let another_run = Uuid::now_v7().to_string();
    world.claim(61, &another_run, true).await;
    let before_this_stream = chrono::DateTime::from_timestamp_nanos(
        world.bus.trigger_stream_epoch() - 3_600 * 1_000_000_000,
    );

    let marked = Uuid::now_v7();
    let (event, marked_event) = dead_letter(61, Some(marked), json!({"n": 61}));
    let mut event = unchecked(
        event,
        "a copy of the trigger is still handling its claim",
        None,
    );
    event.envelope.timestamp = before_this_stream;
    world.seed(&event, Some(marked)).await;

    let err = world
        .refused(json!({ "agent_id": AGENT, "trigger_seq": 61 }), marked)
        .await;
    assert_cannot_tell(
        &err,
        "before the trigger stream was last created",
        marked_event,
        61,
    );
    assert!(
        !err.to_string().contains(&another_run),
        "this stream's claim at the sequence is another trigger's and says nothing; got {err}"
    );
    let requeued = world
        .requeued(json!({ "agent_id": AGENT, "trigger_seq": 61, "checked_not_started": true }))
        .await;
    assert_eq!(requeued.requeued_from, Some(marked));

    let unmarked = Uuid::now_v7();
    let (mut event, _) = dead_letter(61, Some(unmarked), json!({"n": 62}));
    event.envelope.timestamp = before_this_stream;
    world.seed(&event, Some(unmarked)).await;
    let requeued = world
        .requeued(json!({ "agent_id": AGENT, "trigger_seq": 61 }))
        .await;
    assert_eq!(requeued.requeued_from, Some(unmarked));

    world.shutdown();
}

/// **The records cannot be read: cannot tell.** The claim store answers a
/// read with an error, here because its table was renamed out from under
/// the daemon. The request is refused naming no run, since the claim that
/// names one could not be read, and says where to find the run instead;
/// the override lets it through.
#[tokio::test]
async fn when_the_claims_cannot_be_read_a_requeue_needs_the_flag() {
    let server = fq_test_support::NatsServer::start();
    let world = World::start(server.url()).await;

    let original = Uuid::now_v7();
    let (event, event_id) = dead_letter(71, Some(original), json!({"n": 71}));
    world.seed(&event, Some(original)).await;

    let store = sqlx::SqlitePool::connect(&format!(
        "sqlite://{}",
        world.stores().control_plane.display()
    ))
    .await
    .expect("open the claim store");
    sqlx::query("ALTER TABLE trigger_claim RENAME TO trigger_claim_out_of_reach")
        .execute(&store)
        .await
        .expect("hide the claim table");

    let err = world
        .refused(json!({ "agent_id": AGENT, "trigger_seq": 71 }), original)
        .await;
    assert_cannot_tell(&err, "the trigger's claim could not be read", event_id, 71);
    assert!(
        err.to_string()
            .contains(&format!("fq events query --agent {AGENT} --type triggered")),
        "with no run named, the refusal says how to find it; got {err}"
    );
    let requeued = world
        .requeued(json!({ "agent_id": AGENT, "trigger_seq": 71, "checked_not_started": true }))
        .await;
    assert_eq!(requeued.requeued_from, Some(original));

    sqlx::query("ALTER TABLE trigger_claim_out_of_reach RENAME TO trigger_claim")
        .execute(&store)
        .await
        .expect("restore the claim table");
    world.shutdown();
}

/// **A copy of the trigger still handling its claim: cannot tell.** A real
/// run holds the daemon's one worker, its model call held open, and a
/// second trigger for the same agent is claimed and waiting for the
/// worker: a copy is handling its claim and may yet start the run. A dead
/// letter for the second trigger is refused without the override and goes
/// through with it. A dead letter for the first, whose run started and
/// whose claim the dispatcher marked, is refused with or without it: the
/// start on record a real run leaves, not one written by hand.
#[tokio::test]
async fn while_a_copy_still_handles_the_claim_a_requeue_needs_the_flag() {
    let server = fq_test_support::NatsServer::start();
    let model = MockAnthropicServer::start().await;
    model.push_fault(MockFault::stall(Duration::from_secs(600)));
    let world = World::start_running(server.url(), model.base_url()).await;
    let agent = fq_runtime::AgentId::new(RUNNER).unwrap();

    let first = world
        .bus
        .publish_trigger(&agent, &json!({"n": 1}))
        .await
        .expect("publish the first trigger");
    let started = world
        .claim_until(first.stream_seq, "durably started", |row| {
            row.state == ClaimState::DurablyStarted
        })
        .await;
    let first_run = started
        .invocation_id
        .expect("a started claim names its run");
    let second = world
        .bus
        .publish_trigger(&agent, &json!({"n": 2}))
        .await
        .expect("publish the second trigger");
    world
        .claim_until(second.stream_seq, "claimed", |row| {
            row.state == ClaimState::Claimed
        })
        .await;

    let (event, event_id) =
        dead_letter_for(RUNNER, second.stream_seq, Some(second.id), json!({"n": 2}));
    world.seed(&event, Some(second.id)).await;
    let input = json!({ "agent_id": RUNNER, "trigger_seq": second.stream_seq });
    let err = world.refused(input, second.id).await;
    assert_cannot_tell(
        &err,
        "a copy of the trigger is still handling its claim",
        event_id,
        second.stream_seq,
    );
    let requeued = world
        .requeued(json!({
            "agent_id": RUNNER,
            "trigger_seq": second.stream_seq,
            "checked_not_started": true,
        }))
        .await;
    assert_eq!(requeued.requeued_from, Some(second.id));

    let (event, event_id) =
        dead_letter_for(RUNNER, first.stream_seq, Some(first.id), json!({"n": 1}));
    let event = unchecked(
        event,
        "a copy of the trigger is still handling its claim",
        Some(&first_run),
    );
    world.seed(&event, Some(first.id)).await;
    for input in [
        json!({ "agent_id": RUNNER, "trigger_seq": first.stream_seq }),
        json!({
            "agent_id": RUNNER,
            "trigger_seq": first.stream_seq,
            "checked_not_started": true,
        }),
    ] {
        let err = world.refused(input, first.id).await;
        assert_started(&err, &first_run, event_id, first.stream_seq);
    }

    // **Requeue takes the last look.** A third trigger is claimed and
    // waiting for the worker, a copy still handling its claim, and the
    // worker's WAL already holds the run its claim names. The check's
    // verdict alone would say "cannot tell" (a copy is handling the claim)
    // and the override would pass it; the last look reads the WAL for that
    // run and finds the start. Refused with and without the override, and
    // the claim is marked started, as the watch's last look marks it.
    let third = world
        .bus
        .publish_trigger(&agent, &json!({"n": 3}))
        .await
        .expect("publish the third trigger");
    let held = world
        .claim_until(third.stream_seq, "claimed", |row| {
            row.state == ClaimState::Claimed
        })
        .await;
    let third_run = held.invocation_id.expect("a claim names its run");
    world.wal_row(&third_run, RUNNER).await;
    let (event, event_id) =
        dead_letter_for(RUNNER, third.stream_seq, Some(third.id), json!({"n": 3}));
    world.seed(&event, Some(third.id)).await;
    for input in [
        json!({ "agent_id": RUNNER, "trigger_seq": third.stream_seq }),
        json!({
            "agent_id": RUNNER,
            "trigger_seq": third.stream_seq,
            "checked_not_started": true,
        }),
    ] {
        let err = world.refused(input, third.id).await;
        assert_started(&err, &third_run, event_id, third.stream_seq);
        assert!(
            world
                .claim_row(third.stream_seq)
                .await
                .expect("the claim is still there")
                .state
                == ClaimState::DurablyStarted,
            "the last look's start marks the claim; got {err}"
        );
    }

    world.kill();
}
