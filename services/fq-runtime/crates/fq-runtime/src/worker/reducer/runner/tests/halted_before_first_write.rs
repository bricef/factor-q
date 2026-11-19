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
use crate::worker::reducer::types::StepOutput;
use crate::worker::{DurableStart, RowAtHalt};
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
    /// During the second step itself, the run's last: after the boundary's
    /// check, before the step's (finishing) write.
    InTheSecondStep,
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
    /// Arm the halt, which must find the run's row as `expected` says.
    fn arm(&self, expected: RowAtHalt) {
        let runner = self
            .runner
            .get()
            .and_then(Weak::upgrade)
            .expect("the runner is set and alive");
        assert_eq!(
            runner.arm_halt(self.invocation_id).map(|halt| halt.row),
            Some(expected),
            "the run is live here, so the halt is armed, and says where its row stands"
        );
    }
}

impl Reducer for HaltedInItsFirstStep {
    fn step(&self, input: StepInput) -> Result<StepOutput, HarnessError> {
        let step = self.steps.fetch_add(1, Ordering::SeqCst);
        if step == 0 && self.armed == ArmedAt::BeforeTheFirstWrite {
            self.arm(RowAtHalt::NeverWritten);
        }
        if step == 1 && self.armed == ArmedAt::InTheSecondStep {
            self.arm(RowAtHalt::Written);
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
            assert_eq!(
                runner.arm_halt(self.invocation_id).map(|halt| halt.row),
                Some(RowAtHalt::Written),
                "the first write has landed by the step's model call"
            );
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

/// **Only the first write takes a halt.** A halt armed during a later
/// step, after its boundary's check, is the next boundary's, as it always
/// was: the step in flight finishes. Here that step is the run's last, so
/// its finishing write lands and the run completes, its row with it; the
/// drop that follows is a request on a run that reached its own ending.
#[tokio::test]
async fn a_halt_armed_in_a_later_step_lets_that_step_finish() {
    let ran = run_armed(ArmedAt::InTheSecondStep).await;
    assert!(
        matches!(ran.outcome, Ok(InvocationOutcome::Completed { .. })),
        "the step in flight finishes, and it is the run's last: {:?}",
        ran.outcome
    );
    assert_eq!(ran.calls, 1);
    assert_eq!(
        ran.row.map(|row| row.phase),
        Some("completed".to_string()),
        "the finishing write landed"
    );
}

/// A runner over `store` whose reducer counts its steps and arms nothing,
/// with the model that answers at once.
fn mid_write_runner(
    store: &Arc<WorkerStore>,
    sink: &Arc<RecordingSink>,
    invocation_id: Uuid,
) -> (Arc<ReducerRunner<HaltedInItsFirstStep>>, Answering) {
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
            armed: ArmedAt::Never,
            steps: AtomicUsize::new(0),
        },
    ));
    slot.set(Arc::downgrade(&runner))
        .unwrap_or_else(|_| panic!("set once"));
    let model = Answering {
        reducer: slot,
        invocation_id,
        armed: ArmedAt::Never,
        calls: AtomicUsize::new(0),
    };
    (runner, model)
}

/// A store at `path`, and a second connection to it that holds the
/// store's write lock, so the run's first write waits inside SQLite.
async fn a_store_with_its_write_lock_held(
    path: &std::path::Path,
) -> (
    Arc<WorkerStore>,
    sqlx::pool::PoolConnection<sqlx::Sqlite>,
    sqlx::SqlitePool,
) {
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    let store = Arc::new(WorkerStore::open(path).await.unwrap());
    let other = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().filename(path))
        .await
        .unwrap();
    let mut held = other.acquire().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *held)
        .await
        .unwrap();
    (store, held, other)
}

/// Poll `run` until it is parked inside its first write. Everything from
/// the step to the start of the first write is synchronous, so once a poll
/// that ran the step returns `Pending`, the run is waiting inside the
/// write, which the held lock keeps it in.
async fn park_inside_its_first_write<F: std::future::Future>(
    runner: &ReducerRunner<HaltedInItsFirstStep>,
    mut run: std::pin::Pin<&mut F>,
) {
    let mut polls = 0;
    loop {
        assert!(
            futures::poll!(run.as_mut()).is_pending(),
            "the run cannot pass its first write while the write lock is held"
        );
        if runner.reducer.steps.load(Ordering::SeqCst) == 1 {
            return;
        }
        polls += 1;
        assert!(polls < 2000, "the run never reached its first step");
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
}

/// **A halt armed while the first write is under way stops the run before
/// its first model call, and the drop's mark is not lost.** Another writer
/// holds the store's write lock, so the run's first write waits inside
/// SQLite, past every check before it. The operator's live drop arms the
/// halt then, which finds the first write under way, and the drop's mark,
/// the coordination consumer's fold, wins the lock ahead of the row and
/// finds nothing to mark. The drop's handler, its publish done, gives the
/// run its word. When the lock goes, the row lands and the durable start
/// fires, which is true: the run wrote its row. The run then takes the
/// halt and, told the drop is published, marks its own row as the fold
/// would have, and stops, with no model call.
#[tokio::test]
async fn a_halt_armed_while_the_first_write_is_under_way_stops_the_run_before_its_model_call() {
    let dir = tempdir().unwrap();
    let (store, mut held, _other) =
        a_store_with_its_write_lock_held(&dir.path().join("worker.db")).await;

    let sink = Arc::new(RecordingSink::new());
    let invocation_id = Uuid::now_v7();
    let (runner, model) = mid_write_runner(&store, &sink, invocation_id);
    let (durable_start, fired) = DurableStart::channel(invocation_id);
    let agent = simple_responder_agent(&unique_agent_id("halted-mid-write"));
    let outcome = {
        let run = runner.run_signalling(
            &agent,
            &model,
            Trigger::mint(TriggerSource::Subject, None, json!({"input": "go"})),
            Some(1),
            durable_start,
        );
        tokio::pin!(run);
        park_inside_its_first_write(&runner, run.as_mut()).await;
        assert!(
            store
                .get_invocation_state(&invocation_id.to_string())
                .await
                .unwrap()
                .is_none(),
            "the first write has not landed yet"
        );
        let halt = runner
            .arm_halt(invocation_id)
            .expect("the run is live, so the halt arms");
        assert_eq!(
            halt.row,
            RowAtHalt::FirstWriteUnderWay,
            "the live drop's halt finds the first write under way"
        );
        // The drop's handler, once its publish returned.
        halt.word
            .expect("a halt that found the write under way carries the run's word")
            .published("failed");
        // The drop's mark, made on the connection that holds the lock: it
        // comes before the row and finds none.
        let marked = sqlx::query(
            "UPDATE invocation_state SET phase = 'failed', updated_at = 1, terminal_at = 1 \
             WHERE invocation_id = ? AND terminal_at IS NULL",
        )
        .bind(invocation_id.to_string())
        .execute(&mut *held)
        .await
        .unwrap()
        .rows_affected();
        assert_eq!(marked, 0, "the drop's mark came before the row");
        sqlx::query("COMMIT").execute(&mut *held).await.unwrap();
        drop(held);
        run.await
    };

    let calls = model.calls.load(Ordering::SeqCst);
    assert_eq!(
        calls, 0,
        "a run halted while its first write was under way made {calls} model call(s): its row \
         landed and the durable start fired, but the halt was seen only at the next step \
         boundary, after the step's model call; the run then stopped {outcome:?}, its row in \
         flight, which the drop's mark, made before the row existed, never reached"
    );
    assert!(
        matches!(outcome, Ok(InvocationOutcome::Suspended { .. })),
        "the row is there, so the run stops as at a boundary after its first write: {outcome:?}"
    );
    assert!(
        fired.await.is_ok(),
        "the durable start fired: the row is there"
    );
    assert_eq!(
        sink.events()
            .iter()
            .map(|e| EventKind::from(&e.payload))
            .collect::<Vec<_>>(),
        [EventKind::Triggered],
        "nothing is published after `triggered`"
    );
    let row = store
        .get_invocation_state(&invocation_id.to_string())
        .await
        .unwrap()
        .expect("the row landed");
    assert_eq!(
        (
            row.phase.as_str(),
            row.terminal_at.is_some(),
            row.terminal_outcome
        ),
        ("failed", true, None),
        "the drop's mark is on it, and no ending of the run's own"
    );
}

/// **A first write that fails while a halt is armed stops the run as
/// halted, so its trigger is not retried.** Another writer holds the
/// store's write lock across the run's first write, and the live drop arms
/// its halt then. When the lock goes, the write is refused: a trigger the
/// test installs under the held lock refuses the row. A store error is one
/// the dispatcher retries: it releases the trigger's claim, the trigger
/// comes back, and the work the operator dropped runs again under a new id
/// (the review ran it: redelivered after 30 seconds, then a model call and
/// `completed`). With a halt armed, the run must stop as halted before its
/// first write instead, `Unstarted` by the halt, which consumes the trigger.
#[tokio::test]
async fn a_first_write_that_fails_under_an_armed_halt_stops_the_run_as_halted() {
    let dir = tempdir().unwrap();
    let (store, mut held, _other) =
        a_store_with_its_write_lock_held(&dir.path().join("worker.db")).await;
    sqlx::query(
        "CREATE TRIGGER refuse_the_first_write BEFORE INSERT ON invocation_state \
         BEGIN SELECT RAISE(ABORT, 'the test refuses the first write'); END",
    )
    .execute(&mut *held)
    .await
    .unwrap();

    let sink = Arc::new(RecordingSink::new());
    let invocation_id = Uuid::now_v7();
    let (runner, model) = mid_write_runner(&store, &sink, invocation_id);
    let (durable_start, fired) = DurableStart::channel(invocation_id);
    let agent = simple_responder_agent(&unique_agent_id("halted-failed-write"));
    let outcome = {
        let run = runner.run_signalling(
            &agent,
            &model,
            Trigger::mint(TriggerSource::Subject, None, json!({"input": "go"})),
            Some(1),
            durable_start,
        );
        tokio::pin!(run);
        park_inside_its_first_write(&runner, run.as_mut()).await;
        let halt = runner
            .arm_halt(invocation_id)
            .expect("the run is live, so the halt arms");
        assert_eq!(halt.row, RowAtHalt::FirstWriteUnderWay);
        // The drop is published; the run has nothing to mark.
        halt.word
            .expect("a halt that found the write under way carries the run's word")
            .published("failed");
        sqlx::query("COMMIT").execute(&mut *held).await.unwrap();
        drop(held);
        run.await
    };

    match &outcome {
        Ok(InvocationOutcome::Unstarted {
            invocation_id: stopped,
            stopped_by: crate::worker::BoundaryStop::Halt,
        }) => assert_eq!(*stopped, invocation_id),
        other => panic!(
            "a run whose first write failed while an operator's halt was armed returned \
             {other:?}: the dispatcher takes a store error as transient, releases the trigger's \
             claim and retries it, so the work the operator dropped runs again under a new id; \
             the run must stop as halted before its first write (Unstarted, by the halt), which \
             consumes the trigger"
        ),
    }
    assert_eq!(model.calls.load(Ordering::SeqCst), 0, "no model call");
    assert!(
        store
            .get_invocation_state(&invocation_id.to_string())
            .await
            .unwrap()
            .is_none(),
        "the write was refused: no row"
    );
    assert!(fired.await.is_err(), "no durable start is signalled");
    assert_eq!(
        sink.events()
            .iter()
            .map(|e| EventKind::from(&e.payload))
            .collect::<Vec<_>>(),
        [EventKind::Triggered],
        "nothing is published after `triggered`"
    );
}

/// A workspace provider whose reclaim waits until the test lets it
/// through: the end of a drive held open.
#[derive(Debug)]
struct ParkedReclaim {
    inner: crate::worker::workspace::PerInvocationWorkspace,
    _root: tempfile::TempDir,
    reclaiming: AtomicUsize,
    gate: tokio::sync::Semaphore,
}

#[async_trait::async_trait]
impl crate::worker::workspace::WorkspaceProvider for ParkedReclaim {
    async fn provision(
        &self,
        invocation_id: Uuid,
    ) -> Result<std::path::PathBuf, crate::worker::workspace::WorkspaceError> {
        self.inner.provision(invocation_id).await
    }

    async fn reclaim(
        &self,
        invocation_id: Uuid,
        path: &std::path::Path,
    ) -> Result<(), crate::worker::workspace::WorkspaceError> {
        self.reclaiming.fetch_add(1, Ordering::SeqCst);
        self.gate.acquire().await.expect("gate open").forget();
        self.inner.reclaim(invocation_id, path).await
    }
}

/// **A halt armed while the drive ends, after its first write failed,
/// stops the run as halted.** The run's first write is refused (a trigger
/// the test installs under the held lock), with no halt armed, so the run
/// goes on to end in the store's error. Its drive is still live while it
/// ends: the grant servers' shutdown and the workspace's reclaim come
/// after, and the test holds the reclaim. The operator's live drop arms
/// its halt then, which finds no row and none to come. A store error is
/// one the dispatcher retries: it releases the trigger's claim, and the
/// work the operator dropped runs again under a new id (the review ran
/// it: NAKed for 30 seconds, the claim released). The drive must look for
/// a halt once more as it ends, and stop as halted before its first
/// write, which consumes the trigger.
#[tokio::test]
async fn a_halt_armed_while_the_drive_ends_after_a_failed_first_write_stops_it_as_halted() {
    let dir = tempdir().unwrap();
    let (store, mut held, _other) =
        a_store_with_its_write_lock_held(&dir.path().join("worker.db")).await;
    sqlx::query(
        "CREATE TRIGGER refuse_the_first_write BEFORE INSERT ON invocation_state \
         BEGIN SELECT RAISE(ABORT, 'the test refuses the first write'); END",
    )
    .execute(&mut *held)
    .await
    .unwrap();
    let root = tempdir().unwrap();
    let workspace = Arc::new(ParkedReclaim {
        inner: crate::worker::workspace::PerInvocationWorkspace::new(root.path().to_path_buf()),
        _root: root,
        reclaiming: AtomicUsize::new(0),
        gate: tokio::sync::Semaphore::new(0),
    });

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
                .workspace(Some(
                    workspace.clone() as Arc<dyn crate::worker::workspace::WorkspaceProvider>
                ))
                .build(),
        ),
        HaltedInItsFirstStep {
            inner: Harness::new(),
            runner: slot.clone(),
            invocation_id,
            armed: ArmedAt::Never,
            steps: AtomicUsize::new(0),
        },
    ));
    slot.set(Arc::downgrade(&runner))
        .unwrap_or_else(|_| panic!("set once"));
    let model = Answering {
        reducer: slot,
        invocation_id,
        armed: ArmedAt::Never,
        calls: AtomicUsize::new(0),
    };
    let (durable_start, fired) = DurableStart::channel(invocation_id);
    let agent = simple_responder_agent(&unique_agent_id("halted-as-it-ends"));
    let outcome = {
        let run = runner.run_signalling(
            &agent,
            &model,
            Trigger::mint(TriggerSource::Subject, None, json!({"input": "go"})),
            Some(1),
            durable_start,
        );
        tokio::pin!(run);
        park_inside_its_first_write(&runner, run.as_mut()).await;
        // No halt yet: the lock goes, the write is refused, the run looks
        // for a halt, finds none, and its drive ends as far as the reclaim.
        sqlx::query("COMMIT").execute(&mut *held).await.unwrap();
        drop(held);
        let mut polls = 0;
        while workspace.reclaiming.load(Ordering::SeqCst) == 0 {
            assert!(
                futures::poll!(run.as_mut()).is_pending(),
                "the reclaim holds the drive's end open"
            );
            polls += 1;
            assert!(polls < 2000, "the drive never reached its reclaim");
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        let halt = runner
            .arm_halt(invocation_id)
            .expect("the run's drive is still live as it ends, so the live drop's halt arms");
        assert_eq!(
            (halt.row, halt.word.is_some()),
            (RowAtHalt::NeverWritten, false),
            "the write failed: no row, and none to come"
        );
        workspace.gate.add_permits(1);
        run.await
    };

    match &outcome {
        Ok(InvocationOutcome::Unstarted {
            invocation_id: stopped,
            stopped_by: crate::worker::BoundaryStop::Halt,
        }) => assert_eq!(*stopped, invocation_id),
        other => panic!(
            "a run whose drive was ending after its first write failed, when an operator's \
             halt was armed, returned {other:?}: the dispatcher NAKs a store error and \
             releases the trigger's claim, so the work the operator dropped runs again under \
             a new id; the drive must look for a halt once more as it ends, and stop as \
             halted before its first write"
        ),
    }
    assert_eq!(model.calls.load(Ordering::SeqCst), 0, "no model call");
    assert!(fired.await.is_err(), "no durable start is signalled");
}
