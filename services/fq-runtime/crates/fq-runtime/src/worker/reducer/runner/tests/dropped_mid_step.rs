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

/// A model dropped during its first call, which then answers `first` (or
/// fails, when `first` is `None`). Every later call gives the final answer.
struct DroppedDuringCall {
    dropper: Dropper,
    first: Option<ChatResponse>,
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
        self.first
            .clone()
            .ok_or_else(|| crate::llm::LlmError::Auth("provider refused the key".into()))
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

/// Drive one fresh run dropped during its first model call, which then
/// answers `first` (or fails, when `None`), or during `DROPPED_TOOL` when
/// `first` asks for it.
async fn a_dropped_run(
    drop: Option<Drop>,
    during: During,
    first: Option<ChatResponse>,
    sink: SinkKind,
) -> Dropped {
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
        drop,
    };
    let (in_call, in_tool) = match during {
        During::ModelCall => (drop, None),
        During::Tool => (None, drop),
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
    let llm = DroppedDuringCall {
        dropper: Dropper {
            drop: in_call,
            ..dropper
        },
        first,
        calls: AtomicUsize::new(0),
    };
    let agent = Agent::builder()
        .id(unique_agent_id("dropped-mid-step"))
        .model("claude-haiku")
        .system_prompt("be brief")
        .tools([DROPPED_TOOL])
        .budget(1.0)
        .build()
        .unwrap();
    let (durable_start, _started) = DurableStart::channel(invocation_id);
    let outcome = runner
        .run_signalling(
            &agent,
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
    let row = store
        .get_invocation_state(&invocation_id.to_string())
        .await
        .unwrap();
    Dropped {
        outcome,
        events: recording.events(),
        row,
        model_calls: llm.calls.load(Ordering::SeqCst),
        _dir: dir,
    }
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
}

fn final_answer() -> Option<ChatResponse> {
    Some(canned("done", 10, 5))
}

fn a_tool_request(name: &str) -> Option<ChatResponse> {
    Some(tool_use(name, "tc-0", json!({}), (10, 5)))
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
    let run = a_dropped_run(LIVE, During::ModelCall, None, SinkKind::Recording).await;
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

/// Guard, and the one exception to the rule. A mid-step failure publishes
/// its `failed` before it marks the row, so a drop's mark that lands after
/// the runner has read the row and before the event is out lets the run's
/// own `failed` through: the run is recorded as failing with its own
/// error. The window is between one row read and one publish.
#[tokio::test]
async fn a_drop_landing_between_the_read_and_the_publish_lets_the_runs_own_failed_out() {
    let run = a_dropped_run(None, During::ModelCall, None, SinkKind::MarksOnFailed).await;
    assert_eq!(run.published("failed"), 1, "the run's own failed");
    let failed = run
        .events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::Failed(failed) => Some(failed),
            _ => None,
        })
        .unwrap();
    assert!(
        matches!(failed.error_kind, crate::events::FailureKind::LlmError),
        "the run's own error: {:?}",
        failed.error_kind
    );
    let row = run.row.as_ref().expect("the row stays for the sweeper");
    assert_eq!(
        (row.phase.as_str(), row.terminal_at),
        ("failed", Some(7_000)),
        "the drop's mark landed first, and the run's own did not land over it"
    );
}
