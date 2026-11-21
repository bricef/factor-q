//! A drop whose event says no archive of the run will come (issue 1070).
//!
//! The daemon that drops a run says `archive: none` when the run has no
//! row on its worker and none will be written, and no archive is on
//! record. Folded on a run that has not ended, such a drop leaves the
//! ownership row `failed` with its archive `none`, and writes no archive
//! record: none will come, and an empty one would claim the run's state
//! was archived. A finished row, and a drop that does not say `none`, are
//! folded as before. And `none` is not final: an archive that arrives is
//! recorded like any other, and the run's own ending replaces the drop's,
//! its archive pending.
//!
//! Rows are read with SQL, as the store holds them, and the drop's payload
//! carries the field as the daemon writes it.

use super::*;
use crate::agent::AgentId;
use crate::control_plane::store::OwnerStatus;
use crate::events::{
    CompletedPayload, DropArchive, FailedPayload, FailureKind, FailurePhase,
    InvocationArchivedPayload, InvocationOperatorRecoveredPayload, InvocationTotals,
};
use crate::test_support::nats::{NatsServer, test_nats};
use crate::worker::WorkerId;
use tempfile::TempDir;

struct Fixture {
    consumer: CoordinationConsumer,
    store: Arc<ControlPlaneStore>,
    raw: sqlx::SqlitePool,
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
        let path = dir.path().join("control-plane.db");
        let store = Arc::new(ControlPlaneStore::open(&path).await.unwrap());
        let raw = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        Self {
            consumer: CoordinationConsumer::new(bus, store.clone()),
            store,
            raw,
            agent_id: AgentId::new(format!("archive-none-{}", Uuid::now_v7().simple())).unwrap(),
            worker_id: WorkerId::new(Uuid::now_v7().to_string()).unwrap(),
            invocation_id: Uuid::now_v7(),
            _dir: dir,
            _server: server,
        }
    }

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

    async fn seeded(&self, status: OwnerStatus) {
        self.store
            .upsert_invocation_ownership(
                &self.invocation_id.to_string(),
                self.worker_id.as_str(),
                1_000,
                status,
            )
            .await
            .unwrap();
    }

    async fn dropped(&self, archive: Option<DropArchive>) {
        self.deliver(EventPayload::InvocationOperatorRecovered(
            InvocationOperatorRecoveredPayload {
                action: "drop".to_string(),
                final_phase: "failed".to_string(),
                reason: Some("the run will never start".to_string()),
                requested_by: Some("on-call".to_string()),
                archive,
            },
        ))
        .await;
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

    async fn failed(&self) {
        self.deliver(EventPayload::Failed(FailedPayload {
            error_kind: FailureKind::LlmError,
            error_message: "the provider refused the request".to_string(),
            phase: FailurePhase::LlmRequest,
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
                final_state_blob: b"{\"state\":1}".to_vec(),
                started_at_ms: 1_000,
                terminal_at_ms: 2_000,
            },
        ))
        .await;
    }

    /// The row as the store holds it: `status` and `archive_state`.
    async fn row(&self) -> Option<(String, String)> {
        sqlx::query_as(
            "SELECT status, archive_state FROM coordination_invocation_owner \
             WHERE invocation_id = ?",
        )
        .bind(self.invocation_id.to_string())
        .fetch_optional(&self.raw)
        .await
        .unwrap()
    }

    /// The archive record's final phase and stored state, if one is written.
    async fn record(&self) -> Option<(String, Vec<u8>)> {
        self.store
            .get_archive(&self.invocation_id.to_string())
            .await
            .unwrap()
            .map(|row| (row.final_phase, row.final_state_blob))
    }
}

fn pair(status: &str, archive: &str) -> Option<(String, String)> {
    Some((status.to_string(), archive.to_string()))
}

/// **A drop saying no archive will come, of a run that has not ended:
/// `failed`, archive `none`, no record.** From no row, `in_flight` and
/// `ambiguous`.
#[tokio::test]
async fn a_drop_saying_none_of_a_run_not_ended_leaves_failed_none_and_no_record() {
    for seed in [
        None,
        Some(OwnerStatus::InFlight),
        Some(OwnerStatus::Ambiguous),
    ] {
        let fixture = Fixture::new().await;
        if let Some(status) = seed {
            fixture.seeded(status).await;
        }
        fixture.dropped(Some(DropArchive::WillNotCome)).await;
        assert_eq!(
            (fixture.row().await, fixture.record().await),
            (pair("failed", "none"), None),
            "from {seed:?}: a drop saying no archive will come leaves the run failed with its \
             archive none, and writes no record, since none will come"
        );
    }
}

/// **A drop of a finished run, or one not saying `none`, is folded as
/// before:** `failed`, its archive written, with the drop's empty record.
#[tokio::test]
async fn a_drop_of_a_finished_run_or_not_saying_none_is_folded_as_before() {
    let cases = [
        (Some(OwnerStatus::Completed), Some(DropArchive::WillNotCome)),
        (Some(OwnerStatus::Failed), Some(DropArchive::WillNotCome)),
        (None, Some(DropArchive::Pending)),
        (None, None),
        (None, Some(DropArchive::Unknown)),
        (Some(OwnerStatus::InFlight), None),
    ];
    for (seed, archive) in cases {
        let fixture = Fixture::new().await;
        if let Some(status) = seed {
            fixture.seeded(status).await;
        }
        fixture.dropped(archive).await;
        assert_eq!(
            (fixture.row().await, fixture.record().await),
            (
                pair("failed", "archived"),
                Some(("failed".to_string(), Vec::new()))
            ),
            "from {seed:?}, archive {archive:?}: folded as before"
        );
    }
}

/// **A second drop saying `none` is a duplicate.** The row stays `failed`
/// with its archive `none`, and no record is written.
#[tokio::test]
async fn a_second_drop_saying_none_changes_nothing() {
    let fixture = Fixture::new().await;
    fixture.dropped(Some(DropArchive::WillNotCome)).await;
    fixture.dropped(Some(DropArchive::WillNotCome)).await;
    assert_eq!(
        (fixture.row().await, fixture.record().await),
        (pair("failed", "none"), None)
    );
}

/// **`none` is not final: an archive that arrives is recorded like any
/// other.** Saying `failed`, the row becomes `failed`, archived; saying
/// `completed`, it keeps the drop's ending, as a `failed` row awaiting its
/// archive does, and the archive is recorded.
#[tokio::test]
async fn an_archive_after_a_drop_saying_none_is_recorded_like_any_other() {
    for final_phase in ["failed", "completed"] {
        let fixture = Fixture::new().await;
        fixture.dropped(Some(DropArchive::WillNotCome)).await;
        fixture.archived(final_phase).await;
        assert_eq!(
            (fixture.row().await, fixture.record().await),
            (
                pair("failed", "archived"),
                Some((final_phase.to_string(), b"{\"state\":1}".to_vec()))
            ),
            "an archive saying {final_phase} reached a row the drop left with no archive coming"
        );
    }
}

/// **`none` is not final: the run's own ending replaces the drop's**, its
/// archive pending, for the archive that ending brings.
#[tokio::test]
async fn the_runs_own_ending_after_a_drop_saying_none_replaces_it() {
    let fixture = Fixture::new().await;
    fixture.dropped(Some(DropArchive::WillNotCome)).await;
    fixture.completed().await;
    assert_eq!(
        (fixture.row().await, fixture.record().await),
        (pair("completed", "pending"), None),
        "the run's own completed replaces a drop that said no archive would come"
    );

    let fixture = Fixture::new().await;
    fixture.dropped(Some(DropArchive::WillNotCome)).await;
    fixture.failed().await;
    assert_eq!(
        (fixture.row().await, fixture.record().await),
        (pair("failed", "pending"), None),
        "the run's own failed replaces it too, its archive pending"
    );
}
