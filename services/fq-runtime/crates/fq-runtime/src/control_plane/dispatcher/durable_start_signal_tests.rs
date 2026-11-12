//! `handle` waits on two things at once: the durable-start signal and
//! the invocation itself. The signal is a oneshot receiver, and polling
//! one again after it has resolved panics with "called after complete",
//! so once the signal has resolved, whatever it resolved to, `handle`
//! must stop polling it (#987).
//!
//! Each test here resolves the signal and then keeps the invocation
//! running for one more turn of `handle`'s loop, by yielding once before
//! it returns. That turn is the one on which the spent receiver used to
//! be polled again. The tests run the real `handle` against a real
//! broker and a real control-plane store, with a stub worker.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_nats::jetstream::Message;
use async_nats::jetstream::consumer::{PullConsumer, pull};
use futures::StreamExt;
use serde_json::json;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection};
use uuid::Uuid;

use super::TriggerDispatcher;
use super::tests::{
    ack_pending_stays, canned_response, recovered_claims, registry_with, test_key, unique_agent_id,
    unique_consumer_name,
};
use crate::agent::{Agent, AgentId};
use crate::bus::{EventBus, NATS_DEFAULT_MAX_ACK_PENDING, TRIGGER_RETRY_BACKOFF};
use crate::control_plane::{ControlPlaneStore, TriggerClaim};
use crate::llm::LlmClient;
use crate::llm::fixture::FixtureClient;
use crate::test_support::nats::{NatsServer, test_nats};
use crate::trigger::Trigger;
use crate::worker::{
    DrainRequest, DrainState, DurableStart, ExecutorError, InvocationOutcome, Worker,
};

/// What the stub invocation does with its durable-start signal.
enum Resolve {
    /// Drop it unfired. `run_loop_for` does this when `run_loop_inner`
    /// returns before the first WAL write, and then still awaits the MCP
    /// shutdown and the workspace reclaim before its future finishes.
    Drop,
    /// Make the claim store refuse the mark, then fire it: the first WAL
    /// write has landed and the claim's update fails.
    FireIntoRefusedMark,
}

/// What the stub invocation returns once it has yielded.
enum Returns {
    /// A transient error, which `handle` NAKs when no durable start was
    /// observed.
    TransientError,
    /// A completed run.
    Completed,
}

/// An invocation that resolves its durable-start signal, yields once,
/// and only then returns: `handle` sees the signal resolve on one turn
/// of its loop and the invocation finish on a later one.
struct ResolvesThenYields {
    resolve: Resolve,
    returns: Returns,
    /// The claim store's file, for [`Resolve::FireIntoRefusedMark`].
    claim_db: PathBuf,
    ran_to_end: AtomicBool,
}

#[async_trait::async_trait]
impl Worker for ResolvesThenYields {
    async fn run_invocation(
        &self,
        _agent: &Agent,
        _llm: &dyn LlmClient,
        _trigger: Trigger,
        _delivery_attempt: Option<u32>,
        mut durable_start: DurableStart,
    ) -> Result<InvocationOutcome, ExecutorError> {
        match self.resolve {
            Resolve::Drop => drop(durable_start),
            Resolve::FireIntoRefusedMark => {
                refuse_claim_marks(&self.claim_db).await;
                durable_start.fire();
            }
        }
        tokio::task::yield_now().await;
        self.ran_to_end.store(true, Ordering::SeqCst);
        match self.returns {
            Returns::TransientError => Err(ExecutorError::WorkerStore(
                "simulated transient store outage".to_string(),
            )),
            Returns::Completed => Ok(InvocationOutcome::Completed {
                invocation_id: Uuid::now_v7(),
                response: canned_response(),
                cost: 0.0,
                duration_ms: 0,
            }),
        }
    }

    async fn request_drain(&self, _req: DrainRequest) {}

    fn drain_status(&self) -> DrainState {
        DrainState::Running
    }
}

/// Make every update of a claim row fail from now on, through a
/// connection of its own to the store's file. The claim this delivery
/// already holds was an insert and a release is a delete, so the mark is
/// the one write on this path the refusal meets.
async fn refuse_claim_marks(claim_db: &Path) {
    let mut conn = SqliteConnectOptions::new()
        .filename(claim_db)
        .connect()
        .await
        .expect("open the claim store's file");
    sqlx::query(
        "CREATE TRIGGER refuse_claim_marks BEFORE UPDATE ON trigger_claim \
         BEGIN SELECT RAISE(ABORT, 'injected: the claim store refuses the mark'); END",
    )
    .execute(&mut conn)
    .await
    .expect("install the refusal");
    conn.close().await.expect("close the claim store's file");
}

/// One trigger pulled off a real broker and not yet handled, and a
/// dispatcher that arbitrates it through a real claim store as
/// `worker-a` and hands it to a [`ResolvesThenYields`].
struct Pulled {
    bus: EventBus,
    dispatcher: TriggerDispatcher,
    worker: Arc<ResolvesThenYields>,
    claims: Arc<ControlPlaneStore>,
    consumer: PullConsumer,
    messages: pull::Stream,
    msg: Message,
    _held: (tempfile::TempDir, tempfile::TempDir, NatsServer),
}

impl Pulled {
    async fn new(resolve: Resolve, returns: Returns) -> Self {
        let server = test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let agent_id = unique_agent_id("durable-start-signal");
        let (agent_dir, registry) = registry_with(&agent_id);

        let claim_dir = tempfile::tempdir().unwrap();
        let claim_db = claim_dir.path().join("control-plane.db");
        let claims = Arc::new(ControlPlaneStore::open(&claim_db).await.unwrap());
        let worker = Arc::new(ResolvesThenYields {
            resolve,
            returns,
            claim_db,
            ran_to_end: AtomicBool::new(false),
        });
        let dispatcher = TriggerDispatcher::new(
            bus.clone(),
            registry,
            worker.clone() as Arc<dyn Worker>,
            Arc::new(FixtureClient::new()) as Arc<dyn LlmClient>,
            1,
        )
        .with_trigger_claims(recovered_claims(&claims).await, "worker-a");

        let consumer = bus
            .trigger_consumer_with_filter(
                &unique_consumer_name(),
                &crate::events::subjects::trigger(&agent_id),
                NATS_DEFAULT_MAX_ACK_PENDING,
            )
            .await
            .expect("consumer");
        bus.publish_trigger(&AgentId::new(&agent_id).unwrap(), &json!({"input": "hi"}))
            .await
            .expect("publish");
        let mut messages = consumer.messages().await.expect("messages");
        let msg = tokio::time::timeout(Duration::from_secs(5), messages.next())
            .await
            .expect("a message within 5s")
            .expect("stream open")
            .expect("message ok");

        Self {
            bus,
            dispatcher,
            worker,
            claims,
            consumer,
            messages,
            msg,
            _held: (agent_dir, claim_dir, server),
        }
    }

    fn stream_seq(&self) -> u64 {
        self.msg
            .info()
            .expect("a JetStream delivery")
            .stream_sequence
    }

    /// Hand the trigger to `handle` and wait for it to return. Before
    /// #987 this is where the handler panicked.
    async fn handle(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.dispatcher.handle(&self.msg))
            .await
            .expect("handle returns");
    }

    fn ran_to_end(&self) -> bool {
        self.worker.ran_to_end.load(Ordering::SeqCst)
    }

    /// What a fresh claim on this delivery's identity, by another
    /// claimant, gets.
    async fn probe_claim(&self) -> TriggerClaim {
        self.claims
            .claim_trigger(test_key(&self.bus, self.stream_seq()), "probe", "probe", 0)
            .await
            .expect("probe the claim")
    }
}

/// The signal is dropped unfired and the invocation then fails with a
/// transient error: `handle` must not poll the spent receiver again, and
/// the settlement after its loop NAKs the trigger for redelivery and
/// releases its claim.
#[tokio::test]
async fn a_dropped_signal_then_a_transient_error_naks_the_trigger() {
    let mut pulled = Pulled::new(Resolve::Drop, Returns::TransientError).await;
    let seq = pulled.stream_seq();

    pulled.handle().await;

    assert!(pulled.ran_to_end(), "the invocation ran to its end");
    assert_eq!(
        pulled.probe_claim().await,
        TriggerClaim::Won,
        "the NAK released the claim, so a fresh claim on the delivery wins"
    );
    // Only waits for a message to arrive, so a wide margin costs nothing
    // when the test passes.
    let redelivery = TRIGGER_RETRY_BACKOFF[0] + Duration::from_secs(10);
    let again = tokio::time::timeout(redelivery, pulled.messages.next())
        .await
        .expect("the NAKed trigger comes back for redelivery")
        .expect("stream open")
        .expect("message ok");
    let (again_seq, delivered) = {
        let info = again.info().expect("a JetStream delivery");
        (info.stream_sequence, info.delivered)
    };
    assert_eq!(
        (again_seq, delivered),
        (seq, 2),
        "the same trigger, delivered a second time"
    );
    let _ = again.ack().await;
}

/// The signal is dropped unfired and the invocation then completes:
/// `handle` must not poll the spent receiver again, and the settlement
/// after its loop acks the trigger and releases its claim.
#[tokio::test]
async fn a_dropped_signal_then_a_clean_return_acks_the_trigger() {
    let mut pulled = Pulled::new(Resolve::Drop, Returns::Completed).await;

    pulled.handle().await;

    assert!(pulled.ran_to_end(), "the invocation ran to its end");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let pending = pulled
            .consumer
            .info()
            .await
            .expect("consumer info")
            .num_ack_pending;
        if pending == 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the trigger was not acked (num_ack_pending={pending})"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        pulled.probe_claim().await,
        TriggerClaim::Won,
        "the ack released the claim, so a fresh claim on the delivery wins"
    );
}

/// The signal fires and the claim store's mark fails: `handle` must not
/// poll the spent receiver again, and it stops the run there. It returns
/// without driving the invocation any further, leaving the claim
/// `claimed` and the delivery un-acked, so the invocation's in-flight WAL
/// row is the next start's to resume (`mark_durable_started` says why;
/// see #967 for the alternatives).
#[tokio::test]
async fn a_failed_mark_after_the_signal_fires_stops_the_run() {
    let mut pulled = Pulled::new(Resolve::FireIntoRefusedMark, Returns::Completed).await;

    pulled.handle().await;

    // Deterministic, not a race: the stub fires the signal and yields,
    // and under `biased` the next turn of `handle`'s loop takes the
    // signal branch before it polls the invocation again. The failed mark
    // returns from that branch, so the stub never resumes past its yield.
    assert!(
        !pulled.ran_to_end(),
        "the invocation was driven on after its mark failed"
    );
    assert_eq!(
        pulled.probe_claim().await,
        TriggerClaim::Held {
            claimant: "worker-a".to_string()
        },
        "the mark was refused, so the claim is still `claimed` by the dispatcher"
    );
    ack_pending_stays(&mut pulled.consumer, 1, Duration::from_millis(300)).await;
}
