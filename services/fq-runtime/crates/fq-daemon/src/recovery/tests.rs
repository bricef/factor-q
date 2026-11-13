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
/// (A worker that fires and returns in one poll was ADR-0032's second gap,
/// #966, which `handle` now treats as the durable start it is; these tests
/// are not about it.)
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
            claimant: DEAD_DAEMON.to_string(),
            invocation_id: Some(never_started.clone()),
            claimed_at: 0,
        },
        "a claim whose invocation never reached the WAL is left for the redelivery to adopt"
    );
}

// ---- #994: recovery's resumes are admitted like dispatched work ----

/// An agent declaring `max_concurrent: 1`, granted the one tool a crashed
/// invocation of it was calling (see [`seed_a_crashed_tool_call`]).
const CAPPED_AGENT: &str = "capped-recovery";

/// A registry holding [`CAPPED_AGENT`], defined in `dir`.
fn capped_agent_registry(dir: &std::path::Path) -> AgentRegistry {
    let definition = dir.join(format!("{CAPPED_AGENT}.md"));
    std::fs::write(
        &definition,
        format!(
            "---\nname: {CAPPED_AGENT}\nmodel: claude-haiku\nbudget: 1.0\nmax_concurrent: 1\n\
             tools:\n  - builtin__self_inspect\n---\n\nTest agent."
        ),
    )
    .unwrap();
    let mut registry = AgentRegistry::new();
    registry.load_file(&definition);
    assert!(registry.errors().is_empty(), "{:?}", registry.errors());
    registry
}

/// The model a resumed run asks: its first call is held until the test
/// lets it return, and every call then declares the invocation done.
struct HeldModel {
    entered: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
    calls: std::sync::atomic::AtomicUsize,
}

impl HeldModel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
            calls: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Wait until a run is inside its first model call.
    async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.entered.acquire())
            .await
            .expect("the resumed run reached its model call")
            .expect("entry semaphore open")
            .forget();
    }

    fn release(&self) {
        self.release.add_permits(1);
    }
}

#[async_trait::async_trait]
impl LlmClient for HeldModel {
    async fn chat(
        &self,
        _request: fq_runtime::ChatRequest,
    ) -> Result<ChatResponse, fq_runtime::LlmError> {
        use std::sync::atomic::Ordering::SeqCst;
        if self.calls.fetch_add(1, SeqCst) == 0 {
            self.entered.add_permits(1);
            self.release
                .acquire()
                .await
                .expect("release semaphore open")
                .forget();
        }
        Ok(ChatResponse {
            parts: fq_runtime::events::assistant_parts(
                None,
                vec![fq_runtime::events::MessageToolCall {
                    tool_call_id: fq_runtime::events::ToolCallId::new("report-outcome").unwrap(),
                    tool_name: fq_runtime::tools::REPORT_OUTCOME_CANONICAL_NAME.to_string(),
                    parameters: serde_json::json!({"status": "success", "summary": "done"}),
                }],
            ),
            stop_reason: fq_runtime::events::StopReason::ToolUse,
            usage: fq_runtime::events::TokenUsage {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cache_write_5m_tokens: None,
                cache_write_1h_tokens: None,
                reasoning_tokens: None,
            },
            reported_cost_usd: None,
        })
    }
}

/// What the daemon that stopped left in flight.
#[derive(Clone, Copy, Debug)]
enum Seed {
    /// This many invocations of [`CAPPED_AGENT`], each a safe resume, in
    /// the order the scan finds them.
    Resumable(usize),
    /// One invocation killed mid-tool-call: ambiguous, for an operator.
    Ambiguous,
}

/// What a daemon killed mid-tool-call leaves in the WAL: a model turn that
/// asked for a tool, and the tool handed off with no result.
async fn seed_a_crashed_tool_call(store: &WorkerStore, invocation: &str) {
    let tool_use = ChatResponse {
        parts: fq_runtime::events::assistant_parts(
            None,
            vec![fq_runtime::events::MessageToolCall {
                tool_call_id: fq_runtime::events::ToolCallId::new("tc-0").unwrap(),
                tool_name: "builtin__self_inspect".to_string(),
                parameters: serde_json::json!({}),
            }],
        ),
        stop_reason: fq_runtime::events::StopReason::ToolUse,
        usage: fq_runtime::events::TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cache_write_5m_tokens: None,
            cache_write_1h_tokens: None,
            reasoning_tokens: None,
        },
        reported_cost_usd: None,
    };
    let origin = fq_runtime::events::LlmCallOrigin::AgentTurn;
    store
        .write_llm_intent(invocation, "req-0", "claude-haiku", "{}", &origin, 1_001)
        .await
        .unwrap();
    store
        .write_llm_dispatched(invocation, "req-0", 1_002)
        .await
        .unwrap();
    let response = serde_json::to_string(&tool_use).unwrap();
    store
        .write_llm_completed(invocation, "req-0", &response, false, 0.0001, 1_003)
        .await
        .unwrap();
    store
        .write_tool_intent(invocation, "tc-0", "builtin__self_inspect", "{}", 1_004)
        .await
        .unwrap();
    store
        .write_tool_dispatched(invocation, "tc-0", 1_005)
        .await
        .unwrap();
}

/// Issue `op` through the operator surface exactly as the edge dispatches
/// it: the declared op name, the wire input shape.
async fn invoke(
    surface: &fq_edge::EdgeRegistry,
    op: fq_ops::Invocation,
    input: serde_json::Value,
) -> Result<serde_json::Value, fq_edge::wire::WireError> {
    let op = fq_ops::OpId::Verb(fq_ops::VerbId::Invocation(op));
    let handler = surface
        .handler(&op.to_string())
        .expect("the op is registered on the operator surface");
    handler(input).await
}

/// The next daemon's start over what a daemon that stopped with
/// invocations of [`CAPPED_AGENT`] in flight left: its stores, its runner,
/// and what startup recovery made of them.
struct NextStart {
    bus: EventBus,
    cp_store: Arc<ControlPlaneStore>,
    worker_store: Arc<WorkerStore>,
    runner: Arc<fq_runtime::ReducerRunner<fq_runtime::Harness>>,
    model: Arc<HeldModel>,
    worker_id: WorkerId,
    /// The invocation in flight — the first, in scan order.
    invocation: Uuid,
    /// Every invocation in flight, in scan order.
    invocations: Vec<Uuid>,
    /// What recovery hands the resume path, taken once.
    recoverable: Vec<ClassifiedInvocation>,
    /// What recovery hands the dispatcher, taken once.
    claims: Option<TriggerClaims>,
    scratch: tempfile::TempDir,
    _server: fq_test_support::NatsServer,
}

impl NextStart {
    async fn over_one_run_in_flight() -> Self {
        Self::over(Seed::Resumable(1)).await
    }

    async fn over(seed: Seed) -> Self {
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
        let count = match seed {
            Seed::Resumable(n) => n,
            Seed::Ambiguous => 1,
        };
        let invocations: Vec<Uuid> = (0..count).map(|_| Uuid::now_v7()).collect();
        for (n, invocation) in invocations.iter().enumerate() {
            let mut row = first_wal_row(&invocation.to_string());
            row.agent_id = CAPPED_AGENT.to_string();
            // The scan orders by start, so this is the order it finds them.
            row.started_at += n as i64;
            worker_store.upsert_invocation_state(&row).await.unwrap();
        }
        let invocation = invocations[0];
        if let Seed::Ambiguous = seed {
            seed_a_crashed_tool_call(&worker_store, &invocation.to_string()).await;
            // What the coordination consumer folds from the
            // `invocation.ambiguous` recovery publishes below; no consumer
            // runs in these tests.
            cp_store
                .upsert_invocation_ownership(
                    &invocation.to_string(),
                    DEAD_DAEMON,
                    1_000,
                    fq_runtime::control_plane::OwnerStatus::Ambiguous,
                )
                .await
                .unwrap();
        }

        let runtime_id = Uuid::now_v7();
        let worker_id = WorkerId::new(runtime_id.to_string()).unwrap();
        let Recovered {
            recoverable,
            claims,
            ..
        } = classify_in_flight(&worker_store, &cp_store, &bus, runtime_id, &worker_id)
            .await
            .expect("startup recovery");
        let recovered: Vec<String> = recoverable
            .iter()
            .map(|c| c.state.invocation_id.clone())
            .collect();
        match seed {
            Seed::Resumable(_) => assert_eq!(
                recovered,
                invocations.iter().map(Uuid::to_string).collect::<Vec<_>>(),
                "every invocation in flight is recovered as a safe resume, in start order"
            ),
            Seed::Ambiguous => assert!(
                recovered.is_empty(),
                "a crashed tool call is ambiguous: held for an operator, not resumed"
            ),
        }

        let mut pricing = std::collections::HashMap::new();
        pricing.insert(
            "claude-haiku".to_string(),
            fq_runtime::ModelPricing {
                input_per_million: 1.0,
                output_per_million: 5.0,
                cache_read_per_million: None,
                cache_write_per_million: None,
                cache_write_1h_per_million: None,
            },
        );
        let runner = Arc::new(fq_runtime::ReducerRunner::new(
            Arc::new(
                fq_runtime::ReducerContext::builder()
                    .tools(Arc::new(fq_runtime::ToolRegistry::with_builtins()))
                    .build(),
            ),
            Arc::new(
                fq_runtime::RunnerConfig::builder()
                    .bus(bus.clone())
                    .pricing(Arc::new(fq_runtime::PricingTable::from_map(pricing)))
                    .store(worker_store.clone())
                    .worker_id(worker_id.clone())
                    .build(),
            ),
            fq_runtime::Harness::new(),
        ));
        Self {
            bus,
            cp_store,
            worker_store,
            runner,
            model: HeldModel::new(),
            worker_id,
            invocation,
            invocations,
            recoverable,
            claims: Some(claims),
            scratch,
            _server: server,
        }
    }

    /// The daemon's operator surface over these stores and this runner, as
    /// `run_hosted` assembles it, sharing `admission`. No consumer folds
    /// anything here, so a drop's WAL mark is never written while a test
    /// runs: the coordination consumer lagging, held for as long as it
    /// takes rather than raced.
    async fn operator_surface(&self, admission: &RunAdmission) -> fq_edge::EdgeRegistry {
        let paths = fq_runtime::db::RuntimeDbPaths::under(self.scratch.path());
        let projection = Arc::new(
            fq_runtime::control_plane::projection::store::ProjectionStore::open(&paths.projection)
                .await
                .expect("open the projection store"),
        );
        let views = Arc::new(
            fq_runtime::views::Views::open(&paths)
                .await
                .expect("open the views"),
        );
        let agents = shared_registry(capped_agent_registry(self.scratch.path()));
        let llm: Arc<dyn LlmClient> = self.model.clone();
        let resume = Arc::new(crate::resume::ResumeControl::new(
            self.bus.clone(),
            self.worker_store.clone(),
            self.cp_store.clone(),
            self.runner.clone(),
            agents.clone(),
            llm,
            fq_runtime::worker::DeferralQueue::new().0,
            admission.clone(),
        ));
        let (_watermark_tx, watermark) = fq_runtime::watermark::channel();
        crate::operator_surface::operator_registry(
            views,
            fq_runtime::watermark::Horizon::new(vec![watermark]),
            Duration::from_millis(1),
            crate::operator_surface::OperatorDeps {
                facts: crate::operator_surface::DaemonFacts {
                    db_paths: Arc::new(paths.clone()),
                    legacy_events_db: Arc::new(fq_runtime::db::legacy_db_path(self.scratch.path())),
                    drain_deadline_ms: 180_000,
                    stuck_after_ms: 4_210_000,
                    enabled_consumers: Default::default(),
                    mcp_servers: Default::default(),
                    throttle: Arc::new(fq_runtime::llm::ModelThrottle::inert()),
                    agent_caps: admission.agents().clone(),
                    worker_id: self.worker_id.as_str().to_string(),
                    dispatching_since_ms: 0,
                    trigger_claim_retention_ms: None,
                    archive_holds: Default::default(),
                },
                bus: self.bus.clone(),
                projection,
                control_plane: self.cp_store.clone(),
                runner: self.runner.clone(),
                resume,
                agents: agents.clone(),
                machinery: crate::control_commands::MachineryDeps {
                    agents,
                    agents_dir: self.scratch.path().to_path_buf(),
                    default_model: None,
                    worker: self.runner.clone(),
                    down: crate::control_commands::DownSignal::new(),
                    projection_rebuild:
                        fq_runtime::control_plane::projection::ProjectionRebuildHandle::detached(),
                },
            },
        )
        .expect("assemble the operator surface")
    }

    /// Hand the recovered run to the real resume path, as `assemble` does.
    async fn spawn_resumes(&mut self, admission: &RunAdmission) -> ResumeTasks {
        let llm: Arc<dyn LlmClient> = self.model.clone();
        spawn_resume_tasks(
            std::mem::take(&mut self.recoverable),
            &capped_agent_registry(self.scratch.path()),
            &self.runner,
            &llm,
            &self.bus,
            &self.worker_store,
            &fq_runtime::worker::DeferralQueue::new().0,
            admission,
        )
        .await
    }

    /// The dispatcher the daemon starts after recovery, sharing
    /// `admission`, over a stand-in worker that records what it starts.
    fn start_dispatcher(
        &mut self,
        admission: &RunAdmission,
        worker: Arc<RestartedWorker>,
    ) -> (
        oneshot::Sender<()>,
        tokio::task::JoinHandle<Result<(), DispatcherError>>,
    ) {
        let dispatcher = TriggerDispatcher::new(
            self.bus.clone(),
            shared_registry(capped_agent_registry(self.scratch.path())),
            worker,
            Arc::new(FixtureClient::new()),
            admission.workers().max_concurrent(),
        )
        .with_caps_and_claims(
            admission.clone(),
            self.claims.take().expect("one dispatcher per start"),
            self.worker_id.as_str(),
        );
        let (shutdown, shutdown_rx) = oneshot::channel();
        (shutdown, tokio::spawn(dispatcher.run(shutdown_rx)))
    }

    /// The invocation's WAL row, which nothing has written since the
    /// daemon stopped, and which the next start must find in flight again.
    async fn assert_left_for_the_next_start(&self) {
        let row = self
            .worker_store
            .get_invocation_state(&self.invocation.to_string())
            .await
            .unwrap()
            .expect("the WAL row is still there");
        assert_eq!(row.terminal_at, None, "not ended");
        assert_eq!(row.updated_at, 1_000, "not written: nothing resumed it");
        let runtime_id = Uuid::now_v7();
        let again = classify_in_flight(
            &self.worker_store,
            &self.cp_store,
            &self.bus,
            runtime_id,
            &WorkerId::new(runtime_id.to_string()).unwrap(),
        )
        .await
        .expect("the next start's recovery");
        assert_eq!(
            again
                .recoverable
                .iter()
                .map(|c| c.state.invocation_id.clone())
                .collect::<Vec<_>>(),
            [self.invocation.to_string()],
            "the next start resumes it"
        );
    }
}

/// Take the worker permit, at a cap of 1 the only one, for a run this test
/// stands in for.
async fn hold_the_only_permit(admission: &RunAdmission) -> impl Sized + use<> {
    admission
        .workers()
        .queue(Waiter::Trigger(None))
        .await
        .permit()
        .await
        .expect("the permit is free at start")
}

/// How the daemon stops while a recovered run waits.
#[derive(Clone, Copy, Debug)]
enum Stop {
    /// `fq down` or SIGTERM: the runner drains.
    Drain,
    /// Ctrl-C, `fq down --now`, or a failed task: the dispatcher's loop
    /// sees its shutdown signal, with no drain.
    Shutdown,
}

/// **#994: a recovered run waits for a worker permit, and a stop ends the
/// wait cleanly.** At a worker cap of 1 with the permit held, the run
/// startup recovery found is queued, not resumed — while counting against
/// its agent — and a drain or a shutdown ends its task within a keepalive
/// tick, gives its agent's slot back and leaves its WAL row exactly as it
/// was, for the next start to resume.
async fn a_waiting_recovered_run_lets_go_on(stop: Stop) {
    let mut start = NextStart::over_one_run_in_flight().await;
    let admission = RunAdmission::new(start.runner.clone(), 1);
    let (shutdown, dispatcher) =
        start.start_dispatcher(&admission, Arc::new(RestartedWorker::default()));
    // The worker cap is full: a run this test stands in for holds the
    // only permit.
    let running = hold_the_only_permit(&admission).await;
    let mut handles = start.spawn_resumes(&admission).await.handles;
    let resume = handles.pop().expect("one resume task");

    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        !resume.is_finished() && !start.runner.is_active(&start.invocation),
        "the recovered run waits for the only permit rather than running beside its holder"
    );
    assert_eq!(start.model.calls(), 0, "and has not asked the model");
    let agent = AgentId::new(CAPPED_AGENT).unwrap();
    assert_eq!(
        admission.agents().in_flight(&agent),
        1,
        "it counts against its agent while it waits"
    );

    let mut shutdown = Some(shutdown);
    match stop {
        Stop::Drain => {
            start
                .runner
                .request_drain(DrainRequest::new(fq_runtime::worker::DrainReason::Deploy))
                .await;
        }
        Stop::Shutdown => {
            let _ = shutdown.take().expect("not sent yet").send(());
        }
    }
    tokio::time::timeout(Duration::from_secs(2), resume)
        .await
        .unwrap_or_else(|_| panic!("a {stop:?} ends the wait within a keepalive tick"))
        .expect("the resume task joins");
    assert_eq!(start.model.calls(), 0, "it never ran");
    assert_eq!(
        admission.agents().in_flight(&agent),
        0,
        "its agent's slot is given back"
    );
    start.assert_left_for_the_next_start().await;

    drop(running);
    // The stand-in worker the dispatcher runs on never drains — the
    // runner the admission asks is the one that did — so a drained run
    // still sends the dispatcher its shutdown here.
    if let Some(shutdown) = shutdown {
        let _ = shutdown.send(());
    }
    tokio::time::timeout(Duration::from_secs(10), dispatcher)
        .await
        .expect("the dispatcher exits")
        .expect("its task joins")
        .expect("and it exits cleanly");
}

#[tokio::test]
async fn a_recovered_run_waiting_for_a_permit_lets_go_on_a_drain() {
    a_waiting_recovered_run_lets_go_on(Stop::Drain).await;
}

#[tokio::test]
async fn a_recovered_run_waiting_for_a_permit_lets_go_on_a_shutdown() {
    a_waiting_recovered_run_lets_go_on(Stop::Shutdown).await;
}

/// **#994: a recovered run whose row turns terminal while it waits is not
/// reported ambiguous.** An `fq invocation drop` of a queued run does not
/// get this far — the drop holds the run, and it never starts (see
/// `a_queued_recovered_run_dropped_by_an_operator_never_starts`) — but the
/// row can still be ended by another route while the run waits, as the
/// coordination consumer does here, and the resume then finds it ended.
/// Reporting that as `invocation.ambiguous` would flip the ended
/// invocation's owner row back to awaiting triage.
///
/// (A row the archive hand-off has already deleted is not reported
/// either, but not because of this: `publish_ambiguous_once` finds no row
/// to stamp and publishes nothing.)
#[tokio::test]
async fn a_recovered_run_ended_while_it_waits_is_not_reported_ambiguous() {
    let mut start = NextStart::over_one_run_in_flight().await;
    let admission = RunAdmission::new(start.runner.clone(), 1);
    let running = hold_the_only_permit(&admission).await;
    let mut ambiguous = start
        .bus
        .subscribe(format!("fq.agent.{CAPPED_AGENT}.invocation.ambiguous"))
        .await
        .expect("subscribe");
    let mut resumes = start.spawn_resumes(&admission).await.handles;

    start
        .worker_store
        .mark_invocation_operator_terminal(&start.invocation.to_string(), "failed", 2_000)
        .await
        .unwrap();
    drop(running);
    tokio::time::timeout(Duration::from_secs(10), resumes.pop().expect("one resume"))
        .await
        .expect("the resume of a dropped invocation ends")
        .expect("its task joins");
    assert_eq!(start.model.calls(), 0, "nothing ran");
    let quiet = tokio::time::timeout(Duration::from_millis(750), ambiguous.next()).await;
    assert!(
        quiet.is_err(),
        "a dropped invocation was reported ambiguous: {quiet:?}"
    );
}

/// **#994 with #718: the per-agent cap bounds a recovered run plus a
/// dispatched one.** Two worker permits, so the only thing that can hold a
/// trigger for [`CAPPED_AGENT`] (`max_concurrent: 1`) while its recovered
/// run is in flight is that cap. The trigger is held — counted as waiting,
/// with a worker permit free — until the recovered run ends, and then
/// starts.
#[tokio::test]
async fn a_recovered_run_and_a_trigger_for_its_agent_do_not_run_at_once() {
    let mut start = NextStart::over_one_run_in_flight().await;
    let admission = RunAdmission::new(start.runner.clone(), 2);
    let mut resumes = start.spawn_resumes(&admission).await.handles;
    start.model.entered().await;
    assert!(
        start.runner.is_active(&start.invocation),
        "the recovered run is resumed and inside its model call"
    );

    let dispatched = Arc::new(RestartedWorker::default());
    let (shutdown, dispatcher) = start.start_dispatcher(&admission, dispatched.clone());
    let agent = AgentId::new(CAPPED_AGENT).unwrap();
    start
        .bus
        .publish_trigger(
            &agent,
            &serde_json::json!({ "input": "beside the recovered run" }),
        )
        .await
        .expect("publish a trigger");
    let deadline = Instant::now() + POLL * MAX_POLLS;
    while !admission
        .agents()
        .snapshot()
        .iter()
        .any(|a| a.agent == CAPPED_AGENT && a.held == 1)
    {
        assert!(
            Instant::now() < deadline,
            "the trigger was never held at its agent's cap: {:?}",
            admission.agents().snapshot()
        );
        tokio::time::sleep(POLL).await;
    }
    tokio::time::sleep(Duration::from_millis(750)).await;
    assert!(
        dispatched.starts().is_empty(),
        "a trigger for the recovered run's agent started beside it, past `max_concurrent: 1`"
    );
    assert_eq!(
        admission.workers().in_flight(),
        1,
        "with a worker permit free: the agent's cap is what holds it"
    );

    start.model.release();
    tokio::time::timeout(Duration::from_secs(10), resumes.pop().expect("one resume"))
        .await
        .expect("the recovered run finishes once its model call returns")
        .expect("its task joins");
    let deadline = Instant::now() + POLL * MAX_POLLS;
    while dispatched.starts().is_empty() {
        assert!(
            Instant::now() < deadline,
            "the held trigger never started after the recovered run ended"
        );
        tokio::time::sleep(POLL).await;
    }
    assert_eq!(dispatched.starts().len(), 1);

    let _ = shutdown.send(());
    tokio::time::timeout(Duration::from_secs(10), dispatcher)
        .await
        .expect("the dispatcher exits")
        .expect("its task joins")
        .expect("and it exits cleanly");
}

/// **More recovered runs than permits run in turn, in scan order.** Two
/// invocations in flight at a worker cap of 1: the first the scan found
/// is resumed and holds the permit through its model call, the second
/// waits, and runs once the first has finished.
#[tokio::test]
async fn more_recovered_runs_than_permits_run_in_turn_in_scan_order() {
    let mut start = NextStart::over(Seed::Resumable(2)).await;
    let admission = RunAdmission::new(start.runner.clone(), 1);
    let (first, second) = (start.invocations[0], start.invocations[1]);
    let resumes = start.spawn_resumes(&admission).await;
    assert_eq!(
        resumes.waiting, 1,
        "the boot banner's count: one of the two waits for the only permit"
    );
    let resumes = resumes.handles;
    start.model.entered().await;
    assert!(
        start.runner.is_active(&first),
        "the first the scan found is the one running"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !start.runner.is_active(&second),
        "the second waits for the only permit"
    );
    assert_eq!(start.model.calls(), 1, "so one model call is open, not two");

    start.model.release();
    for resume in resumes {
        tokio::time::timeout(Duration::from_secs(10), resume)
            .await
            .expect("each recovered run finishes")
            .expect("its task joins");
    }
    assert_eq!(
        start.model.calls(),
        2,
        "the second ran once the first had given the permit back"
    );
}

// ---- #994: an operator's drop and resume of queued runs, through the
// daemon's own operator surface ----

/// The agent every run here belongs to.
fn capped_agent() -> AgentId {
    AgentId::new(CAPPED_AGENT).unwrap()
}

/// Wait until `agent`'s in-flight count is `n`: how a test sees a
/// detached resume task end, since the count is released when it does.
async fn await_in_flight(admission: &RunAdmission, n: u32) {
    let deadline = Instant::now() + POLL * MAX_POLLS;
    while admission.agents().in_flight(&capped_agent()) != n {
        assert!(
            Instant::now() < deadline,
            "{} in flight, waiting for {n}",
            admission.agents().in_flight(&capped_agent())
        );
        tokio::time::sleep(POLL).await;
    }
}

/// **A drop of a queued recovered run takes effect at once.** The run is
/// waiting for the only worker permit, so the runner is not driving it and
/// a plain `fq invocation drop` takes it. No coordination consumer runs
/// here, so its WAL row is never marked terminal — the consumer lagging,
/// held — and the permit is then freed. The run must not start: not ask the
/// model, not be driven, not keep its agent's slot.
///
/// Before the drop could reach a queued run, the freed permit started it,
/// and it ran to its model call regardless of the receipt.
#[tokio::test]
async fn a_queued_recovered_run_dropped_by_an_operator_never_starts() {
    let mut start = NextStart::over_one_run_in_flight().await;
    let admission = RunAdmission::new(start.runner.clone(), 1);
    let surface = start.operator_surface(&admission).await;
    let running = hold_the_only_permit(&admission).await;
    let resume = start
        .spawn_resumes(&admission)
        .await
        .handles
        .pop()
        .expect("one resume task");

    let dropped = invoke(
        &surface,
        fq_ops::Invocation::Drop,
        serde_json::json!({ "invocation_id": start.invocation.to_string(), "reason": "test" }),
    )
    .await;
    assert!(
        dropped.is_ok(),
        "a queued run is not live, so a plain drop takes it: {dropped:?}"
    );
    drop(running);
    tokio::time::timeout(Duration::from_secs(5), resume)
        .await
        .expect("the dropped run's task ends without starting it")
        .expect("its task joins");
    assert_eq!(start.model.calls(), 0, "a dropped run never asks the model");
    assert!(!start.runner.is_active(&start.invocation));
    assert_eq!(admission.agents().in_flight(&capped_agent()), 0);
}

/// **A drop of a queued operator resume takes effect at once**, the same
/// way: `fq invocation resume` records the interrupted result and queues
/// the re-drive behind the run holding the only permit; a plain drop takes
/// it; the permit is freed; the re-drive never starts.
#[tokio::test]
async fn a_queued_operator_resume_dropped_by_an_operator_never_starts() {
    let start = NextStart::over(Seed::Ambiguous).await;
    let admission = RunAdmission::new(start.runner.clone(), 1);
    let surface = start.operator_surface(&admission).await;
    let running = hold_the_only_permit(&admission).await;
    let id = start.invocation.to_string();

    let resumed = invoke(
        &surface,
        fq_ops::Invocation::Resume,
        serde_json::json!({ "invocation_id": id, "reason": "test" }),
    )
    .await;
    assert!(resumed.is_ok(), "the resume is accepted: {resumed:?}");
    assert_eq!(
        admission.agents().in_flight(&capped_agent()),
        1,
        "and its re-drive is queued, counted against its agent"
    );
    let dropped = invoke(
        &surface,
        fq_ops::Invocation::Drop,
        serde_json::json!({ "invocation_id": id, "reason": "test" }),
    )
    .await;
    assert!(
        dropped.is_ok(),
        "a queued re-drive is not live, so a plain drop takes it: {dropped:?}"
    );
    drop(running);
    await_in_flight(&admission, 0).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        start.model.calls(),
        0,
        "a dropped re-drive never asks the model"
    );
    assert!(!start.runner.is_active(&start.invocation));
}

/// **`fq invocation resume` on a daemon already draining** is refused,
/// saying so, after the interrupted result is recorded: the re-drive does
/// not start, the slot it entered is given back at once, and the
/// invocation is no longer ambiguous, so the next start's recovery resumes
/// it on its own.
#[tokio::test]
async fn an_operator_resume_on_a_draining_daemon_is_refused_and_left_to_the_next_start() {
    let start = NextStart::over(Seed::Ambiguous).await;
    let admission = RunAdmission::new(start.runner.clone(), 1);
    let surface = start.operator_surface(&admission).await;
    let id = start.invocation.to_string();
    start
        .runner
        .request_drain(DrainRequest::new(fq_runtime::worker::DrainReason::Deploy))
        .await;

    let refused = invoke(
        &surface,
        fq_ops::Invocation::Resume,
        serde_json::json!({ "invocation_id": id, "reason": "test" }),
    )
    .await;
    let Err(fq_edge::wire::WireError::InvalidInput { message, .. }) = refused else {
        panic!("a draining daemon refuses the re-drive: {refused:?}");
    };
    assert!(
        message.contains("draining or stopping") && message.contains("next start"),
        "the refusal says why, and what happens next: {message}"
    );
    let tool = start
        .worker_store
        .get_tool_dispatch(&id, "tc-0")
        .await
        .unwrap()
        .expect("the tool row");
    assert_eq!(
        tool.status,
        fq_runtime::worker::DispatchStatus::Completed,
        "the interrupted result is recorded"
    );
    assert!(
        tool.result.unwrap_or_default().contains("interrupted"),
        "as an interrupted result"
    );
    assert_eq!(
        admission.agents().in_flight(&capped_agent()),
        0,
        "the slot it entered is given back with the refusal"
    );
    assert_eq!(start.model.calls(), 0, "nothing ran");
    let runtime_id = Uuid::now_v7();
    let next = classify_in_flight(
        &start.worker_store,
        &start.cp_store,
        &start.bus,
        runtime_id,
        &WorkerId::new(runtime_id.to_string()).unwrap(),
    )
    .await
    .expect("the next start's recovery");
    assert_eq!(
        next.recoverable
            .iter()
            .map(|c| c.state.invocation_id.clone())
            .collect::<Vec<_>>(),
        [id],
        "no longer ambiguous: the next start resumes it"
    );
}

// An operator's resume is counted against its agent's `max_concurrent` but
// not held at it (issue 1009), on the same next start as the tests above.
mod operator_resume_cap;

/// Subscribe to every `invocation.operator_recovered`, whichever agent a
/// drop publishes it under.
async fn drops_published(
    bus: &EventBus,
) -> std::pin::Pin<
    Box<
        dyn futures::Stream<Item = Result<fq_runtime::events::Event, fq_runtime::bus::BusError>>
            + Send,
    >,
> {
    let drops = bus
        .subscribe("fq.agent.*.invocation.operator_recovered".to_string())
        .await
        .expect("subscribe");
    tokio::time::sleep(Duration::from_millis(50)).await;
    drops
}

/// A drop refused with `InvalidInput`, and its message.
fn refusal(answer: Result<serde_json::Value, fq_edge::wire::WireError>) -> String {
    match answer {
        Err(fq_edge::wire::WireError::InvalidInput { message, .. }) => message,
        other => panic!("expected the drop to be refused, got {other:?}"),
    }
}

/// **A queued run whose permit has come is refused like a live one.** In
/// the instant between taking its permit and the runner registering it,
/// a listed run is starting: not driven yet, but no longer waiting. A drop
/// then — plain or `--live` — is refused with "starting to run" and
/// publishes nothing; once that permit is given back, the next drop goes
/// through.
///
/// The instant is held open here by taking the recovered invocation's
/// place and permit exactly as recovery's task does, and not calling
/// `resume`.
#[tokio::test]
async fn a_queued_run_whose_permit_has_come_is_refused_like_a_live_one() {
    let start = NextStart::over_one_run_in_flight().await;
    let admission = RunAdmission::new(start.runner.clone(), 1);
    let surface = start.operator_surface(&admission).await;
    let mut drops = drops_published(&start.bus).await;
    let id = start.invocation.to_string();
    let permit = admission
        .workers()
        .queue(Waiter::Recovered(start.invocation))
        .await
        .permit()
        .await
        .expect("the only permit is free");

    for live in [false, true] {
        let message = refusal(
            invoke(
                &surface,
                fq_ops::Invocation::Drop,
                serde_json::json!({ "invocation_id": id, "live": live }),
            )
            .await,
        );
        assert!(
            message.contains("starting to run"),
            "live: {live}: {message}"
        );
    }
    let published = tokio::time::timeout(Duration::from_millis(500), drops.next()).await;
    assert!(
        published.is_err(),
        "a refused drop publishes nothing: {published:?}"
    );

    drop(permit);
    let dropped = invoke(
        &surface,
        fq_ops::Invocation::Drop,
        serde_json::json!({ "invocation_id": id }),
    )
    .await;
    assert!(
        dropped.is_ok(),
        "with the permit given back the run is neither starting nor live: {dropped:?}"
    );
}

/// **A listed run that is running is dropped as a live one.** A recovered
/// run keeps its listing for its whole life, so a drop of it while the
/// runner drives it must take the runner's path, not the queue's: a plain
/// drop is refused as currently running, and `--live` halts it. The
/// witness is the run's end: halted at its next step boundary, its row is
/// not terminal and nothing reports it completed — whereas the model's
/// reply, left alone, would have completed it.
#[tokio::test]
async fn a_running_recovered_run_is_dropped_only_with_live() {
    let mut start = NextStart::over_one_run_in_flight().await;
    let admission = RunAdmission::new(start.runner.clone(), 1);
    let surface = start.operator_surface(&admission).await;
    let mut completed = start
        .bus
        .subscribe(format!("fq.agent.{CAPPED_AGENT}.completed"))
        .await
        .expect("subscribe");
    let id = start.invocation.to_string();
    let resume = start
        .spawn_resumes(&admission)
        .await
        .handles
        .pop()
        .expect("one resume task");
    start.model.entered().await;
    assert!(start.runner.is_active(&start.invocation));

    let message = refusal(
        invoke(
            &surface,
            fq_ops::Invocation::Drop,
            serde_json::json!({ "invocation_id": id }),
        )
        .await,
    );
    assert!(message.contains("currently running"), "{message}");
    let live = invoke(
        &surface,
        fq_ops::Invocation::Drop,
        serde_json::json!({ "invocation_id": id, "live": true }),
    )
    .await;
    assert!(
        live.is_ok(),
        "a live drop of a running listed run: {live:?}"
    );

    start.model.release();
    tokio::time::timeout(Duration::from_secs(10), resume)
        .await
        .expect("the halted run's task ends")
        .expect("its task joins");
    let row = start
        .worker_store
        .get_invocation_state(&id)
        .await
        .unwrap()
        .expect("the WAL row is still there");
    assert_eq!(
        row.terminal_at, None,
        "halted at its step boundary, not run to its end"
    );
    let reported = tokio::time::timeout(Duration::from_millis(500), completed.next()).await;
    assert!(
        reported.is_err(),
        "a halted run is not reported completed: {reported:?}"
    );
}

// What recovery reports about a resume that returned an error (issue 1005),
// on the same next start as the tests above.
mod resume_errors;
