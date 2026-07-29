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
            content: None,
            tool_calls: vec![fq_runtime::events::MessageToolCall {
                tool_call_id: fq_runtime::events::ToolCallId::new("report-outcome").unwrap(),
                tool_name: fq_runtime::tools::REPORT_OUTCOME_CANONICAL_NAME.to_string(),
                parameters: serde_json::json!({"status": "success", "summary": "ran to the end"}),
            }],
            stop_reason: fq_runtime::events::StopReason::ToolUse,
            usage: fq_runtime::events::TokenUsage {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        })
    }
}

// ---------------------------------------------------------------------
// The surface under test: the daemon's own operator registry.
// ---------------------------------------------------------------------

/// The daemon's operator surface over live stores and a real runner —
/// assembled by the same `operator_registry` call `fq run` makes, so
/// what these tests exercise is the shipped handler, not a re-creation.
///
/// The one deliberate omission is the projection consumer: nothing
/// folds the event stream into the projection store, and nothing writes
/// coordination owner rows. That is the adversarial ordering, held open
/// for the whole test.
struct Surface {
    registry: fq_edge::EdgeRegistry,
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
                    .store(worker_store)
                    .worker_id(
                        fq_runtime::worker::WorkerId::new(format!(
                            "drop-liveness-{}",
                            Uuid::now_v7().simple()
                        ))
                        .expect("worker id"),
                    )
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
        let registry = fq_cli::operator_registry(
            views,
            fq_runtime::watermark::Horizon::new(vec![watermark]),
            Duration::from_millis(1),
            fq_cli::OperatorDeps {
                bus: bus.clone(),
                projection: projection.clone(),
                control_plane: control_plane.clone(),
                runner: runner.clone(),
            },
        )
        .expect("assemble the operator registry");

        Self {
            registry,
            bus,
            projection,
            control_plane,
            runner,
            agent,
            _scratch: scratch,
            _nats: nats,
        }
    }

    /// Issue `invocation.drop` through the registry exactly as the edge
    /// dispatches it: the declared op name, the wire input shape.
    async fn drop_invocation(&self, id: &str, live: bool) -> Result<serde_json::Value, WireError> {
        let op = fq_ops::OpId::Verb(fq_ops::VerbId::Invocation(fq_ops::Invocation::Drop));
        let handler = self
            .registry
            .handler(&op.to_string())
            .expect("invocation.drop is registered on the operator surface");
        handler(serde_json::json!({
            "invocation_id": id,
            "reason": "adversarial ordering",
            "live": live,
        }))
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
            if matches!(
                next.1.payload,
                fq_runtime::events::EventPayload::Triggered(_)
            ) {
                return next.1.envelope.invocation_id;
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
        while let Ok(Some(Ok((seq, event)))) = tokio::time::timeout(PATIENCE, events.next()).await {
            if event.envelope.invocation_id == invocation_id
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
