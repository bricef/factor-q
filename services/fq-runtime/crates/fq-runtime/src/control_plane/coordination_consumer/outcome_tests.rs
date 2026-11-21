//! The ownership row follows the run's outcome (issue 1024).
//!
//! A run's ending moves its row when the ending is published, not when
//! its archive is delivered, so a run whose archive is late, refused or
//! held reads as finished. Each test delivers events through the
//! consumer's own dispatch, in the order the stream would, over a real
//! store.

use super::*;
use crate::agent::AgentId;
use crate::control_plane::store::OwnerRow;
use crate::control_plane::store::{ArchiveState, OwnerStatus};
use crate::events::{
    CompletedPayload, EndingHeldReason, FailedPayload, FailureKind, FailurePhase, HeldOutcome,
    InvocationAmbiguousPayload, InvocationArchivedPayload, InvocationEndingHeldPayload,
    InvocationOperatorRecoveredPayload, InvocationTotals,
};
use crate::test_support::nats::{NatsServer, test_nats};
use crate::worker::WorkerId;
use tempfile::TempDir;

/// A consumer over a fresh store, and the one invocation every test moves.
struct Fixture {
    consumer: CoordinationConsumer,
    store: Arc<ControlPlaneStore>,
    agent_id: AgentId,
    worker_id: WorkerId,
    invocation_id: Uuid,
    _dir: TempDir,
    _server: NatsServer,
}

impl Fixture {
    async fn new() -> Self {
        let server = test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("control-plane.db"))
                .await
                .unwrap(),
        );
        Self {
            consumer: CoordinationConsumer::new(bus, store.clone()),
            store,
            agent_id: AgentId::new(format!("outcome-{}", Uuid::now_v7().simple())).unwrap(),
            worker_id: WorkerId::new(Uuid::now_v7().to_string()).unwrap(),
            invocation_id: Uuid::now_v7(),
            _dir: dir,
            _server: server,
        }
    }

    /// Deliver `payload` for the invocation through the consumer's dispatch.
    async fn deliver(&self, payload: EventPayload) {
        self.consumer
            .handle_event(Event::new(
                self.agent_id.clone(),
                self.invocation_id,
                payload,
            ))
            .await
            .expect("the consumer handles the event");
    }

    /// Recovery restored the run to `in_flight`, owned by this worker.
    async fn restored_in_flight(&self) {
        self.store
            .upsert_invocation_ownership(
                &self.invocation_id.to_string(),
                self.worker_id.as_str(),
                1_000,
                OwnerStatus::InFlight,
            )
            .await
            .unwrap();
    }

    async fn completed(&self) {
        self.deliver(EventPayload::Completed(CompletedPayload {
            task_status: Default::default(),
            result_summary: Some("done".to_string()),
            total_llm_calls: 1,
            total_tool_calls: 0,
            total_cost: 0.0,
            total_duration_ms: 10,
        }))
        .await;
    }

    async fn failed(&self, phase: FailurePhase) {
        self.deliver(EventPayload::Failed(FailedPayload {
            error_kind: FailureKind::LlmError,
            error_message: "the provider refused the request".to_string(),
            phase,
            partial_totals: InvocationTotals::default(),
            detail: None,
        }))
        .await;
    }

    async fn archived(&self, final_phase: &str) {
        self.deliver(EventPayload::InvocationArchived(
            InvocationArchivedPayload {
                worker_id: self.worker_id.clone(),
                final_phase: final_phase.to_string(),
                final_state_blob: b"{}".to_vec(),
                started_at_ms: 1_000,
                terminal_at_ms: 2_000,
            },
        ))
        .await;
    }

    async fn ambiguous(&self) {
        self.deliver(EventPayload::InvocationAmbiguous(
            InvocationAmbiguousPayload {
                stuck_entity: "recovery".to_string(),
                stuck_call_id: self.invocation_id.to_string(),
                note: "automatic resume failed".to_string(),
            },
        ))
        .await;
    }

    async fn dropped(&self) {
        self.deliver(EventPayload::InvocationOperatorRecovered(
            InvocationOperatorRecoveredPayload {
                action: "drop".to_string(),
                final_phase: "failed".to_string(),
                reason: None,
                requested_by: None,
                archive: None,
            },
        ))
        .await;
    }

    /// The worker holding the run's own ending says how the run ended
    /// (issue 1071). It names a worker of its own, so a test can tell
    /// whose id a row it writes takes.
    async fn held(&self, outcome: HeldOutcome) {
        self.deliver(EventPayload::InvocationEndingHeld(
            InvocationEndingHeldPayload {
                outcome,
                ended_at_ms: 2_000,
                // Not the restore's 1_000, so a test can tell them apart.
                started_at_ms: 1_500,
                worker_id: WorkerId::new(HOLDING_WORKER).unwrap(),
                reason: EndingHeldReason::StoredEventRefused,
                totals: None,
            },
        ))
        .await;
    }

    async fn row(&self) -> Option<OwnerRow> {
        self.store
            .get_invocation_owner(&self.invocation_id.to_string())
            .await
            .unwrap()
    }

    async fn status(&self) -> Option<OwnerStatus> {
        self.row().await.map(|row| row.status)
    }
}

/// The worker that holds a run's own ending in the `held` deliveries.
const HOLDING_WORKER: &str = "the-holding-worker";

/// The issue's own case: a run recovery restored ends in `failed`, and its
/// archive never arrives (refused, held, or still on its way).
#[tokio::test]
async fn a_failed_run_whose_archive_has_not_arrived_reads_failed() {
    let fixture = Fixture::new().await;
    fixture.restored_in_flight().await;
    fixture.failed(FailurePhase::LlmRequest).await;
    let row = fixture.row().await.expect("the run is on record");
    assert_eq!(
        (row.status, row.archive_state),
        (OwnerStatus::Failed, ArchiveState::Pending)
    );
    assert_eq!(
        row.worker_id,
        fixture.worker_id.as_str(),
        "the ending keeps the worker recovery restored the row under"
    );
}

/// The same for `completed`, on a triggered run that had no row: the
/// ending writes it.
#[tokio::test]
async fn a_completed_run_whose_archive_has_not_arrived_reads_completed() {
    let fixture = Fixture::new().await;
    assert_eq!(fixture.row().await, None);
    fixture.completed().await;
    let row = fixture.row().await.expect("the ending writes the row");
    assert_eq!(
        (row.status, row.archive_state),
        (OwnerStatus::Completed, ArchiveState::Pending)
    );
}

/// **A run whose row was written at its durable start keeps that row's
/// worker and start through its ending (issue 721).** The dispatcher wrote
/// the row `in_flight` under its own worker id from the run's start; the
/// run's `completed`, folded as on a replay of the stream over the live
/// store, moves it to `completed` and leaves both, so the agent's id never
/// stands in the worker column.
#[tokio::test]
async fn an_ending_keeps_the_worker_and_start_a_durable_start_wrote() {
    use crate::control_plane::ownership::{OwnerFacts, OwnershipEvent, apply};
    let fixture = Fixture::new().await;
    apply(
        fixture.store.as_ref(),
        &fixture.invocation_id.to_string(),
        OwnershipEvent::DurablyStarted,
        OwnerFacts {
            worker_id: fixture.worker_id.as_str(),
            at_ms: 1_000,
            ended_at_ms: 1_000,
            request: None,
        },
    )
    .await
    .expect("the durable start writes the row");

    fixture.completed().await;

    let row = fixture.row().await.expect("the run is on record");
    assert_eq!(
        (row.status, row.archive_state),
        (OwnerStatus::Completed, ArchiveState::Pending)
    );
    assert_eq!(
        (row.worker_id.as_str(), row.assigned_at),
        (fixture.worker_id.as_str(), 1_000),
        "the ending keeps the worker and the start the durable start wrote"
    );
}

/// **A replay into an empty store writes no row for a run until its
/// ending (issue 721).** The durable start is written directly, not
/// published, so nothing on the stream writes it: the run's `triggered`
/// writes no row, and its ending writes one with the agent's id standing
/// in for the worker. A rebuilt store cannot restore a running run's row.
#[tokio::test]
async fn a_replay_into_an_empty_store_writes_no_row_until_the_runs_ending() {
    use crate::events::{ConfigSnapshot, TriggerSource, TriggeredPayload};
    let fixture = Fixture::new().await;
    fixture
        .deliver(EventPayload::Triggered(TriggeredPayload {
            trigger_id: Some(Uuid::now_v7()),
            trigger_source: TriggerSource::Manual,
            trigger_subject: None,
            trigger_payload: serde_json::json!({}),
            config_snapshot: ConfigSnapshot::default(),
        }))
        .await;
    assert_eq!(
        fixture.row().await,
        None,
        "nothing on the stream writes the start"
    );

    fixture.completed().await;

    let row = fixture.row().await.expect("the ending writes the row");
    assert_eq!(
        (row.status, row.worker_id.as_str()),
        (OwnerStatus::Completed, fixture.agent_id.as_str())
    );
}

/// The archive arriving later moves the row on, and keeps the ending.
#[tokio::test]
async fn a_run_whose_archive_arrives_later_keeps_its_ending() {
    let fixture = Fixture::new().await;
    fixture.failed(FailurePhase::ToolCall).await;
    assert_eq!(
        fixture.row().await.map(|row| row.archive_state),
        Some(ArchiveState::Pending)
    );
    fixture.archived("failed").await;
    let row = fixture.row().await.expect("the run is on record");
    assert_eq!(
        (row.status, row.archive_state),
        (OwnerStatus::Failed, ArchiveState::Archived)
    );
    assert_eq!(
        row.worker_id,
        fixture.worker_id.as_str(),
        "the archive names the worker"
    );
}

/// A `failed` in `setup` is published only while the run's WAL holds no
/// row: before its first WAL write, where a row would record a start that
/// never happened (a reducer whose first step errors is one, and the
/// runner's archive step after it still logs `archive publish skipped:
/// invocation_state row missing` at warn), or after a live drop whose
/// archive was handed off, where the drop is already on record. An
/// earlier build's `failed` in another phase for such a run does write a
/// row when a consumer replays it; that is not tested here, since it is
/// the table's `failed` row, not a decision of its own.
#[tokio::test]
async fn a_failure_in_setup_never_writes_a_row() {
    let fixture = Fixture::new().await;
    fixture.failed(FailurePhase::Setup).await;
    assert_eq!(fixture.row().await, None);
}

/// An archive whose phase contradicts the ending on record: two endings
/// were published for one run, and the first stands.
#[tokio::test]
async fn an_archive_contradicting_the_recorded_ending_keeps_it() {
    let fixture = Fixture::new().await;
    fixture.completed().await;
    fixture.archived("failed").await;
    assert_eq!(fixture.status().await, Some(OwnerStatus::Completed));
}

/// The run's own ending, folded after an operator's drop whose archive has
/// not landed, replaces the drop's provisional ending (issue 1070).
#[tokio::test]
async fn an_ending_after_a_drop_replaces_its_provisional_ending() {
    let fixture = Fixture::new().await;
    fixture.dropped().await;
    fixture.completed().await;
    assert_eq!(fixture.status().await, Some(OwnerStatus::Completed));
}

/// An operator's drop of a finished run records the request and keeps the
/// run's ending (issue 1070): it never rewrites what happened.
#[tokio::test]
async fn a_drop_of_a_completed_run_keeps_it_completed() {
    let fixture = Fixture::new().await;
    fixture.completed().await;
    fixture.archived("completed").await;
    fixture.dropped().await;
    assert_eq!(fixture.status().await, Some(OwnerStatus::Completed));
}

/// A move keeps the worker the row names: the ambiguous move of a row
/// recovery restored must not put the agent id where the worker's was.
#[tokio::test]
async fn ambiguous_keeps_the_worker_recovery_restored() {
    let fixture = Fixture::new().await;
    fixture.restored_in_flight().await;
    fixture.ambiguous().await;
    let row = fixture.row().await.expect("the run is on record");
    assert_eq!(row.status, OwnerStatus::Ambiguous);
    assert_eq!(row.worker_id, fixture.worker_id.as_str());
}

/// A row this build cannot read is not moved, and its event is acked as
/// a permanent failure rather than retried: a retry would hold every
/// later event behind it on the daemon's strict-order consumer.
#[tokio::test]
async fn an_event_for_a_row_this_build_cannot_read_is_acked_and_the_row_left_alone() {
    let fixture = Fixture::new().await;
    fixture.restored_in_flight().await;
    let by_hand = sqlx::SqlitePool::connect(&format!(
        "sqlite://{}",
        fixture._dir.path().join("control-plane.db").display()
    ))
    .await
    .unwrap();
    sqlx::query("UPDATE coordination_invocation_owner SET status = 'weird'")
        .execute(&by_hand)
        .await
        .unwrap();
    let outcome = fixture
        .consumer
        .handle_event(Event::new(
            fixture.agent_id.clone(),
            fixture.invocation_id,
            EventPayload::Completed(CompletedPayload {
                task_status: Default::default(),
                result_summary: None,
                total_llm_calls: 1,
                total_tool_calls: 0,
                total_cost: 0.0,
                total_duration_ms: 10,
            }),
        ))
        .await;
    assert!(
        matches!(outcome, Err(HandlerError::Permanent(_))),
        "acked, not retried"
    );
    let status: String = sqlx::query_scalar("SELECT status FROM coordination_invocation_owner")
        .fetch_one(&by_hand)
        .await
        .unwrap();
    assert_eq!(status, "weird");
}

impl Fixture {
    /// Recovery restored the run, then its row was given a status this
    /// build cannot read.
    async fn unreadable(&self) {
        self.restored_in_flight().await;
        assert!(
            self.store
                .overwrite_owner_status_for_test(&self.invocation_id.to_string(), "weird")
                .await
                .unwrap()
        );
    }

    /// The row's raw status, which `row()` cannot read once it is
    /// unreadable.
    async fn raw_status(&self) -> String {
        let by_hand = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            self._dir.path().join("control-plane.db").display()
        ))
        .await
        .unwrap();
        sqlx::query_scalar("SELECT status FROM coordination_invocation_owner")
            .fetch_one(&by_hand)
            .await
            .unwrap()
    }
}

/// An archive for a run whose row this build cannot read is still written
/// and still acknowledged to the worker, then refused as permanent: without
/// the acknowledgement the worker would keep its terminal row and republish
/// the whole archive at every sweep, for good.
#[tokio::test]
async fn an_archive_for_a_row_this_build_cannot_read_is_still_acked_to_the_worker() {
    use futures::StreamExt;
    let fixture = Fixture::new().await;
    fixture.unreadable().await;
    let mut acks = fixture
        .consumer
        .bus
        .subscribe(format!(
            "fq.worker.{}.invocation.archive_acked",
            fixture.worker_id.as_str()
        ))
        .await
        .expect("subscribe to the worker's acks");
    let outcome = fixture
        .consumer
        .handle_event(Event::new(
            fixture.agent_id.clone(),
            fixture.invocation_id,
            EventPayload::InvocationArchived(InvocationArchivedPayload {
                worker_id: fixture.worker_id.clone(),
                final_phase: "failed".to_string(),
                final_state_blob: b"{}".to_vec(),
                started_at_ms: 1_000,
                terminal_at_ms: 2_000,
            }),
        ))
        .await;
    assert!(
        matches!(outcome, Err(HandlerError::Permanent(_))),
        "acked, not retried"
    );
    assert!(
        fixture
            .store
            .get_archive(&fixture.invocation_id.to_string())
            .await
            .unwrap()
            .is_some(),
        "the archive record is written"
    );
    tokio::time::timeout(Duration::from_secs(5), acks.next())
        .await
        .expect("the worker is acknowledged")
        .expect("an ack")
        .expect("the ack reads");
    assert_eq!(
        fixture.raw_status().await,
        "weird",
        "the row is left as it is"
    );
}

/// An operator's drop of a run whose row this build cannot read still
/// stops the run: the worker's WAL row is marked terminal, for the archive
/// sweeper to hand off with the run's state, and no archive record is
/// written, as no drop writes one (issue 1070). The event is then refused
/// as permanent, so the strict-order consumer goes on.
#[tokio::test]
async fn a_drop_of_a_row_this_build_cannot_read_still_stops_the_run() {
    let fixture = Fixture::new().await;
    fixture.unreadable().await;
    let worker_store = Arc::new(
        crate::worker::store::WorkerStore::open(&fixture._dir.path().join("worker.db"))
            .await
            .unwrap(),
    );
    let inv = fixture.invocation_id.to_string();
    worker_store
        .upsert_invocation_state(&crate::worker::store::InvocationStateRow {
            invocation_id: inv.clone(),
            agent_id: fixture.agent_id.as_str().to_string(),
            schema_version: 1,
            phase: "awaiting_model".into(),
            state_blob: vec![],
            step_index: 3,
            started_at: 1_000,
            updated_at: 1_500,
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
    let consumer = CoordinationConsumer::new(fixture.consumer.bus.clone(), fixture.store.clone())
        .with_worker_store(worker_store.clone());
    let outcome = consumer
        .handle_event(Event::new(
            fixture.agent_id.clone(),
            fixture.invocation_id,
            EventPayload::InvocationOperatorRecovered(InvocationOperatorRecoveredPayload {
                action: "drop".to_string(),
                final_phase: "failed".to_string(),
                reason: None,
                requested_by: None,
                archive: None,
            }),
        ))
        .await;
    assert!(
        matches!(outcome, Err(HandlerError::Permanent(_))),
        "acked, not retried: a transient error here would hold every later event"
    );
    assert!(
        fixture.store.get_archive(&inv).await.unwrap().is_none(),
        "the drop writes no archive record"
    );
    let wal = worker_store
        .get_invocation_state(&inv)
        .await
        .unwrap()
        .expect("the WAL row is kept for the sweeper");
    assert!(
        wal.terminal_at.is_some(),
        "the worker's WAL row is marked terminal"
    );
    assert_eq!(
        fixture.raw_status().await,
        "weird",
        "the row is left as it is"
    );
}

impl Fixture {
    /// A trigger that refuses every move of an ownership row already
    /// written, as a rule the store holds the write to would.
    async fn refuse_every_move(&self) {
        let by_hand = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            self._dir.path().join("control-plane.db").display()
        ))
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER refuse_the_move BEFORE UPDATE ON coordination_invocation_owner \
             BEGIN SELECT RAISE(ABORT, 'the test refuses the move'); END",
        )
        .execute(&by_hand)
        .await
        .unwrap();
    }

    /// A worker's WAL holding the run in flight, as the run's worker does.
    async fn a_worker_running_the_run(&self) -> Arc<crate::worker::store::WorkerStore> {
        let worker_store = Arc::new(
            crate::worker::store::WorkerStore::open(&self._dir.path().join("worker.db"))
                .await
                .unwrap(),
        );
        worker_store
            .upsert_invocation_state(&crate::worker::store::InvocationStateRow {
                invocation_id: self.invocation_id.to_string(),
                agent_id: self.agent_id.as_str().to_string(),
                schema_version: 1,
                phase: "awaiting_model".into(),
                state_blob: vec![],
                step_index: 3,
                started_at: 1_000,
                updated_at: 1_500,
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
        worker_store
    }
}

/// **An archive whose row move the store refuses is still recorded and
/// acknowledged to the worker**, as for a row this build cannot read: the
/// row is left as it is, and without the acknowledgement the worker would
/// keep its terminal row and republish the whole archive at every sweep,
/// for good. Then the event is refused as permanent.
#[tokio::test]
async fn an_archive_whose_row_move_is_refused_is_still_acked_to_the_worker() {
    use futures::StreamExt;
    let fixture = Fixture::new().await;
    fixture.restored_in_flight().await;
    fixture.refuse_every_move().await;
    let mut acks = fixture
        .consumer
        .bus
        .subscribe(format!(
            "fq.worker.{}.invocation.archive_acked",
            fixture.worker_id.as_str()
        ))
        .await
        .expect("subscribe to the worker's acks");
    let outcome = fixture
        .consumer
        .handle_event(Event::new(
            fixture.agent_id.clone(),
            fixture.invocation_id,
            EventPayload::InvocationArchived(InvocationArchivedPayload {
                worker_id: fixture.worker_id.clone(),
                final_phase: "failed".to_string(),
                final_state_blob: b"{}".to_vec(),
                started_at_ms: 1_000,
                terminal_at_ms: 2_000,
            }),
        ))
        .await;
    assert!(
        matches!(outcome, Err(HandlerError::Permanent(_))),
        "the archive whose row move was refused was answered {outcome:?}: acked, not retried"
    );
    assert!(
        fixture
            .store
            .get_archive(&fixture.invocation_id.to_string())
            .await
            .unwrap()
            .is_some(),
        "the archive record is written"
    );
    tokio::time::timeout(Duration::from_secs(5), acks.next())
        .await
        .expect("the worker is acknowledged: without it, it republishes the archive at every sweep")
        .expect("an ack")
        .expect("the ack reads");
    assert_eq!(
        fixture.raw_status().await,
        "in_flight",
        "the row is left as it is"
    );
}

/// **An archive whose row insert the store refuses is recorded, and not
/// acknowledged.** The run has no ownership row, and the store refuses the
/// one the archive would write. Acknowledged, the worker would delete its
/// terminal row, and the run would then be on neither record a trigger's
/// claim is settled from: its trigger could be taken again and run twice.
/// So the archive record is stored, the worker is not acknowledged (it
/// keeps its row and republishes, which the record's insert takes as a
/// duplicate), and the event is refused as permanent.
#[tokio::test]
async fn an_archive_whose_row_insert_is_refused_is_recorded_and_not_acked() {
    use futures::StreamExt;
    let fixture = Fixture::new().await;
    let by_hand = sqlx::SqlitePool::connect(&format!(
        "sqlite://{}",
        fixture._dir.path().join("control-plane.db").display()
    ))
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER refuse_the_row BEFORE INSERT ON coordination_invocation_owner \
         BEGIN SELECT RAISE(ABORT, 'the test refuses the row'); END",
    )
    .execute(&by_hand)
    .await
    .unwrap();
    let mut acks = fixture
        .consumer
        .bus
        .subscribe(format!(
            "fq.worker.{}.invocation.archive_acked",
            fixture.worker_id.as_str()
        ))
        .await
        .expect("subscribe to the worker's acks");
    let outcome = fixture
        .consumer
        .handle_event(Event::new(
            fixture.agent_id.clone(),
            fixture.invocation_id,
            EventPayload::InvocationArchived(InvocationArchivedPayload {
                worker_id: fixture.worker_id.clone(),
                final_phase: "completed".to_string(),
                final_state_blob: b"{}".to_vec(),
                started_at_ms: 1_000,
                terminal_at_ms: 2_000,
            }),
        ))
        .await;
    assert!(
        matches!(outcome, Err(HandlerError::Permanent(_))),
        "the archive whose row insert was refused was answered {outcome:?}: acked, not retried"
    );
    assert!(
        fixture
            .store
            .get_archive(&fixture.invocation_id.to_string())
            .await
            .unwrap()
            .is_some(),
        "the archive record is stored"
    );
    let ack = tokio::time::timeout(Duration::from_millis(700), acks.next()).await;
    assert!(
        ack.is_err(),
        "the worker was acknowledged for a run with no ownership row: it would delete its \
         terminal row, and the run would be on no record its trigger's claim reads"
    );
    assert_eq!(fixture.row().await, None, "no ownership row is written");
}

/// **A drop whose row move the store refuses still stops the run**, as
/// for a row this build cannot read: the worker's WAL row is marked
/// terminal, and the row is left as it is; no archive record is written,
/// as no drop writes one (issue 1070). Then the event is refused as
/// permanent.
#[tokio::test]
async fn a_drop_whose_row_move_is_refused_still_stops_the_run() {
    let fixture = Fixture::new().await;
    fixture.restored_in_flight().await;
    fixture.refuse_every_move().await;
    let worker_store = fixture.a_worker_running_the_run().await;
    let consumer = CoordinationConsumer::new(fixture.consumer.bus.clone(), fixture.store.clone())
        .with_worker_store(worker_store.clone());
    let outcome = consumer
        .handle_event(Event::new(
            fixture.agent_id.clone(),
            fixture.invocation_id,
            EventPayload::InvocationOperatorRecovered(InvocationOperatorRecoveredPayload {
                action: "drop".to_string(),
                final_phase: "failed".to_string(),
                reason: None,
                requested_by: None,
                archive: None,
            }),
        ))
        .await;
    let inv = fixture.invocation_id.to_string();
    let wal = worker_store
        .get_invocation_state(&inv)
        .await
        .unwrap()
        .expect("the WAL row is kept for the sweeper");
    assert!(
        wal.terminal_at.is_some(),
        "the drop whose row move was refused was answered {outcome:?} and did not mark the \
         worker's WAL row terminal: the run the operator dropped goes on"
    );
    assert!(
        matches!(outcome, Err(HandlerError::Permanent(_))),
        "acked, not retried: {outcome:?}"
    );
    assert!(
        fixture.store.get_archive(&inv).await.unwrap().is_none(),
        "the drop writes no archive record"
    );
    assert_eq!(
        fixture.raw_status().await,
        "in_flight",
        "the row is left as it is"
    );
}

/// **A write the store refuses for good is acknowledged, not retried.** A
/// refusal the store makes by its own rules is not one a retry can change,
/// and on a strict-order consumer a retry would hold every later event
/// behind it for good. A trigger stands in for such a rule here: it
/// refuses every write of the ownership row.
#[tokio::test]
async fn a_write_the_store_refuses_for_good_is_acknowledged() {
    let fixture = Fixture::new().await;
    let by_hand = sqlx::SqlitePool::connect(&format!(
        "sqlite://{}",
        fixture._dir.path().join("control-plane.db").display()
    ))
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER refuse_the_row BEFORE INSERT ON coordination_invocation_owner \
         BEGIN SELECT RAISE(ABORT, 'the test refuses the row'); END",
    )
    .execute(&by_hand)
    .await
    .unwrap();
    let outcome = fixture
        .consumer
        .handle_event(Event::new(
            fixture.agent_id.clone(),
            fixture.invocation_id,
            EventPayload::InvocationOperatorRecovered(InvocationOperatorRecoveredPayload {
                action: "drop".to_string(),
                final_phase: "failed".to_string(),
                reason: None,
                requested_by: None,
                archive: None,
            }),
        ))
        .await;
    assert!(
        matches!(outcome, Err(HandlerError::Permanent(_))),
        "a write the store refused by its own rules was answered {outcome:?}: a transient \
         error is redelivered for ever and holds every later event behind it"
    );
}

/// **A locked store is retried, not acknowledged.** Another connection
/// holds the store's write lock past its busy timeout, as a long write or
/// a stuck process would: the run's ending cannot be folded now, but it
/// can later, so the event is transient and redelivered, not taken as a
/// refusal and lost.
#[tokio::test]
async fn an_ending_the_store_is_too_busy_to_fold_is_retried() {
    let fixture = Fixture::new().await;
    let by_hand = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&format!(
            "sqlite://{}",
            fixture._dir.path().join("control-plane.db").display()
        ))
        .await
        .unwrap();
    let mut holder = by_hand.acquire().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *holder)
        .await
        .unwrap();
    let outcome = fixture
        .consumer
        .handle_event(Event::new(
            fixture.agent_id.clone(),
            fixture.invocation_id,
            EventPayload::Completed(CompletedPayload {
                task_status: Default::default(),
                result_summary: None,
                total_llm_calls: 1,
                total_tool_calls: 0,
                total_cost: 0.0,
                total_duration_ms: 10,
            }),
        ))
        .await;
    sqlx::query("ROLLBACK").execute(&mut *holder).await.unwrap();
    assert!(
        matches!(outcome, Err(HandlerError::Transient(_))),
        "an ending the store was too busy to fold was answered {outcome:?}: it is retried"
    );
}

/// A row this build cannot read, and a write the store refuses by its own
/// rules, are refused as permanent. Every other error of a move, a row
/// that kept changing under it and a store that failed included, is
/// transient: the event is redelivered and decided again, not dropped.
#[test]
fn an_unreadable_row_and_a_refused_write_are_the_permanent_errors() {
    let event = Event::new(
        AgentId::new("a").unwrap(),
        Uuid::now_v7(),
        EventPayload::InvocationOperatorRecovered(InvocationOperatorRecoveredPayload {
            action: "drop".to_string(),
            final_phase: "failed".to_string(),
            reason: None,
            requested_by: None,
            archive: None,
        }),
    );
    let permanent = |err| matches!(as_handler_error(&event, err), HandlerError::Permanent(_));
    assert!(permanent(CoordinationConsumerError::Ownership(
        OwnershipError::Store(ControlPlaneStoreError::Refused("CHECK".to_string()))
    )));
    assert!(permanent(CoordinationConsumerError::Store(
        ControlPlaneStoreError::Refused("CHECK".to_string())
    )));
    assert!(permanent(CoordinationConsumerError::Ownership(
        OwnershipError::Unreadable {
            invocation_id: "inv".to_string(),
            detail: "unknown owner status `weird`".to_string(),
        }
    )));
    assert!(!permanent(CoordinationConsumerError::Ownership(
        OwnershipError::Contended {
            invocation_id: "inv".to_string(),
            attempts: crate::control_plane::ownership::APPLY_ATTEMPTS,
        }
    )));
    assert!(!permanent(CoordinationConsumerError::Ownership(
        OwnershipError::Store(ControlPlaneStoreError::Backend("disk".to_string()))
    )));
    assert!(!permanent(CoordinationConsumerError::Store(
        ControlPlaneStoreError::Backend("disk".to_string())
    )));
}

/// A permanently refused event still advances the coordination
/// watermark, so a read gated at its sequence answers at once rather than
/// waiting for the next event on the stream: after an operator drops a
/// run whose row this build cannot read, `fq invocation drop` reads back
/// at the drop's receipt.
#[tokio::test]
async fn a_refused_event_still_advances_the_watermark() {
    let fixture = Fixture::new().await;
    fixture.unreadable().await;
    let bus = fixture.consumer.bus.clone();
    let (mark, coordination) = crate::watermark::channel();
    let consumer =
        CoordinationConsumer::new(bus.clone(), fixture.store.clone()).with_watermark(mark);
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(async move { consumer.run(stopped).await });
    let warm = bus
        .publish(&Event::new(
            fixture.agent_id.clone(),
            Uuid::now_v7(),
            EventPayload::HostNotice(crate::events::HostNoticePayload {
                kind: "test_barrier".to_string(),
                body: String::new(),
            }),
        ))
        .await
        .expect("publish the warm-up");
    coordination
        .wait_for(warm, Duration::from_secs(10))
        .await
        .expect("the consumer is running");
    let dropped = bus
        .publish(&Event::new(
            fixture.agent_id.clone(),
            fixture.invocation_id,
            EventPayload::InvocationOperatorRecovered(InvocationOperatorRecoveredPayload {
                action: "drop".to_string(),
                final_phase: "failed".to_string(),
                reason: None,
                requested_by: None,
                archive: None,
            }),
        ))
        .await
        .expect("publish the drop");
    let waited = coordination.wait_for(dropped, Duration::from_secs(4)).await;
    let _ = stop.send(());
    let _ = tokio::time::timeout(Duration::from_secs(10), task).await;
    waited.expect("a read gated at the refused drop's sequence answers");
    assert_eq!(
        fixture.raw_status().await,
        "weird",
        "the drop was folded, and left the row as it is"
    );
    assert!(
        fixture
            .store
            .get_archive(&fixture.invocation_id.to_string())
            .await
            .unwrap()
            .is_none(),
        "the drop writes no archive record"
    );
}

// --- A held ending (issue 1071) ------------------------------------------
//
// `invocation.ending_held` stands for the run's own ending, which the
// worker holds, and folds through the same cells of the transition table.

/// A run with no row, a triggered one: the held ending writes the row with
/// that ending, its archive pending, owned by the worker holding it.
#[tokio::test]
async fn a_held_ending_writes_the_row_of_a_run_that_had_none() {
    let fixture = Fixture::new().await;
    fixture.held(HeldOutcome::Completed).await;
    let row = fixture.row().await.expect("the held ending writes the row");
    assert_eq!(
        (row.status, row.archive_state, row.worker_id.as_str()),
        (
            OwnerStatus::Completed,
            ArchiveState::Pending,
            HOLDING_WORKER
        )
    );
}

/// A run recovery restored to `in_flight` reads the ending, and keeps the
/// worker it was restored under, as the run's own ending would.
#[tokio::test]
async fn a_held_ending_moves_a_restored_run_and_keeps_its_worker() {
    let fixture = Fixture::new().await;
    fixture.restored_in_flight().await;
    fixture.held(HeldOutcome::Failed).await;
    let row = fixture.row().await.expect("the run is on record");
    assert_eq!(
        (row.status, row.archive_state, row.worker_id.as_str()),
        (
            OwnerStatus::Failed,
            ArchiveState::Pending,
            fixture.worker_id.as_str()
        )
    );
}

/// A run reported ambiguous reads the ending.
#[tokio::test]
async fn a_held_ending_moves_an_ambiguous_run() {
    let fixture = Fixture::new().await;
    fixture.ambiguous().await;
    fixture.held(HeldOutcome::Failed).await;
    assert_eq!(fixture.status().await, Some(OwnerStatus::Failed));
}

/// The run's own ending, published later by a build that can republish
/// it, finds the row already saying it, and so does a second copy of the
/// held ending, as after a restart: neither changes the row.
#[tokio::test]
async fn the_run_s_own_ending_after_its_held_one_changes_nothing() {
    let fixture = Fixture::new().await;
    fixture.held(HeldOutcome::Completed).await;
    let first = fixture.row().await.expect("the held ending writes the row");
    fixture.held(HeldOutcome::Completed).await;
    fixture.completed().await;
    assert_eq!(fixture.row().await, Some(first));
}

/// A held ending after the run's own, the same one, changes nothing.
#[tokio::test]
async fn a_held_ending_after_the_run_s_own_changes_nothing() {
    let fixture = Fixture::new().await;
    fixture.restored_in_flight().await;
    fixture.failed(FailurePhase::ToolCall).await;
    let first = fixture.row().await.expect("the ending moved the row");
    fixture.held(HeldOutcome::Failed).await;
    assert_eq!(fixture.row().await, Some(first));
}

/// A held ending that contradicts the one on record is a conflict, and the
/// row keeps the ending it recorded.
#[tokio::test]
async fn a_held_ending_contradicting_the_recorded_one_keeps_it() {
    let fixture = Fixture::new().await;
    fixture.completed().await;
    fixture.held(HeldOutcome::Failed).await;
    assert_eq!(fixture.status().await, Some(OwnerStatus::Completed));
}

/// The archive that follows once an operator releases the row moves it to
/// archived with the same ending, owned by the worker the archive names.
#[tokio::test]
async fn the_archive_after_a_held_ending_keeps_it() {
    let fixture = Fixture::new().await;
    fixture.held(HeldOutcome::Failed).await;
    fixture.archived("failed").await;
    let row = fixture.row().await.expect("the run is on record");
    assert_eq!(
        (row.status, row.archive_state),
        (OwnerStatus::Failed, ArchiveState::Archived)
    );
}

/// An operator's drop that came first ends the run provisionally, and
/// the held ending, the run's own, replaces it (issue 1070).
#[tokio::test]
async fn a_held_ending_after_a_drop_replaces_its_provisional_ending() {
    let fixture = Fixture::new().await;
    fixture.dropped().await;
    fixture.held(HeldOutcome::Completed).await;
    assert_eq!(fixture.status().await, Some(OwnerStatus::Completed));
}

/// The worker reads a held run's outcome from its row's phase by the rule
/// the run's `invocation.archived` is read by, so the archive that follows
/// a release never contradicts the held ending, whatever the phase says.
#[tokio::test]
async fn a_held_ending_and_the_archive_after_it_agree_for_every_phase() {
    for phase in [
        "completed",
        "failed",
        "in_flight",
        "deferred",
        "something-newer",
    ] {
        let fixture = Fixture::new().await;
        let outcome = HeldOutcome::from(
            crate::control_plane::ownership::Ending::of_archived_phase(phase),
        );
        fixture.held(outcome).await;
        let held = fixture.status().await;
        fixture.archived(phase).await;
        let row = fixture.row().await.expect("the run is on record");
        assert_eq!(
            (Some(row.status), row.archive_state),
            (held, ArchiveState::Archived),
            "phase {phase:?}: the archive keeps the held ending and records the archive"
        );
    }
}

/// A row the held ending writes is dated at the run's start, which the
/// event carries, not at the fold, which can come days after the run:
/// `fq invocation list` shows the row's time as when the run started. A
/// row that was already there keeps its own time.
#[tokio::test]
async fn a_row_a_held_ending_writes_is_dated_at_the_run_s_start() {
    let fixture = Fixture::new().await;
    fixture.held(HeldOutcome::Completed).await;
    let row = fixture.row().await.expect("the held ending writes the row");
    assert_eq!(
        row.assigned_at, 1_500,
        "the run's start, as the held ending reports it"
    );

    let restored = Fixture::new().await;
    restored.restored_in_flight().await;
    let before = restored.row().await.expect("restored").assigned_at;
    restored.held(HeldOutcome::Failed).await;
    assert_eq!(restored.row().await.expect("on record").assigned_at, before);
}
