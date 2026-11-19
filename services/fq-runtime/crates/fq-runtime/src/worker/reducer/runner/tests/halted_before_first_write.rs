//! A run whose halt is armed before its first WAL write (issue 1070).
//!
//! An operator's `--live` drop arms a halt on the run and publishes the
//! drop; the coordination consumer then marks the run's WAL row finished.
//! A run checks the halt at the top of every step, the first included,
//! and a halt seen at the first boundary stops it as `Unstarted`: no row,
//! no model call. But between that check and the step's row being written
//! the reducer runs, and a halt armed in that instant used to be seen only
//! at the next boundary. By then the run had written its row, which the
//! drop's mark, made while there was no row, never reached, and had made
//! its first model call; it then stopped `Suspended`, its row in flight
//! until the next start.
//!
//! The first write now takes the halt's lock and is not made when a halt
//! is armed, so a halt armed before it stops the run exactly as one seen
//! at the first boundary does. The reducer below arms it from inside the
//! first step, the one place in that instant a test can stand.

use super::*;
use crate::bus::EventSink;
use crate::events::EventKind;
use crate::test_support::sim::RecordingSink;
use crate::trigger::Trigger;
use crate::worker::DurableStart;
use crate::worker::reducer::types::StepOutput;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{OnceLock, Weak};

/// When, in the run's first step, the halt is armed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ArmedAt {
    /// During the step itself: after the boundary's check, before the
    /// step's row is written.
    BeforeTheFirstWrite,
    /// During the step's model call: after the row is written.
    AfterTheFirstWrite,
    /// Never: the run is not dropped.
    Never,
}

/// The real reducer, with the operator's halt armed on the run from
/// inside its first step when `armed` says so.
struct HaltedInItsFirstStep {
    inner: Harness,
    runner: Arc<OnceLock<Weak<ReducerRunner<HaltedInItsFirstStep>>>>,
    invocation_id: Uuid,
    armed: ArmedAt,
    steps: AtomicUsize,
}

impl HaltedInItsFirstStep {
    fn arm(&self) {
        let runner = self
            .runner
            .get()
            .and_then(Weak::upgrade)
            .expect("the runner is set and alive");
        assert!(
            runner.request_halt(self.invocation_id),
            "the run is live here, so the halt is armed"
        );
    }
}

impl Reducer for HaltedInItsFirstStep {
    fn step(&self, input: StepInput) -> Result<StepOutput, HarnessError> {
        if self.steps.fetch_add(1, Ordering::SeqCst) == 0
            && self.armed == ArmedAt::BeforeTheFirstWrite
        {
            self.arm();
        }
        self.inner.step(input)
    }
}

/// A model that answers every call with a final answer, counting them,
/// and arms the halt during the first call when the run is armed after
/// its first write.
struct Answering {
    reducer: Arc<OnceLock<Weak<ReducerRunner<HaltedInItsFirstStep>>>>,
    invocation_id: Uuid,
    armed: ArmedAt,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl crate::llm::LlmClient for Answering {
    async fn chat(
        &self,
        _request: crate::llm::ChatRequest,
    ) -> Result<ChatResponse, crate::llm::LlmError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0
            && self.armed == ArmedAt::AfterTheFirstWrite
        {
            let runner = self
                .reducer
                .get()
                .and_then(Weak::upgrade)
                .expect("the runner is set and alive");
            assert!(runner.request_halt(self.invocation_id));
        }
        Ok(end_turn_response("done."))
    }
}

/// What a run with its halt armed at `armed` did.
struct Ran {
    outcome: Result<InvocationOutcome, ExecutorError>,
    invocation_id: Uuid,
    calls: usize,
    row: Option<crate::worker::store::InvocationStateRow>,
    durably_started: bool,
    events: Vec<EventKind>,
}

async fn run_armed(armed: ArmedAt) -> Ran {
    let dir = tempdir().unwrap();
    let store = Arc::new(
        WorkerStore::open(&dir.path().join("worker.db"))
            .await
            .unwrap(),
    );
    let sink = Arc::new(RecordingSink::new());
    let invocation_id = Uuid::now_v7();
    let slot = Arc::new(OnceLock::new());
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
        HaltedInItsFirstStep {
            inner: Harness::new(),
            runner: slot.clone(),
            invocation_id,
            armed,
            steps: AtomicUsize::new(0),
        },
    ));
    slot.set(Arc::downgrade(&runner))
        .unwrap_or_else(|_| panic!("set once"));
    let model = Answering {
        reducer: slot,
        invocation_id,
        armed,
        calls: AtomicUsize::new(0),
    };
    let (durable_start, fired) = DurableStart::channel(invocation_id);
    let outcome = runner
        .run_signalling(
            &simple_responder_agent(&unique_agent_id("halted-first-step")),
            &model,
            Trigger::mint(TriggerSource::Subject, None, json!({"input": "go"})),
            Some(1),
            durable_start,
        )
        .await;
    Ran {
        outcome,
        invocation_id,
        calls: model.calls.load(Ordering::SeqCst),
        row: store
            .get_invocation_state(&invocation_id.to_string())
            .await
            .unwrap(),
        durably_started: fired.await.is_ok(),
        events: sink
            .events()
            .iter()
            .map(|e| EventKind::from(&e.payload))
            .collect(),
    }
}

/// **A halt armed before the first write stops the run before it.** The
/// run writes no row, makes no model call, signals no durable start, and
/// returns what a halt at its first boundary returns: `Unstarted`, by the
/// halt, which the dispatcher takes as the drop's outcome and consumes
/// the trigger for. Nothing is published after `triggered`.
#[tokio::test]
async fn a_halt_armed_before_the_first_write_stops_the_run_before_it() {
    let ran = run_armed(ArmedAt::BeforeTheFirstWrite).await;
    assert!(
        ran.row.is_none() && ran.calls == 0,
        "a run halted before its first WAL write wrote its row and made {} model call(s): the \
         halt was seen only at the next step boundary, after the row the drop's mark could not \
         reach (it came before the row existed) and after the call; the run then stopped \
         {:?}, its row {:?}, in flight until the next start",
        ran.calls,
        ran.outcome,
        ran.row.as_ref().map(|row| (&row.phase, row.terminal_at)),
    );
    match ran.outcome {
        Ok(InvocationOutcome::Unstarted {
            invocation_id,
            stopped_by: crate::worker::BoundaryStop::Halt,
        }) => assert_eq!(invocation_id, ran.invocation_id),
        other => panic!("stops as at its first boundary, by the halt: {other:?}"),
    }
    assert!(!ran.durably_started, "no durable start is signalled");
    assert_eq!(
        ran.events,
        [EventKind::Triggered],
        "nothing is published after `triggered`"
    );
}

/// **A halt armed after the first write is the next boundary's, as
/// before.** The row is written and the step's model call made; the halt
/// stops the run at the next boundary, `Suspended`, with its row in
/// flight for the drop's mark to finish.
#[tokio::test]
async fn a_halt_armed_after_the_first_write_stops_the_run_at_its_next_boundary() {
    let ran = run_armed(ArmedAt::AfterTheFirstWrite).await;
    assert_eq!(ran.calls, 1, "the step's model call is made");
    match ran.outcome {
        Ok(InvocationOutcome::Suspended { invocation_id }) => {
            assert_eq!(invocation_id, ran.invocation_id)
        }
        other => panic!("stops at the next boundary: {other:?}"),
    }
    let row = ran.row.expect("the first write landed");
    assert_eq!(row.terminal_at, None, "in flight, for the drop's mark");
    assert!(ran.durably_started);
}

/// **A run never halted is not touched by the first write's check.** It
/// writes its row, signals its durable start and completes.
#[tokio::test]
async fn a_run_never_halted_writes_its_first_row_and_completes() {
    let ran = run_armed(ArmedAt::Never).await;
    assert!(
        matches!(ran.outcome, Ok(InvocationOutcome::Completed { .. })),
        "{:?}",
        ran.outcome
    );
    assert_eq!(ran.calls, 1);
    assert!(ran.durably_started);
    assert_eq!(ran.row.map(|row| row.phase), Some("completed".to_string()));
}
