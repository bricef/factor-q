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
use crate::events::{
    CompletedPayload, FailedPayload, FailureKind, FailurePhase, InvocationAmbiguousPayload,
    InvocationArchivedPayload, InvocationOperatorRecoveredPayload, InvocationTotals,
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

/// The issue's own case: a run recovery restored ends in `failed`, and its
/// archive never arrives (refused, held, or still on its way).
#[tokio::test]
async fn a_failed_run_whose_archive_has_not_arrived_reads_failed() {
    let fixture = Fixture::new().await;
    fixture.restored_in_flight().await;
    fixture.failed(FailurePhase::LlmRequest).await;
    assert_eq!(fixture.status().await, Some(OwnerStatus::Failed));
}

/// The same for `completed`, on a triggered run that had no row: the
/// ending writes it.
#[tokio::test]
async fn a_completed_run_whose_archive_has_not_arrived_reads_completed() {
    let fixture = Fixture::new().await;
    assert_eq!(fixture.row().await, None);
    fixture.completed().await;
    assert_eq!(fixture.status().await, Some(OwnerStatus::Completed));
}

/// The archive arriving later moves the row on, and keeps the ending.
#[tokio::test]
async fn a_run_whose_archive_arrives_later_keeps_its_ending() {
    let fixture = Fixture::new().await;
    fixture.failed(FailurePhase::ToolCall).await;
    fixture.archived("failed").await;
    let row = fixture.row().await.expect("the run is on record");
    assert_eq!(row.status, OwnerStatus::Failed);
    assert_eq!(
        row.worker_id,
        fixture.worker_id.as_str(),
        "the archive names the worker"
    );
}

/// A `failed` in `setup` is published only for a run that never made its
/// first WAL write, and a row would record a start that never happened.
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

/// An ending published after an operator's drop does not undo the drop.
#[tokio::test]
async fn an_ending_after_a_drop_leaves_the_run_dropped() {
    let fixture = Fixture::new().await;
    fixture.dropped().await;
    fixture.completed().await;
    assert_eq!(fixture.status().await, Some(OwnerStatus::Failed));
}

/// An operator's drop wins over a finished run, as it always has.
#[tokio::test]
async fn a_drop_turns_a_completed_run_failed() {
    let fixture = Fixture::new().await;
    fixture.completed().await;
    fixture.archived("completed").await;
    fixture.dropped().await;
    assert_eq!(fixture.status().await, Some(OwnerStatus::Failed));
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
