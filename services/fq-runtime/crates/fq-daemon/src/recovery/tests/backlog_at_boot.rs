//! Startup recovery decides only after the coordination consumer has
//! folded what the stopped daemon left on the event stream (issue 1070).
//!
//! Each test leaves what a daemon that died leaves: a run in flight in the
//! WAL, and an event about it on the stream that no consumer folded. The
//! event is published straight onto the stream while no coordination
//! consumer runs, which is the dead daemon's state exactly: on the stream,
//! not folded. The next start then runs as `assemble` runs it:
//! `fold_backlog`, then `reconcile_terminal_owners` and
//! `classify_in_flight`, then the split into the runs resumed now and the
//! runs held for a later start. Nothing is timed but the bound on the fold.

use super::*;
use crate::recovery::backlog::FoldProbe;
use crate::resume::ResumeControl;
use fq_runtime::control_plane::{ArchiveState, OwnerStatus};
use fq_runtime::events::{
    FailedPayload, FailureKind, FailurePhase, HostNoticePayload,
    InvocationOperatorRecoveredPayload, InvocationTotals,
};

/// The next start, as far as `assemble` takes recovery: the coordination
/// consumer it started, the runs it resumes now, and the runs it holds.
struct NextBoot {
    coordination: Coordination,
    recoverable: Vec<ClassifiedInvocation>,
    held: Vec<String>,
    worker_id: WorkerId,
}

impl NextBoot {
    /// Start over these stores, with the fold bounded by `wait_secs`.
    async fn start(
        bus: &EventBus,
        cp_store: &Arc<ControlPlaneStore>,
        worker_store: &Arc<WorkerStore>,
        wait_secs: u64,
    ) -> Self {
        let config = fq_runtime::Config::from_toml_str(&format!(
            "[worker]\nrecovery_backlog_wait_secs = {wait_secs}\n"
        ))
        .unwrap();
        let runtime_id = Uuid::now_v7();
        let worker_id = WorkerId::new(runtime_id.to_string()).unwrap();
        let (coordination, backlog) =
            fold_backlog(bus, cp_store, worker_store, runtime_id, &worker_id, &config)
                .await
                .expect("start the coordination consumer");
        reconcile_terminal_owners(
            worker_store,
            cp_store,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .expect("reconcile terminal owners");
        let recovered = classify_in_flight(worker_store, cp_store, bus, runtime_id, &worker_id)
            .await
            .expect("startup recovery");
        let found = ids(&recovered.recoverable);
        let (recoverable, sweeps) = backlog.hold(recovered.recoverable);
        sweeps.release();
        let resumed = ids(&recoverable);
        let held = found
            .into_iter()
            .filter(|id| !resumed.contains(id))
            .collect();
        Self {
            coordination,
            recoverable,
            held,
            worker_id,
        }
    }

    fn resumes(&self) -> Vec<String> {
        ids(&self.recoverable)
    }

    fn holds(&self) -> Vec<String> {
        self.held.clone()
    }

    /// Stop the consumer, as the daemon's teardown does.
    async fn stop(self) {
        let Coordination {
            handle, shutdown, ..
        } = self.coordination;
        let _ = shutdown.send(());
        tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("the consumer stops")
            .expect("its task joins")
            .expect("it exits cleanly");
    }
}

fn ids(runs: &[ClassifiedInvocation]) -> Vec<String> {
    runs.iter().map(|c| c.state.invocation_id.clone()).collect()
}

/// A run of [`CAPPED_AGENT`] in flight in the WAL, resumable.
async fn a_run_in_flight(worker_store: &WorkerStore) -> Uuid {
    let invocation = Uuid::now_v7();
    let mut row = first_wal_row(&invocation.to_string());
    row.agent_id = CAPPED_AGENT.to_string();
    worker_store.upsert_invocation_state(&row).await.unwrap();
    invocation
}

/// Publish `payload` for `invocation` onto the event stream.
async fn publish(bus: &EventBus, invocation: Uuid, payload: EventPayload) -> u64 {
    bus.publish(&Event::new(
        AgentId::new(CAPPED_AGENT).unwrap(),
        invocation,
        payload,
    ))
    .await
    .expect("publish onto the event stream")
}

fn a_drop() -> EventPayload {
    EventPayload::InvocationOperatorRecovered(InvocationOperatorRecoveredPayload {
        action: "drop".to_string(),
        final_phase: "failed".to_string(),
        reason: Some("dropped just before the daemon died".to_string()),
    })
}

fn its_own_failed() -> EventPayload {
    EventPayload::Failed(FailedPayload {
        error_kind: FailureKind::LlmError,
        error_message: "the provider refused the request".to_string(),
        phase: FailurePhase::LlmRequest,
        partial_totals: InvocationTotals::default(),
        detail: None,
    })
}

/// An event declaring a schema version this build does not read: the
/// coordination consumer halts on it, and folds nothing after it.
async fn publish_an_event_this_build_cannot_read(bus: &EventBus) {
    let mut event = serde_json::to_value(Event::new(
        AgentId::new(CAPPED_AGENT).unwrap(),
        Uuid::now_v7(),
        EventPayload::HostNotice(HostNoticePayload {
            kind: "from_a_newer_build".to_string(),
            body: String::new(),
        }),
    ))
    .unwrap();
    event["envelope"]["schema_version"] = serde_json::json!(99);
    bus.jetstream()
        .publish(
            format!("fq.agent.{CAPPED_AGENT}.host_notice"),
            event.to_string().into(),
        )
        .await
        .expect("publish")
        .await
        .expect("stored");
}

async fn owner(
    cp_store: &ControlPlaneStore,
    invocation: Uuid,
) -> Option<(OwnerStatus, ArchiveState)> {
    cp_store
        .get_invocation_owner(&invocation.to_string())
        .await
        .unwrap()
        .map(|owner| (owner.status, owner.archive_state))
}

async fn wal_row(worker_store: &WorkerStore, invocation: Uuid) -> InvocationStateRow {
    worker_store
        .get_invocation_state(&invocation.to_string())
        .await
        .unwrap()
        .expect("the WAL row is still there")
}

/// **Item 3: a drop the stopped daemon published and never folded.** The
/// next start folds it before recovery reads anything: the drop marks the
/// run's WAL row terminal and ends its ownership row, so recovery finds
/// nothing in flight, resumes nothing and holds nothing.
#[tokio::test]
async fn a_drop_on_the_stream_is_folded_before_recovery_decides() {
    let server = fq_test_support::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let scratch = tempfile::tempdir().unwrap();
    let (cp_store, worker_store) = stores_under(scratch.path()).await;
    let invocation = a_run_in_flight(&worker_store).await;
    publish(&bus, invocation, a_drop()).await;

    let next = NextBoot::start(&bus, &cp_store, &worker_store, 30).await;
    assert_eq!(
        (next.resumes(), next.holds()),
        (Vec::new(), Vec::new()),
        "the dropped run is neither resumed nor held"
    );
    assert!(
        wal_row(&worker_store, invocation)
            .await
            .terminal_at
            .is_some(),
        "the drop's fold marked the WAL row terminal"
    );
    assert_eq!(
        owner(&cp_store, invocation).await,
        Some((OwnerStatus::Failed, ArchiveState::Archived)),
        "the drop ended the ownership row"
    );
    next.stop().await;
}

/// **Item 5: the run's own `failed`, published and never folded.** A run
/// that fails part-way through a step publishes `failed` before it marks
/// its WAL row. The next start folds the `failed` first, so the ownership
/// row records the ending, and recovery marks the WAL row terminal from it
/// and resumes nothing.
#[tokio::test]
async fn a_failed_on_the_stream_is_folded_before_recovery_decides() {
    let server = fq_test_support::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let scratch = tempfile::tempdir().unwrap();
    let (cp_store, worker_store) = stores_under(scratch.path()).await;
    let invocation = a_run_in_flight(&worker_store).await;
    publish(&bus, invocation, its_own_failed()).await;

    let next = NextBoot::start(&bus, &cp_store, &worker_store, 30).await;
    assert_eq!(
        (next.resumes(), next.holds()),
        (Vec::new(), Vec::new()),
        "the failed run is neither resumed nor held"
    );
    let row = wal_row(&worker_store, invocation).await;
    assert_eq!(
        (row.phase.as_str(), row.terminal_at.is_some()),
        ("failed", true),
        "recovery marked the WAL row terminal from the ownership row"
    );
    assert_eq!(
        owner(&cp_store, invocation).await,
        Some((OwnerStatus::Failed, ArchiveState::Pending)),
        "the ownership row follows the `failed`, its archive pending"
    );
    next.stop().await;
}

/// **Item 6: the ownership row and the archive record agree.** Over the
/// same start as item 5, the archive sweeper hands the terminal WAL row
/// off and the consumer folds the archive: it records `failed`, as the
/// ownership row does. Before, the run was resumed, could complete, and
/// archived `completed` under a row that read `failed`.
#[tokio::test]
async fn the_archive_of_a_run_whose_failed_was_folded_at_boot_agrees_with_its_row() {
    let server = fq_test_support::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let scratch = tempfile::tempdir().unwrap();
    let (cp_store, worker_store) = stores_under(scratch.path()).await;
    let invocation = a_run_in_flight(&worker_store).await;
    publish(&bus, invocation, its_own_failed()).await;

    let next = NextBoot::start(&bus, &cp_store, &worker_store, 30).await;
    assert!(
        next.resumes().is_empty(),
        "not resumed into a second ending"
    );
    fq_runtime::ArchiveRetrySweeper::new(bus.clone(), next.worker_id.clone(), worker_store.clone())
        .sweep_once(&mut HashSet::new())
        .await
        .expect("the archive sweeper hands the row off");
    // The fold writes the archive record first and then moves the row, so
    // wait for the row.
    let deadline = Instant::now() + Duration::from_secs(10);
    let archive = loop {
        let archive = cp_store.get_archive(&invocation.to_string()).await.unwrap();
        if let Some(archive) = archive
            && matches!(
                owner(&cp_store, invocation).await,
                Some((_, ArchiveState::Archived))
            )
        {
            break archive;
        }
        assert!(Instant::now() < deadline, "the archive is never folded");
        tokio::time::sleep(POLL).await;
    };
    assert_eq!(
        archive.final_phase, "failed",
        "the archive records the ending"
    );
    assert_eq!(
        owner(&cp_store, invocation).await,
        Some((OwnerStatus::Failed, ArchiveState::Archived)),
        "and the row the same ending, archived"
    );
    next.stop().await;
}

/// **The bound, and a second start while the fold is still behind.** The
/// stopped daemon left an event this build cannot read ahead of a drop:
/// the consumer halts on it and never reaches the drop. The start waits
/// out its bound, goes on, and resumes nothing: the run it would have
/// resumed is held, in flight, its drop unfolded. The next start, behind
/// the same event, holds it again.
#[tokio::test]
async fn a_run_the_fold_did_not_reach_in_time_is_held_at_every_start() {
    let server = fq_test_support::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let scratch = tempfile::tempdir().unwrap();
    let (cp_store, worker_store) = stores_under(scratch.path()).await;
    let invocation = a_run_in_flight(&worker_store).await;
    publish_an_event_this_build_cannot_read(&bus).await;
    publish(&bus, invocation, a_drop()).await;

    for start in ["first", "second"] {
        let next = NextBoot::start(&bus, &cp_store, &worker_store, 1).await;
        assert_eq!(
            (next.resumes(), next.holds()),
            (Vec::new(), vec![invocation.to_string()]),
            "the {start} start resumes nothing and holds the run"
        );
        assert_eq!(
            wal_row(&worker_store, invocation).await.terminal_at,
            None,
            "the drop is not folded, so the run is still in flight"
        );
        next.stop().await;
    }
}

/// **A halted consumer ends the wait at once.** The stopped daemon left
/// an event this build cannot read ahead of a drop, so the consumer halts
/// on it and folds nothing more in this process, however long the boot
/// waits. With the default thirty-second bound, the start still holds the
/// run, and returns as soon as the consumer has halted rather than when the
/// bound runs out.
#[tokio::test]
async fn a_halted_consumer_ends_the_wait_at_once() {
    let server = fq_test_support::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let scratch = tempfile::tempdir().unwrap();
    let (cp_store, worker_store) = stores_under(scratch.path()).await;
    let invocation = a_run_in_flight(&worker_store).await;
    publish_an_event_this_build_cannot_read(&bus).await;
    publish(&bus, invocation, a_drop()).await;

    let started = Instant::now();
    let next = NextBoot::start(&bus, &cp_store, &worker_store, 30).await;
    let took = started.elapsed();
    assert_eq!(
        (next.resumes(), next.holds()),
        (Vec::new(), vec![invocation.to_string()]),
        "the start resumes nothing and holds the run"
    );
    assert!(
        took < Duration::from_secs(10),
        "the start waited {took:?} for a consumer that had halted"
    );
    next.stop().await;
}

/// **A tail that aged out while the daemon was down is not waited for.**
/// Every stop leaves events its coordination consumer never folded: a
/// clean stop's own `system.shutdown`, at least. Once the stream's
/// retention ages them out, the stream's last sequence lies past anything
/// the durable will ever be delivered, and its acknowledged floor stays
/// where the last consumer left it, so the two never meet. The broker then
/// reports nothing pending and nothing unacknowledged for the durable:
/// folded as far as it can be. The next start does not wait out its
/// bound, and resumes the run, which nothing on the stream can end.
#[tokio::test]
async fn a_start_after_the_unfolded_tail_aged_out_does_not_wait() {
    let server = fq_test_support::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let scratch = tempfile::tempdir().unwrap();
    let (cp_store, worker_store) = stores_under(scratch.path()).await;

    // A first start creates the durable and folds; then the daemon stops
    // and events it never folded follow.
    publish(&bus, Uuid::now_v7(), its_own_failed()).await;
    NextBoot::start(&bus, &cp_store, &worker_store, 30)
        .await
        .stop()
        .await;
    for _ in 0..3 {
        publish(&bus, Uuid::now_v7(), its_own_failed()).await;
    }

    // They age out.
    let mut stream = bus
        .jetstream()
        .get_stream(fq_runtime::bus::STREAM_NAME)
        .await
        .expect("the event stream");
    let mut config = stream.info().await.expect("stream info").config.clone();
    config.max_age = Duration::from_secs(1);
    config.duplicate_window = Duration::from_secs(1);
    bus.jetstream()
        .update_stream(config)
        .await
        .expect("shorten retention");
    let deadline = Instant::now() + Duration::from_secs(10);
    while stream.info().await.expect("stream info").state.messages > 0 {
        assert!(Instant::now() < deadline, "the tail never aged out");
        tokio::time::sleep(POLL).await;
    }

    let invocation = a_run_in_flight(&worker_store).await;
    let started = Instant::now();
    let next = NextBoot::start(&bus, &cp_store, &worker_store, 10).await;
    let took = started.elapsed();
    assert_eq!(
        (next.resumes(), next.holds()),
        (vec![invocation.to_string()], Vec::new()),
        "nothing left on the stream can end the run, so it is resumed"
    );
    assert!(
        took < Duration::from_secs(5),
        "the start waited {took:?} for events that had aged out"
    );
    next.stop().await;
}

/// **A start with nothing unfolded does not wait.** The last daemon
/// folded and acknowledged everything before it stopped, so this
/// process's consumer has nothing to deliver and its watermark stays
/// where it starts; what is folded is read from the broker. The start
/// decides at once and resumes the run.
#[tokio::test]
async fn a_start_with_nothing_unfolded_does_not_wait() {
    let server = fq_test_support::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let scratch = tempfile::tempdir().unwrap();
    let (cp_store, worker_store) = stores_under(scratch.path()).await;
    publish(&bus, Uuid::now_v7(), its_own_failed()).await;
    let first = NextBoot::start(&bus, &cp_store, &worker_store, 30).await;
    // Let it fold what the first start itself published.
    tokio::time::sleep(Duration::from_millis(500)).await;
    first.stop().await;

    let invocation = a_run_in_flight(&worker_store).await;
    let started = Instant::now();
    let next = NextBoot::start(&bus, &cp_store, &worker_store, 4).await;
    let took = started.elapsed();
    assert_eq!(next.resumes(), [invocation.to_string()]);
    assert!(
        took < Duration::from_secs(2),
        "the start waited {took:?} with nothing to fold"
    );
    next.stop().await;
}

/// **The durable's acknowledged floor counts as folded.** The watermark
/// moves only with what this process folds, so how far the last daemon
/// folded is read from the broker. Here the durable's floor is at the boot
/// point while later events still wait to be delivered, so the floor
/// alone says the boot point is reached; one past it is not.
#[tokio::test]
async fn the_durables_acknowledged_floor_counts_as_folded() {
    let server = fq_test_support::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let scratch = tempfile::tempdir().unwrap();
    let (cp_store, worker_store) = stores_under(scratch.path()).await;
    publish(&bus, Uuid::now_v7(), its_own_failed()).await;
    let first = NextBoot::start(&bus, &cp_store, &worker_store, 30).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    first.stop().await;
    let mut stream = bus
        .jetstream()
        .get_stream(fq_runtime::bus::STREAM_NAME)
        .await
        .expect("the event stream");
    let floor = stream
        .info()
        .await
        .expect("stream info")
        .state
        .last_sequence;
    for _ in 0..2 {
        publish(&bus, Uuid::now_v7(), its_own_failed()).await;
    }

    let (_mark, watermark) = fq_runtime::watermark::channel();
    let at_floor = FoldProbe {
        bus: bus.clone(),
        durable: fq_runtime::control_plane::coordination_consumer::CONSUMER_NAME.to_string(),
        watermark,
        boot_point: floor,
    };
    assert!(
        at_floor.folded_to().await >= floor,
        "the floor the last daemon left counts as folded"
    );
    let past_floor = FoldProbe {
        boot_point: floor + 1,
        ..at_floor
    };
    assert!(
        past_floor.folded_to().await < floor + 1,
        "an event past the floor, still to be delivered, is not folded"
    );
}

/// A `fq invocation resume` handle over these stores, its runner asking
/// `model`.
fn resume_control(
    bus: &EventBus,
    cp_store: &Arc<ControlPlaneStore>,
    worker_store: &Arc<WorkerStore>,
    worker_id: &WorkerId,
    model: &Arc<HeldModel>,
    dir: &std::path::Path,
) -> Arc<ResumeControl> {
    let runner = runner_over(
        Arc::new(bus.clone()),
        worker_store.clone(),
        worker_id.clone(),
    );
    let llm: Arc<dyn LlmClient> = model.clone();
    Arc::new(ResumeControl::new(
        bus.clone(),
        worker_store.clone(),
        cp_store.clone(),
        runner.clone(),
        shared_registry(capped_agent_registry(dir)),
        llm,
        fq_runtime::worker::DeferralQueue::new().0,
        RunAdmission::new(runner, 1),
    ))
}

/// **`fq invocation resume` does not apply to a held run.** It takes only
/// ambiguous runs, and a held run is one recovery would have resumed by
/// itself; its ownership row, restored, reads `in_flight`. So it is
/// refused as live, and the run waits for a later start.
#[tokio::test]
async fn an_operator_resume_of_a_held_run_is_refused() {
    let server = fq_test_support::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let scratch = tempfile::tempdir().unwrap();
    let (cp_store, worker_store) = stores_under(scratch.path()).await;
    let invocation = a_run_in_flight(&worker_store).await;
    publish_an_event_this_build_cannot_read(&bus).await;
    let next = NextBoot::start(&bus, &cp_store, &worker_store, 1).await;
    assert_eq!(next.holds(), [invocation.to_string()]);
    let model = HeldModel::new();
    let control = resume_control(
        &bus,
        &cp_store,
        &worker_store,
        &next.worker_id,
        &model,
        scratch.path(),
    );
    let answer = crate::resume::handle_resume_request(
        &control,
        crate::resume::InvocationResumeRequest {
            invocation_id: invocation.to_string(),
            reason: Some("test".to_string()),
        },
    )
    .await;
    assert!(!answer.ok, "refused: {}", answer.message);
    assert!(
        answer
            .message
            .contains("is live; only Ambiguous invocations can be resumed"),
        "{}",
        answer.message
    );
    assert_eq!(model.calls(), 0);
    next.stop().await;
}
