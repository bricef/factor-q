//! The two duplicate-invocation incidents of July 2026, replayed against a
//! real broker through the dispatcher's production consume loop
//! (<https://github.com/bricef/factor-q/issues/835>).
//!
//! Both incidents were one trigger becoming several invocations, by two
//! different routes:
//!
//! - **2026-07-06.** The trigger was acked when its invocation
//!   *completed*. An invocation that ran longer than the consumer's ack
//!   window was redelivered while it was still running, and run again.
//! - **2026-07-18.** The gap between a trigger's delivery and its
//!   invocation's first WAL write outgrew the ack window (the fleet was
//!   saturated and the trigger had to queue), so the broker redelivered a
//!   trigger that had not durably started. Nothing checked a redelivered
//!   copy against the first one, and each copy started an invocation as an
//!   earlier one freed a worker permit: one trigger, three pull requests.
//!
//! Two mechanisms on `main` answer them, and each test below belongs to
//! one. The ack at *durable start* (`DurableStart`, issue #41) settles the
//! message while the invocation is still in flight, so the first incident's
//! redelivery never happens. The durable claim
//! (`ControlPlaneStore::claim_trigger`, arbitrated in
//! `control_plane/dispatcher/claim.rs`) refuses any copy of a stream
//! message that is already claimed, so the second incident's redelivery
//! starts nothing.
//!
//! # What is real here, and what is not
//!
//! Real: a private `nats-server` process; the `fq-triggers` stream and the
//! `fq-dispatcher` durable, created by the code under test with its
//! production configuration (so the first-delivery ack window is the real
//! `TRIGGER_RETRY_BACKOFF[0]`, thirty seconds); the dispatcher's own
//! `TriggerDispatcher::run`, wired with a claim store exactly as
//! `fq-daemon/src/hosted.rs` wires it; and the control-plane SQLite store
//! holding the claim rows. **Every redelivery in this file is the broker's
//! own, caused by a real ack deadline passing** — nothing NAKs a message
//! or hands a copy to the dispatcher by hand. That is why each test takes
//! a little over thirty seconds.
//!
//! Stubbed: the worker. [`StagedWorker`] stands in at the `Worker` seam
//! and says when its first WAL write "lands" by firing `DurableStart`,
//! where the real `ReducerRunner` fires it after
//! `upsert_invocation_state`. There is no reducer, no WAL and no model
//! call, so these tests say nothing about *when* a real invocation reaches
//! its first WAL write — only what the dispatcher does on either side of
//! that moment.
//!
//! The unit tests beside the dispatcher (`control_plane/dispatcher.rs`)
//! cover the same two mechanisms one call at a time: they pull a message
//! themselves and pass it to `handle`, and force a redelivery with a NAK.
//! None of them runs the consume loop with a claim store installed. These
//! do, which is the path a redelivery took in July.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use fq_runtime::agent::{Agent, AgentId, AgentRegistry};
use fq_runtime::bus::{
    EventBus, NATS_DEFAULT_MAX_ACK_PENDING, TRIGGER_RETRY_BACKOFF, TRIGGER_STREAM_NAME,
};
use fq_runtime::control_plane::dispatcher::{CONSUMER_NAME, TriggerClaims};
use fq_runtime::control_plane::{ControlPlaneStore, TriggerClaim, TriggerKey};
use fq_runtime::llm::fixture::FixtureClient;
use fq_runtime::llm::{ChatResponse, LlmClient};
use fq_runtime::trigger::{PublishedTrigger, Trigger};
use fq_runtime::worker::{
    DrainRequest, DrainState, DurableStart, ExecutorError, InFlightReport, InvocationOutcome,
    Worker, WorkerStore,
};
use fq_runtime::{DispatcherError, TriggerDispatcher, shared_registry};
use serde_json::json;
use tokio::sync::{Semaphore, oneshot};
use uuid::Uuid;

/// One call to `run_invocation`: which trigger it was for, and the
/// delivery attempt the dispatcher said it was. `attempt: 2` in a
/// transcript preamble was the fingerprint of both incidents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Start {
    trigger: Uuid,
    attempt: Option<u32>,
}

/// A worker whose two interesting moments are in the test's hands: the
/// first WAL write, and the end of the invocation.
struct StagedWorker {
    starts: Mutex<Vec<Start>>,
    /// One permit per invocation allowed to reach its first WAL write.
    first_wal_write: Semaphore,
    /// One permit per invocation allowed to finish.
    finish: Semaphore,
}

impl StagedWorker {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            starts: Mutex::new(Vec::new()),
            first_wal_write: Semaphore::new(0),
            finish: Semaphore::new(0),
        })
    }

    fn starts(&self) -> Vec<Start> {
        self.starts.lock().unwrap().clone()
    }

    async fn wait_for_starts(&self, n: usize, within: Duration) {
        let deadline = Instant::now() + within;
        while self.starts().len() < n {
            assert!(
                Instant::now() < deadline,
                "expected {n} invocation start(s) within {within:?}, saw {:?}",
                self.starts()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

#[async_trait]
impl Worker for StagedWorker {
    async fn run_invocation(
        &self,
        _agent: &Agent,
        _llm: &dyn LlmClient,
        trigger: Trigger,
        delivery_attempt: Option<u32>,
        mut durable_start: DurableStart,
    ) -> Result<InvocationOutcome, ExecutorError> {
        self.starts.lock().unwrap().push(Start {
            trigger: trigger.id,
            attempt: delivery_attempt,
        });
        let invocation_id = durable_start
            .invocation_id()
            .expect("the dispatcher names the invocation on its claim before it starts");
        self.first_wal_write
            .acquire()
            .await
            .expect("the gate is never closed")
            .forget();
        durable_start.fire();
        // An invocation that has just made its first WAL write has work
        // still to do, and the real runner's next move is always an
        // await on the store or the broker. So this stub yields once
        // before it can return. `TriggerDispatcher::handle` used to
        // handle a worker that fires the signal and returns within the
        // same poll as one that never started, releasing its claim row
        // instead of marking it `durably_started` (#966). It now asks
        // the signal once more when the invocation returns, so the
        // yield keeps the stub faithful to the runner and no longer
        // decides the claim row.
        tokio::task::yield_now().await;
        self.finish
            .acquire()
            .await
            .expect("the gate is never closed")
            .forget();
        Ok(InvocationOutcome::Completed {
            invocation_id,
            response: ChatResponse::completed(None),
            cost: 0.0,
            duration_ms: 0,
        })
    }

    async fn request_drain(&self, _req: DrainRequest) {}

    fn drain_status(&self) -> DrainState {
        DrainState::Running
    }
}

/// A broker, a control-plane store, and a dispatcher running on them the
/// way the daemon runs it.
struct World {
    bus: EventBus,
    store: Arc<ControlPlaneStore>,
    worker: Arc<StagedWorker>,
    agent: AgentId,
    /// The test's own handle on the dispatcher's durable. Only ever asked
    /// for `info`: it never pulls, so it cannot take a delivery.
    consumer: async_nats::jetstream::consumer::PullConsumer,
    shutdown: oneshot::Sender<()>,
    run: tokio::task::JoinHandle<Result<(), DispatcherError>>,
    _server: fq_test_support::NatsServer,
    _scratch: tempfile::TempDir,
}

impl World {
    /// Start a dispatcher with `max_concurrent` worker permits on a
    /// private broker, through [`TriggerDispatcher::run`]: the real
    /// durable name, the real filter, the real ack window.
    async fn start(max_concurrent: usize) -> Self {
        let server = fq_test_support::NatsServer::start();
        let bus = EventBus::connect(server.url()).await.expect("connect");
        let scratch = tempfile::tempdir().unwrap();

        let agent_name = "july-replay";
        let definition = scratch.path().join(format!("{agent_name}.md"));
        std::fs::write(
            &definition,
            format!(
                "---\nname: {agent_name}\nmodel: claude-haiku\nbudget: 1.0\n---\n\nTest agent."
            ),
        )
        .unwrap();
        let mut registry = AgentRegistry::new();
        registry.load_file(&definition);
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());

        let store = Arc::new(
            ControlPlaneStore::open(&scratch.path().join("control-plane.db"))
                .await
                .expect("open the control-plane store"),
        );
        // The claims the daemon hands its dispatcher come out of startup
        // recovery over its worker's WAL. The stub worker writes no WAL, so
        // this one is fresh and holds nothing in flight.
        let wal = WorkerStore::open(&scratch.path().join("worker.db"))
            .await
            .expect("open the worker store");
        let in_flight = InFlightReport::scan(&wal)
            .await
            .expect("scan the worker's WAL");
        let claims = TriggerClaims::recover(store.clone(), &in_flight)
            .await
            .expect("recover the trigger claims");
        let worker = StagedWorker::new();
        let dispatcher = TriggerDispatcher::new(
            bus.clone(),
            shared_registry(registry),
            worker.clone(),
            Arc::new(FixtureClient::new()),
            max_concurrent,
        )
        .with_trigger_claims(claims, "replay-daemon");

        // The same durable the dispatcher is about to open, with the same
        // configuration, so opening it here changes nothing about it.
        let consumer = bus
            .trigger_consumer(CONSUMER_NAME, NATS_DEFAULT_MAX_ACK_PENDING)
            .await
            .expect("open the dispatcher's durable");
        let (shutdown, shutdown_rx) = oneshot::channel();
        let run = tokio::spawn(dispatcher.run(shutdown_rx));

        Self {
            bus,
            store,
            worker,
            agent: AgentId::new(agent_name).unwrap(),
            consumer,
            shutdown,
            run,
            _server: server,
            _scratch: scratch,
        }
    }

    async fn publish(&self, label: &str) -> PublishedTrigger {
        self.bus
            .publish_trigger(&self.agent, &json!({ "input": label }))
            .await
            .expect("publish a trigger")
    }

    async fn consumer_info(&mut self) -> async_nats::jetstream::consumer::Info {
        self.consumer.info().await.expect("consumer info").clone()
    }

    /// Wait until the broker holds no delivered-and-unacked trigger.
    async fn wait_until_nothing_is_unacked(&mut self, within: Duration, otherwise: &str) {
        let deadline = Instant::now() + within;
        loop {
            let pending = self.consumer_info().await.num_ack_pending;
            if pending == 0 {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{otherwise} (num_ack_pending={pending} after {within:?})"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// What the claim table says about one stream sequence. Asking is a
    /// claim attempt, so this is only for a sequence that is expected to
    /// have a row already.
    async fn claim_on(&self, stream_seq: u64) -> TriggerClaim {
        self.store
            .claim_trigger(
                TriggerKey {
                    stream: TRIGGER_STREAM_NAME,
                    stream_epoch: self.bus.trigger_stream_epoch(),
                    stream_seq,
                },
                "replay-probe",
                "replay-probe-invocation",
                0,
            )
            .await
            .expect("read the claim")
    }

    /// The dispatcher's two duplicate counts, as
    /// `(duplicate_dropped, held_duplicate_dropped)`: copies dropped after
    /// their run's start was recorded, and before.
    fn drop_counts(&self) -> (u64, u64) {
        let record = self.bus.consumer_ledger().record(CONSUMER_NAME);
        (record.duplicate_dropped, record.held_duplicate_dropped)
    }

    /// Wait until the dispatcher has dropped `held` copies before a start
    /// was recorded.
    async fn wait_for_held_drops(&self, held: u64, within: Duration, otherwise: &str) {
        let deadline = Instant::now() + within;
        while self.drop_counts().1 < held {
            assert!(
                Instant::now() < deadline,
                "{otherwise} (counts {:?} after {within:?})",
                self.drop_counts()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        tokio::time::timeout(Duration::from_secs(10), self.run)
            .await
            .expect("the dispatcher exits")
            .expect("its task joins")
            .expect("and it exits cleanly");
    }
}

/// **2026-07-06: an invocation that outlives the ack window is not
/// redelivered.**
///
/// One trigger, one invocation that reaches its first WAL write at once
/// and then stays in flight for longer than the durable's whole
/// first-delivery window. The ack has to land at the WAL write, while the
/// invocation is still running, for the window to pass without the broker
/// delivering the trigger a second time.
///
/// The claim is installed, as it is in the daemon, and the last assertion
/// is that it had nothing to do: this incident is bounded by *when the ack
/// is sent*. Take the ack out of `mark_durable_started` and this fails at
/// the first assertion, with the message still unacked; the claim would
/// then catch the redelivery thirty seconds later, which is a second line
/// of defence and not the property under test.
#[tokio::test]
async fn an_invocation_that_outlives_the_ack_window_is_not_redelivered() {
    let mut world = World::start(1).await;
    world.worker.first_wal_write.add_permits(1);

    let trigger = world.publish("long-running").await;
    world
        .worker
        .wait_for_starts(1, Duration::from_secs(10))
        .await;

    world
        .wait_until_nothing_is_unacked(
            Duration::from_secs(5),
            "the trigger must be acked at the invocation's first WAL write, while the \
             invocation is still in flight",
        )
        .await;

    tokio::time::sleep(TRIGGER_RETRY_BACKOFF[0] + Duration::from_secs(3)).await;

    let info = world.consumer_info().await;
    assert_eq!(
        (info.delivered.consumer_sequence, info.num_redelivered),
        (1, 0),
        "the broker must have delivered the trigger exactly once across a whole ack window \
         with its invocation still running: {info:?}"
    );
    assert_eq!(
        world.worker.starts(),
        vec![Start {
            trigger: trigger.id,
            attempt: Some(1),
        }],
        "one trigger, one invocation"
    );
    assert_eq!(
        world
            .bus
            .consumer_ledger()
            .record(CONSUMER_NAME)
            .duplicate_dropped,
        0,
        "no copy reached the claim: the ack prevented the redelivery"
    );

    world.worker.finish.add_permits(1);
    world.stop().await;
}

/// **2026-07-18: a trigger redelivered before its first WAL write starts
/// one invocation.**
///
/// The incident's conditions, at the smallest size that has them all: one
/// worker permit, an invocation that takes it and has not reached its
/// first WAL write when the ack window ends, and a second trigger
/// published into that saturation. The broker redelivers the first
/// trigger on its own, because its deadline really has passed.
///
/// In July the redelivered copy queued for a permit like any trigger and
/// started when one came free. So the gates are then opened and everything
/// that is going to start is allowed to: the first trigger finishes, the
/// queued one runs, and a redelivered copy that had been admitted would
/// run third, stamped `attempt: 2`. With the claim it was refused before
/// it could queue, and each trigger starts exactly once.
#[tokio::test]
async fn a_trigger_redelivered_before_its_first_wal_write_starts_once() {
    let mut world = World::start(1).await;

    let stalled = world.publish("stalled before its first WAL write").await;
    world
        .worker
        .wait_for_starts(1, Duration::from_secs(10))
        .await;
    let queued = world.publish("published into saturation").await;

    // The deadline passes and the broker redelivers. Nothing here asks it to.
    let window = TRIGGER_RETRY_BACKOFF[0] + Duration::from_secs(15);
    let deadline = Instant::now() + window;
    loop {
        let info = world.consumer_info().await;
        if info.num_redelivered >= 1 {
            assert!(
                info.delivered.consumer_sequence >= 3,
                "both triggers were delivered, and one of them twice: {info:?}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the broker never redelivered the un-started trigger within {window:?}; the \
             replay needs that redelivery to mean anything: {info:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        world.worker.starts(),
        vec![Start {
            trigger: stalled.id,
            attempt: Some(1),
        }],
        "only the stalled invocation has started: it holds the one worker permit"
    );
    // The redelivered copy reaches the claim and is dropped there, before
    // a start was recorded, and counted as such (#968). The broker has
    // sent it; the consume loop pulls and refuses it within moments.
    world
        .wait_for_held_drops(
            1,
            Duration::from_secs(10),
            "the redelivered copy was not counted as dropped before a start was recorded",
        )
        .await;

    // Let everything that is going to run, run.
    world.worker.first_wal_write.add_permits(8);
    world.worker.finish.add_permits(8);
    world
        .worker
        .wait_for_starts(2, Duration::from_secs(10))
        .await;
    // A redelivered copy that was admitted is parked on the permit the
    // queued trigger is about to give back, so it would start within
    // milliseconds. Hold the assertion open well past that.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        world.worker.starts(),
        vec![
            Start {
                trigger: stalled.id,
                attempt: Some(1),
            },
            Start {
                trigger: queued.id,
                attempt: Some(1),
            },
        ],
        "each trigger starts exactly one invocation, as its first delivery. A third start \
         for the stalled trigger, stamped `attempt: 2`, is the 2026-07-18 incident"
    );

    world
        .wait_until_nothing_is_unacked(
            Duration::from_secs(10),
            "each durable start must settle its trigger, redelivered copies included",
        )
        .await;
    for (name, published) in [("stalled", &stalled), ("queued", &queued)] {
        assert!(
            matches!(
                world.claim_on(published.stream_seq).await,
                TriggerClaim::Started { .. }
            ),
            "the {name} trigger's claim must record its durable start"
        );
    }
    assert_eq!(
        world.drop_counts(),
        (0, 1),
        "one copy refused, before a start was recorded, and none after: the queued trigger \
         waited with its delivery kept alive, and the stalled one's start acked it"
    );

    world.stop().await;
}
