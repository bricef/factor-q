//! A drop and a run's own ending, end to end (issue 1070).
//!
//! The real runner drives a run against a model the test holds, publishing
//! on a real broker. The coordination consumer folds the stream with the
//! worker's store beside it, so a drop's fold marks the run's WAL row as a
//! daemon's does. The worker's archive sweeper and its acknowledgement
//! consumer hand the finished row off. The drop is `drop_invocation`'s, as
//! the operator's command publishes it, or, where the test needs a drop
//! built while the run runs and published after the run's mark, the same
//! event built by hand.
//!
//! **The test drives every step that decides the order.** Nothing that
//! decides it runs on a timer: the sweeper is not spawned, and the test
//! runs one pass of it when the row is to be handed off. The
//! acknowledgement consumer starts only then, after the test has looked at
//! the row it would delete. A drop staged before the run's ending is
//! published by the run's own sink, and the sink waits for the drop's fold
//! before it lets the ending out. Every wait is for the exact state the
//! next step needs, a state nothing else in the test can move the row past.
//! Each place an order is staged is marked `Staged:`.
//!
//! The orders:
//!
//! 1. the drop's mark lands first: the run publishes no ending of its own,
//!    and the sweeper hands off the row the drop finished;
//! 2. the run's mark lands first, and the drop, requested after it, is
//!    folded before the run's ending: the run's ending replaces the drop's
//!    provisional one, and reads "already ended when the termination was
//!    requested";
//! 3. the run's mark lands first, and the drop is folded after the run has
//!    ended and been archived: the drop only records its request;
//! 4. the drop is requested while the run runs, and the run's mark lands
//!    before the drop reaches the stream: the run's ending replaces the
//!    drop's, and is after the request, "requested while running; it
//!    reached its own ending first";
//! 5. as 2, with the run's own `failed`: the run keeps its own failure.
//!
//! Each must leave an ownership row and an archive record that agree: the
//! row's status is the record's phase, the record holds the run's state as
//! its worker row held it, the row names its ender at the ending's own time
//! beside the request, and the worker's row is gone once the archive is
//! acknowledged.

use std::collections::HashSet;
use std::sync::Mutex;

use super::*;
use crate::agent::AgentId;
use crate::bus::{BusError, EventSink, STREAM_NAME};
use crate::control_plane::CoordinationConsumer;
use crate::control_plane::operator::{self, DropRecord};
use crate::control_plane::projection::ProjectionStore;
use crate::control_plane::store::{
    ArchiveState, ControlPlaneStore, EndedBy, OwnerRow, OwnerStatus,
};
use crate::events::{DropArchive, InvocationOperatorRecoveredPayload};
use crate::test_support::nats::{NatsServer, test_nats};
use crate::trigger::Trigger;
use crate::worker::store::InvocationStateRow;
use crate::worker::{ArchiveAckConsumer, ArchiveRetrySweeper, DurableStart};
use chrono::Utc;
use tokio::sync::{Notify, oneshot};

const WHO: &str = "on-call";
const WHY: &str = "runaway";

/// How long a wait for a state the test has staged may take before the
/// test says what it saw instead.
const DEADLINE: Duration = Duration::from_secs(20);

/// What the model answers once the test lets it.
#[derive(Clone, Copy)]
enum Answer {
    /// The final answer: the run completes.
    Done,
    /// An error no caller retries: the run fails by itself.
    Refused,
}

/// A model whose call waits, when `held`, until the test releases it.
struct Model {
    held: bool,
    answer: Answer,
    entered: Notify,
    release: Notify,
}

impl Model {
    fn answering(answer: Answer) -> Self {
        Self {
            held: false,
            answer,
            entered: Notify::new(),
            release: Notify::new(),
        }
    }

    fn held(answer: Answer) -> Self {
        Self {
            held: true,
            ..Self::answering(answer)
        }
    }
}

#[async_trait::async_trait]
impl crate::llm::LlmClient for Model {
    async fn chat(
        &self,
        _request: crate::llm::ChatRequest,
    ) -> Result<ChatResponse, crate::llm::LlmError> {
        if self.held {
            self.entered.notify_one();
            self.release.notified().await;
        }
        match self.answer {
            Answer::Done => Ok(canned("done", 10, 5)),
            Answer::Refused => Err(crate::llm::LlmError::Auth(
                "the provider refused the key".to_string(),
            )),
        }
    }
}

/// The drop a sink publishes just before the run's own ending.
enum StagedDrop {
    /// The operator's drop, published then, at that time.
    Now,
    /// A drop built earlier, while the run ran, published then.
    Built(Box<Event>),
}

/// The run's sink: the broker, with a drop staged before the run's own
/// ending when the test stages one, and the ending's own time kept.
struct Sink {
    bus: EventBus,
    projection: Arc<ProjectionStore>,
    control_plane: Arc<ControlPlaneStore>,
    agent: AgentId,
    drop: Mutex<Option<StagedDrop>>,
    ending_at: Mutex<Option<i64>>,
}

impl Sink {
    fn stage(&self, drop: StagedDrop) {
        *self.drop.lock().unwrap() = Some(drop);
    }

    /// The event time of the run's own `completed` or `failed`.
    fn ending_at(&self) -> i64 {
        self.ending_at
            .lock()
            .unwrap()
            .expect("the run published its own ending")
    }
}

#[async_trait::async_trait]
impl EventSink for Sink {
    async fn publish(&self, event: &Event) -> Result<u64, BusError> {
        if matches!(
            event.payload,
            EventPayload::Completed(_) | EventPayload::Failed(_)
        ) {
            *self.ending_at.lock().unwrap() = Some(event.envelope.timestamp.timestamp_millis());
            let staged = self.drop.lock().unwrap().take();
            if let Some(staged) = staged {
                let id = event.envelope.invocation_id;
                // Staged: the run's mark has landed; its ending waits.
                match staged {
                    StagedDrop::Now => {
                        drop_now(
                            &self.bus,
                            &self.projection,
                            &self.control_plane,
                            id,
                            &self.agent,
                        )
                        .await;
                    }
                    StagedDrop::Built(drop) => {
                        EventBus::publish(&self.bus, drop.as_ref()).await?;
                    }
                }
                row_until(
                    &self.control_plane,
                    id,
                    "the drop folded before the run's ending",
                    |row| row.status == OwnerStatus::Failed && is_dropped_provisionally(row),
                )
                .await;
                // Staged: the drop is folded; the run's ending goes out.
            }
        }
        EventBus::publish(&self.bus, event).await
    }

    fn check_size(&self, event: &Event, body_len: usize) -> Result<(), BusError> {
        EventSink::check_size(&self.bus, event, body_len)
    }
}

/// Whether `row` reads ended by a drop, provisionally, with the request.
fn is_dropped_provisionally(row: &OwnerRow) -> bool {
    row.ended_by == Some(EndedBy::Drop)
        && row.archive_state == ArchiveState::Pending
        && row.termination.is_some()
}

/// Wait until `id`'s ownership row satisfies `wanted`, and return it.
async fn row_until(
    control_plane: &ControlPlaneStore,
    id: Uuid,
    what: &str,
    wanted: impl Fn(&OwnerRow) -> bool,
) -> OwnerRow {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let row = control_plane
            .get_invocation_owner(&id.to_string())
            .await
            .unwrap();
        if let Some(row) = row.as_ref().filter(|row| wanted(row)) {
            return row.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "never {what}: the row is {row:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The broker, the stores, and the parts of a daemon beside a runner. The
/// archive sweeper is not spawned: the test runs its passes.
struct World {
    bus: EventBus,
    control_plane: Arc<ControlPlaneStore>,
    projection: Arc<ProjectionStore>,
    worker: Arc<WorkerStore>,
    worker_id: WorkerId,
    agent: Agent,
    sweeper: ArchiveRetrySweeper,
    acks_started: bool,
    stops: Vec<oneshot::Sender<()>>,
    _dir: tempfile::TempDir,
    _server: NatsServer,
}

impl World {
    /// A world whose coordination consumer is attached to the stream, so
    /// it folds everything published from here on.
    async fn new() -> Self {
        let server = test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let dir = tempdir().unwrap();
        let control_plane = Arc::new(
            ControlPlaneStore::open(&dir.path().join("control-plane.db"))
                .await
                .unwrap(),
        );
        let projection = Arc::new(
            ProjectionStore::open(&dir.path().join("projection.db"))
                .await
                .unwrap(),
        );
        let worker = Arc::new(
            WorkerStore::open(&dir.path().join("events.db"))
                .await
                .unwrap(),
        );
        let worker_id = test_worker_id();
        let agent = Agent::builder()
            .id(unique_agent_id("dropped-end-to-end"))
            .model("claude-haiku")
            .system_prompt("be brief")
            .budget(1.0)
            .build()
            .unwrap();
        let sweeper = ArchiveRetrySweeper::new(bus.clone(), worker_id.clone(), worker.clone());
        let mut world = Self {
            bus,
            control_plane,
            projection,
            worker,
            worker_id,
            agent,
            sweeper,
            acks_started: false,
            stops: Vec::new(),
            _dir: dir,
            _server: server,
        };
        let name = format!("end-to-end-{}", Uuid::now_v7().simple());
        let consumer = CoordinationConsumer::new(world.bus.clone(), world.control_plane.clone())
            .with_worker_store(world.worker.clone())
            .with_test_consumer_name(name.clone())
            .with_test_filter_subject(format!("fq.agent.{}.>", world.agent.id().as_str()));
        let stop = world.stop();
        tokio::spawn(async move { consumer.run(stop).await });
        world.consumer_attached(&name).await;
        world
    }

    /// Wait until the consumer's durable is on the stream: it delivers
    /// everything published after that.
    async fn consumer_attached(&self, name: &str) {
        let stream = self
            .bus
            .jetstream()
            .get_stream(STREAM_NAME)
            .await
            .expect("the event stream");
        let deadline = tokio::time::Instant::now() + DEADLINE;
        while stream
            .get_consumer::<async_nats::jetstream::consumer::pull::Config>(name)
            .await
            .is_err()
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the coordination consumer never attached"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn stop(&mut self) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.stops.push(tx);
        rx
    }

    /// The run's sink, with nothing staged.
    fn sink(&self) -> Arc<Sink> {
        Arc::new(Sink {
            bus: self.bus.clone(),
            projection: self.projection.clone(),
            control_plane: self.control_plane.clone(),
            agent: self.agent.id().clone(),
            drop: Mutex::new(None),
            ending_at: Mutex::new(None),
        })
    }

    /// A runner publishing through `sink`, on this world's WAL and worker.
    fn runner(&self, sink: Arc<Sink>) -> ReducerRunner<Harness> {
        ReducerRunner::new(
            Arc::new(
                ReducerContext::builder()
                    .tools(Arc::new(ToolRegistry::with_builtins()))
                    .build(),
            ),
            Arc::new(
                RunnerConfig::builder()
                    .event_sink(sink)
                    .pricing(test_pricing())
                    .store(self.worker.clone())
                    .worker_id(self.worker_id.clone())
                    .build(),
            ),
            Harness::new(),
        )
    }

    /// The operator's drop, as the command publishes it.
    async fn drop_it(&self, id: Uuid) {
        drop_now(
            &self.bus,
            &self.projection,
            &self.control_plane,
            id,
            self.agent.id(),
        )
        .await;
    }

    async fn row_until(
        &self,
        id: Uuid,
        what: &str,
        wanted: impl Fn(&OwnerRow) -> bool,
    ) -> OwnerRow {
        row_until(&self.control_plane, id, what, wanted).await
    }

    /// The run's worker row, finished: nothing in the test hands it off
    /// before [`Self::hand_off`].
    async fn finished_worker_row(&self, id: Uuid) -> InvocationStateRow {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            let row = self
                .worker
                .get_invocation_state(&id.to_string())
                .await
                .unwrap();
            if let Some(row) = row.as_ref().filter(|row| row.terminal_at.is_some()) {
                return row.clone();
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the worker's row never read finished: {row:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Hand the finished row off as a worker does: start the
    /// acknowledgement consumer, and run the sweeper's passes until the
    /// acknowledgement of an archive the pass published has deleted the
    /// row. An acknowledgement published before the consumer has
    /// subscribed is lost, as one is to a worker that is not listening,
    /// and the next pass publishes the archive again.
    async fn hand_off(&mut self, id: Uuid) {
        if !self.acks_started {
            let acks = ArchiveAckConsumer::new(
                self.bus.clone(),
                self.worker_id.clone(),
                self.worker.clone(),
            );
            let stop = self.stop();
            tokio::spawn(async move { acks.run(stop).await });
            self.acks_started = true;
        }
        let deadline = tokio::time::Instant::now() + DEADLINE;
        let mut warned = HashSet::new();
        loop {
            // Staged: one pass of the sweeper.
            self.sweeper
                .sweep_once(&mut warned)
                .await
                .expect("a pass of the sweeper");
            let row = self
                .worker
                .get_invocation_state(&id.to_string())
                .await
                .unwrap();
            if row.is_none() {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the archive was never acknowledged to the worker: {row:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The archive record: its phase and the state it holds.
    async fn record(&self, id: Uuid) -> (String, Vec<u8>) {
        self.control_plane
            .get_archive(&id.to_string())
            .await
            .unwrap()
            .map(|record| (record.final_phase, record.final_state_blob))
            .expect("the archive record")
    }
}

impl std::ops::Drop for World {
    fn drop(&mut self) {
        for stop in self.stops.drain(..) {
            let _ = stop.send(());
        }
    }
}

async fn drop_now(
    bus: &EventBus,
    projection: &ProjectionStore,
    control_plane: &ControlPlaneStore,
    id: Uuid,
    agent: &AgentId,
) {
    operator::drop_invocation(
        bus,
        projection,
        control_plane,
        &id.to_string(),
        &DropRecord {
            reason: Some(WHY.to_string()),
            requested_by: Some(WHO.to_string()),
            archive: Some(DropArchive::Pending),
        },
        Some(agent),
    )
    .await
    .expect("the drop is published");
}

/// The event `drop_invocation` publishes, built now and published later.
fn a_drop_built_now(agent: &AgentId, id: Uuid) -> Event {
    Event::new(
        agent.clone(),
        id,
        EventPayload::InvocationOperatorRecovered(InvocationOperatorRecoveredPayload {
            action: "drop".to_string(),
            final_phase: "failed".to_string(),
            reason: Some(WHY.to_string()),
            requested_by: Some(WHO.to_string()),
            archive: Some(DropArchive::Pending),
        }),
    )
}

/// Run the agent to the end on `runner`, asking `model`.
async fn run(
    runner: &ReducerRunner<Harness>,
    agent: &Agent,
    model: &Model,
    id: Uuid,
) -> Result<InvocationOutcome, ExecutorError> {
    let (durable_start, _started) = DurableStart::channel(id);
    runner
        .run_signalling(
            agent,
            model,
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

/// The request on `row`: its time, who asked and why.
fn the_request_on(row: &OwnerRow) -> (i64, Option<&str>, Option<&str>) {
    let request = row.termination.as_ref().expect("the request is on record");
    (
        request.requested_at,
        request.requested_by.as_deref(),
        request.reason.as_deref(),
    )
}

/// Whether `row` reads the run's own ending, `status`, archived.
fn is_the_runs_own_archived(row: &OwnerRow, status: OwnerStatus) -> bool {
    row.status == status
        && row.archive_state == ArchiveState::Archived
        && row.ended_by == Some(EndedBy::Run)
}

/// **1. The drop's mark lands first.** The run is in its model call when
/// the drop is folded and marks its row; the call then answers, and the
/// run, finding its row finished, publishes no ending of its own. The
/// sweeper hands the row off: the run reads `failed`, archived, ended by
/// the drop at the drop's own time, and the archive record holds the
/// run's state as its worker row held it, not an empty one.
#[tokio::test]
async fn a_drop_whose_mark_lands_first_is_archived_with_the_runs_state() {
    let mut world = World::new().await;
    let runner = world.runner(world.sink());
    let model = Model::held(Answer::Done);
    let id = Uuid::now_v7();
    let (outcome, worker_row) = tokio::join!(run(&runner, &world.agent, &model, id), async {
        model.entered.notified().await;
        // Staged: the run is in its model call.
        world.drop_it(id).await;
        let marked = world.finished_worker_row(id).await;
        world
            .row_until(id, "ended by the drop", |row| {
                row.status == OwnerStatus::Failed && is_dropped_provisionally(row)
            })
            .await;
        // Staged: the drop's mark is on the row; the model answers.
        model.release.notify_one();
        marked
    });
    assert!(
        matches!(outcome, Err(ExecutorError::FinishedElsewhere { .. })),
        "the run stops without an ending of its own: {outcome:?}"
    );
    world.hand_off(id).await;

    let row = world
        .row_until(id, "archived", |row| {
            row.archive_state == ArchiveState::Archived
        })
        .await;
    let (phase, state) = world.record(id).await;
    assert_eq!(
        (row.status, row.ended_by, phase.as_str()),
        (OwnerStatus::Failed, Some(EndedBy::Drop), "failed"),
        "ended by the drop, the row and the record agreeing"
    );
    let (requested_at, who, why) = the_request_on(&row);
    assert_eq!(
        row.ended_at,
        Some(requested_at),
        "the drop's ending is at the drop's own time"
    );
    assert_eq!((who, why), (Some(WHO), Some(WHY)));
    assert!(!state.is_empty(), "the record holds the run's state");
    assert_eq!(
        state, worker_row.state_blob,
        "the record holds the state the worker's row held"
    );
}

/// The run's own ending, folded after a drop staged before it, then
/// handed off: the row and the record as they then read, and the worker's
/// row as it was before the hand-off.
async fn a_run_with_a_drop_before_its_ending(
    world: &mut World,
    sink: &Arc<Sink>,
    model: &Model,
    id: Uuid,
    status: OwnerStatus,
) -> (OwnerRow, (String, Vec<u8>), InvocationStateRow) {
    let runner = world.runner(sink.clone());
    let outcome = run(&runner, &world.agent, model, id).await;
    match status {
        OwnerStatus::Completed => assert!(outcome.is_ok(), "the run completes: {outcome:?}"),
        _ => assert!(outcome.is_err(), "the run fails by itself: {outcome:?}"),
    }
    world
        .row_until(id, "the run's own ending, archived", |row| {
            is_the_runs_own_archived(row, status)
        })
        .await;
    let worker_row = world.finished_worker_row(id).await;
    world.hand_off(id).await;
    let row = world
        .row_until(id, "the run's own ending, archived", |row| {
            is_the_runs_own_archived(row, status)
        })
        .await;
    (row, world.record(id).await, worker_row)
}

/// **2. The run's mark lands first, and the drop, requested after it, is
/// folded before the run's ending.** The drop ends the run provisionally;
/// the run's own `completed`, folded next, replaces it, and the run's
/// archive follows. The run reads `completed`, archived, ended by the run
/// at its own ending's time, which is before the request's: "already
/// ended when the termination was requested", the same record as when the
/// drop is folded after the run's ending (3).
#[tokio::test]
async fn a_drop_folded_before_the_runs_own_ending_gives_way_to_it() {
    let mut world = World::new().await;
    let sink = world.sink();
    sink.stage(StagedDrop::Now);
    let id = Uuid::now_v7();
    let (row, (phase, state), worker_row) = a_run_with_a_drop_before_its_ending(
        &mut world,
        &sink,
        &Model::answering(Answer::Done),
        id,
        OwnerStatus::Completed,
    )
    .await;
    assert_eq!(phase, "completed", "the record agrees with the row");
    assert_eq!(
        state, worker_row.state_blob,
        "the record holds the run's state"
    );
    let (requested_at, who, why) = the_request_on(&row);
    assert_eq!(
        row.ended_at,
        Some(sink.ending_at()),
        "the run's ending, at its own time"
    );
    assert!(
        sink.ending_at() <= requested_at,
        "the run had already ended when the termination was requested: {row:?}"
    );
    assert_eq!((who, why), (Some(WHO), Some(WHY)));
}

/// **3. The run's mark lands first, and the drop is folded after the
/// run has ended and been archived.** The drop changes nothing but the
/// request: the run reads `completed`, archived, ended by the run before
/// the request, as in (2).
#[tokio::test]
async fn a_drop_folded_after_the_runs_ending_only_records_its_request() {
    let mut world = World::new().await;
    let sink = world.sink();
    let runner = world.runner(sink.clone());
    let id = Uuid::now_v7();
    run(&runner, &world.agent, &Model::answering(Answer::Done), id)
        .await
        .expect("the run completes");
    let before = world
        .row_until(id, "the run's own ending, archived", |row| {
            is_the_runs_own_archived(row, OwnerStatus::Completed)
        })
        .await;
    assert_eq!(before.termination, None, "no request yet");
    world.hand_off(id).await;
    // Staged: the run is ended, archived and handed off.
    world.drop_it(id).await;

    let row = world
        .row_until(id, "the drop's request on the row", |row| {
            row.termination.is_some()
        })
        .await;
    let (phase, _) = world.record(id).await;
    assert_eq!(
        (row.status, row.archive_state, row.ended_by, phase.as_str()),
        (
            OwnerStatus::Completed,
            ArchiveState::Archived,
            Some(EndedBy::Run),
            "completed"
        ),
        "the drop rewrote no ending, and the row and the record agree"
    );
    let (requested_at, who, why) = the_request_on(&row);
    assert_eq!(
        row.ended_at,
        Some(sink.ending_at()),
        "the ending's time is kept"
    );
    assert!(
        sink.ending_at() <= requested_at,
        "already ended when the termination was requested: {row:?}"
    );
    assert_eq!((who, why), (Some(WHO), Some(WHY)));
}

/// **4. The drop is requested while the run runs, and the run's mark
/// lands before the drop reaches the stream.** The drop's event is built
/// while the model call is held, at that time, as `drop_invocation` builds
/// it; the run then answers, marks its row and publishes; the drop reaches
/// the stream just before the run's `completed`. The drop ends the run
/// provisionally, and the run's own ending replaces it: `completed`,
/// archived, ended by the run at its own time, which is after the
/// request's. That is "requested while running; it reached its own ending
/// first".
#[tokio::test]
async fn a_drop_requested_while_the_run_runs_gives_way_to_an_ending_after_it() {
    let mut world = World::new().await;
    let sink = world.sink();
    let model = Model::held(Answer::Done);
    let id = Uuid::now_v7();
    let runner = world.runner(sink.clone());
    let (outcome, requested_at) = tokio::join!(run(&runner, &world.agent, &model, id), async {
        model.entered.notified().await;
        // Staged: the run is in its model call; the drop is requested.
        let drop = a_drop_built_now(world.agent.id(), id);
        let requested_at = drop.envelope.timestamp.timestamp_millis();
        while Utc::now().timestamp_millis() <= requested_at {
            tokio::task::yield_now().await;
        }
        sink.stage(StagedDrop::Built(Box::new(drop)));
        // Staged: the drop is built and staged; the model answers.
        model.release.notify_one();
        requested_at
    });
    outcome.expect("the run completes");
    world
        .row_until(id, "the run's own ending, archived", |row| {
            is_the_runs_own_archived(row, OwnerStatus::Completed)
        })
        .await;
    let worker_row = world.finished_worker_row(id).await;
    world.hand_off(id).await;

    let row = world
        .row_until(id, "the run's own ending, archived", |row| {
            is_the_runs_own_archived(row, OwnerStatus::Completed)
        })
        .await;
    let (phase, state) = world.record(id).await;
    assert_eq!(phase, "completed", "the record agrees with the row");
    assert_eq!(
        state, worker_row.state_blob,
        "the record holds the run's state"
    );
    assert_eq!(
        the_request_on(&row),
        (requested_at, Some(WHO), Some(WHY)),
        "the request, at the time it was made"
    );
    assert_eq!(
        row.ended_at,
        Some(sink.ending_at()),
        "the run's ending, at its own time"
    );
    assert!(
        sink.ending_at() > requested_at,
        "requested while running, the run reached its own ending first: {row:?}"
    );
}

/// **5. A run's own `failed` against a drop.** As in (2), the run's mark
/// lands first and the drop is folded before the run's ending, but the
/// run fails by itself: its model refuses the call. The run's `failed`
/// replaces the drop's provisional one: `failed`, archived, ended by the
/// run at its own time, before the request. The run keeps its own failure.
#[tokio::test]
async fn a_drop_folded_before_the_runs_own_failure_gives_way_to_it() {
    let mut world = World::new().await;
    let sink = world.sink();
    sink.stage(StagedDrop::Now);
    let id = Uuid::now_v7();
    let (row, (phase, state), worker_row) = a_run_with_a_drop_before_its_ending(
        &mut world,
        &sink,
        &Model::answering(Answer::Refused),
        id,
        OwnerStatus::Failed,
    )
    .await;
    assert_eq!(phase, "failed", "the record agrees with the row");
    assert_eq!(
        state, worker_row.state_blob,
        "the record holds the run's state"
    );
    let (requested_at, who, why) = the_request_on(&row);
    assert_eq!(
        row.ended_at,
        Some(sink.ending_at()),
        "the run's failure, at its own time"
    );
    assert!(
        sink.ending_at() <= requested_at,
        "the run had already failed when the termination was requested: {row:?}"
    );
    assert_eq!((who, why), (Some(WHO), Some(WHY)));
}
