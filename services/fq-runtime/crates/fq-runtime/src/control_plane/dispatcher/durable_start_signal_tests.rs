//! `handle` waits on two things at once: the durable-start signal and
//! the invocation itself, in a `biased` loop that polls the signal
//! first. Two ways that can go wrong are pinned here, and what a refused
//! claim mark does once the signal has fired.
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
//! **A refused mark (#967).** The run has made its first WAL write, so it
//! is never stopped and never redelivered: a mark the claim store refuses
//! is tried again, and one refused on every attempt is answered by acking
//! the delivery without releasing its claim. The claim store is made to
//! refuse marks through a SQLite trigger of the test's own
//! ([`refuse_claim_marks`]), for a set number of attempts or for good.
//!
//! The tests run the real `handle` against a real broker and a real
//! control-plane store, with a stub worker.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_nats::jetstream::Message;
use async_nats::jetstream::consumer::{AckPolicy, PullConsumer, pull};
use futures::StreamExt;
use serde_json::json;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection};
use uuid::Uuid;

use super::TriggerDispatcher;
use super::claim::MARK_RETRY_PAUSES;
use super::duplicate_drop_tests::Captured;
use super::tests::{
    ack_pending_stays, canned_response, recovered_claims, registry_with, test_key, unique_agent_id,
    unique_consumer_name,
};
use crate::agent::{Agent, AgentId};
use crate::bus::{
    EventBus, NATS_DEFAULT_MAX_ACK_PENDING, TRIGGER_MAX_DELIVER, TRIGGER_RETRY_BACKOFF,
    TRIGGER_STREAM_NAME,
};
use crate::control_plane::{ControlPlaneStore, TriggerClaim};
use crate::llm::LlmClient;
use crate::llm::fixture::FixtureClient;
use crate::test_support::nats::{NatsServer, test_nats};
use crate::trigger::Trigger;
use crate::worker::{
    DrainRequest, DrainState, DurableStart, ExecutorError, InvocationOutcome, Worker,
};

/// The start of the line `handle` logs, at `error`, when the claim store
/// has refused every attempt to mark a durable start.
const MARK_GAVE_UP: &str = "could not mark trigger durably started";
/// The start of the line `handle` logs, at `warn`, when a mark lands
/// after the claim store refused the attempts before it.
const MARK_HEALED: &str = "marked trigger durably started after the claim store refused";
/// The line a delivery's ack logs when the client cannot send it.
const ACK_FAILED: &str = "failed to ack trigger message";

/// An ack window shorter than the pauses between a refused mark's
/// attempts ([`MARK_RETRY_PAUSES`], 1.5 seconds in all), for a consumer
/// of the test's own: a delivery that nothing kept alive while its mark
/// was retried would be redelivered underneath it. The production window
/// is 30 seconds (`TRIGGER_RETRY_BACKOFF[0]`).
const SHORT_ACK_WINDOW: Duration = Duration::from_secs(1);

/// What the stub invocation does with its durable-start signal.
enum Resolve {
    /// Drop it unfired. `run_loop_for` does this when `run_loop_inner`
    /// returns before the first WAL write, and then still awaits the MCP
    /// shutdown and the workspace reclaim before its future finishes.
    Drop,
    /// Fire it: the first WAL write has landed.
    Fire,
    /// Make the claim store refuse marks as the [`Refusals`] say, then
    /// fire it: the first WAL write has landed and the claim's update is
    /// refused.
    FireIntoRefusedMarks(Refusals),
}

/// Which attempts to mark the claim the store refuses.
#[derive(Clone, Copy)]
enum Refusals {
    /// Every one, for the rest of the test.
    Every,
    /// The first `n`, and none after: a store that heals.
    First(u32),
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
    /// The claim store's file, for [`Resolve::FireIntoRefusedMarks`].
    claim_db: PathBuf,
    ran_to_end: AtomicBool,
    /// The trigger's id and the invocation id its signal carries, as the
    /// dispatcher handed them over.
    named: Mutex<Option<(Uuid, Uuid)>>,
}

#[async_trait::async_trait]
impl Worker for ResolvesThenReturns {
    async fn run_invocation(
        &self,
        _agent: &Agent,
        _llm: &dyn LlmClient,
        trigger: Trigger,
        _delivery_attempt: Option<u32>,
        mut durable_start: DurableStart,
    ) -> Result<InvocationOutcome, ExecutorError> {
        let invocation_id = durable_start
            .invocation_id()
            .expect("the dispatcher names the run it starts");
        *self.named.lock().unwrap() = Some((trigger.id, invocation_id));
        match self.resolve {
            Resolve::Drop => drop(durable_start),
            Resolve::Fire => durable_start.fire(),
            Resolve::FireIntoRefusedMarks(refusals) => {
                refuse_claim_marks(&self.claim_db, refusals).await;
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

/// Make updates of a claim row fail from now on, as `refusals` says,
/// through a connection of its own to the store's file. The claim this
/// delivery already holds was an insert and a release is a delete, so the
/// mark is the one write on this path the refusal meets.
///
/// A counted refusal spends one row of `injected_mark_refusals` per
/// refused update. It raises `FAIL` rather than `ABORT` because `FAIL`
/// keeps what the trigger did before it, the spent row, where `ABORT`
/// would roll that back with the refused update and refuse for ever.
async fn refuse_claim_marks(claim_db: &Path, refusals: Refusals) {
    let mut statements: Vec<&'static str> = Vec::new();
    match refusals {
        Refusals::Every => statements.push(
            "CREATE TRIGGER refuse_claim_marks BEFORE UPDATE ON trigger_claim \
             BEGIN SELECT RAISE(ABORT, 'injected: the claim store refuses the mark'); END",
        ),
        Refusals::First(n) => {
            statements.push("CREATE TABLE injected_mark_refusals (spent INTEGER)");
            statements.extend(std::iter::repeat_n(
                "INSERT INTO injected_mark_refusals VALUES (0)",
                n as usize,
            ));
            statements.push(
                "CREATE TRIGGER refuse_claim_marks BEFORE UPDATE ON trigger_claim \
                 WHEN EXISTS (SELECT 1 FROM injected_mark_refusals) \
                 BEGIN DELETE FROM injected_mark_refusals \
                 WHERE rowid = (SELECT min(rowid) FROM injected_mark_refusals); \
                 SELECT RAISE(FAIL, 'injected: the claim store refuses the mark'); END",
            );
        }
    }
    let mut conn = SqliteConnectOptions::new()
        .filename(claim_db)
        .connect()
        .await
        .expect("open the claim store's file");
    for statement in statements {
        sqlx::query(statement)
            .execute(&mut conn)
            .await
            .expect("install the refusal");
    }
    conn.close().await.expect("close the claim store's file");
}

/// How many of a [`Refusals::First`] count's refusals are still unspent.
async fn refusals_left(claim_db: &Path) -> i64 {
    let mut conn = SqliteConnectOptions::new()
        .filename(claim_db)
        .connect()
        .await
        .expect("open the claim store's file");
    let (left,): (i64,) = sqlx::query_as("SELECT count(*) FROM injected_mark_refusals")
        .fetch_one(&mut conn)
        .await
        .expect("count the refusals left");
    conn.close().await.expect("close the claim store's file");
    left
}

/// One trigger pulled off a real broker and not yet handled, and a
/// dispatcher that arbitrates it through a real claim store as
/// `worker-a` and hands it to a [`ResolvesThenReturns`].
struct Pulled {
    bus: EventBus,
    dispatcher: TriggerDispatcher,
    worker: Arc<ResolvesThenReturns>,
    claims: Arc<ControlPlaneStore>,
    claim_db: PathBuf,
    url: String,
    consumer: PullConsumer,
    messages: pull::Stream,
    msg: Message,
    _held: (tempfile::TempDir, tempfile::TempDir, NatsServer),
}

impl Pulled {
    /// On a consumer configured as the dispatcher's own is, with the
    /// production ack window.
    async fn new(resolve: Resolve, turn: Turn, returns: Returns) -> Self {
        Self::on(resolve, turn, returns, None).await
    }

    /// On a consumer of the test's own whose ack window is `ack_window`.
    async fn with_ack_window(
        resolve: Resolve,
        turn: Turn,
        returns: Returns,
        ack_window: Duration,
    ) -> Self {
        Self::on(resolve, turn, returns, Some(ack_window)).await
    }

    async fn on(
        resolve: Resolve,
        turn: Turn,
        returns: Returns,
        ack_window: Option<Duration>,
    ) -> Self {
        let server = test_nats();
        let url = server.url().to_string();
        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let agent_id = unique_agent_id("durable-start-signal");
        let (agent_dir, registry) = registry_with(&agent_id);

        let claim_dir = tempfile::tempdir().unwrap();
        let claim_db = claim_dir.path().join("control-plane.db");
        let claims = Arc::new(ControlPlaneStore::open(&claim_db).await.unwrap());
        let worker = Arc::new(ResolvesThenReturns {
            resolve,
            turn,
            returns,
            claim_db: claim_db.clone(),
            ran_to_end: AtomicBool::new(false),
            named: Mutex::new(None),
        });
        let dispatcher = TriggerDispatcher::new(
            bus.clone(),
            registry,
            worker.clone() as Arc<dyn Worker>,
            Arc::new(FixtureClient::new()) as Arc<dyn LlmClient>,
            1,
        )
        .with_trigger_claims(recovered_claims(&claims).await, "worker-a");

        let filter = crate::events::subjects::trigger(&agent_id);
        let consumer = match ack_window {
            None => bus
                .trigger_consumer_with_filter(
                    &unique_consumer_name(),
                    &filter,
                    NATS_DEFAULT_MAX_ACK_PENDING,
                )
                .await
                .expect("consumer"),
            Some(ack_wait) => bus
                .jetstream()
                .get_stream(TRIGGER_STREAM_NAME)
                .await
                .expect("the trigger stream")
                .create_consumer(pull::Config {
                    durable_name: Some(unique_consumer_name()),
                    filter_subject: filter,
                    ack_policy: AckPolicy::Explicit,
                    ack_wait,
                    max_deliver: TRIGGER_MAX_DELIVER,
                    ..Default::default()
                })
                .await
                .expect("consumer"),
        };
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
            claim_db,
            url,
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

    /// Hand the trigger to `handle` with a capture of what it logs at
    /// `warn` and above installed.
    async fn handle_capturing(&self) -> Captured {
        let log = Captured::default();
        let _capture = log.install();
        self.handle().await;
        log
    }

    fn ran_to_end(&self) -> bool {
        self.worker.ran_to_end.load(Ordering::SeqCst)
    }

    /// The trigger's id and its invocation's id, once the invocation
    /// has started.
    fn named(&self) -> (Uuid, Uuid) {
        self.worker
            .named
            .lock()
            .unwrap()
            .expect("the invocation started")
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

    /// Start reading every answer this delivery sends the broker.
    async fn watch_answers(&self) -> Answers {
        Answers::watch(&self.url, &self.msg).await
    }

    /// Make every answer this delivery sends fail in the client, before
    /// it can reach the broker: the delivery answers through a connection
    /// of its own that has been drained and closed. It still names the
    /// same message, because `info()` reads the reply subject alone.
    async fn refuse_acks(&mut self) {
        let client = async_nats::connect(self.url.as_str())
            .await
            .expect("connect a connection to close");
        client.drain().await.expect("drain it");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while client
            .publish("fq.test.closed-probe", "".into())
            .await
            .is_ok()
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the drained connection still accepts a publish after 5s"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.msg.context = async_nats::jetstream::new(client);
    }
}

/// Every answer one delivery sends the broker, read off its ack subject
/// by a connection of its own. An ack is an empty body, an in-progress
/// ack `+WPI`, a NAK `-NAK`.
struct Answers {
    _client: async_nats::Client,
    subscription: async_nats::Subscriber,
}

/// What [`Answers::sent`] read, counted by kind.
#[derive(Debug)]
struct Sent {
    acks: usize,
    keepalives: usize,
    other: Vec<String>,
}

impl Answers {
    async fn watch(url: &str, msg: &Message) -> Self {
        let client = async_nats::connect(url).await.expect("connect a watcher");
        let reply = msg
            .reply
            .clone()
            .expect("a JetStream delivery has an ack subject");
        let subscription = client.subscribe(reply).await.expect("watch the answers");
        client
            .flush()
            .await
            .expect("the watch is in place before the delivery is answered");
        Self {
            _client: client,
            subscription,
        }
    }

    /// The answers sent so far, read until none has arrived for half a
    /// second.
    async fn sent(&mut self) -> Sent {
        let mut sent = Sent {
            acks: 0,
            keepalives: 0,
            other: Vec::new(),
        };
        while let Ok(Some(answer)) =
            tokio::time::timeout(Duration::from_millis(500), self.subscription.next()).await
        {
            match answer.payload.as_ref() {
                b"" | b"+ACK" => sent.acks += 1,
                b"+WPI" => sent.keepalives += 1,
                other => sent.other.push(String::from_utf8_lossy(other).into_owned()),
            }
        }
        sent
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

/// The signal fires and the claim store refuses the first two attempts
/// to mark the claim, then takes the third: a store that heals inside the
/// retries. Nothing else may happen. The run is not stopped, the claim
/// ends `durably_started`, the delivery is acked once and nothing else is
/// sent for it but keepalives, and the heal is logged once at `warn`.
/// Before #967 the first refusal stopped the run there, un-acked.
#[tokio::test]
async fn a_mark_refused_then_accepted_inside_its_retries_marks_the_claim_and_acks_once() {
    let mut pulled = Pulled::new(
        Resolve::FireIntoRefusedMarks(Refusals::First(2)),
        Turn::Later,
        Returns::Completed,
    )
    .await;
    let mut answers = pulled.watch_answers().await;

    let log = pulled.handle_capturing().await;

    assert!(
        pulled.ran_to_end(),
        "the invocation was stopped instead of driven on while its mark was retried"
    );
    assert_eq!(
        refusals_left(&pulled.claim_db).await,
        0,
        "both refusals were spent, so the mark that landed was a retry"
    );
    assert_marked_started(pulled.probe_claim().await);
    pulled.wait_until_acked().await;
    let sent = answers.sent().await;
    assert!(
        sent.acks == 1 && sent.other.is_empty(),
        "the delivery is acked once, and nothing but keepalives is sent besides: {sent:?}"
    );
    assert_eq!(
        log.lines_with(MARK_HEALED).len(),
        1,
        "the heal is logged once"
    );
    assert!(
        log.lines_with(MARK_GAVE_UP).is_empty(),
        "the mark landed, so nothing says it was given up"
    );
}

/// The signal fires and the claim store refuses every attempt to mark the
/// claim. The run started, so it is neither stopped nor redelivered: the
/// delivery is acked without its claim being released, the claim stays
/// `claimed` by the dispatcher, the run goes on to its end, and the give-up
/// is logged once at `error`, naming the invocation and the trigger.
///
/// The consumer's ack window is shorter than the pauses between the
/// attempts ([`SHORT_ACK_WINDOW`]), so this also pins the keepalive that
/// runs alongside them: a delivery left to lapse while its mark was
/// retried would be redelivered underneath it. No copy comes back
/// either during the retries or for three windows after the ack, so none
/// can be dropped as a held duplicate or end in a dead letter.
#[tokio::test]
async fn a_mark_refused_past_its_retries_acks_keeping_the_claim_and_the_run_runs_on() {
    assert!(
        SHORT_ACK_WINDOW < MARK_RETRY_PAUSES.iter().sum::<Duration>(),
        "the window must lapse during the retries' pauses, or this test cannot see a missing \
         keepalive"
    );
    let mut pulled = Pulled::with_ack_window(
        Resolve::FireIntoRefusedMarks(Refusals::Every),
        Turn::Later,
        Returns::Completed,
        SHORT_ACK_WINDOW,
    )
    .await;
    let mut answers = pulled.watch_answers().await;

    let log = pulled.handle_capturing().await;

    assert!(
        pulled.ran_to_end(),
        "the invocation was stopped instead of driven on after its mark was refused"
    );
    assert_held_by_the_dispatcher(
        pulled.probe_claim().await,
        "the mark never landed, so the claim must be left `claimed` by the dispatcher, \
         not released",
    );
    pulled.wait_until_acked().await;
    let sent = answers.sent().await;
    assert!(
        sent.acks == 1 && sent.other.is_empty(),
        "the delivery is acked once, and nothing but keepalives is sent besides: {sent:?}"
    );
    assert!(
        sent.keepalives > 0,
        "the delivery was kept alive while its mark was retried: {sent:?}"
    );
    let redelivered = tokio::time::timeout(SHORT_ACK_WINDOW * 3, pulled.messages.next())
        .await
        .ok()
        .flatten()
        .map(|copy| copy.map(|copy| copy.info().map(|info| info.delivered).ok()));
    assert!(
        redelivered.is_none(),
        "a started run's delivery must not come back, and came back as delivery {redelivered:?}"
    );

    let (trigger_id, invocation_id) = pulled.named();
    let gave_up = log.lines_with(MARK_GAVE_UP);
    assert_eq!(gave_up.len(), 1, "the give-up is logged once: {gave_up:?}");
    let line = &gave_up[0];
    assert!(
        line.contains("ERROR")
            && line.contains(&format!("invocation_id={invocation_id}"))
            && line.contains(&format!("trigger_id={trigger_id}")),
        "logged at error, naming the invocation and the trigger: {line}"
    );
}

/// The signal fires, the claim store refuses every attempt to mark the
/// claim, and the ack sent in place of the mark cannot be sent either.
/// What follows is the risk #967 accepted: the delivery is left un-acked
/// with its claim `claimed`, so its redeliveries will meet this daemon's
/// own claim and be dropped. The failure must be logged, naming the
/// trigger, and the run must still go on to its end.
#[tokio::test]
async fn a_failed_ack_after_a_mark_refused_past_its_retries_is_logged_and_the_run_runs_on() {
    let mut pulled = Pulled::new(
        Resolve::FireIntoRefusedMarks(Refusals::Every),
        Turn::Later,
        Returns::Completed,
    )
    .await;
    pulled.refuse_acks().await;

    let log = pulled.handle_capturing().await;

    assert!(
        pulled.ran_to_end(),
        "the invocation was stopped instead of driven on after its ack failed"
    );
    assert_held_by_the_dispatcher(
        pulled.probe_claim().await,
        "the claim must be left `claimed` by the dispatcher, not released",
    );
    ack_pending_stays(&mut pulled.consumer, 1, Duration::from_millis(300)).await;
    let (trigger_id, _) = pulled.named();
    let failed = log.lines_with(ACK_FAILED);
    assert_eq!(failed.len(), 1, "the failed ack is logged once: {failed:?}");
    assert!(
        failed[0].contains("ERROR") && failed[0].contains(&format!("trigger_id={trigger_id}")),
        "logged at error, naming the trigger: {}",
        failed[0]
    );
    assert_eq!(
        log.lines_with(MARK_GAVE_UP).len(),
        1,
        "the refused mark is logged once"
    );
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

/// What a mark refused on every attempt must leave: the claim as it was,
/// `claimed` by the dispatcher, and the delivery acked.
///
/// The run started, so a redelivery is never wanted: on this daemon the
/// same-claimant `Held` arm would drop each one until the trigger was
/// dead-lettered as `trigger_exhausted` although it ran, and after a
/// restart a finished run's claim would be adopted and the work run
/// again. So the delivery is acked. The claim is not released first: a
/// lost ack would then let the redelivery win a fresh claim. A started
/// run under a claim that still says `claimed` is what is left (#967).
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
/// the claim store refuses every mark (#966): a finished run, acked with
/// its claim kept ([`assert_acked_with_the_claim_kept`]).
#[tokio::test]
async fn a_failed_mark_after_a_completion_in_the_same_poll_acks_and_keeps_the_claim() {
    let mut pulled = Pulled::new(
        Resolve::FireIntoRefusedMarks(Refusals::Every),
        Turn::Same,
        Returns::Completed,
    )
    .await;

    pulled.handle().await;

    assert_acked_with_the_claim_kept(&mut pulled).await;
}

/// The signal fires and the invocation fails with a transient error in
/// the same poll, and the claim store refuses every mark (#966). Before
/// durable start a transient error is NAKed for a retry; this run had
/// started, so it is acked with its claim kept instead
/// ([`assert_acked_with_the_claim_kept`]).
#[tokio::test]
async fn a_failed_mark_after_a_transient_error_in_the_same_poll_acks_and_keeps_the_claim() {
    let mut pulled = Pulled::new(
        Resolve::FireIntoRefusedMarks(Refusals::Every),
        Turn::Same,
        Returns::TransientError,
    )
    .await;

    pulled.handle().await;

    assert_acked_with_the_claim_kept(&mut pulled).await;
}
