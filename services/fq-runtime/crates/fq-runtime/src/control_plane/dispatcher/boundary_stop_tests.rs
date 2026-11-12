//! What a stop at a step boundary does to the trigger of an invocation
//! this dispatcher started (#974): a drain or an operator halt, landing
//! either during the invocation's setup, before its first WAL write, or
//! at a later boundary, after it.
//!
//! Each test runs the real `handle` against a real broker and a real
//! control-plane store, and hands the trigger to the real
//! `ReducerRunner` over a real WAL. The timing is held, not slept on:
//! [`HeldSetup`] parks the invocation in workspace provisioning, which
//! `run_loop_for` reaches after the claim and before `triggered`, the
//! step loop and the first WAL write; [`ParkedModel`] parks it in its
//! first model call, which comes after that write. The test lands the
//! drain or the halt while the invocation is parked, then lets it go.
//!
//! `duplicate_drop_tests` parks an invocation in its setup the same way,
//! and borrows this file's harness to do it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_nats::jetstream::AckKind;
use async_nats::jetstream::consumer::PullConsumer;
use futures::StreamExt;
use serde_json::json;
use tokio::sync::Semaphore;
use uuid::Uuid;

use super::tests::{
    ack_pending_stays, canned_response, registry_with, test_key, test_pricing, test_tools,
    unique_agent_id, unique_consumer_name,
};
use super::{SharedRegistry, TriggerClaims, TriggerDispatcher};
use crate::agent::AgentId;
use crate::bus::{EventBus, NATS_DEFAULT_MAX_ACK_PENDING};
use crate::control_plane::{ControlPlaneStore, TriggerClaim};
use crate::llm::fixture::FixtureClient;
use crate::llm::{ChatRequest, ChatResponse, LlmClient, LlmError};
use crate::worker::workspace::{PerInvocationWorkspace, WorkspaceError, WorkspaceProvider};
use crate::worker::{
    Harness, InFlightReport, RecoveryCategory, ReducerContext, ReducerRunner, RunnerConfig, Worker,
    WorkerId, WorkerStore, scan_in_flight,
};

/// Every wait in this file polls at this interval, at most
/// [`MAX_POLLS`] times, under a wall-clock deadline of the same length.
const POLL: Duration = Duration::from_millis(25);
const MAX_POLLS: u32 = 400;

/// Poll `ready` until it holds, bounded by count and by time.
async fn wait_until(what: &str, mut ready: impl AsyncFnMut() -> bool) {
    let deadline = std::time::Instant::now() + POLL * MAX_POLLS;
    for _ in 0..MAX_POLLS {
        if ready().await {
            return;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(POLL).await;
    }
    panic!("timed out waiting for {what}");
}

/// A per-invocation workspace provider that parks each fresh invocation
/// in `provision` until the test lets it through, and records which
/// invocations it provisioned and which it reclaimed. The directories
/// are real, so "kept" and "reclaimed" are facts about the disk.
#[derive(Debug)]
pub(super) struct HeldSetup {
    inner: PerInvocationWorkspace,
    _root: tempfile::TempDir,
    gate: Semaphore,
    provisioned: Mutex<Vec<Uuid>>,
    reclaimed: Mutex<Vec<Uuid>>,
}

impl HeldSetup {
    /// `open`: how many invocations may provision without waiting.
    pub(super) fn new(open: usize) -> Arc<Self> {
        let root = tempfile::tempdir().expect("workspace root");
        Arc::new(Self {
            inner: PerInvocationWorkspace::new(root.path().to_path_buf()),
            _root: root,
            gate: Semaphore::new(open),
            provisioned: Mutex::new(Vec::new()),
            reclaimed: Mutex::new(Vec::new()),
        })
    }

    /// The invocation parked in its setup, once there is one: the id
    /// the dispatcher named on the trigger's claim.
    pub(super) async fn parked(&self) -> Uuid {
        wait_until("an invocation to enter its setup", async || {
            !self.provisioned.lock().unwrap().is_empty()
        })
        .await;
        self.provisioned.lock().unwrap()[0]
    }

    pub(super) fn let_through(&self) {
        self.gate.add_permits(1);
    }

    fn reclaimed(&self) -> Vec<Uuid> {
        self.reclaimed.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl WorkspaceProvider for HeldSetup {
    async fn provision(&self, invocation_id: Uuid) -> Result<std::path::PathBuf, WorkspaceError> {
        self.provisioned.lock().unwrap().push(invocation_id);
        self.gate.acquire().await.expect("gate open").forget();
        self.inner.provision(invocation_id).await
    }

    async fn reclaim(
        &self,
        invocation_id: Uuid,
        path: &std::path::Path,
    ) -> Result<(), WorkspaceError> {
        self.reclaimed.lock().unwrap().push(invocation_id);
        self.inner.reclaim(invocation_id, path).await
    }
}

/// A model that parks every call until the test lets it answer, and
/// counts the calls it was asked.
struct ParkedModel {
    inner: FixtureClient,
    asked: AtomicUsize,
    gate: Semaphore,
}

impl ParkedModel {
    fn new() -> Arc<Self> {
        let inner = FixtureClient::new();
        inner.push_response(canned_response());
        Arc::new(Self {
            inner,
            asked: AtomicUsize::new(0),
            gate: Semaphore::new(0),
        })
    }
}

#[async_trait::async_trait]
impl LlmClient for ParkedModel {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.gate.acquire().await.expect("gate open").forget();
        self.inner.chat(request).await
    }
}

/// A model with one answer, the outcome report, for an invocation that
/// is meant to run to completion.
pub(super) fn one_answer() -> Arc<FixtureClient> {
    let model = FixtureClient::new();
    model.push_response(canned_response());
    Arc::new(model)
}

/// One binary's worth of state directory: the control-plane store the
/// claims live in and the worker's WAL. A restarted daemon opens the
/// same two files, so the binaries in a test share them.
pub(super) struct StateDir {
    _dir: tempfile::TempDir,
    claims: Arc<ControlPlaneStore>,
    wal: Arc<WorkerStore>,
}

impl StateDir {
    pub(super) async fn new() -> Self {
        let dir = tempfile::tempdir().expect("state dir");
        let claims = Arc::new(
            ControlPlaneStore::open(&dir.path().join("control-plane.db"))
                .await
                .expect("control-plane store"),
        );
        let wal = Arc::new(
            WorkerStore::open(&dir.path().join("worker.db"))
                .await
                .expect("worker store"),
        );
        Self {
            _dir: dir,
            claims,
            wal,
        }
    }

    /// The real runner, claiming as `binary`, over this WAL.
    pub(super) fn runner(
        &self,
        bus: &EventBus,
        binary: &str,
        workspace: Option<Arc<HeldSetup>>,
    ) -> Arc<ReducerRunner> {
        Arc::new(ReducerRunner::new(
            Arc::new(ReducerContext::builder().tools(test_tools()).build()),
            Arc::new(
                RunnerConfig::builder()
                    .bus(bus.clone())
                    .pricing(test_pricing())
                    .store(self.wal.clone())
                    .worker_id(WorkerId::new(binary).expect("worker id"))
                    .workspace(workspace.map(|w| w as Arc<dyn WorkspaceProvider>))
                    .build(),
            ),
            Harness::new(),
        ))
    }

    /// A dispatcher for one binary, built the way the daemon builds it:
    /// startup recovery over this WAL first, then the claims it returns.
    pub(super) async fn dispatcher(
        &self,
        bus: &EventBus,
        registry: &SharedRegistry,
        worker: Arc<dyn Worker>,
        model: Arc<dyn LlmClient>,
        binary: &str,
    ) -> Arc<TriggerDispatcher> {
        let in_flight = InFlightReport::scan(&self.wal).await.expect("scan WAL");
        let claims = TriggerClaims::recover(self.claims.clone(), &in_flight)
            .await
            .expect("recover claims");
        Arc::new(
            TriggerDispatcher::new(bus.clone(), registry.clone(), worker, model, 1)
                .with_trigger_claims(claims, binary),
        )
    }

    /// What the claim row for `seq` says, read by a claim that cannot win
    /// it while a row exists. On a row that exists the probe writes
    /// nothing; on a missing one it wins and writes one, so a probe that
    /// finds no row is the last thing a test can learn about the claim.
    pub(super) async fn claim(&self, bus: &EventBus, seq: u64) -> TriggerClaim {
        self.claims
            .claim_trigger(test_key(bus, seq), "probe", "probe", 0)
            .await
            .expect("probe the claim")
    }
}

/// A consumer of `agent`'s triggers, a published trigger, and its first
/// delivery.
pub(super) async fn one_delivery(
    bus: &EventBus,
    agent: &str,
) -> (PullConsumer, async_nats::jetstream::Message) {
    let mut consumer = bus
        .trigger_consumer_with_filter(
            &unique_consumer_name(),
            &crate::events::subjects::trigger(agent),
            NATS_DEFAULT_MAX_ACK_PENDING,
        )
        .await
        .expect("consumer");
    bus.publish_trigger(&AgentId::new(agent).unwrap(), &json!({"input": "deploy"}))
        .await
        .expect("publish");
    let msg = next_delivery(&mut consumer).await;
    (consumer, msg)
}

pub(super) async fn next_delivery(consumer: &mut PullConsumer) -> async_nats::jetstream::Message {
    let mut messages = consumer.messages().await.expect("messages");
    tokio::time::timeout(Duration::from_secs(20), messages.next())
        .await
        .expect("a delivery within 20s")
        .expect("stream open")
        .expect("message ok")
}

/// Listen on `msg`'s own ack subject from a connection of the test's
/// own, so every answer anyone sends the broker for that delivery
/// (`+ACK`, `-NAK`, `+WPI`, `+TERM`) is heard here. The client is
/// returned with the subscription to keep the connection open.
pub(super) async fn answers_to(
    server_url: &str,
    msg: &async_nats::jetstream::Message,
) -> (async_nats::Client, async_nats::Subscriber) {
    let client = async_nats::connect(server_url).await.expect("listen");
    let reply = msg.reply.clone().expect("a delivery has an ack subject");
    let answers = client.subscribe(reply).await.expect("subscribe");
    client
        .flush()
        .await
        .expect("the subscription reached the broker");
    (client, answers)
}

/// Wait until the broker has nothing unacked on `consumer`, and then
/// check it has nothing queued either: the trigger was consumed, not
/// handed back.
pub(super) async fn settled(consumer: &mut PullConsumer, what: &str) {
    wait_until(what, async || {
        consumer.info().await.expect("info").num_ack_pending == 0
    })
    .await;
    let info = consumer.info().await.expect("info").clone();
    assert_eq!(
        info.num_pending, 0,
        "{what}: the trigger was put back: {info:?}"
    );
}

/// **A drain that lands during an invocation's setup gives its trigger
/// back to the broker, and the next binary runs it once (#974).**
///
/// The drain arrives after `handle` has claimed and dispatched the
/// trigger, and before the runner's first step: the window the
/// dispatcher's own drain check cannot see. Nothing durable exists for
/// the invocation, so its trigger is the only record of the work: it
/// must stay un-acked, with its claim still held by the binary that
/// drained. The next binary, a restart on the same stores under a new
/// worker id, adopts that claim when the broker redelivers and runs the
/// trigger to completion, once.
#[tokio::test]
async fn a_drain_during_setup_leaves_the_trigger_for_the_next_binary_which_runs_it_once() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let agent = unique_agent_id("drained-in-setup");
    let (_agents, registry) = registry_with(&agent);
    let state = StateDir::new().await;

    // ---- The binary being deployed away. ----
    let setup = HeldSetup::new(0);
    let draining = state.runner(&bus, "binary-1", Some(setup.clone()));
    let unasked = Arc::new(FixtureClient::new());
    let first = state
        .dispatcher(
            &bus,
            &registry,
            draining.clone(),
            unasked.clone(),
            "binary-1",
        )
        .await;
    let (mut consumer, msg) = one_delivery(&bus, &agent).await;
    let seq = msg.info().unwrap().stream_sequence;
    let redelivery = msg.clone();
    let (_listener, mut answers) = answers_to(server.url(), &msg).await;
    let handling = tokio::spawn(async move { first.handle(&msg).await });

    let drained = setup.parked().await;
    draining.drain_signal().request();
    setup.let_through();
    tokio::time::timeout(Duration::from_secs(10), handling)
        .await
        .expect("handle returns once the invocation stops")
        .expect("task joins");

    // Neither acked nor NAKed. The broker still counts the delivery as
    // pending, which a NAK with a delay would leave it as too, so what
    // settles the question is the delivery's ack subject: nothing
    // answered it, in either direction.
    ack_pending_stays(&mut consumer, 1, Duration::from_millis(300)).await;
    if let Ok(Some(answer)) = tokio::time::timeout(Duration::from_millis(300), answers.next()).await
    {
        panic!(
            "the drained delivery was answered with {:?}; it must be left for the next binary",
            String::from_utf8_lossy(&answer.payload)
        );
    }
    // The claim stays with the binary that drained, still naming the
    // invocation that wrote nothing. The next binary's recovery has no
    // WAL row to mark it by, so its adoption starts the trigger; and had
    // a row existed, recovery would have marked this claim started and
    // the redelivery would be dropped instead. A released claim would
    // lose that: the redelivery would win a fresh one either way.
    let held = state.claim(&bus, seq).await;
    assert!(
        matches!(
            &held,
            TriggerClaim::Held { claimant, invocation_id: Some(named), .. }
                if claimant == "binary-1" && *named == drained.to_string()
        ),
        "the drained trigger's claim must stay held for the next binary to adopt, still \
         naming the invocation that wrote nothing; got {held:?}"
    );
    assert_eq!(
        setup.reclaimed(),
        vec![drained],
        "nothing will resume the drained invocation, so its workspace goes"
    );
    assert!(
        state
            .wal
            .get_invocation_state(&drained.to_string())
            .await
            .unwrap()
            .is_none(),
        "the drain landed before the first WAL write, so there is no row to resume"
    );
    assert!(
        unasked.requests().is_empty(),
        "the drained invocation never took a step"
    );

    // ---- The next binary, restarted on the same stores. ----
    // The broker's redelivery, brought forward from the end of the ack
    // window.
    redelivery
        .ack_with(AckKind::Nak(None))
        .await
        .expect("redeliver the trigger");
    let model = one_answer();
    let next = state
        .dispatcher(
            &bus,
            &registry,
            state.runner(&bus, "binary-2", None),
            model.clone(),
            "binary-2",
        )
        .await;
    let redelivered = next_delivery(&mut consumer).await;
    let info = redelivered.info().unwrap();
    assert_eq!(info.stream_sequence, seq, "the same trigger comes back");
    assert_eq!(info.delivered, 2, "as its second delivery");
    tokio::time::timeout(Duration::from_secs(20), next.handle(&redelivered))
        .await
        .expect("the next binary runs the trigger to an end");
    settled(
        &mut consumer,
        "the next binary's durable start acks the trigger",
    )
    .await;

    // Once: one model call, one WAL row, and the claim names that run.
    assert_eq!(model.requests().len(), 1, "the trigger ran once");
    let TriggerClaim::Started { invocation_id } = state.claim(&bus, seq).await else {
        panic!("the claim must record the next binary's durable start");
    };
    assert_ne!(
        invocation_id,
        drained.to_string(),
        "the adopted claim runs under the next binary's own invocation id"
    );
    let row = state
        .wal
        .get_invocation_state(&invocation_id)
        .await
        .unwrap()
        .expect("the run's WAL row");
    assert!(row.terminal_at.is_some(), "the run completed: {row:?}");
    assert!(
        state
            .wal
            .find_in_flight_invocations()
            .await
            .unwrap()
            .is_empty(),
        "nothing is left in flight"
    );
}

/// **An operator halt that lands during an invocation's setup consumes
/// its trigger (#974).**
///
/// The same window as the drain above, and the opposite answer. A halt
/// is armed by `invocation.drop --live`, which records the drop as the
/// invocation's outcome; the daemon goes on running. Handing the trigger
/// back would start the work the operator just dropped, or, with the
/// claim left held by this live process, have every redelivery refused
/// until the trigger is dead-lettered. So the trigger is acked and its
/// unfinished claim given back, as for any invocation that returned
/// before its durable start for good. Only the halt is armed here; the
/// drop's own event is the operator surface's business and plays no part
/// in what `handle` does.
#[tokio::test]
async fn an_operator_halt_during_setup_consumes_the_trigger() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let agent = unique_agent_id("halted-in-setup");
    let (_agents, registry) = registry_with(&agent);
    let state = StateDir::new().await;

    let setup = HeldSetup::new(0);
    let runner = state.runner(&bus, "binary-1", Some(setup.clone()));
    let unasked = Arc::new(FixtureClient::new());
    let dispatcher = state
        .dispatcher(&bus, &registry, runner.clone(), unasked.clone(), "binary-1")
        .await;
    let (mut consumer, msg) = one_delivery(&bus, &agent).await;
    let seq = msg.info().unwrap().stream_sequence;
    let handling = tokio::spawn(async move { dispatcher.handle(&msg).await });

    let halted = setup.parked().await;
    assert!(
        runner.request_halt(halted),
        "the invocation is live during its setup, so the halt arms"
    );
    setup.let_through();
    tokio::time::timeout(Duration::from_secs(10), handling)
        .await
        .expect("handle returns once the invocation stops")
        .expect("task joins");

    settled(&mut consumer, "the halted invocation's trigger is acked").await;
    assert!(
        state
            .wal
            .get_invocation_state(&halted.to_string())
            .await
            .unwrap()
            .is_none(),
        "the halt landed before the first WAL write"
    );
    assert!(unasked.requests().is_empty(), "nothing ran");
    assert_eq!(
        setup.reclaimed(),
        vec![halted],
        "nothing will resume the halted invocation, so its workspace goes"
    );
    assert_eq!(
        state.claim(&bus, seq).await,
        TriggerClaim::Won,
        "the unfinished claim was given back with the ack"
    );
}

/// **A drain at a later step boundary still suspends the invocation
/// with a row to resume, and its trigger stays acked.** Unchanged by
/// #974, and pinned beside it.
///
/// The model call is the first step's action, so by the time it is
/// parked the first WAL write has landed and the trigger has been acked
/// at its durable start. The drain lands then, and the invocation stops
/// at the next boundary.
#[tokio::test]
async fn a_drain_at_a_later_step_boundary_suspends_with_a_row_to_resume() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let agent = unique_agent_id("drained-mid-run");
    let (_agents, registry) = registry_with(&agent);
    let state = StateDir::new().await;

    let setup = HeldSetup::new(1);
    let runner = state.runner(&bus, "binary-1", Some(setup.clone()));
    let model = ParkedModel::new();
    let dispatcher = state
        .dispatcher(&bus, &registry, runner.clone(), model.clone(), "binary-1")
        .await;
    let (mut consumer, msg) = one_delivery(&bus, &agent).await;
    let seq = msg.info().unwrap().stream_sequence;
    let handling = tokio::spawn(async move { dispatcher.handle(&msg).await });

    wait_until("the first model call", async || {
        model.asked.load(Ordering::SeqCst) == 1
    })
    .await;
    settled(&mut consumer, "the first WAL write acks the trigger").await;
    runner.drain_signal().request();
    model.gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(10), handling)
        .await
        .expect("handle returns once the invocation suspends")
        .expect("task joins");

    let suspended = setup.parked().await;
    let in_flight = scan_in_flight(&state.wal).await.expect("scan WAL");
    assert_eq!(in_flight.len(), 1, "the suspended invocation is in flight");
    assert_eq!(in_flight[0].state.invocation_id, suspended.to_string());
    assert!(
        matches!(
            in_flight[0].category,
            RecoveryCategory::SafeResume | RecoveryCategory::SafeReplay
        ),
        "a drain suspends at a resumable boundary: {:?}",
        in_flight[0].category
    );
    assert!(
        setup.reclaimed().is_empty(),
        "a suspended invocation keeps its workspace for its resume"
    );
    assert_eq!(
        state.claim(&bus, seq).await,
        TriggerClaim::Started {
            invocation_id: suspended.to_string()
        },
        "the claim recorded the durable start"
    );
    let info = consumer.info().await.expect("info").clone();
    assert_eq!(
        (info.num_ack_pending, info.num_pending),
        (0, 0),
        "the trigger stays acked: {info:?}"
    );
}
