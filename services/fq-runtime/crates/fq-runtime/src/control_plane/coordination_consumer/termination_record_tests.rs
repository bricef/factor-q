//! A drop records a termination request beside the run's outcome (issue
//! 1070).
//!
//! The ownership row keeps two facts, and neither overwrites the other:
//! who ended the run and when (`ended_by`, `ended_at`), and the first
//! termination an operator requested (its time, who asked and why). A
//! drop of a run that has not ended ends it, provisionally: the run's own
//! ending, or an archive saying `completed`, replaces the drop's while its
//! archive has not landed. A drop of a run that has ended changes nothing
//! but the request. The drop writes no archive record: the worker's
//! archive brings the run's real state.
//!
//! Every time is the event's own, never the fold's: each event here is
//! stamped with a fixed time far from the clock, so a fold that took its
//! own time would show. Rows are read through the store's reader.

use super::*;
use crate::agent::AgentId;
use crate::control_plane::store::{
    ArchiveState, EndedBy, OwnerRow, OwnerStatus, TerminationRequest,
};
use crate::events::{
    CompletedPayload, DropArchive, EndingHeldReason, FailedPayload, FailureKind, FailurePhase,
    HeldOutcome, InvocationArchivedPayload, InvocationEndingHeldPayload,
    InvocationOperatorRecoveredPayload, InvocationTotals,
};
use crate::test_support::nats::{NatsServer, test_nats};
use crate::worker::WorkerId;
use chrono::TimeZone;
use tempfile::TempDir;

/// When the drop was requested.
const T_REQ: i64 = 1_700_000_000_000;
/// A run's ending before the request, and one after it.
const T_BEFORE: i64 = T_REQ - 60_000;
const T_AFTER: i64 = T_REQ + 60_000;

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
            agent_id: AgentId::new(format!("record-{}", Uuid::now_v7().simple())).unwrap(),
            worker_id: WorkerId::new(Uuid::now_v7().to_string()).unwrap(),
            invocation_id: Uuid::now_v7(),
            _dir: dir,
            _server: server,
        }
    }

    /// Fold `payload`, its event stamped `at_ms`.
    async fn deliver(&self, payload: EventPayload, at_ms: i64) {
        let mut event = Event::new(self.agent_id.clone(), self.invocation_id, payload);
        event.envelope.timestamp = Utc.timestamp_millis_opt(at_ms).unwrap();
        self.consumer
            .handle_event(event)
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

    async fn dropped_by(
        &self,
        at_ms: i64,
        requested_by: &str,
        reason: &str,
        archive: Option<DropArchive>,
    ) {
        self.deliver(
            EventPayload::InvocationOperatorRecovered(InvocationOperatorRecoveredPayload {
                action: "drop".to_string(),
                final_phase: "failed".to_string(),
                reason: Some(reason.to_string()),
                requested_by: Some(requested_by.to_string()),
                archive,
            }),
            at_ms,
        )
        .await;
    }

    async fn dropped(&self) {
        self.dropped_by(T_REQ, "on-call", "runaway", None).await;
    }

    async fn completed(&self, at_ms: i64) {
        self.deliver(
            EventPayload::Completed(CompletedPayload {
                task_status: Default::default(),
                result_summary: Some("done".to_string()),
                total_llm_calls: 1,
                total_tool_calls: 0,
                total_cost: 0.25,
                total_duration_ms: 10,
            }),
            at_ms,
        )
        .await;
    }

    async fn failed(&self, at_ms: i64) {
        self.deliver(
            EventPayload::Failed(FailedPayload {
                error_kind: FailureKind::LlmError,
                error_message: "the provider refused the request".to_string(),
                phase: FailurePhase::LlmRequest,
                partial_totals: InvocationTotals::default(),
                detail: None,
            }),
            at_ms,
        )
        .await;
    }

    /// The worker's archive, saying `final_phase`, with the run's state; its
    /// event is stamped well after the run ended, as a sweeper's hand-off
    /// can be.
    async fn archived(&self, final_phase: &str, terminal_at_ms: i64) {
        self.deliver(
            EventPayload::InvocationArchived(InvocationArchivedPayload {
                worker_id: self.worker_id.clone(),
                final_phase: final_phase.to_string(),
                final_state_blob: b"{\"state\":1}".to_vec(),
                started_at_ms: 1_000,
                terminal_at_ms,
            }),
            terminal_at_ms + 86_400_000,
        )
        .await;
    }

    /// The worker holding the run's own ending says how the run ended
    /// (issue 1071), at `ended_at_ms`; its event is stamped a day later, as
    /// a held row's can be.
    async fn held(&self, outcome: HeldOutcome, ended_at_ms: i64) {
        self.deliver(
            EventPayload::InvocationEndingHeld(InvocationEndingHeldPayload {
                outcome,
                ended_at_ms,
                started_at_ms: 1_000,
                worker_id: self.worker_id.clone(),
                reason: EndingHeldReason::StoredEventRefused,
                totals: None,
            }),
            ended_at_ms + 86_400_000,
        )
        .await;
    }

    /// The row's record: its state, who ended it and when, and the request.
    async fn record(&self) -> Option<Record> {
        self.store
            .get_invocation_owner(&self.invocation_id.to_string())
            .await
            .unwrap()
            .map(|row: OwnerRow| Record {
                status: row.status,
                archive: row.archive_state,
                ended_by: row.ended_by,
                ended_at: row.ended_at,
                termination: row.termination,
            })
    }

    /// The archive record's phase and state, if one is written.
    async fn archive(&self) -> Option<(String, Vec<u8>)> {
        self.store
            .get_archive(&self.invocation_id.to_string())
            .await
            .unwrap()
            .map(|row| (row.final_phase, row.final_state_blob))
    }
}

/// What a row records, less its worker and times of ownership.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Record {
    status: OwnerStatus,
    archive: ArchiveState,
    ended_by: Option<EndedBy>,
    ended_at: Option<i64>,
    termination: Option<TerminationRequest>,
}

fn record(
    status: OwnerStatus,
    archive: ArchiveState,
    ended: Option<(EndedBy, i64)>,
    requested: bool,
) -> Option<Record> {
    Some(Record {
        status,
        archive,
        ended_by: ended.map(|(by, _)| by),
        ended_at: ended.map(|(_, at)| at),
        termination: requested.then(the_request),
    })
}

/// The request [`Fixture::dropped`] makes.
fn the_request() -> TerminationRequest {
    TerminationRequest {
        requested_at: T_REQ,
        requested_by: Some("on-call".to_string()),
        reason: Some("runaway".to_string()),
    }
}

fn the_state() -> Vec<u8> {
    b"{\"state\":1}".to_vec()
}

/// **A drop of a run that has not ended ends it, provisionally, and
/// records the request.** From no row, `in_flight` and `ambiguous`: the
/// row reads `failed`, ended by the drop at the drop's own time, the
/// request beside it, and no archive record is written, since the worker's
/// archive brings the run's real state. A drop saying no archive will come
/// leaves the archive `none`; any other leaves it `pending`.
#[tokio::test]
async fn a_drop_of_a_run_not_ended_ends_it_provisionally_and_records_the_request() {
    for seed in [
        None,
        Some(OwnerStatus::InFlight),
        Some(OwnerStatus::Ambiguous),
    ] {
        for (archive, state) in [
            (None, ArchiveState::Pending),
            (Some(DropArchive::Pending), ArchiveState::Pending),
            (Some(DropArchive::Unknown), ArchiveState::Pending),
            (Some(DropArchive::WillNotCome), ArchiveState::WillNotCome),
        ] {
            let fixture = Fixture::new().await;
            if let Some(status) = seed {
                fixture.seeded(status).await;
            }
            fixture
                .dropped_by(T_REQ, "on-call", "runaway", archive)
                .await;
            assert_eq!(
                (fixture.record().await, fixture.archive().await),
                (
                    record(
                        OwnerStatus::Failed,
                        state,
                        Some((EndedBy::Drop, T_REQ)),
                        true
                    ),
                    None
                ),
                "from {seed:?}, a drop saying archive {archive:?}"
            );
        }
    }
}

/// **The ending's time is each event's own, never the fold's or a
/// hand-off's.** A `completed`, a `failed` and a drop are the ending, so
/// each records its event's time, which a republished event keeps; every
/// event here is stamped far from the fold's clock. An archive that ends a
/// run whose ending was not folded records the time the run ended
/// (`terminal_at_ms`), not its event's, which is the hand-off's, a day
/// later here. The run is the ender of its own ending, the drop of its.
#[tokio::test]
async fn the_ending_time_is_each_events_own() {
    let fixture = Fixture::new().await;
    fixture.dropped().await;
    assert_eq!(
        fixture.record().await.and_then(|row| row.ended_at),
        Some(T_REQ),
        "a drop's ending is at the drop's own time"
    );

    let fixture = Fixture::new().await;
    fixture.completed(T_BEFORE).await;
    assert_eq!(
        fixture.record().await,
        record(
            OwnerStatus::Completed,
            ArchiveState::Pending,
            Some((EndedBy::Run, T_BEFORE)),
            false
        )
    );

    let fixture = Fixture::new().await;
    fixture.failed(T_BEFORE).await;
    assert_eq!(
        fixture.record().await,
        record(
            OwnerStatus::Failed,
            ArchiveState::Pending,
            Some((EndedBy::Run, T_BEFORE)),
            false
        )
    );

    let fixture = Fixture::new().await;
    fixture.archived("completed", T_BEFORE).await;
    assert_eq!(
        fixture.record().await,
        record(
            OwnerStatus::Completed,
            ArchiveState::Archived,
            Some((EndedBy::Run, T_BEFORE)),
            false
        )
    );
}

/// **A drop of a run that had completed keeps it completed and records the
/// request**, and its archive record keeps the worker's state: the status
/// and the record agree.
#[tokio::test]
async fn a_drop_after_a_completed_run_records_the_request_and_keeps_completed() {
    let fixture = Fixture::new().await;
    fixture.completed(T_BEFORE).await;
    fixture.archived("completed", T_BEFORE).await;
    fixture.dropped().await;
    assert_eq!(
        (fixture.record().await, fixture.archive().await),
        (
            record(
                OwnerStatus::Completed,
                ArchiveState::Archived,
                Some((EndedBy::Run, T_BEFORE)),
                true
            ),
            Some(("completed".to_string(), the_state()))
        )
    );
}

/// **The run's own ending, folded after a drop, replaces the drop's
/// provisional one**: the run reached its ending first, though the drop
/// was folded first.
#[tokio::test]
async fn a_completed_folded_after_a_drop_replaces_its_provisional_ending() {
    let fixture = Fixture::new().await;
    fixture.dropped().await;
    fixture.completed(T_AFTER).await;
    assert_eq!(
        fixture.record().await,
        record(
            OwnerStatus::Completed,
            ArchiveState::Pending,
            Some((EndedBy::Run, T_AFTER)),
            true
        )
    );
}

/// **A drop and the run's own ending give the same record in either fold
/// order, and twice over.** For `completed` and `failed`, ended before the
/// request and after it: the run is its ender, at its ending's time, with
/// the request beside it.
#[tokio::test]
async fn a_drop_and_an_ending_read_the_same_in_either_fold_order() {
    for ending in ["completed", "failed"] {
        for t_end in [T_BEFORE, T_AFTER] {
            let mut records = Vec::new();
            for drop_first in [true, false] {
                let fixture = Fixture::new().await;
                for _ in 0..2 {
                    if drop_first {
                        fixture.dropped().await;
                    }
                    match ending {
                        "completed" => fixture.completed(t_end).await,
                        _ => fixture.failed(t_end).await,
                    }
                    if !drop_first {
                        fixture.dropped().await;
                    }
                }
                records.push(fixture.record().await);
            }
            let status = if ending == "completed" {
                OwnerStatus::Completed
            } else {
                OwnerStatus::Failed
            };
            let expected = record(
                status,
                ArchiveState::Pending,
                Some((EndedBy::Run, t_end)),
                true,
            );
            assert_eq!(
                records,
                vec![expected.clone(), expected],
                "{ending} at {t_end}: the drop folded first, then last"
            );
        }
    }
}

/// **A drop whose mark won is archived with the run's state.** No ending of
/// the run's own follows the drop; the worker's sweeper hands the row the
/// drop marked off, saying `failed`, and the drop's ending becomes final
/// with the run's real state in the archive record.
#[tokio::test]
async fn a_drop_whose_mark_won_is_archived_with_the_runs_state() {
    let fixture = Fixture::new().await;
    fixture.dropped().await;
    fixture.archived("failed", T_REQ + 5).await;
    assert_eq!(
        (fixture.record().await, fixture.archive().await),
        (
            record(
                OwnerStatus::Failed,
                ArchiveState::Archived,
                Some((EndedBy::Drop, T_REQ)),
                true
            ),
            Some(("failed".to_string(), the_state()))
        )
    );
}

/// **A drop of a run that failed by itself keeps its own failure**, in
/// either order: the run is its ender, the request beside it.
#[tokio::test]
async fn a_drop_of_a_run_that_failed_keeps_its_own_failure() {
    let fixture = Fixture::new().await;
    fixture.failed(T_BEFORE).await;
    fixture.dropped().await;
    assert_eq!(
        fixture.record().await,
        record(
            OwnerStatus::Failed,
            ArchiveState::Pending,
            Some((EndedBy::Run, T_BEFORE)),
            true
        )
    );

    let fixture = Fixture::new().await;
    fixture.dropped().await;
    fixture.failed(T_AFTER).await;
    assert_eq!(
        fixture.record().await,
        record(
            OwnerStatus::Failed,
            ArchiveState::Pending,
            Some((EndedBy::Run, T_AFTER)),
            true
        )
    );
}

/// **A second drop is a duplicate: the first request is kept**, on a run
/// the first drop ended and on one that had ended by itself.
#[tokio::test]
async fn a_second_drop_is_a_duplicate() {
    let fixture = Fixture::new().await;
    fixture.dropped().await;
    fixture
        .dropped_by(T_AFTER, "someone-else", "again", None)
        .await;
    assert_eq!(
        fixture.record().await,
        record(
            OwnerStatus::Failed,
            ArchiveState::Pending,
            Some((EndedBy::Drop, T_REQ)),
            true
        )
    );

    let fixture = Fixture::new().await;
    fixture.completed(T_BEFORE).await;
    fixture.dropped().await;
    fixture
        .dropped_by(T_AFTER, "someone-else", "again", None)
        .await;
    assert_eq!(
        fixture.record().await,
        record(
            OwnerStatus::Completed,
            ArchiveState::Pending,
            Some((EndedBy::Run, T_BEFORE)),
            true
        )
    );
}

/// **An archive saying `completed` replaces a provisional drop**, its
/// archive pending or none to come: the run reached its ending, at the
/// time its archive says, and the record holds its state.
#[tokio::test]
async fn an_archive_saying_completed_replaces_a_provisional_drop() {
    for archive in [None, Some(DropArchive::WillNotCome)] {
        let fixture = Fixture::new().await;
        fixture
            .dropped_by(T_REQ, "on-call", "runaway", archive)
            .await;
        fixture.archived("completed", T_AFTER).await;
        assert_eq!(
            (fixture.record().await, fixture.archive().await),
            (
                record(
                    OwnerStatus::Completed,
                    ArchiveState::Archived,
                    Some((EndedBy::Run, T_AFTER)),
                    true
                ),
                Some(("completed".to_string(), the_state()))
            ),
            "a drop saying archive {archive:?}"
        );
    }
}

/// **A `completed` after a dropped run is archived is a conflict**: the
/// drop's ending is final once its archive landed `failed`, and the row
/// keeps it.
#[tokio::test]
async fn a_completed_after_a_dropped_run_is_archived_is_a_conflict() {
    let fixture = Fixture::new().await;
    fixture.dropped().await;
    fixture.archived("failed", T_REQ + 5).await;
    fixture.completed(T_AFTER).await;
    assert_eq!(
        fixture.record().await,
        record(
            OwnerStatus::Failed,
            ArchiveState::Archived,
            Some((EndedBy::Drop, T_REQ)),
            true
        )
    );
}

/// **A held ending is the run's own, at the time the run ended.** The
/// worker holding a run's stored ending publishes `invocation.ending_held`
/// in its place (issue 1071), possibly long after the run ended. The row
/// records the run as its ender at the time it ended (`ended_at_ms`), not
/// at the event's time, as for an archive.
#[tokio::test]
async fn a_held_ending_is_the_runs_own_at_the_time_the_run_ended() {
    for (outcome, status) in [
        (HeldOutcome::Completed, OwnerStatus::Completed),
        (HeldOutcome::Failed, OwnerStatus::Failed),
    ] {
        let fixture = Fixture::new().await;
        fixture.held(outcome, T_BEFORE).await;
        assert_eq!(
            fixture.record().await,
            record(
                status,
                ArchiveState::Pending,
                Some((EndedBy::Run, T_BEFORE)),
                false
            ),
            "a held {outcome:?}"
        );
    }
}

/// **A held ending replaces a drop's provisional ending**, as the run's
/// own does: the run becomes the ender, at the time it ended, the request
/// stays on record, and the archive is pending, since the ending brings
/// one. So it does after a drop that said no archive would come.
#[tokio::test]
async fn a_held_ending_replaces_a_drops_provisional_ending() {
    for archive in [None, Some(DropArchive::WillNotCome)] {
        for (outcome, status) in [
            (HeldOutcome::Completed, OwnerStatus::Completed),
            (HeldOutcome::Failed, OwnerStatus::Failed),
        ] {
            let fixture = Fixture::new().await;
            fixture
                .dropped_by(T_REQ, "on-call", "runaway", archive)
                .await;
            fixture.held(outcome, T_AFTER).await;
            assert_eq!(
                fixture.record().await,
                record(
                    status,
                    ArchiveState::Pending,
                    Some((EndedBy::Run, T_AFTER)),
                    true
                ),
                "a held {outcome:?} after a drop saying archive {archive:?}"
            );
        }
    }
}
