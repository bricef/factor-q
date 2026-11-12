//! `handle` waits on two things at once: the durable-start signal and
//! the invocation itself, in a `biased` loop that polls the signal
//! first. Two ways that can go wrong are pinned here.
//!
//! **A spent signal polled again (#987).** The signal is a oneshot
//! receiver, and polling one again after it has resolved panics with
//! "called after complete", so once the signal has resolved, whatever it
//! resolved to, `handle` must stop polling it. These tests resolve the
//! signal and then keep the invocation running for one more turn of
//! `handle`'s loop, by yielding once before it returns ([`Turn::Later`]).
//! That turn is the one on which the spent receiver used to be polled
//! again.
//!
//! **A signal fired in the poll that ends the invocation (#966).** An
//! invocation that fires the signal and returns without an await in
//! between ends on a turn whose signal poll had already come back empty,
//! so the loop ends on the result without having seen the signal. These
//! tests fire and return in one poll ([`Turn::Same`]), and `handle` must
//! still treat the run as durably started.
//!
//! The tests run the real `handle` against a real broker and a real
//! control-plane store, with a stub worker.

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
    /// Fire it: the first WAL write has landed.
    Fire,
    /// Make the claim store refuse the mark, then fire it: the first WAL
    /// write has landed and the claim's update fails.
    FireIntoRefusedMark,
}

/// On which turn of `handle`'s loop the stub invocation returns.
enum Turn {
    /// A later one than the turn that resolved the signal: the stub
    /// yields once before it returns, so `handle` sees the signal resolve
    /// on one turn of its loop and the invocation finish on a later one.
    Later,
    /// The same one: the stub returns in the poll that resolved the
    /// signal, with no await in between. The loop polled the signal
    /// before it polled the invocation, so that turn ends on the result
    /// with the signal unseen (#966).
    Same,
}

/// What the stub invocation returns.
enum Returns {
    /// A transient error, which `handle` NAKs when no durable start was
    /// observed.
    TransientError,
    /// A completed run.
    Completed,
}

/// An invocation that resolves its durable-start signal and then
/// returns, on the turn of `handle`'s loop that its [`Turn`] says.
struct ResolvesThenReturns {
    resolve: Resolve,
    turn: Turn,
    returns: Returns,
    /// The claim store's file, for [`Resolve::FireIntoRefusedMark`].
    claim_db: PathBuf,
    ran_to_end: AtomicBool,
}

#[async_trait::async_trait]
impl Worker for ResolvesThenReturns {
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
            Resolve::Fire => durable_start.fire(),
            Resolve::FireIntoRefusedMark => {
                refuse_claim_marks(&self.claim_db).await;
                durable_start.fire();
            }
        }
        if let Turn::Later = self.turn {
            tokio::task::yield_now().await;
        }
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
/// `worker-a` and hands it to a [`ResolvesThenReturns`].
struct Pulled {
    bus: EventBus,
    dispatcher: TriggerDispatcher,
    worker: Arc<ResolvesThenReturns>,
    claims: Arc<ControlPlaneStore>,
    consumer: PullConsumer,
    messages: pull::Stream,
    msg: Message,
    _held: (tempfile::TempDir, tempfile::TempDir, NatsServer),
}

impl Pulled {
    async fn new(resolve: Resolve, turn: Turn, returns: Returns) -> Self {
        let server = test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let agent_id = unique_agent_id("durable-start-signal");
        let (agent_dir, registry) = registry_with(&agent_id);

        let claim_dir = tempfile::tempdir().unwrap();
        let claim_db = claim_dir.path().join("control-plane.db");
        let claims = Arc::new(ControlPlaneStore::open(&claim_db).await.unwrap());
        let worker = Arc::new(ResolvesThenReturns {
            resolve,
            turn,
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

    /// Wait until the broker has the delivery acked: nothing is pending
    /// acknowledgement on the consumer.
    async fn wait_until_acked(&mut self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let pending = self
                .consumer
                .info()
                .await
                .expect("consumer info")
                .num_ack_pending;
            if pending == 0 {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the trigger was not acked (num_ack_pending={pending})"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// The run durably started, so its claim must read `durably_started`:
/// released, it would let the next delivery win a fresh claim and start
/// the work again.
fn assert_marked_started(claim: TriggerClaim) {
    assert!(
        matches!(claim, TriggerClaim::Started { .. }),
        "the run durably started, so its claim must be marked `durably_started`; \
         a fresh claim on the delivery got {claim:?}"
    );
}

/// The signal is dropped unfired and the invocation then fails with a
/// transient error: `handle` must not poll the spent receiver again, and
/// the settlement after its loop NAKs the trigger for redelivery and
/// releases its claim.
#[tokio::test]
async fn a_dropped_signal_then_a_transient_error_naks_the_trigger() {
    let mut pulled = Pulled::new(Resolve::Drop, Turn::Later, Returns::TransientError).await;
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
    let mut pulled = Pulled::new(Resolve::Drop, Turn::Later, Returns::Completed).await;

    pulled.handle().await;

    assert!(pulled.ran_to_end(), "the invocation ran to its end");
    pulled.wait_until_acked().await;
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
    let mut pulled = Pulled::new(
        Resolve::FireIntoRefusedMark,
        Turn::Later,
        Returns::Completed,
    )
    .await;

    pulled.handle().await;

    // Deterministic, not a race: the stub fires the signal and yields,
    // and under `biased` the next turn of `handle`'s loop takes the
    // signal branch before it polls the invocation again. The failed mark
    // returns from that branch, so the stub never resumes past its yield.
    assert!(
        !pulled.ran_to_end(),
        "the invocation was driven on after its mark failed"
    );
    assert_held_by_the_dispatcher(
        pulled.probe_claim().await,
        "the mark was refused, so the claim is still `claimed` by the dispatcher",
    );
    ack_pending_stays(&mut pulled.consumer, 1, Duration::from_millis(300)).await;
}

/// The signal fires and the invocation completes in the same poll, so
/// `handle`'s loop ends on the result without having seen the signal
/// (#966). The run durably started all the same: the claim must be
/// marked `durably_started` and the delivery acked, as the loop's own
/// signal branch would have done, not released as if nothing had run.
#[tokio::test]
async fn a_signal_fired_in_the_poll_that_completes_marks_the_claim_and_acks() {
    let mut pulled = Pulled::new(Resolve::Fire, Turn::Same, Returns::Completed).await;

    pulled.handle().await;

    assert!(pulled.ran_to_end(), "the invocation ran to its end");
    assert_marked_started(pulled.probe_claim().await);
    pulled.wait_until_acked().await;
}

/// The signal fires and the invocation fails with a transient error in
/// the same poll (#966). Before durable start a transient error is NAKed
/// for a retry, but this run had started: the claim must be marked
/// `durably_started` and the delivery acked, so that no redelivery comes
/// back to start the work a second time.
#[tokio::test]
async fn a_signal_fired_in_the_poll_that_fails_transiently_is_acked_not_retried() {
    let mut pulled = Pulled::new(Resolve::Fire, Turn::Same, Returns::TransientError).await;

    pulled.handle().await;

    assert!(pulled.ran_to_end(), "the invocation ran to its end");
    assert_marked_started(pulled.probe_claim().await);
    // Acked, and so not NAKed: a NAK with a delay keeps the delivery
    // pending until it is redelivered, so nothing pending means no retry
    // is on its way.
    pulled.wait_until_acked().await;
}

/// What a refused mark after a return in the same poll must leave: the
/// claim as it was, `claimed` by the dispatcher, and the delivery acked.
///
/// The run started and has returned, so it cannot be stopped, and a
/// redelivery is never wanted: on this daemon the same-claimant `Held`
/// arm would drop each one until the trigger was dead-lettered as
/// `trigger_exhausted` although it ran, and after a restart a finished
/// run's claim would be adopted and the work run again. So the delivery
/// is acked. The claim is not released first: a lost ack would then let
/// the redelivery win a fresh claim. A started run under a claim that
/// still says `claimed` is what is left; see #967.
async fn assert_acked_with_the_claim_kept(pulled: &mut Pulled) {
    assert!(pulled.ran_to_end(), "the invocation ran to its end");
    assert_held_by_the_dispatcher(
        pulled.probe_claim().await,
        "the claim must be left as it was, `claimed` by the dispatcher, not released",
    );
    pulled.wait_until_acked().await;
}

/// The claim is still `claimed`, by `worker-a`, the dispatcher's id.
fn assert_held_by_the_dispatcher(claim: TriggerClaim, why: &str) {
    assert!(
        matches!(&claim, TriggerClaim::Held { claimant, .. } if claimant == "worker-a"),
        "{why}; a fresh claim on the delivery got {claim:?}"
    );
}

/// The signal fires and the invocation completes in the same poll, and
/// the claim store refuses the mark (#966): a finished run, acked with
/// its claim kept ([`assert_acked_with_the_claim_kept`]).
#[tokio::test]
async fn a_failed_mark_after_a_completion_in_the_same_poll_acks_and_keeps_the_claim() {
    let mut pulled =
        Pulled::new(Resolve::FireIntoRefusedMark, Turn::Same, Returns::Completed).await;

    pulled.handle().await;

    assert_acked_with_the_claim_kept(&mut pulled).await;
}

/// The signal fires and the invocation fails with a transient error in
/// the same poll, and the claim store refuses the mark (#966). Before
/// durable start a transient error is NAKed for a retry; this run had
/// started, so it is acked with its claim kept instead
/// ([`assert_acked_with_the_claim_kept`]).
#[tokio::test]
async fn a_failed_mark_after_a_transient_error_in_the_same_poll_acks_and_keeps_the_claim() {
    let mut pulled = Pulled::new(
        Resolve::FireIntoRefusedMark,
        Turn::Same,
        Returns::TransientError,
    )
    .await;

    pulled.handle().await;

    assert_acked_with_the_claim_kept(&mut pulled).await;
}
