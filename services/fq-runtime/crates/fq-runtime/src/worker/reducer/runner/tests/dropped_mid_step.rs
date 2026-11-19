//! A drop that lands while a step is in flight (issue 1070).
//!
//! A drop does three things to the worker's log row (its WAL row), in this
//! order: the daemon's drop handler arms a halt on the runner driving the
//! run (a live drop), the coordination consumer folding the drop marks the
//! row finished, and the archive sweeper hands the finished row off, whose
//! acknowledgement deletes it. These tests do them from inside a model
//! call or a tool, the step's long part, and then let it return.
//!
//! The rule they hold: a run whose row was finished by someone else, or is
//! gone, publishes no ending of its own and never writes that row again.
//! Whoever marks the row finished first decides the outcome; when it is
//! the drop, what the step in flight returns is not the run's outcome.
//! The spend of a call that was in flight still reaches the stream.
//!
//! With the live halt armed, the runner already stops at the next step
//! boundary, before the reducer decides anything and before any row write,
//! so the two live-drop tests, and the tool test, pass without the rule
//! and guard it; so does the test that pins the rule's one exception. The
//! rule is for a runner holding no halt for the run (a deferred run whose
//! resume starts while the drop is being folded, or a drop another daemon
//! handled), and for a step that fails after a live drop's mark, which
//! reaches the runner with no boundary between.

use super::*;
use crate::bus::{BusError, EventSink};
use crate::test_support::sim::RecordingSink;
use crate::trigger::Trigger;
use crate::worker::DurableStart;
use fq_tools::{Tool, ToolContext, ToolError, ToolResult};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

/// What the drop has done by the time the step's long part returns.
#[derive(Clone, Copy, Debug)]
struct Drop {
    /// The live drop's halt was armed on this runner.
    halt: bool,
    /// The finished row was handed off and its acknowledgement deleted it.
    handed_off: bool,
}

/// A drop, done to `invocation_id`'s row in `store` from inside the step,
/// in the order a drop does it; nothing when `drop` is `None`.
#[derive(Clone)]
struct Dropper {
    runner: Arc<OnceLock<Arc<ReducerRunner<Harness>>>>,
    store: Arc<WorkerStore>,
    invocation_id: Uuid,
    drop: Option<Drop>,
}

impl Dropper {
    async fn drop_now(&self) {
        let Some(drop) = self.drop else {
            return;
        };
        let id = self.invocation_id.to_string();
        if drop.halt {
            let runner = self.runner.get().expect("the runner is set");
            assert!(
                runner.request_halt(self.invocation_id),
                "the run is live here"
            );
        }
        // The coordination consumer folding the drop.
        let marked = self
            .store
            .mark_invocation_operator_terminal(&id, "failed", 7_000)
            .await
            .expect("the WAL writes");
        assert_eq!(marked, 1, "the row was in flight");
        if drop.handed_off {
            // The sweeper's hand-off, acknowledged.
            let deleted = self
                .store
                .delete_invocation_state(&id)
                .await
                .expect("the WAL writes");
            assert_eq!(deleted, 1);
        }
    }
}

/// What the call the drop lands in returns.
enum First {
    Answers(ChatResponse),
    Fails(fn() -> crate::llm::LlmError),
}

/// The provider refuses the call outright: no usage, nothing billed.
fn refused_key() -> crate::llm::LlmError {
    crate::llm::LlmError::Auth("provider refused the key".into())
}

/// The provider rate-limits the call for longer than the retry policy
/// waits, so the run is deferred (#278).
fn rate_limited() -> crate::llm::LlmError {
    crate::llm::LlmError::RateLimited {
        model: "claude-haiku".into(),
        retry_after: Some(std::time::Duration::from_secs(600)),
    }
}

/// A model dropped during its first call, which then returns `first`.
/// Every later call gives the final answer.
struct DroppedDuringCall {
    dropper: Dropper,
    first: First,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl crate::llm::LlmClient for DroppedDuringCall {
    async fn chat(
        &self,
        _request: crate::llm::ChatRequest,
    ) -> Result<ChatResponse, crate::llm::LlmError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) > 0 {
            return Ok(canned("done", 10, 5));
        }
        self.dropper.drop_now().await;
        match &self.first {
            First::Answers(response) => Ok(response.clone()),
            First::Fails(error) => Err(error()),
        }
    }
}

/// A tool dropped while it runs, which then finishes.
struct DroppedDuringTool {
    dropper: Dropper,
}

#[async_trait::async_trait]
impl Tool for DroppedDuringTool {
    fn name(&self) -> &str {
        DROPPED_TOOL
    }

    fn description(&self) -> &str {
        "a tool an operator drops the run during"
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn execute(
        &self,
        _ctx: &ToolContext<'_>,
        _params: Value,
    ) -> Result<ToolResult, ToolError> {
        self.dropper.drop_now().await;
        Ok(ToolResult::ok("the side effect happened"))
    }
}

const DROPPED_TOOL: &str = "dropped_tool";

/// A sink that marks the run's row finished, as a drop's fold would,
/// when the run publishes its `failed`: after the runner has read the row
/// and before the event is out.
struct MarksOnFailed {
    store: Arc<WorkerStore>,
    inner: Arc<RecordingSink>,
}

#[async_trait::async_trait]
impl EventSink for MarksOnFailed {
    async fn publish(&self, event: &Event) -> Result<u64, BusError> {
        if matches!(event.payload, EventPayload::Failed(_)) {
            self.store
                .mark_invocation_operator_terminal(
                    &event.envelope.invocation_id.to_string(),
                    "failed",
                    7_000,
                )
                .await
                .expect("the WAL writes");
        }
        self.inner.publish(event).await
    }
}

/// A broker that refuses a run's own `llm_error` `failed` for its size, as
/// it would one carrying a long provider error, and takes everything
/// else: the refusal is the publish's, and the same answer the runner gets
/// when it measures an event before storing it.
struct RefusesTheLlmErrorFailed {
    inner: Arc<RecordingSink>,
}

impl RefusesTheLlmErrorFailed {
    fn refuses(event: &Event) -> bool {
        matches!(
            &event.payload,
            EventPayload::Failed(failed)
                if matches!(failed.error_kind, crate::events::FailureKind::LlmError)
        )
    }
}

#[async_trait::async_trait]
impl EventSink for RefusesTheLlmErrorFailed {
    async fn publish(&self, event: &Event) -> Result<u64, BusError> {
        let body_len = serde_json::to_vec(event).unwrap().len();
        self.check_size(event, body_len)?;
        self.inner.publish(event).await
    }

    fn check_size(&self, event: &Event, body_len: usize) -> Result<(), BusError> {
        if Self::refuses(event) {
            return Err(BusError::PayloadTooLarge {
                event_type: crate::events::EventKind::Failed,
                size: body_len,
                limit: body_len - 1,
            });
        }
        Ok(())
    }
}

/// The drop's fold landing its mark at the last instant before the run's
/// own: a trigger on the WAL table, fired by the run's write of its row
/// finished with its stored `failed`, marks the row as the fold does and
/// abandons the run's write, which then finds the row finished.
async fn the_drop_lands_just_before_the_runs_mark(dir: &std::path::Path) {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(dir.join("events.db")))
        .await
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER the_drop_lands_first \
         BEFORE UPDATE OF terminal_outcome ON invocation_state \
         WHEN OLD.terminal_at IS NULL AND NEW.terminal_at IS NOT NULL \
              AND NEW.phase = 'failed' \
         BEGIN \
           UPDATE invocation_state SET phase = 'failed', updated_at = 7000, terminal_at = 7000 \
             WHERE invocation_id = OLD.invocation_id; \
           SELECT RAISE(IGNORE); \
         END",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}

/// Two copies of one event, compared in full: id, time, parent, payload
/// and annotations, as a reader of either would see them.
fn assert_same_event(left: &Event, right: &Event, what: &str) {
    assert_eq!(
        serde_json::to_value(left).unwrap(),
        serde_json::to_value(right).unwrap(),
        "{what}"
    );
}

/// What one dropped run left behind.
struct Dropped {
    outcome: Result<InvocationOutcome, ExecutorError>,
    events: Vec<Event>,
    row: Option<crate::worker::store::InvocationStateRow>,
    model_calls: usize,
    _dir: tempfile::TempDir,
}

impl Dropped {
    fn published(&self, kind: &str) -> usize {
        self.events
            .iter()
            .filter(|event| event.payload.event_type() == kind)
            .count()
    }

    /// The call in flight when the drop landed answered, and its usage and
    /// cost are on the stream: no spend figure is lost.
    fn assert_the_calls_spend_is_on_the_stream(&self) {
        let responses: Vec<&Event> = self
            .events
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::LlmResponse(_)))
            .collect();
        let first = responses.first().expect("the call's response is published");
        let EventPayload::LlmResponse(response) = &first.payload else {
            unreachable!()
        };
        assert_eq!(
            (response.usage.input_tokens, response.usage.output_tokens),
            (10, 5),
            "the call's usage"
        );
        let cost = first.envelope.cost.as_ref().expect("the call is priced");
        assert!(cost.total_cost > 0.0, "the call's cost: {cost:?}");
    }

    /// The run stopped without an ending of its own: an error no caller
    /// retries, so the dispatcher consumes the trigger.
    fn assert_stopped_without_an_ending(&self) {
        assert_eq!(self.published("completed"), 0, "no completed");
        assert_eq!(self.published("failed"), 0, "no failed");
        assert_eq!(
            self.published("invocation_archived"),
            0,
            "the archive is the sweeper's, from the row the drop finished"
        );
        let err = self
            .outcome
            .as_ref()
            .expect_err("the run stopped without an outcome of its own");
        assert!(
            matches!(err, ExecutorError::FinishedElsewhere { .. }),
            "{err}"
        );
        assert!(!err.is_transient(), "nothing retries it: {err}");
    }
}

/// One run's runner, store and agent, its drop done during `DROPPED_TOOL`
/// when `in_tool` says so.
struct World {
    runner: Arc<ReducerRunner<Harness>>,
    store: Arc<WorkerStore>,
    recording: Arc<RecordingSink>,
    agent: Agent,
    invocation_id: Uuid,
    dropper: Dropper,
    dir: tempfile::TempDir,
}

async fn a_world(in_tool: Option<Drop>, sink: SinkKind) -> World {
    let dir = tempdir().unwrap();
    let store = Arc::new(
        WorkerStore::open(&dir.path().join("events.db"))
            .await
            .unwrap(),
    );
    let invocation_id = Uuid::now_v7();
    let handle = Arc::new(OnceLock::new());
    let dropper = Dropper {
        runner: handle.clone(),
        store: store.clone(),
        invocation_id,
        drop: None,
    };
    let mut tools = ToolRegistry::with_builtins();
    tools.register_fixture(Arc::new(DroppedDuringTool {
        dropper: Dropper {
            drop: in_tool,
            ..dropper.clone()
        },
    }));
    let recording = Arc::new(RecordingSink::new());
    let sink: Arc<dyn EventSink> = match sink {
        SinkKind::Recording => recording.clone(),
        SinkKind::MarksOnFailed => Arc::new(MarksOnFailed {
            store: store.clone(),
            inner: recording.clone(),
        }),
        SinkKind::RefusesTheLlmErrorFailed => Arc::new(RefusesTheLlmErrorFailed {
            inner: recording.clone(),
        }),
    };
    let runner = Arc::new(ReducerRunner::new(
        Arc::new(ReducerContext::builder().tools(Arc::new(tools)).build()),
        Arc::new(
            RunnerConfig::builder()
                .event_sink(sink.clone())
                .pricing(test_pricing())
                .store(store.clone())
                .worker_id(test_worker_id())
                .build(),
        ),
        Harness::new(),
    ));
    handle.set(runner.clone()).ok().expect("set once");
    let agent = Agent::builder()
        .id(unique_agent_id("dropped-mid-step"))
        .model("claude-haiku")
        .system_prompt("be brief")
        .tools([DROPPED_TOOL])
        .budget(1.0)
        .build()
        .unwrap();
    World {
        runner,
        store,
        recording,
        agent,
        invocation_id,
        dropper,
        dir,
    }
}

impl World {
    /// A model that does `drop` during its first call, which returns `first`.
    fn model(&self, drop: Option<Drop>, first: First) -> DroppedDuringCall {
        DroppedDuringCall {
            dropper: Dropper {
                drop,
                ..self.dropper.clone()
            },
            first,
            calls: AtomicUsize::new(0),
        }
    }

    /// The run's fresh drive, asking `llm`.
    async fn run(&self, llm: &DroppedDuringCall) -> Result<InvocationOutcome, ExecutorError> {
        let (durable_start, _started) = DurableStart::channel(self.invocation_id);
        self.runner
            .run_signalling(
                &self.agent,
                llm,
                Trigger::named(
                    Uuid::now_v7(),
                    TriggerSource::Manual,
                    None,
                    json!({"input": "go"}),
                ),
                Some(1),
                durable_start,
            )
            .await
    }

    /// What the run left: its outcome, the events published from the
    /// `from`th on, and its row.
    async fn left(
        self,
        outcome: Result<InvocationOutcome, ExecutorError>,
        from: usize,
        llm: &DroppedDuringCall,
    ) -> Dropped {
        let row = self
            .store
            .get_invocation_state(&self.invocation_id.to_string())
            .await
            .unwrap();
        Dropped {
            outcome,
            events: self.recording.events().split_off(from),
            row,
            model_calls: llm.calls.load(Ordering::SeqCst),
            _dir: self.dir,
        }
    }
}

/// Drive one fresh run dropped during its first model call, which then
/// returns `first`, or during `DROPPED_TOOL` when `first` asks for it.
async fn a_dropped_run(
    drop: Option<Drop>,
    during: During,
    first: First,
    sink: SinkKind,
) -> Dropped {
    let (in_call, in_tool) = match during {
        During::ModelCall => (drop, None),
        During::Tool => (None, drop),
    };
    let world = a_world(in_tool, sink).await;
    let llm = world.model(in_call, first);
    let outcome = world.run(&llm).await;
    world.left(outcome, 0, &llm).await
}

/// Where in the step the drop lands.
#[derive(Clone, Copy)]
enum During {
    ModelCall,
    Tool,
}

/// Which sink a test publishes through.
#[derive(Clone, Copy)]
enum SinkKind {
    Recording,
    MarksOnFailed,
    RefusesTheLlmErrorFailed,
}

fn final_answer() -> First {
    First::Answers(canned("done", 10, 5))
}

fn a_tool_request(name: &str) -> First {
    First::Answers(tool_use(name, "tc-0", json!({}), (10, 5)))
}

const LIVE: Option<Drop> = Some(Drop {
    halt: true,
    handed_off: false,
});
const LIVE_HANDED_OFF: Option<Drop> = Some(Drop {
    halt: true,
    handed_off: true,
});
const NO_HALT: Option<Drop> = Some(Drop {
    halt: false,
    handed_off: false,
});
const NO_HALT_HANDED_OFF: Option<Drop> = Some(Drop {
    halt: false,
    handed_off: true,
});

/// Guard. A live drop during a model call that returns the final answer:
/// the halt stops the run at the next boundary, before the reducer turns
/// the answer into an outcome. The call's spend is on the stream.
#[tokio::test]
async fn a_live_drop_stops_the_run_at_the_next_step_boundary() {
    let run = a_dropped_run(LIVE, During::ModelCall, final_answer(), SinkKind::Recording).await;
    assert!(
        matches!(run.outcome, Ok(InvocationOutcome::Suspended { .. })),
        "{:?}",
        run.outcome
    );
    assert_eq!(run.published("completed"), 0);
    assert_eq!(run.published("failed"), 0);
    run.assert_the_calls_spend_is_on_the_stream();
    let row = run
        .row
        .as_ref()
        .expect("the drop's row stays for the sweeper");
    assert_eq!(
        (row.phase.as_str(), row.terminal_at),
        ("failed", Some(7_000))
    );
}

/// Guard. A live drop whose row is handed off during a model call that
/// asks for a tool: the run stops at the next boundary, writes no row,
/// and the tool is never run.
#[tokio::test]
async fn a_live_drop_whose_row_is_handed_off_writes_no_row() {
    let run = a_dropped_run(
        LIVE_HANDED_OFF,
        During::ModelCall,
        a_tool_request("builtin__self_inspect"),
        SinkKind::Recording,
    )
    .await;
    assert!(
        matches!(run.outcome, Ok(InvocationOutcome::Suspended { .. })),
        "{:?}",
        run.outcome
    );
    assert!(run.row.is_none(), "no row is re-created");
    assert_eq!(run.published("tool_result"), 0, "the tool never ran");
    run.assert_the_calls_spend_is_on_the_stream();
}

/// Red before the rule. A drop's mark on a runner holding no halt, during
/// a model call that returns the final answer: the runner's finishing
/// write does not land, and the run publishes no `completed`.
#[tokio::test]
async fn a_drop_without_a_halt_publishes_no_completed() {
    let run = a_dropped_run(
        NO_HALT,
        During::ModelCall,
        final_answer(),
        SinkKind::Recording,
    )
    .await;
    run.assert_stopped_without_an_ending();
    run.assert_the_calls_spend_is_on_the_stream();
    let row = run
        .row
        .as_ref()
        .expect("the drop's row stays for the sweeper");
    assert_eq!(
        (row.phase.as_str(), row.terminal_at),
        ("failed", Some(7_000)),
        "the row is the drop's"
    );
}

/// Red before the rule. A drop's mark and the row's hand-off on a runner
/// holding no halt, during a model call that asks for a tool: the next
/// checkpoint write finds no row and stops the run, rather than
/// re-creating an unfinished row that the next start would resume.
#[tokio::test]
async fn a_drop_without_a_halt_whose_row_is_handed_off_re_creates_no_row() {
    let run = a_dropped_run(
        NO_HALT_HANDED_OFF,
        During::ModelCall,
        a_tool_request("builtin__self_inspect"),
        SinkKind::Recording,
    )
    .await;
    run.assert_stopped_without_an_ending();
    run.assert_the_calls_spend_is_on_the_stream();
    assert!(run.row.is_none(), "no row is re-created");
    assert_eq!(run.model_calls, 1, "the run went no further");
    assert_eq!(run.published("tool_result"), 0, "the tool never ran");
}

/// Red before the rule. A live drop's mark during a model call that then
/// fails: the failure reaches the runner with no step boundary between,
/// and the run publishes no `failed` of its own. The call's
/// `llm.failure`, the record of what it cost, still goes out.
#[tokio::test]
async fn a_live_drop_then_a_failing_call_publishes_no_failed() {
    let run = a_dropped_run(
        LIVE,
        During::ModelCall,
        First::Fails(refused_key),
        SinkKind::Recording,
    )
    .await;
    run.assert_stopped_without_an_ending();
    assert_eq!(run.published("llm_failure"), 1, "the call's own record");
}

/// Guard. A tool already running when a live drop lands cannot be
/// interrupted: it finishes, its result is published and recorded, and
/// the run stops at the next boundary, before another model call.
#[tokio::test]
async fn a_tool_running_when_a_live_drop_lands_finishes_and_is_recorded() {
    let run = a_dropped_run(
        LIVE,
        During::Tool,
        a_tool_request(DROPPED_TOOL),
        SinkKind::Recording,
    )
    .await;
    assert!(
        matches!(run.outcome, Ok(InvocationOutcome::Suspended { .. })),
        "{:?}",
        run.outcome
    );
    assert_eq!(
        run.published("tool_result"),
        1,
        "the tool's result: {:?}",
        run.events
            .iter()
            .map(|event| event.payload.event_type())
            .collect::<Vec<_>>()
    );
    assert_eq!(run.model_calls, 1, "no model call after the tool");
    assert_eq!(run.published("completed"), 0);
    assert_eq!(run.published("failed"), 0);
}

/// **Red before the mark came first.** A drop's mark that lands after the
/// run has decided its mid-step `failed`, and before the run's own mark:
/// the drop marked the row first, so the run publishes no `failed` of its
/// own and no `invocation.archived` over the drop's, and stops with
/// `FinishedElsewhere`. The failing call was billed, and its usage and
/// cost still go out on its `llm.failure`.
///
/// Staged at both places that instant has been: when the run hands its
/// `failed` to the bus (`MarksOnFailed`), where a run that published
/// before it marked had already read its row, and at the run's own mark
/// (`the_drop_lands_just_before_the_runs_mark`).
#[tokio::test]
async fn a_drop_marking_just_before_the_runs_own_mark_leaves_no_failed_of_its_own() {
    let world = a_world(None, SinkKind::MarksOnFailed).await;
    the_drop_lands_just_before_the_runs_mark(world.dir.path()).await;
    let llm = world.model(None, billed_but_empty());
    let outcome = world.run(&llm).await;
    let run = world.left(outcome, 0, &llm).await;
    run.assert_stopped_without_an_ending();
    let row = run.row.as_ref().expect("the row stays for the sweeper");
    assert_eq!(
        (
            row.phase.as_str(),
            row.terminal_at,
            row.terminal_outcome.is_none()
        ),
        ("failed", Some(7_000), true),
        "the drop's mark, and nothing of the run's stored over it"
    );
    let failure = run
        .events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::LlmFailure(failure) => Some((failure, event.envelope.cost.as_ref())),
            _ => None,
        })
        .expect("the failing call's llm.failure");
    assert_eq!(
        failure.0.usage.map(|usage| usage.input_tokens),
        Some(1_000),
        "the call's usage"
    );
    assert!(
        failure.1.is_some_and(|cost| cost.total_cost > 0.0),
        "the call's cost: {:?}",
        failure.1
    );
}

/// **Red before the mark came first.** The mirror: the run's own mark
/// lands first, and the drop's mark after it, when the run hands its
/// `failed` to the bus. The run ended by itself: its own `failed` goes
/// out and is the ending, the one its row stores, and its own
/// `invocation.archived` follows. The drop's mark lands on nothing.
#[tokio::test]
async fn a_drop_marking_after_the_runs_own_mark_leaves_the_runs_failed_the_ending() {
    let run = a_dropped_run(
        None,
        During::ModelCall,
        First::Fails(refused_key),
        SinkKind::MarksOnFailed,
    )
    .await;
    let failed: Vec<&Event> = run
        .events
        .iter()
        .filter(|event| matches!(event.payload, EventPayload::Failed(_)))
        .collect();
    assert_eq!(failed.len(), 1, "the run's own failed");
    let EventPayload::Failed(payload) = &failed[0].payload else {
        unreachable!()
    };
    assert!(
        matches!(payload.error_kind, crate::events::FailureKind::LlmError),
        "with the run's own error: {:?}",
        payload.error_kind
    );
    assert_eq!(
        run.published("invocation_archived"),
        1,
        "and the run's own archive"
    );
    let row = run.row.as_ref().expect("the row awaits its archive's ack");
    assert_ne!(
        row.terminal_at,
        Some(7_000),
        "the row carries the run's own mark, not the drop's"
    );
    let stored = Event::from_wire(
        row.terminal_outcome
            .as_deref()
            .expect("the row stores its ending")
            .as_bytes(),
    )
    .unwrap();
    assert_same_event(
        &stored,
        failed[0],
        "the row stores the failed that went out",
    );
    assert!(row.terminal_published_at.is_some(), "and knows it went out");
    assert_eq!(
        Some(row.updated_at),
        row.terminal_at,
        "the mark is the row's last write, and moves its time to the mark's"
    );
}

/// **Red before the mark came first.** A publish lost after the mark: the
/// broker refuses the run's `failed` for a moment. The row is already
/// finished, with the `failed` stored in it and not stamped published, so
/// the next start does not resume the run (issue 1070, item 5: a run that
/// published its `failed` and died before its mark was resumed), and the
/// archive sweeper republishes the stored `failed`, the very event the
/// broker refused, then the archive; a second pass repeats the archive
/// only.
#[tokio::test]
async fn a_failed_lost_after_the_mark_is_republished_by_the_sweeper() {
    let world = a_world(None, SinkKind::Recording).await;
    // The run publishes `triggered`, `llm.request`, `llm.dispatched` and
    // `llm.failure`, then its `failed`: the fifth publish is refused.
    world.recording.fail_publish_at(4);
    let llm = world.model(None, billed_but_empty());
    let outcome = world.run(&llm).await;
    assert!(
        matches!(outcome, Err(ExecutorError::Bus(_))),
        "the publish failed: {outcome:?}"
    );
    let refused = world.recording.refused();
    let [refused] = &refused[..] else {
        panic!("one refused publish: {refused:?}");
    };
    assert!(
        matches!(refused.payload, EventPayload::Failed(_)),
        "the refused publish is the failed: {refused:?}"
    );
    world.recording.clear_fault();
    let id = world.invocation_id.to_string();
    let row = world
        .store
        .get_invocation_state(&id)
        .await
        .unwrap()
        .expect("the row is kept");
    assert!(
        row.terminal_at.is_some(),
        "the row is marked before the publish"
    );
    assert!(
        row.terminal_published_at.is_none(),
        "and its ending is owed"
    );
    let stored =
        Event::from_wire(row.terminal_outcome.as_deref().expect("stored").as_bytes()).unwrap();
    assert_same_event(
        &stored,
        refused,
        "the row stores the failed that was refused",
    );
    assert!(
        world
            .store
            .find_in_flight_invocations()
            .await
            .unwrap()
            .is_empty(),
        "nothing for the next start to resume"
    );

    let before = world.recording.events().len();
    let sweeper = crate::worker::archive_retry::ArchiveRetrySweeper::new_with_sink(
        world.recording.clone() as Arc<dyn EventSink>,
        test_worker_id(),
        world.store.clone(),
    );
    sweeper
        .sweep_once(&mut std::collections::HashSet::new())
        .await
        .unwrap();
    let after: Vec<Event> = world.recording.events().split_off(before);
    let kinds: Vec<&str> = after
        .iter()
        .map(|event| event.payload.event_type())
        .collect();
    assert_eq!(
        kinds,
        ["failed", "invocation_archived"],
        "the sweeper's republish"
    );
    assert_same_event(&after[0], refused, "the refused failed itself, in full");

    let again = world.recording.events().len();
    sweeper
        .sweep_once(&mut std::collections::HashSet::new())
        .await
        .unwrap();
    let kinds: Vec<&str> = world
        .recording
        .events()
        .split_off(again)
        .iter()
        .map(|event| event.payload.event_type())
        .collect();
    assert_eq!(
        kinds,
        ["invocation_archived"],
        "a second pass repeats the archive only"
    );
}

/// Red before the rule reached deferrals. A drop's mark on a runner
/// holding no halt, during a model call the provider then rate-limits past
/// the retry policy: the run stops without an ending, and announces no
/// deferral. Before, it published `invocation.deferred` and returned
/// `Deferred`, and the dispatcher then queued a resume that could only
/// find the row finished, holding the agent's concurrency slot for the
/// whole delay. Handed off as well, the run re-creates no row.
#[tokio::test]
async fn a_dropped_run_whose_call_is_then_rate_limited_is_not_deferred() {
    for drop in [NO_HALT, NO_HALT_HANDED_OFF] {
        let run = a_dropped_run(
            drop,
            During::ModelCall,
            First::Fails(rate_limited),
            SinkKind::Recording,
        )
        .await;
        assert_eq!(
            run.published("invocation_deferred"),
            0,
            "{drop:?}: no deferral is announced"
        );
        run.assert_stopped_without_an_ending();
        match drop.map(|drop| drop.handed_off) {
            Some(true) => assert!(run.row.is_none(), "no row is re-created"),
            _ => {
                let row = run.row.as_ref().expect("the drop's row");
                assert_eq!(
                    (row.phase.as_str(), row.terminal_at),
                    ("failed", Some(7_000)),
                    "the row is the drop's"
                );
            }
        }
    }
}

/// A call that comes back empty, and is billed for its prompt.
fn billed_but_empty() -> First {
    First::Answers(ChatResponse {
        parts: vec![],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 1_000,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cache_write_5m_tokens: None,
            cache_write_1h_tokens: None,
            reasoning_tokens: None,
        },
        reported_cost_usd: None,
    })
}

/// Guard. The issue's "the failure's usage still reaches the stream", for
/// a failure that was billed: a drop's mark on a runner holding no halt,
/// during a call that comes back empty but charged for its prompt. The
/// run stops without an ending, and the call's `llm.failure` carries its
/// usage and its cost, row marked or handed off.
#[tokio::test]
async fn a_dropped_runs_billed_failure_keeps_its_usage_and_cost() {
    for drop in [NO_HALT, NO_HALT_HANDED_OFF] {
        let run = a_dropped_run(
            drop,
            During::ModelCall,
            billed_but_empty(),
            SinkKind::Recording,
        )
        .await;
        run.assert_stopped_without_an_ending();
        let failure = run
            .events
            .iter()
            .find(|event| matches!(event.payload, EventPayload::LlmFailure(_)))
            .expect("the call's llm.failure");
        let EventPayload::LlmFailure(payload) = &failure.payload else {
            unreachable!()
        };
        assert_eq!(
            payload.usage.map(|usage| usage.input_tokens),
            Some(1_000),
            "{drop:?}: the call's usage"
        );
        let cost = failure.envelope.cost.as_ref().expect("the call is priced");
        assert!(cost.total_cost > 0.0, "{drop:?}: the call's cost: {cost:?}");
    }
}

/// Guard: a resumed drive notes its row. A run rate-limited at its first
/// call is deferred, and its drive ends with the row in flight. The real
/// `resume` drives it again; a drop marks the row and its hand-off
/// deletes it during the resumed drive's model call, which then fails.
/// The resumed drive began from a row of its own, so the missing row is
/// a hand-off, not a run that never started: no `failed` (in `setup` or
/// otherwise), the call's `llm.failure` out, `FinishedElsewhere`, no row,
/// and the note gone once the drive ends.
#[tokio::test]
async fn a_resumed_run_whose_row_a_drop_handed_off_publishes_no_failed() {
    let world = a_world(None, SinkKind::Recording).await;
    let deferred = world
        .run(&world.model(None, First::Fails(rate_limited)))
        .await;
    assert!(
        matches!(deferred, Ok(InvocationOutcome::Deferred { .. })),
        "{deferred:?}"
    );
    assert!(
        !world.runner.live.wrote_row(world.invocation_id),
        "the first drive's note went with it"
    );

    let from = world.recording.events().len();
    let llm = world.model(NO_HALT_HANDED_OFF, First::Fails(refused_key));
    let outcome = world
        .runner
        .resume(&world.agent, &llm, world.invocation_id)
        .await;
    let noted_after = world.runner.live.wrote_row(world.invocation_id);
    let run = world.left(outcome, from, &llm).await;
    run.assert_stopped_without_an_ending();
    assert_eq!(run.published("llm_failure"), 1, "the call's own record");
    assert!(run.row.is_none(), "no row is re-created");
    assert!(
        !noted_after,
        "the note is forgotten when the resumed drive ends"
    );
}

/// A mid-step `failed` the broker would refuse for its size is measured
/// before it is stored, as a step's terminal event is, and the run ends on
/// the refusal's `payload_too_large` `failed` instead: the row never
/// stores an ending that no publish or sweep could deliver. The one
/// stored is the one published, in full, and it carries the run's totals,
/// the billed call's cost among them: no spend figure is lost.
#[tokio::test]
async fn a_mid_step_failed_the_broker_would_refuse_is_stored_as_the_refusal() {
    let run = a_dropped_run(
        None,
        During::ModelCall,
        billed_but_empty(),
        SinkKind::RefusesTheLlmErrorFailed,
    )
    .await;
    let failed: Vec<&Event> = run
        .events
        .iter()
        .filter(|event| matches!(event.payload, EventPayload::Failed(_)))
        .collect();
    assert_eq!(failed.len(), 1, "one failed: {:?}", run.events);
    let EventPayload::Failed(payload) = &failed[0].payload else {
        unreachable!()
    };
    assert!(
        matches!(
            payload.error_kind,
            crate::events::FailureKind::PayloadTooLarge
        ),
        "the refusal's: {:?}",
        payload.error_kind
    );
    let billed = run
        .events
        .iter()
        .find(|event| matches!(event.payload, EventPayload::LlmFailure(_)))
        .and_then(|event| event.envelope.cost.as_ref())
        .expect("the billed call's cost")
        .total_cost;
    assert!(billed > 0.0);
    assert_eq!(
        (
            payload.partial_totals.total_cost,
            payload.partial_totals.total_llm_calls
        ),
        (billed, 0),
        "the run's totals: the billed call's cost, and no call that produced an outcome"
    );
    let row = run.row.as_ref().expect("the row awaits its archive's ack");
    let stored = Event::from_wire(
        row.terminal_outcome
            .as_deref()
            .expect("the row stores its ending")
            .as_bytes(),
    )
    .unwrap();
    assert_same_event(&stored, failed[0], "stored as published, in full");
}

/// A caller that builds a mid-step `failed` in `setup` for a run with a
/// WAL row has made a mistake the row corrects: the `failed` is stored,
/// and published, in `reducer`, since `setup` would tell every reader the
/// run never started (issue 1024).
#[tokio::test]
async fn a_mid_step_failed_built_in_setup_for_a_started_run_is_stored_in_reducer() {
    let world = a_world(None, SinkKind::Recording).await;
    let id = world.invocation_id.to_string();
    world
        .store
        .upsert_invocation_state(&crate::worker::store::InvocationStateRow {
            invocation_id: id.clone(),
            agent_id: world.agent.id().as_str().to_string(),
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
    let failed = super::super::terminal::failed_event(
        world.agent.id(),
        world.invocation_id,
        crate::events::FailureKind::LlmError,
        "the provider refused the key".to_string(),
        crate::events::FailurePhase::Setup,
        InvocationTotals::default(),
        None,
    );
    world
        .runner
        .end_mid_step(&mut None, failed)
        .await
        .expect("the run ends");
    let published: Vec<Event> = world.recording.events();
    let [event] = &published[..] else {
        panic!("one failed: {published:?}");
    };
    let EventPayload::Failed(payload) = &event.payload else {
        panic!("a failed: {event:?}");
    };
    assert!(
        matches!(payload.phase, crate::events::FailurePhase::Reducer),
        "{:?}",
        payload.phase
    );
    let row = world
        .store
        .get_invocation_state(&id)
        .await
        .unwrap()
        .unwrap();
    let stored = Event::from_wire(row.terminal_outcome.as_deref().unwrap().as_bytes()).unwrap();
    assert_same_event(&stored, event, "stored as published, in full");
}

/// A sink that loses the run's first `llm.failure` to a bus error, as a
/// broker away for a moment does, and refuses an `llm_error` `failed` for
/// its size, on publish and on measure.
struct LosesTheLlmFailureThenRefusesForSize {
    lost: AtomicUsize,
    inner: Arc<RecordingSink>,
}

#[async_trait::async_trait]
impl EventSink for LosesTheLlmFailureThenRefusesForSize {
    async fn publish(&self, event: &Event) -> Result<u64, BusError> {
        if matches!(event.payload, EventPayload::LlmFailure(_))
            && self.lost.fetch_add(1, Ordering::SeqCst) == 0
        {
            return Err(BusError::Publish("the broker is away".to_string()));
        }
        let body_len = serde_json::to_vec(event).unwrap().len();
        self.check_size(event, body_len)?;
        self.inner.publish(event).await
    }

    fn check_size(&self, event: &Event, body_len: usize) -> Result<(), BusError> {
        if RefusesTheLlmErrorFailed::refuses(event) {
            return Err(BusError::PayloadTooLarge {
                event_type: crate::events::EventKind::Failed,
                size: body_len,
                limit: body_len - 1,
            });
        }
        Ok(())
    }
}

/// A provider failure reproduced on resume whose `failed` is over the
/// broker's limit ends the run on the refusal's `payload_too_large`
/// `failed`, with the row finished and nothing left in flight. Before the
/// mark came first, that resume path published its `failed` unmeasured,
/// and a refusal left the row in flight for every start to resume into
/// again. The fault that sends the run to that path, a lost
/// `llm.failure`, also loses the call's cost from the stream: issue 1108,
/// not this change's.
#[tokio::test]
async fn an_oversized_failure_reproduced_on_resume_ends_on_the_refusal() {
    let dir = tempdir().unwrap();
    let store = Arc::new(
        WorkerStore::open(&dir.path().join("events.db"))
            .await
            .unwrap(),
    );
    let recording = Arc::new(RecordingSink::new());
    let sink = Arc::new(LosesTheLlmFailureThenRefusesForSize {
        lost: AtomicUsize::new(0),
        inner: recording.clone(),
    });
    let runner = Arc::new(ReducerRunner::new(
        Arc::new(
            ReducerContext::builder()
                .tools(Arc::new(ToolRegistry::with_builtins()))
                .build(),
        ),
        Arc::new(
            RunnerConfig::builder()
                .event_sink(sink.clone() as Arc<dyn EventSink>)
                .pricing(test_pricing())
                .store(store.clone())
                .worker_id(test_worker_id())
                .build(),
        ),
        Harness::new(),
    ));
    let agent = Agent::builder()
        .id(unique_agent_id("reproduced-oversized"))
        .model("claude-haiku")
        .system_prompt("be brief")
        .budget(1.0)
        .build()
        .unwrap();
    let invocation_id = Uuid::now_v7();
    let model = || DroppedDuringCall {
        dropper: Dropper {
            runner: Arc::new(OnceLock::new()),
            store: store.clone(),
            invocation_id,
            drop: None,
        },
        first: billed_but_empty(),
        calls: AtomicUsize::new(0),
    };
    let (durable_start, _started) = DurableStart::channel(invocation_id);
    let outcome = runner
        .run_signalling(
            &agent,
            &model(),
            Trigger::named(
                Uuid::now_v7(),
                TriggerSource::Manual,
                None,
                json!({"input": "go"}),
            ),
            Some(1),
            durable_start,
        )
        .await;
    assert!(matches!(outcome, Err(ExecutorError::Bus(_))), "{outcome:?}");
    assert_eq!(
        store.find_in_flight_invocations().await.unwrap().len(),
        1,
        "the lost llm.failure leaves the row in flight, unmarked"
    );

    let resumed = runner.resume(&agent, &model(), invocation_id).await;
    assert!(
        resumed.is_err(),
        "the reproduced failure ends the run: {resumed:?}"
    );
    let failed: Vec<Event> = recording
        .events()
        .into_iter()
        .filter(|event| matches!(event.payload, EventPayload::Failed(_)))
        .collect();
    let [failed] = &failed[..] else {
        panic!("one failed: {failed:?}");
    };
    let EventPayload::Failed(payload) = &failed.payload else {
        unreachable!()
    };
    assert!(
        matches!(
            payload.error_kind,
            crate::events::FailureKind::PayloadTooLarge
        ),
        "the refusal's: {:?}",
        payload.error_kind
    );
    let row = store
        .get_invocation_state(&invocation_id.to_string())
        .await
        .unwrap()
        .expect("the row awaits its archive's ack");
    assert!(row.terminal_at.is_some(), "the row is finished");
    let stored = Event::from_wire(row.terminal_outcome.as_deref().unwrap().as_bytes()).unwrap();
    assert_same_event(&stored, failed, "stored as published, in full");
    assert!(
        store.find_in_flight_invocations().await.unwrap().is_empty(),
        "nothing for the next start to resume"
    );
}

/// A model whose only call fails while an operator's drop really races
/// the run: a spawned task runs the drop's own mark, on its own pooled
/// connection, `delay` after the call returns.
struct RacedByADrop {
    store: Arc<WorkerStore>,
    invocation_id: Uuid,
    delay: std::time::Duration,
    handle: std::sync::Mutex<Option<tokio::task::JoinHandle<u64>>>,
}

#[async_trait::async_trait]
impl crate::llm::LlmClient for RacedByADrop {
    async fn chat(
        &self,
        _request: crate::llm::ChatRequest,
    ) -> Result<ChatResponse, crate::llm::LlmError> {
        let store = self.store.clone();
        let id = self.invocation_id.to_string();
        let delay = self.delay;
        let handle = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            store
                .mark_invocation_operator_terminal(&id, "failed", 7_000)
                .await
                .expect("the WAL writes")
        });
        *self.handle.lock().unwrap() = Some(handle);
        Err(refused_key())
    }
}

/// A real race between the drop's mark and the run's own, at delays
/// spread over a few milliseconds after the failing call returns: whichever
/// statement SQLite runs first, exactly one lands, and the stream agrees
/// with the row. Either winner passes, so the test does not depend on
/// timing to pass; the spread is so that both win across the runs. Bounded:
/// a fixed number of runs, each one model call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drop_racing_the_runs_own_mark_has_one_winner() {
    const RUNS: u64 = 48;
    const STEP_US: u64 = 100;
    for run in 0..RUNS {
        let world = a_world(None, SinkKind::Recording).await;
        let llm = RacedByADrop {
            store: world.store.clone(),
            invocation_id: world.invocation_id,
            delay: std::time::Duration::from_micros(run * STEP_US),
            handle: std::sync::Mutex::new(None),
        };
        let (durable_start, _started) = DurableStart::channel(world.invocation_id);
        let outcome = world
            .runner
            .run_signalling(
                &world.agent,
                &llm,
                Trigger::named(
                    Uuid::now_v7(),
                    TriggerSource::Manual,
                    None,
                    json!({"input": "go"}),
                ),
                Some(1),
                durable_start,
            )
            .await;
        let handle = llm
            .handle
            .lock()
            .unwrap()
            .take()
            .expect("the call was made");
        let drop_marked = handle.await.expect("the drop's task") == 1;
        let events = world.recording.events();
        let count = |kind: &str| {
            events
                .iter()
                .filter(|event| event.payload.event_type() == kind)
                .count()
        };
        let row = world
            .store
            .get_invocation_state(&world.invocation_id.to_string())
            .await
            .unwrap()
            .expect("the row is kept");
        assert_eq!(count("llm_failure"), 1, "run {run}: the call's record");
        if drop_marked {
            assert!(
                matches!(outcome, Err(ExecutorError::FinishedElsewhere { .. })),
                "run {run}: the drop won, outcome {outcome:?}"
            );
            assert_eq!(count("failed"), 0, "run {run}: the drop won, no failed");
            assert_eq!(count("invocation_archived"), 0, "run {run}: no archive");
            assert_eq!(row.terminal_at, Some(7_000), "run {run}: the drop's mark");
            assert!(row.terminal_outcome.is_none(), "run {run}: nothing stored");
        } else {
            assert!(
                matches!(outcome, Err(ExecutorError::Llm(_))),
                "run {run}: the run won, outcome {outcome:?}"
            );
            assert_eq!(count("failed"), 1, "run {run}: the run won, its failed");
            assert_eq!(count("invocation_archived"), 1, "run {run}: its archive");
            assert_ne!(row.terminal_at, Some(7_000), "run {run}: the run's mark");
            let failed = events
                .iter()
                .find(|event| matches!(event.payload, EventPayload::Failed(_)))
                .unwrap();
            let stored =
                Event::from_wire(row.terminal_outcome.as_deref().unwrap().as_bytes()).unwrap();
            assert_same_event(&stored, failed, "the stored failed is the published one");
            assert!(row.terminal_published_at.is_some(), "run {run}: stamped");
        }
    }
}
