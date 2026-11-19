//! The drop-liveness invariant, sequenced adversarially:
//!
//! > **`invocation.drop` must never return `NotFound` for an
//! > invocation the daemon is actively running.**
//!
//! `invocation.drop` reads two different truths in one handler. The
//! liveness gate asks the **runner** — the process actually driving the
//! work, which knows with zero lag. The write then resolves the
//! invocation through the **projection** (and the coordination owner
//! row), both of which are built by asynchronous durable consumers and
//! are therefore *behind* the runner by construction. Nothing writes an
//! owner row on the dispatch path at all, so for an invocation this
//! daemon has picked up but whose `triggered` event the projector has
//! not yet folded, the two truths disagree — and a `--live` drop would
//! arm the halt (stopping real work) and then report that the
//! invocation does not exist.
//!
//! These tests hold that disagreement open deterministically. The
//! surface below is the daemon's real `operator_registry`, over live
//! stores and a real `ReducerRunner`, with one deliberate omission: no
//! projection consumer runs. "The fold has not caught up" is then a
//! *state* the test holds for its whole duration, not a moment it has
//! to race — no sleeps, no timing luck. The invocation is parked inside
//! its first model call, so the runner genuinely reports it active.
//!
//! The oracle is stronger than the error code: after each drop the
//! invocation is released and run to its outcome, and the verdict and
//! the fate must agree. A drop that reports failure must leave the work
//! running (`Completed`); a drop that reports success must have stopped
//! it (`Suspended`). Anything else is a partial application — the
//! caller told "no" about work that was in fact stopped.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use fq_edge::wire::WireError;
use fq_runtime::events::TriggerSource;
use fq_runtime::worker::InvocationOutcome;
use fq_runtime::{ChatRequest, ChatResponse, LlmClient, LlmError};
use futures::StreamExt;
use tokio::sync::Semaphore;
use uuid::Uuid;

/// The model this test's agent declares, and the one entry in its
/// pricing table — the ADR-0004 guarantee refuses to dispatch an
/// unpriced model, so the table is part of the fixture, not decoration.
const MODEL: &str = "claude-haiku";

/// Ceiling on every wait in these tests. Nothing here is timing-
/// dependent; a deadline reached means something is genuinely stuck, so
/// it is generous and its expiry is a failure, never a branch.
const PATIENCE: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------
// The parked model call: how "the daemon is running this invocation"
// becomes a state the test holds rather than a window it races.
// ---------------------------------------------------------------------

/// An LLM whose first call parks. It signals that the invocation is in
/// flight — the runner has marked it active and published `triggered` —
/// and then blocks until the test releases it. Released, it answers
/// with `report_outcome`, which drives the invocation to `Completed`
/// unless something stopped it first.
struct ParkedLlm {
    /// Permits added when a call enters: the test acquires one to learn
    /// the invocation is in flight.
    entered: Semaphore,
    /// Permits the test adds to let the parked call return.
    release: Semaphore,
    calls: AtomicUsize,
}

impl ParkedLlm {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
            calls: AtomicUsize::new(0),
        })
    }

    /// Wait until an invocation is parked inside its model call.
    async fn await_entry(&self) {
        tokio::time::timeout(PATIENCE, self.entered.acquire())
            .await
            .expect("an invocation entered its first model call")
            .expect("entry semaphore open")
            .forget();
    }

    /// Let the parked call return.
    fn release(&self) {
        self.release.add_permits(1);
    }
}

#[async_trait::async_trait]
impl LlmClient for ParkedLlm {
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, LlmError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
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
                    parameters: serde_json::json!({"status": "success", "summary": "ran to the end"}),
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

// ---------------------------------------------------------------------
// The surface under test: the daemon's own operator registry.
// ---------------------------------------------------------------------

/// The daemon's operator surface over live stores and a real runner —
/// assembled by the same `operator_registry` call `fqd` makes, so
/// what these tests exercise is the shipped handler, not a re-creation.
///
/// The one deliberate omission is the projection consumer: nothing
/// folds the event stream into the projection store, and nothing writes
/// coordination owner rows. That is the adversarial ordering, held open
/// for the whole test.
struct Surface {
    registry: Arc<fq_edge::EdgeRegistry>,
    bus: fq_runtime::EventBus,
    projection: Arc<fq_runtime::control_plane::projection::store::ProjectionStore>,
    control_plane: Arc<fq_runtime::control_plane::store::ControlPlaneStore>,
    runner: Arc<fq_runtime::ReducerRunner<fq_runtime::Harness>>,
    agent: fq_runtime::Agent,
    _scratch: tempfile::TempDir,
    _nats: fq_test_support::NatsServer,
}

impl Surface {
    async fn start() -> Self {
        Self::start_with(None).await
    }

    /// The surface, its runner binding each run's workspace with
    /// `workspace` when one is given.
    async fn start_with(
        workspace: Option<Arc<dyn fq_runtime::worker::workspace::WorkspaceProvider>>,
    ) -> Self {
        let nats = fq_test_support::NatsServer::start();
        let scratch = tempfile::tempdir().expect("scratch dir");
        let paths = fq_runtime::db::RuntimeDbPaths::under(scratch.path());
        let projection = Arc::new(
            fq_runtime::control_plane::projection::store::ProjectionStore::open(&paths.projection)
                .await
                .expect("open projection store"),
        );
        let control_plane = Arc::new(
            fq_runtime::control_plane::store::ControlPlaneStore::open(&paths.control_plane)
                .await
                .expect("open control-plane store"),
        );
        let worker_store = Arc::new(
            fq_runtime::worker::store::WorkerStore::open(&paths.worker)
                .await
                .expect("open worker store"),
        );
        let views = Arc::new(
            fq_runtime::views::Views::open(&paths)
                .await
                .expect("open views"),
        );
        let bus = fq_runtime::EventBus::connect(nats.url())
            .await
            .expect("connect the broker");

        let mut pricing = std::collections::HashMap::new();
        pricing.insert(
            MODEL.to_string(),
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
                    .worker_id(
                        fq_runtime::worker::WorkerId::new(format!(
                            "drop-liveness-{}",
                            Uuid::now_v7().simple()
                        ))
                        .expect("worker id"),
                    )
                    .workspace(workspace)
                    .build(),
            ),
            fq_runtime::Harness::new(),
        ));

        let agent = fq_runtime::Agent::builder()
            .id(format!("drop-liveness-{}", Uuid::now_v7().simple()))
            .model(MODEL)
            .system_prompt("Adversarial probe agent.")
            .budget(1.0)
            .build()
            .expect("agent");

        let (_watermark_tx, watermark) = fq_runtime::watermark::channel();
        // Nothing here drops an agent definition; the Agent view is
        // registered either way, so it gets an empty registry rather
        // than a mock — and `invocation.resume`'s handle reads the same
        // one, as the daemon wires it.
        let agents = fq_runtime::shared_registry(fq_runtime::AgentRegistry::new());
        // `invocation.resume` is a command on the edge, so the surface
        // cannot assemble without its handle. This probe only issues
        // `invocation.drop`, so the model here is never asked — a
        // parked one would simply never be entered.
        let resume = Arc::new(fq_daemon::ResumeControl::new(
            bus.clone(),
            worker_store,
            control_plane.clone(),
            runner.clone(),
            agents.clone(),
            ParkedLlm::new(),
            fq_runtime::worker::DeferralQueue::new().0,
            fq_runtime::control_plane::dispatcher::RunAdmission::new(runner.clone(), 1),
        ));
        // `dead_letter.requeue` asks a dispatcher's records, so the
        // surface takes one; this probe never requeues, so it holds no
        // claim store.
        let dispatcher = fq_runtime::TriggerDispatcher::new(
            bus.clone(),
            agents.clone(),
            runner.clone(),
            ParkedLlm::new(),
            1,
        );
        let registry = fq_daemon::operator_registry(
            views,
            fq_runtime::watermark::Horizon::new(vec![watermark]),
            Duration::from_millis(1),
            fq_daemon::OperatorDeps {
                facts: fq_daemon::DaemonFacts {
                    db_paths: Arc::new(paths.clone()),
                    legacy_events_db: Arc::new(fq_runtime::db::legacy_db_path(scratch.path())),
                    drain_deadline_ms: 180_000,
                    stuck_after_ms: 4_210_000,
                    enabled_consumers: fq_runtime::health::EnabledConsumers::default(),
                    mcp_servers: Default::default(),
                    throttle: Arc::new(fq_runtime::llm::ModelThrottle::inert()),
                    agent_caps: fq_runtime::control_plane::agent_cap::AgentConcurrency::new(),
                    worker_id: "operator-surface-test".to_string(),
                    dispatching_since_ms: 0,
                    trigger_claim_retention_ms: None,
                    archive_holds: Default::default(),
                },
                bus: bus.clone(),
                projection: projection.clone(),
                control_plane: control_plane.clone(),
                runner: runner.clone(),
                resume,
                trigger_records: fq_daemon::TriggerRecords::of(&dispatcher),
                agents,
                // The machinery verbs are registered like every other
                // op, and this probe never issues one — the stop
                // switch is armed so the surface assembles, and
                // nothing here throws it.
                machinery: fq_daemon::MachineryDeps {
                    agents: fq_runtime::shared_registry(fq_runtime::AgentRegistry::new()),
                    agents_dir: scratch.path().join("agents"),
                    default_model: None,
                    worker: runner.clone(),
                    down: fq_daemon::DownSignal::new(),
                    projection_rebuild:
                        fq_runtime::control_plane::projection::ProjectionRebuildHandle::detached(),
                },
            },
        )
        .expect("assemble the operator registry");

        Self {
            registry: Arc::new(registry),
            bus,
            projection,
            control_plane,
            runner,
            agent,
            _scratch: scratch,
            _nats: nats,
        }
    }

    /// Serve the registry on the real edge, on a loopback port: TLS, the
    /// token preamble, then tarpc, as `fqd` serves it. Returns the address
    /// and the identity tokens are minted under.
    async fn serve_over_the_edge(&self) -> (std::net::SocketAddr, fq_edge::EdgeIdentity) {
        let identity = fq_edge::EdgeIdentity::provision().expect("provision an edge identity");
        let (addr, serving) = fq_edge::bind("127.0.0.1:0", &identity, self.registry.clone())
            .await
            .expect("bind the edge");
        tokio::spawn(serving);
        (addr, identity)
    }

    /// The drop event a receipt names, as the broker holds it: the
    /// payload's JSON, and the message's bytes, payload and headers, as
    /// text.
    async fn drop_event(&self, receipt: &serde_json::Value) -> (serde_json::Value, String) {
        let receipt: fq_ops::Receipt =
            serde_json::from_value(receipt.clone()).expect("a drop answers with a receipt");
        let seq = receipt
            .watermark(fq_ops::Domain::Event)
            .expect("the receipt names the drop event's sequence");
        let message = self
            .bus
            .jetstream()
            .get_stream(fq_runtime::bus::STREAM_NAME)
            .await
            .expect("the event stream")
            .get_raw_message(seq)
            .await
            .expect("the drop event is on the stream");
        let event: serde_json::Value =
            serde_json::from_slice(&message.payload).expect("the event is JSON");
        let raw = format!(
            "{}\n{:?}",
            String::from_utf8_lossy(&message.payload),
            message.headers
        );
        (event["payload"]["payload"].clone(), raw)
    }

    /// Issue `invocation.drop` through the registry exactly as the edge
    /// dispatches it: the declared op name, the wire input shape.
    async fn drop_invocation(&self, id: &str, live: bool) -> Result<serde_json::Value, WireError> {
        let op = fq_ops::OpId::Verb(fq_ops::VerbId::Invocation(fq_ops::Invocation::Drop));
        let handler = self
            .registry
            .handler(&op.to_string())
            .expect("invocation.drop is registered on the operator surface");
        handler(
            serde_json::json!({
                "invocation_id": id,
                "reason": "adversarial ordering",
                "live": live,
            }),
            fq_edge::CallContext::none(),
        )
        .await
    }

    /// Park one invocation inside its first model call and return its
    /// id, the join handle for its eventual outcome, and the LLM handle
    /// that releases it.
    async fn park_one_invocation(
        &self,
    ) -> (
        Uuid,
        tokio::task::JoinHandle<Result<InvocationOutcome, fq_runtime::worker::ExecutorError>>,
        Arc<ParkedLlm>,
    ) {
        let llm = ParkedLlm::new();
        let run = {
            let runner = self.runner.clone();
            let agent = self.agent.clone();
            let llm = llm.clone();
            tokio::spawn(async move {
                runner
                    .run(
                        &agent,
                        llm.as_ref(),
                        TriggerSource::Manual,
                        None,
                        serde_json::json!({"input": "go"}),
                    )
                    .await
            })
        };
        llm.await_entry().await;
        let invocation_id = self.triggered_invocation().await;

        // The adversarial preconditions, asserted rather than assumed:
        // the runner knows, and neither durable store does.
        assert!(
            self.runner.is_active(&invocation_id),
            "the runner must be driving {invocation_id} — the whole invariant is about this state"
        );
        assert_eq!(
            self.projection
                .agent_id_for_invocation(&invocation_id.to_string())
                .await
                .expect("projection query"),
            None,
            "the projection must still be lagging: no consumer folds it in this test"
        );
        assert_eq!(
            self.control_plane
                .get_invocation_owner(&invocation_id.to_string())
                .await
                .expect("owner query")
                .map(|o| o.status),
            None,
            "no coordination owner row: nothing on the dispatch path writes one"
        );

        (invocation_id, run, llm)
    }

    /// The invocation id the runner minted, read off the event log's
    /// `triggered` event. JetStream replay from the first sequence, so
    /// there is no subscribe-before-publish race to lose.
    async fn triggered_invocation(&self) -> Uuid {
        let mut events = self
            .bus
            .events_from("fq.agent.>", 1)
            .await
            .expect("read the event log");
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            let next = tokio::time::timeout(
                deadline.saturating_duration_since(tokio::time::Instant::now()),
                events.next(),
            )
            .await
            .expect("a `triggered` event within the deadline")
            .expect("the event log stream stayed open")
            .expect("event deserialises");
            let Some(event) = next.event else { continue };
            if matches!(
                event.payload,
                fq_runtime::events::EventPayload::Triggered(_)
            ) {
                return event.envelope.invocation_id;
            }
        }
    }

    /// The agent an `invocation.operator_recovered` event was published
    /// under, read back off the log — the drop's attribution in the
    /// system of record (ADR-0026). `None` when no drop was published.
    async fn drop_attributed_to(&self, invocation_id: Uuid) -> Option<String> {
        let tip = self.bus.last_event_seq().await.expect("stream tip");
        let mut events = self
            .bus
            .events_from("fq.agent.>", 1)
            .await
            .expect("read the event log");
        while let Ok(Some(Ok(message))) = tokio::time::timeout(PATIENCE, events.next()).await {
            let seq = message.seq;
            if let Some(event) = message.event
                && event.envelope.invocation_id == invocation_id
                && matches!(
                    event.payload,
                    fq_runtime::events::EventPayload::InvocationOperatorRecovered(_)
                )
            {
                return Some(event.envelope.agent_id.as_str().to_string());
            }
            if seq >= tip {
                break;
            }
        }
        None
    }
}

/// Release a parked invocation and wait for the outcome its run
/// actually reached — the oracle for "was the halt armed".
async fn outcome_of(
    llm: &ParkedLlm,
    run: tokio::task::JoinHandle<Result<InvocationOutcome, fq_runtime::worker::ExecutorError>>,
) -> InvocationOutcome {
    llm.release();
    tokio::time::timeout(PATIENCE, run)
        .await
        .expect("the released invocation reached an outcome")
        .expect("the invocation task did not panic")
        .expect("the invocation did not error")
}

fn halted(outcome: &InvocationOutcome) -> bool {
    matches!(outcome, InvocationOutcome::Suspended { .. })
}

// ---------------------------------------------------------------------
// The invariant.
// ---------------------------------------------------------------------

/// A `--live` drop of an invocation this daemon is running, with the
/// projection lagging behind the runner: it must not be told the
/// invocation does not exist. And whichever way it answers, the answer
/// and the invocation's fate must agree — a refusal that nonetheless
/// halted the work is the partial application this invariant exists to
/// forbid.
#[tokio::test(flavor = "multi_thread")]
async fn live_drop_of_a_running_invocation_is_never_not_found() {
    let surface = Surface::start().await;
    let (invocation_id, run, llm) = surface.park_one_invocation().await;

    let verdict = surface
        .drop_invocation(&invocation_id.to_string(), true)
        .await;
    let outcome = outcome_of(&llm, run).await;

    assert!(
        !matches!(verdict, Err(WireError::NotFound { .. })),
        "`invocation.drop --live` reported NotFound for an invocation the daemon was \
         actively running (verdict: {verdict:?}); the runner had just confirmed it live"
    );
    match &verdict {
        Ok(_) => assert!(
            halted(&outcome),
            "the drop succeeded but the invocation ran on to {outcome:?} — the halt was never armed"
        ),
        Err(err) => assert!(
            !halted(&outcome),
            "the drop reported `{err}` yet the halt had already been armed and stopped the \
             invocation ({outcome:?}) — a partial application: the caller is told the work \
             was not dropped while its work is being stopped"
        ),
    }

    // Resolving from the runner must resolve *truthfully*: the drop is
    // recorded against the agent whose work it stopped, not smuggled in
    // under `operator` to dodge the lookup. The event log is the system
    // of record (ADR-0026), and the archive row is built from this.
    assert_eq!(
        surface.drop_attributed_to(invocation_id).await.as_deref(),
        Some(surface.agent.id().as_str()),
        "the drop event must be attributed to the agent the daemon was driving"
    );
}

/// A bare drop of a running invocation stays a refusal about *this
/// request* — `InvalidInput` naming `--live`, never `NotFound` — and
/// arms nothing: the invocation runs to completion.
#[tokio::test(flavor = "multi_thread")]
async fn bare_drop_of_a_running_invocation_refuses_and_arms_nothing() {
    let surface = Surface::start().await;
    let (invocation_id, run, llm) = surface.park_one_invocation().await;

    let verdict = surface
        .drop_invocation(&invocation_id.to_string(), false)
        .await;
    let outcome = outcome_of(&llm, run).await;

    match &verdict {
        Err(WireError::InvalidInput { message, .. }) => {
            assert!(
                message.contains("currently running") && message.contains("--live"),
                "the refusal must name its remedy, got: {message}"
            );
        }
        other => panic!(
            "a bare drop of a running invocation must be refused as InvalidInput \
             (never NotFound, never silently accepted), got: {other:?}"
        ),
    }
    assert!(
        !halted(&outcome),
        "a refused bare drop must leave the invocation alone; it reached {outcome:?}"
    );
}

/// The other side of the invariant: a genuinely unknown id is still
/// `NotFound`, with or without `--live`. Live-drop must not become an
/// escape hatch that invents invocations.
#[tokio::test(flavor = "multi_thread")]
async fn drop_of_a_genuinely_unknown_invocation_is_still_not_found() {
    let surface = Surface::start().await;
    let unknown = Uuid::now_v7().to_string();

    for live in [false, true] {
        let verdict = surface.drop_invocation(&unknown, live).await;
        assert!(
            matches!(verdict, Err(WireError::NotFound { .. })),
            "an id no daemon has ever driven must be NotFound (live={live}), got: {verdict:?}"
        );
    }
    // Nothing was published for it: no event, so no phantom archive.
    assert_eq!(
        surface
            .projection
            .agent_id_for_invocation(&unknown)
            .await
            .expect("projection query"),
        None
    );
}

/// **A live drop during a run's first write marks the row its fold may
/// miss, after the request is over** (issue 1070). Another writer holds
/// the worker store's write lock, so the run's first write waits inside
/// SQLite. The live drop arms the halt then, which finds the write under
/// way, and publishes; in the daemon the drop's fold could now come before
/// the row exists and find nothing (no fold runs in this arena). The
/// receipt does not wait for the write: the handler gives the run word of
/// the publish and returns, so the request is over while the write is
/// still held, as one the edge client's deadline cuts off after its
/// publish would be. When the write lands, the run stops before any model
/// call and marks its own row as the fold would have.
#[tokio::test(flavor = "multi_thread")]
async fn a_live_drop_during_the_first_write_marks_the_row_once_it_lands() {
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    let surface = Arc::new(Surface::start().await);
    let worker_db = fq_runtime::db::RuntimeDbPaths::under(surface._scratch.path()).worker;
    let other = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().filename(&worker_db))
        .await
        .expect("a second connection to the worker store");
    let mut held = other.acquire().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *held)
        .await
        .unwrap();

    let llm = ParkedLlm::new();
    let run = {
        let runner = surface.runner.clone();
        let agent = surface.agent.clone();
        let llm = llm.clone();
        tokio::spawn(async move {
            runner
                .run(
                    &agent,
                    llm.as_ref(),
                    TriggerSource::Manual,
                    None,
                    serde_json::json!({"input": "go"}),
                )
                .await
        })
    };
    let invocation_id = surface.triggered_invocation().await;
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while !surface.runner.first_write_under_way(invocation_id) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the run reaches its first write and waits on the lock"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let verdict = tokio::time::timeout(
        PATIENCE,
        surface.drop_invocation(&invocation_id.to_string(), true),
    )
    .await
    .expect("the receipt does not wait for the run's first write");
    let receipt = verdict.expect("a live drop is accepted");
    let (payload, _) = surface.drop_event(&receipt).await;
    assert_eq!(
        payload["archive"],
        serde_json::json!("pending"),
        "the run's first write is under way, so its row is coming and its archive with it: \
         {payload}"
    );
    assert!(
        surface.runner.first_write_under_way(invocation_id) && !run.is_finished(),
        "the request is over while the run's first write is still held"
    );
    sqlx::query("ROLLBACK").execute(&mut *held).await.unwrap();
    drop(held);

    let outcome = tokio::time::timeout(PATIENCE, run)
        .await
        .expect("the run stops")
        .expect("the run's task joins");
    assert!(
        matches!(outcome, Ok(InvocationOutcome::Suspended { .. })),
        "the row is there, so the run stops as at a boundary after it: {outcome:?}"
    );
    assert_eq!(
        llm.calls.load(Ordering::SeqCst),
        0,
        "the run made no model call"
    );
    let row: (String, Option<i64>) =
        sqlx::query_as("SELECT phase, terminal_at FROM invocation_state WHERE invocation_id = ?")
            .bind(invocation_id.to_string())
            .fetch_one(&other)
            .await
            .expect("the row landed");
    assert_eq!(row.0, "failed", "the drop's mark is on the row");
    assert!(row.1.is_some());
}

/// **A live drop whose publish fails makes no mark** (issue 1070). The
/// run's first write is held, the live drop arms its halt, which finds the
/// write under way, and its publish fails: the reason given is larger than
/// the broker accepts, which the bus refuses before sending, so the
/// failure is immediate and the write's lock is let go well within the
/// store's busy timeout. The operator is told the drop failed. The handler
/// gives the run its word only once the publish has returned, so the run
/// hears nothing: when its row lands it stops before any model call and
/// leaves the row in flight, as a drop that is not on the record must.
#[tokio::test(flavor = "multi_thread")]
async fn a_live_drop_whose_publish_fails_during_the_first_write_marks_nothing() {
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    let surface = Arc::new(Surface::start().await);
    let worker_db = fq_runtime::db::RuntimeDbPaths::under(surface._scratch.path()).worker;
    let other = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().filename(&worker_db))
        .await
        .expect("a second connection to the worker store");
    let mut held = other.acquire().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *held)
        .await
        .unwrap();

    let llm = ParkedLlm::new();
    let run = {
        let runner = surface.runner.clone();
        let agent = surface.agent.clone();
        let llm = llm.clone();
        tokio::spawn(async move {
            runner
                .run(
                    &agent,
                    llm.as_ref(),
                    TriggerSource::Manual,
                    None,
                    serde_json::json!({"input": "go"}),
                )
                .await
        })
    };
    let invocation_id = surface.triggered_invocation().await;
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while !surface.runner.first_write_under_way(invocation_id) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the run reaches its first write and waits on the lock"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let op = fq_ops::OpId::Verb(fq_ops::VerbId::Invocation(fq_ops::Invocation::Drop));
    let handler = surface
        .registry
        .handler(&op.to_string())
        .expect("invocation.drop is registered on the operator surface");
    let verdict = tokio::time::timeout(
        PATIENCE,
        handler(
            serde_json::json!({
                "invocation_id": invocation_id.to_string(),
                "reason": "x".repeat(2 * 1024 * 1024),
                "live": true,
            }),
            fq_edge::CallContext::none(),
        ),
    )
    .await
    .expect("the drop answers once its publish has failed");
    assert!(
        verdict.is_err(),
        "the drop's event is larger than the broker accepts, so the drop fails"
    );
    assert!(
        surface.runner.first_write_under_way(invocation_id),
        "the drop failed while the run's first write was still held"
    );
    sqlx::query("ROLLBACK").execute(&mut *held).await.unwrap();
    drop(held);

    let outcome = tokio::time::timeout(PATIENCE, run)
        .await
        .expect("the run stops")
        .expect("the run's task joins");
    assert!(
        matches!(outcome, Ok(InvocationOutcome::Suspended { .. })),
        "the halt was armed, so the run stops once its row has landed: {outcome:?}"
    );
    assert_eq!(llm.calls.load(Ordering::SeqCst), 0, "no model call");
    let row: (String, Option<i64>) =
        sqlx::query_as("SELECT phase, terminal_at FROM invocation_state WHERE invocation_id = ?")
            .bind(invocation_id.to_string())
            .fetch_one(&other)
            .await
            .expect("the row landed");
    assert_eq!(
        row,
        ("awaiting_model".to_string(), None),
        "a drop that is not on the record leaves the row in flight"
    );
}

// ---------------------------------------------------------------------
// What the drop's event records (issue 1070): who asked, and whether an
// archive of the run can still come.
// ---------------------------------------------------------------------

/// **A drop through the edge records who asked, and never the token.**
/// The drop goes over the real edge, TLS and the token preamble, with a
/// token minted for the principal `on-call` and granted only the drop's
/// authority. Its event names that principal as `requested_by`, a name
/// signed into the token, and nothing on the broker carries the token
/// itself, a bearer credential. The run was live, its row written, so an
/// archive is pending.
#[tokio::test(flavor = "multi_thread")]
async fn a_drop_through_the_edge_records_who_asked_and_never_the_token() {
    let surface = Surface::start().await;
    let (invocation_id, run, llm) = surface.park_one_invocation().await;
    let (addr, identity) = surface.serve_over_the_edge().await;
    let token = identity
        .mint_token("on-call", &[("write", "invocation")])
        .expect("mint a token");
    let client = fq_edge::EdgeClient::connect(&addr.to_string(), identity.fingerprint(), &token)
        .await
        .expect("connect to the edge");
    let receipt = client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: fq_ops::OpId::Verb(fq_ops::VerbId::Invocation(fq_ops::Invocation::Drop)),
                version: 1,
                input: serde_json::json!({
                    "invocation_id": invocation_id.to_string(),
                    "reason": "requested over the edge",
                    "live": true,
                }),
                min_seq: None,
            },
        )
        .await
        .expect("rpc")
        .expect("the drop is accepted")
        .output;
    assert!(halted(&outcome_of(&llm, run).await));

    let (payload, raw) = surface.drop_event(&receipt).await;
    assert_eq!(
        payload["requested_by"],
        serde_json::json!("on-call"),
        "the drop's event names the principal its request came in on: {payload}"
    );
    assert_eq!(
        payload["archive"],
        serde_json::json!("pending"),
        "{payload}"
    );
    assert!(
        !raw.contains(&token),
        "the token, a bearer credential, is on the broker: {raw}"
    );
}

/// **A drop with no caller records none.** The handler called in-process,
/// as these tests call it, has no edge request around it, so the drop's
/// event says nothing of who asked: not recorded.
#[tokio::test(flavor = "multi_thread")]
async fn a_drop_with_no_caller_records_no_requester() {
    let surface = Surface::start().await;
    let (invocation_id, run, llm) = surface.park_one_invocation().await;
    let receipt = surface
        .drop_invocation(&invocation_id.to_string(), true)
        .await
        .expect("the drop is accepted");
    assert!(halted(&outcome_of(&llm, run).await));
    let (payload, _) = surface.drop_event(&receipt).await;
    assert!(
        payload.get("requested_by").is_none(),
        "no caller, so nothing is recorded of who asked: {payload}"
    );
}

/// A run this daemon is not driving, known to the projection under
/// `agent`: what a drop of it resolves from.
async fn known_to_the_projection(surface: &Surface, invocation_id: Uuid) {
    let event = fq_runtime::events::Event::new(
        surface.agent.id().clone(),
        invocation_id,
        // Any event of the run's names its agent for the drop; this one is
        // the simplest to write.
        fq_runtime::events::EventPayload::InvocationOperatorResumed(
            fq_runtime::events::InvocationOperatorResumedPayload {
                completed_call_ids: Vec::new(),
                reason: None,
            },
        ),
    );
    surface
        .projection
        .insert_event(&event, Some(1))
        .await
        .expect("the projection records the run");
}

/// The `archive` a plain drop of `invocation_id` records.
async fn archive_of_a_drop(surface: &Surface, invocation_id: Uuid) -> serde_json::Value {
    let receipt = surface
        .drop_invocation(&invocation_id.to_string(), false)
        .await
        .expect("the drop is accepted");
    let (payload, _) = surface.drop_event(&receipt).await;
    payload["archive"].clone()
}

/// **No row and no archive: none will come.** A run this daemon is not
/// driving, with no row on its worker and no archive on record, gets no
/// archive after its drop: nothing that starts a run here begins one
/// without a row, and the run has none. Its event says so.
#[tokio::test(flavor = "multi_thread")]
async fn a_drop_of_a_run_with_no_row_and_no_archive_says_none_will_come() {
    let surface = Surface::start().await;
    let invocation_id = Uuid::now_v7();
    known_to_the_projection(&surface, invocation_id).await;
    assert_eq!(
        archive_of_a_drop(&surface, invocation_id).await,
        serde_json::json!("none"),
        "no row on the worker, none to come, no archive on record: the drop's event must say \
         no archive will come"
    );
}

/// **A row on the worker: an archive is pending.** The run's row is on
/// this daemon's worker, so the archive sweeper hands it off once it is
/// finished.
#[tokio::test(flavor = "multi_thread")]
async fn a_drop_of_a_run_whose_row_is_on_the_worker_says_an_archive_is_pending() {
    let surface = Surface::start().await;
    let invocation_id = Uuid::now_v7();
    known_to_the_projection(&surface, invocation_id).await;
    let worker = fq_runtime::worker::store::WorkerStore::open(
        &fq_runtime::db::RuntimeDbPaths::under(surface._scratch.path()).worker,
    )
    .await
    .expect("open the worker store");
    worker
        .upsert_invocation_state(&fq_runtime::worker::store::InvocationStateRow {
            invocation_id: invocation_id.to_string(),
            agent_id: surface.agent.id().as_str().to_string(),
            schema_version: 1,
            phase: "awaiting_model".to_string(),
            state_blob: b"{}".to_vec(),
            step_index: 0,
            started_at: 1,
            updated_at: 1,
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
        .expect("the run's row");
    assert_eq!(
        archive_of_a_drop(&surface, invocation_id).await,
        serde_json::json!("pending"),
        "the run's row is on the worker: the drop's event must say an archive is pending"
    );
}

/// **An archive on record: pending.** The run's row has been handed off
/// already, so its archive is on record, and `none` would deny it.
#[tokio::test(flavor = "multi_thread")]
async fn a_drop_of_a_run_whose_archive_is_on_record_says_pending() {
    let surface = Surface::start().await;
    let invocation_id = Uuid::now_v7();
    known_to_the_projection(&surface, invocation_id).await;
    surface
        .control_plane
        .insert_archive(&fq_runtime::control_plane::store::InvocationArchiveRow {
            invocation_id: invocation_id.to_string(),
            agent_id: surface.agent.id().as_str().to_string(),
            final_phase: "completed".to_string(),
            final_state_blob: b"{}".to_vec(),
            started_at: 1,
            terminal_at: 2,
            archived_at: 3,
        })
        .await
        .expect("the run's archive");
    assert_eq!(
        archive_of_a_drop(&surface, invocation_id).await,
        serde_json::json!("pending"),
        "the run's archive is on record: the drop's event must say an archive is pending"
    );
}

/// A workspace provider whose provisioning waits until the test lets it
/// through: a run live in the runner, before its first write.
#[derive(Debug)]
struct ParkedProvision {
    inner: fq_runtime::worker::workspace::PerInvocationWorkspace,
    _root: tempfile::TempDir,
    provisioning: AtomicUsize,
    gate: Semaphore,
}

#[async_trait::async_trait]
impl fq_runtime::worker::workspace::WorkspaceProvider for ParkedProvision {
    async fn provision(
        &self,
        invocation_id: Uuid,
    ) -> Result<std::path::PathBuf, fq_runtime::worker::workspace::WorkspaceError> {
        self.provisioning.fetch_add(1, Ordering::SeqCst);
        self.gate.acquire().await.expect("gate open").forget();
        self.inner.provision(invocation_id).await
    }

    async fn reclaim(
        &self,
        invocation_id: Uuid,
        path: &std::path::Path,
    ) -> Result<(), fq_runtime::worker::workspace::WorkspaceError> {
        self.inner.reclaim(invocation_id, path).await
    }
}

/// **A live drop of a run before its first write says no archive will
/// come, through the real handler.** The run is live in the runner, its
/// workspace being provisioned, before its first write. The live drop's
/// halt finds no row and none to come, and the handler passes that to its
/// decision: the event says `none`. Let through, the run takes the halt at
/// its first step boundary, before its first write, and stops unstarted,
/// so no row ever comes. A handler that
/// read every halt as one on a written row would say `pending`, for good,
/// of a run that never wrote one.
#[tokio::test(flavor = "multi_thread")]
async fn a_live_drop_of_a_run_before_its_first_write_says_no_archive_will_come() {
    let root = tempfile::tempdir().unwrap();
    let workspace = Arc::new(ParkedProvision {
        inner: fq_runtime::worker::workspace::PerInvocationWorkspace::new(
            root.path().to_path_buf(),
        ),
        _root: root,
        provisioning: AtomicUsize::new(0),
        gate: Semaphore::new(0),
    });
    let surface = Surface::start_with(Some(
        workspace.clone() as Arc<dyn fq_runtime::worker::workspace::WorkspaceProvider>
    ))
    .await;
    let invocation_id = Uuid::now_v7();
    let llm = ParkedLlm::new();
    let run = {
        let runner = surface.runner.clone();
        let agent = surface.agent.clone();
        let llm = llm.clone();
        tokio::spawn(async move {
            let (durable_start, _fired) = fq_runtime::worker::DurableStart::channel(invocation_id);
            runner
                .run_signalling(
                    &agent,
                    llm.as_ref(),
                    fq_runtime::Trigger::mint(
                        TriggerSource::Manual,
                        None,
                        serde_json::json!({"input": "go"}),
                    ),
                    None,
                    durable_start,
                )
                .await
        })
    };
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while workspace.provisioning.load(Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the run reaches its workspace"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        surface.runner.is_active(&invocation_id),
        "the run is live, before its first write"
    );

    let receipt = surface
        .drop_invocation(&invocation_id.to_string(), true)
        .await
        .expect("a live drop is accepted");
    let (payload, _) = surface.drop_event(&receipt).await;
    workspace.gate.add_permits(1);
    let outcome = tokio::time::timeout(PATIENCE, run)
        .await
        .expect("the run stops")
        .expect("the run's task joins");
    assert_eq!(
        payload["archive"],
        serde_json::json!("none"),
        "the halt found no row and none to come: {payload}"
    );
    assert!(
        matches!(
            outcome,
            Ok(InvocationOutcome::Unstarted {
                stopped_by: fq_runtime::worker::BoundaryStop::Halt,
                ..
            })
        ),
        "the run took the halt at its first step boundary, before its first write: {outcome:?}"
    );
    assert_eq!(llm.calls.load(Ordering::SeqCst), 0, "no model call");
}
