use super::*;
use async_nats::jetstream::AckKind;
use async_nats::jetstream::consumer::PullConsumer;
use fq_runtime::agent::Agent;
use fq_runtime::bus::{NATS_DEFAULT_MAX_ACK_PENDING, TRIGGER_STREAM_NAME};
use fq_runtime::control_plane::dispatcher::CONSUMER_NAME;
use fq_runtime::control_plane::{TriggerClaim, TriggerKey};
use fq_runtime::llm::ChatResponse;
use fq_runtime::llm::fixture::FixtureClient;
use fq_runtime::worker::{
    DrainRequest, DrainState, DurableStart, ExecutorError, InvocationOutcome, InvocationStateRow,
    Worker,
};
use fq_runtime::{DispatcherError, Trigger, TriggerDispatcher, shared_registry};
use futures::StreamExt;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// The #64 idempotency AC: a persistently-failing recovery
/// (re-classified or re-failed on every daemon restart) emits
/// `invocation.ambiguous` exactly once, not once per restart.
#[tokio::test]
async fn publish_ambiguous_once_fires_exactly_once_per_invocation() {
    let server = fq_test_support::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let dir = tempfile::tempdir().unwrap();
    let wstore = fq_runtime::WorkerStore::open(&dir.path().join("worker.db"))
        .await
        .expect("open worker store");

    let inv_id = Uuid::now_v7();
    let agent = format!("amb-once-{}", Uuid::now_v7().simple());
    wstore
        .upsert_invocation_state(&fq_runtime::worker::InvocationStateRow {
            invocation_id: inv_id.to_string(),
            agent_id: agent.clone(),
            schema_version: 1,
            phase: "awaiting_model".to_string(),
            state_blob: b"{}".to_vec(),
            step_index: 0,
            started_at: 1_000,
            updated_at: 1_000,
            terminal_at: None,
            workspace_ref: None,
            archive_status: None,
            archive_published_at: None,
            trigger_source: None,
            trigger_subject: None,
            trigger_payload: None,
            terminal_outcome: None,
            terminal_published_at: None,
        })
        .await
        .unwrap();

    let mut sub = bus
        .subscribe(format!("fq.agent.{agent}.invocation.ambiguous"))
        .await
        .expect("subscribe");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let payload = || fq_runtime::events::InvocationAmbiguousPayload {
        stuck_entity: "recovery".to_string(),
        stuck_call_id: inv_id.to_string(),
        note: "resume failed (test)".to_string(),
    };
    let agent_id = AgentId::new(agent.clone()).unwrap();

    // First failure publishes…
    publish_ambiguous_once(
        &wstore,
        &bus,
        agent_id.clone(),
        inv_id,
        &inv_id.to_string(),
        payload(),
    )
    .await;
    let event = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("invocation.ambiguous within 5s")
        .expect("stream open")
        .expect("event deserialises");
    assert!(matches!(
        event.payload,
        EventPayload::InvocationAmbiguous(_)
    ));

    // …the second (same invocation, "next restart") is stamped out.
    publish_ambiguous_once(
        &wstore,
        &bus,
        agent_id,
        inv_id,
        &inv_id.to_string(),
        payload(),
    )
    .await;
    let quiet = tokio::time::timeout(Duration::from_millis(500), sub.next()).await;
    assert!(
        quiet.is_err(),
        "second failure must not re-publish invocation.ambiguous"
    );
}

// ---- #959: a daemon that dies between an invocation's first WAL write
// and its claim being marked started ----

/// The agent every trigger below is for.
const CRASH_AGENT: &str = "crash-replay";

/// The claimant id of the daemon that died. The next daemon's dispatcher
/// claims under its own runtime id, and a different id is all it takes for
/// it to treat a `claimed` row as a dead process's.
const DEAD_DAEMON: &str = "daemon-that-died";

/// Polls for a condition every 50 ms, at most this many times (ten
/// seconds' worth), with a wall-clock deadline of the same length.
const MAX_POLLS: u32 = 200;
const POLL: Duration = Duration::from_millis(50);

/// An in-flight WAL row as the runner's first `upsert_invocation_state`
/// leaves it: step 0, nothing terminal.
fn first_wal_row(invocation_id: &str) -> InvocationStateRow {
    InvocationStateRow {
        invocation_id: invocation_id.to_string(),
        agent_id: CRASH_AGENT.to_string(),
        schema_version: 1,
        phase: "awaiting_model".to_string(),
        state_blob: b"{}".to_vec(),
        step_index: 0,
        started_at: 1_000,
        updated_at: 1_000,
        terminal_at: None,
        workspace_ref: None,
        archive_status: None,
        archive_published_at: None,
        trigger_source: None,
        trigger_subject: None,
        trigger_payload: None,
        terminal_outcome: None,
        terminal_published_at: None,
    }
}

/// A registry holding [`CRASH_AGENT`], defined in `dir`.
fn crash_agent_registry(dir: &std::path::Path) -> AgentRegistry {
    let definition = dir.join(format!("{CRASH_AGENT}.md"));
    std::fs::write(
        &definition,
        format!("---\nname: {CRASH_AGENT}\nmodel: claude-haiku\nbudget: 1.0\n---\n\nTest agent."),
    )
    .unwrap();
    let mut registry = AgentRegistry::new();
    registry.load_file(&definition);
    assert!(registry.errors().is_empty(), "{:?}", registry.errors());
    registry
}

/// The broker and the two stores a daemon keeps, as a daemon that died
/// holding one trigger left them.
struct AfterTheCrash {
    bus: EventBus,
    cp_store: Arc<ControlPlaneStore>,
    worker_store: Arc<WorkerStore>,
    /// The dispatcher's own durable. The test pulls from it once, as the
    /// daemon that died; after that it only asks it for `info`.
    consumer: PullConsumer,
    /// The delivery the dead daemon never settled.
    delivery: async_nats::jetstream::Message,
    /// That delivery's claim key.
    key: TriggerKey<'static>,
    /// The invocation id the dead daemon named on its claim.
    dead_invocation: Uuid,
    scratch: tempfile::TempDir,
    _server: fq_test_support::NatsServer,
}

/// A daemon pulls a trigger off the dispatcher's durable, claims it under
/// the invocation id it chose, makes that invocation's first WAL write if
/// `reached_first_wal_write`, and dies before anything marks the claim
/// started or acks the message.
///
/// Its writes are the ones the dispatcher and the runner make, in their
/// order and through the same store calls: the dispatcher's per-delivery
/// `handle` is private to the runtime crate. One variable stands for both
/// the id on the claim and the id on the WAL row, and the runtime crate's
/// tests are what make that honest, each by reading the row back:
/// `a_dispatcher_killed_after_the_first_wal_write_leaves_a_claim_naming_the_wal_row`
/// kills the real `handle` after a worker's first WAL write and finds the
/// claim naming the WAL row's id, and
/// `started_claim_acks_duplicate_without_second_invocation` finds the id
/// the worker was handed on the started claim. That the real runner writes
/// its WAL rows under the id it is handed is
/// `a_dispatched_invocation_runs_under_the_id_its_claim_names`. What
/// follows the death is shown here, with the daemon's own recovery and the
/// dispatcher's production consume loop, and against the real binary in
/// `tests/crash_before_claim_mark.rs`.
async fn a_daemon_dies_holding_a_trigger(reached_first_wal_write: bool) -> AfterTheCrash {
    let server = fq_test_support::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let scratch = tempfile::tempdir().unwrap();
    let cp_store = Arc::new(
        ControlPlaneStore::open(&scratch.path().join("control-plane.db"))
            .await
            .expect("open the control-plane store"),
    );
    let worker_store = Arc::new(
        WorkerStore::open(&scratch.path().join("worker.db"))
            .await
            .expect("open the worker store"),
    );

    let consumer = bus
        .trigger_consumer(CONSUMER_NAME, NATS_DEFAULT_MAX_ACK_PENDING)
        .await
        .expect("open the dispatcher's durable");
    let published = bus
        .publish_trigger(
            &AgentId::new(CRASH_AGENT).unwrap(),
            &serde_json::json!({ "input": "the trigger a daemon died holding" }),
        )
        .await
        .expect("publish a trigger");
    // One message, and a pull request that ends with it: nothing the dead
    // daemon opened is left waiting on the durable to take the redelivery.
    let delivery = consumer
        .batch()
        .max_messages(1)
        .expires(Duration::from_secs(5))
        .messages()
        .await
        .expect("pull")
        .next()
        .await
        .expect("the trigger within 5s")
        .expect("message ok");
    let key = TriggerKey {
        stream: TRIGGER_STREAM_NAME,
        stream_epoch: bus.trigger_stream_epoch(),
        stream_seq: published.stream_seq,
    };

    let dead_invocation = Uuid::now_v7();
    assert_eq!(
        cp_store
            .claim_trigger(key, DEAD_DAEMON, &dead_invocation.to_string(), 0)
            .await
            .unwrap(),
        TriggerClaim::Won
    );
    if reached_first_wal_write {
        worker_store
            .upsert_invocation_state(&first_wal_row(&dead_invocation.to_string()))
            .await
            .unwrap();
    }
    // And then it dies: no `mark_trigger_started`, no ack.

    AfterTheCrash {
        bus,
        cp_store,
        worker_store,
        consumer,
        delivery,
        key,
        dead_invocation,
        scratch,
        _server: server,
    }
}

/// The restarted daemon's worker. Records the invocation id each start was
/// told to run under, then reaches its first WAL write the way the real
/// runner does: it fires the durable start and awaits before it can return.
/// (A worker that fires and returns in one poll is ADR-0032's second gap,
/// #966, and not what these tests are about.)
#[derive(Default)]
struct RestartedWorker {
    starts: std::sync::Mutex<Vec<Option<Uuid>>>,
}

impl RestartedWorker {
    fn starts(&self) -> Vec<Option<Uuid>> {
        self.starts.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Worker for RestartedWorker {
    async fn run_invocation(
        &self,
        _agent: &Agent,
        _llm: &dyn LlmClient,
        _trigger: Trigger,
        _delivery_attempt: Option<u32>,
        mut durable_start: DurableStart,
    ) -> Result<InvocationOutcome, ExecutorError> {
        let invocation_id = durable_start.invocation_id();
        self.starts.lock().unwrap().push(invocation_id);
        durable_start.fire();
        tokio::task::yield_now().await;
        Ok(InvocationOutcome::Completed {
            invocation_id: invocation_id.unwrap_or_else(Uuid::now_v7),
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

/// The next daemon, started on what the dead one left.
struct Restarted {
    /// The invocations startup recovery handed back to be resumed.
    recoverable: Vec<String>,
    worker: Arc<RestartedWorker>,
    shutdown: oneshot::Sender<()>,
    run: tokio::task::JoinHandle<Result<(), DispatcherError>>,
}

impl Restarted {
    async fn stop(self) {
        let _ = self.shutdown.send(());
        tokio::time::timeout(Duration::from_secs(10), self.run)
            .await
            .expect("the dispatcher exits")
            .expect("its task joins")
            .expect("and it exits cleanly");
    }
}

/// Start the next daemon in `assemble`'s order: startup recovery under a
/// new runtime id, then the trigger dispatcher on the production durable,
/// claiming under that id with the claims recovery returned, through
/// `TriggerDispatcher::run` as `hosted.rs` runs it. The dead daemon's
/// message is redelivered between the two.
async fn restart(crash: &AfterTheCrash) -> Restarted {
    let runtime_id = Uuid::now_v7();
    let worker_id = WorkerId::new(runtime_id.to_string()).unwrap();
    let Recovered {
        recoverable,
        claims,
        ..
    } = classify_in_flight(
        &crash.worker_store,
        &crash.cp_store,
        &crash.bus,
        runtime_id,
        &worker_id,
    )
    .await
    .expect("startup recovery");

    // A real crash gets this redelivery when the durable's ack window
    // ends. The NAK brings it forward so the test need not wait thirty
    // seconds; the dispatcher cannot tell the two apart.
    crash
        .delivery
        .ack_with(AckKind::Nak(None))
        .await
        .expect("redeliver the dead daemon's trigger");

    let worker = Arc::new(RestartedWorker::default());
    let dispatcher = TriggerDispatcher::new(
        crash.bus.clone(),
        shared_registry(crash_agent_registry(crash.scratch.path())),
        worker.clone(),
        Arc::new(FixtureClient::new()),
        1,
    )
    .with_trigger_claims(claims, worker_id.as_str());
    let (shutdown, shutdown_rx) = oneshot::channel();
    let run = tokio::spawn(dispatcher.run(shutdown_rx));
    Restarted {
        recoverable: recoverable
            .into_iter()
            .map(|c| c.state.invocation_id)
            .collect(),
        worker,
        shutdown,
        run,
    }
}

/// Wait until the broker has delivered the trigger a second time and
/// nothing is unacked: the redelivery has been answered, whichever way.
/// A start is recorded before its durable start acks the message, so a
/// start the redelivery caused has been recorded by the time this returns.
async fn wait_until_the_redelivery_is_settled(consumer: &mut PullConsumer) {
    let deadline = Instant::now() + POLL * MAX_POLLS;
    for _ in 0..MAX_POLLS {
        let info = consumer.info().await.expect("consumer info").clone();
        if info.delivered.consumer_sequence >= 2 && info.num_ack_pending == 0 {
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(POLL).await;
    }
    let info = consumer.info().await.expect("consumer info").clone();
    panic!("the redelivered trigger was not settled in time: {info:?}");
}

/// What the claim table says about the crashed trigger. Asking is a claim
/// attempt, which changes nothing on a row that exists.
async fn claim_on(crash: &AfterTheCrash) -> TriggerClaim {
    crash
        .cp_store
        .claim_trigger(crash.key, "probe", "probe", 0)
        .await
        .expect("read the claim")
}

/// **A daemon that dies between an invocation's first WAL write and its
/// claim being marked started does not get the trigger run twice (#959).**
///
/// It leaves an in-flight WAL row, a claim row still `claimed` under its
/// own id, and an unacked trigger. The next daemon resumes the invocation
/// from the WAL, and the broker redelivers the trigger to it. The claim
/// row used to have nothing to say about the first invocation — the write
/// that would have said it is the one the crash prevented — so the new
/// dispatcher saw a dead daemon's unfinished claim, adopted it, and
/// started a second invocation beside the resumed one.
///
/// The claim row now names the invocation from claim time, and startup
/// recovery, which runs before any dispatcher exists, marks the claim of
/// every invocation it finds in flight. The redelivery meets a
/// `durably_started` row and is acked and dropped.
#[tokio::test]
async fn a_daemon_that_dies_after_the_first_wal_write_runs_the_trigger_once() {
    let mut crash = a_daemon_dies_holding_a_trigger(true).await;
    let restarted = restart(&crash).await;
    assert_eq!(
        restarted.recoverable,
        vec![crash.dead_invocation.to_string()],
        "recovery resumes the invocation the dead daemon started: that is the trigger's one run"
    );

    wait_until_the_redelivery_is_settled(&mut crash.consumer).await;
    // Margin on top of the ordering argument above, for a start that would
    // come late.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        restarted.worker.starts(),
        Vec::<Option<Uuid>>::new(),
        "the redelivered trigger started a second invocation beside the one recovery \
         resumes: one trigger, two runs (#959)"
    );
    assert_eq!(
        claim_on(&crash).await,
        TriggerClaim::Started {
            invocation_id: crash.dead_invocation.to_string()
        },
        "the claim records the durable start of the invocation the dead daemon began"
    );
    assert_eq!(
        crash
            .bus
            .consumer_ledger()
            .record(CONSUMER_NAME)
            .duplicate_dropped,
        1,
        "the redelivery is dropped as a duplicate, and counted"
    );
    restarted.stop().await;
}

/// **A daemon that dies after claiming a trigger and before its first WAL
/// write leaves the trigger to run once, under the id the claim names.**
///
/// Nothing durable was started, so nothing is resumed and recovery marks
/// nothing. The redelivery meets a dead daemon's unfinished claim, which
/// the new dispatcher adopts and starts, under an invocation id of its own
/// that it writes on the row as it adopts it. That is the id the row then
/// records as durably started, so the id on the claim and the id the
/// invocation ran under never part.
#[tokio::test]
async fn a_daemon_that_dies_before_the_first_wal_write_leaves_the_trigger_to_run_once() {
    let mut crash = a_daemon_dies_holding_a_trigger(false).await;
    let restarted = restart(&crash).await;
    assert!(
        restarted.recoverable.is_empty(),
        "nothing durable was started, so nothing is resumed: {:?}",
        restarted.recoverable
    );

    wait_until_the_redelivery_is_settled(&mut crash.consumer).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let starts = restarted.worker.starts();
    assert_eq!(
        starts.len(),
        1,
        "the trigger starts exactly once: {starts:?}"
    );
    let adopted = starts[0].expect("the dispatcher names the invocation it starts");
    assert_ne!(
        adopted, crash.dead_invocation,
        "an adopted claim runs under the adopter's own id, never the dead daemon's"
    );
    assert_eq!(
        claim_on(&crash).await,
        TriggerClaim::Started {
            invocation_id: adopted.to_string()
        },
        "the claim records the durable start under the id the invocation ran under"
    );
    assert_eq!(
        crash
            .bus
            .consumer_ledger()
            .record(CONSUMER_NAME)
            .duplicate_dropped,
        0,
        "the redelivery was the trigger's only run, not a duplicate"
    );
    restarted.stop().await;
}

/// **Startup recovery marks the claim of every invocation it finds in
/// flight, ambiguous ones included, and nothing else (#959).**
///
/// An ambiguous invocation is not resumed: it waits for an operator. It
/// did make its first WAL write, so its trigger has durably started, and a
/// redelivery must not run it again behind the operator's back. A claim
/// whose invocation left no WAL row stays `claimed`, for the redelivery to
/// adopt. An in-flight invocation that no claim names, such as a direct
/// run, is not an error.
#[tokio::test]
async fn startup_recovery_marks_the_claim_of_every_invocation_in_flight() {
    let server = fq_test_support::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let scratch = tempfile::tempdir().unwrap();
    let cp_store = Arc::new(
        ControlPlaneStore::open(&scratch.path().join("control-plane.db"))
            .await
            .expect("open the control-plane store"),
    );
    let worker_store = Arc::new(
        WorkerStore::open(&scratch.path().join("worker.db"))
            .await
            .expect("open the worker store"),
    );
    let key = |stream_seq| TriggerKey {
        stream: TRIGGER_STREAM_NAME,
        stream_epoch: 1,
        stream_seq,
    };

    let resumable = Uuid::now_v7().to_string();
    let ambiguous = Uuid::now_v7().to_string();
    let never_started = Uuid::now_v7().to_string();
    let unclaimed = Uuid::now_v7().to_string();
    for (stream_seq, invocation) in [(1, &resumable), (2, &ambiguous), (3, &never_started)] {
        cp_store
            .claim_trigger(key(stream_seq), DEAD_DAEMON, invocation, 0)
            .await
            .unwrap();
    }
    for invocation in [&resumable, &ambiguous, &unclaimed] {
        worker_store
            .upsert_invocation_state(&first_wal_row(invocation))
            .await
            .unwrap();
    }
    // A model call that went out and never came back: ambiguous.
    worker_store
        .write_llm_intent(
            &ambiguous,
            "request-1",
            "claude-haiku",
            "{}",
            &fq_runtime::events::LlmCallOrigin::AgentTurn,
            1_000,
        )
        .await
        .unwrap();
    worker_store
        .write_llm_dispatched(&ambiguous, "request-1", 1_001)
        .await
        .unwrap();

    let runtime_id = Uuid::now_v7();
    let Recovered {
        recoverable,
        in_flight_ids: in_flight,
        ..
    } = classify_in_flight(
        &worker_store,
        &cp_store,
        &bus,
        runtime_id,
        &WorkerId::new(runtime_id.to_string()).unwrap(),
    )
    .await
    .expect("startup recovery");
    assert!(
        !recoverable
            .iter()
            .any(|c| c.state.invocation_id == ambiguous),
        "the ambiguous invocation is held for an operator, not resumed"
    );
    assert!(in_flight.contains(&ambiguous));

    let probe = |stream_seq| cp_store.claim_trigger(key(stream_seq), "probe", "probe", 0);
    assert_eq!(
        probe(1).await.unwrap(),
        TriggerClaim::Started {
            invocation_id: resumable
        },
        "a resumed invocation's trigger has durably started"
    );
    assert_eq!(
        probe(2).await.unwrap(),
        TriggerClaim::Started {
            invocation_id: ambiguous
        },
        "so has an ambiguous one's, which a redelivery must not run again"
    );
    assert_eq!(
        probe(3).await.unwrap(),
        TriggerClaim::Held {
            claimant: DEAD_DAEMON.to_string()
        },
        "a claim whose invocation never reached the WAL is left for the redelivery to adopt"
    );
}
