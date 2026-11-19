//! [`apply`] against a store: what it answers, and what it leaves the row
//! as, for every event from every state; and what it does with a row it
//! cannot read and a row that will not stay still.
//!
//! The answer is the seam the coordination consumer logs from (a move or
//! a duplicate quietly, a refusal at warn, a conflict at error), so each
//! case pins the answer and the row together: a mis-mapped answer would
//! leave the row right and the log wrong.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::control_plane::store::{
    ControlPlaneStore, ControlPlaneStoreError, EndedBy, MoveFacts, OwnerStatus, TerminationRequest,
};

const BEFORE: &str = "worker-before";
const EVENT: &str = "worker-of-the-event";

const STATUSES: [OwnerStatus; 4] = [
    OwnerStatus::InFlight,
    OwnerStatus::Ambiguous,
    OwnerStatus::Completed,
    OwnerStatus::Failed,
];
const ARCHIVES: [ArchiveState; 4] = [
    ArchiveState::NotDue,
    ArchiveState::Pending,
    ArchiveState::Archived,
    ArchiveState::WillNotCome,
];
const ENDERS: [Option<EndedBy>; 3] = [None, Some(EndedBy::Run), Some(EndedBy::Drop)];

/// Every state, whether a row can hold it or not: each status, archive
/// state and ender, with and without a request.
fn every_state() -> Vec<RowState> {
    let mut states = Vec::new();
    for status in STATUSES {
        for archive in ARCHIVES {
            for ended_by in ENDERS {
                for requested in [false, true] {
                    states.push(RowState {
                        status,
                        archive,
                        ended_by,
                        requested,
                    });
                }
            }
        }
    }
    states
}

/// Every state a row can be in, `None` for no row.
fn states() -> Vec<Option<RowState>> {
    std::iter::once(None)
        .chain(
            every_state()
                .into_iter()
                .filter(|state| state.is_valid())
                .map(Some),
        )
        .collect()
}

/// The request a seeded row records, and the time its ender is recorded
/// at.
fn seeded_request() -> TerminationRequest {
    TerminationRequest {
        requested_at: 50,
        requested_by: Some("first".to_string()),
        reason: Some("the first request".to_string()),
    }
}
const SEEDED_ENDED_AT: i64 = 40;

/// The request the event brings, and its ending's time.
fn the_events_request() -> TerminationRequest {
    TerminationRequest {
        requested_at: 300,
        requested_by: Some("second".to_string()),
        reason: None,
    }
}
const THE_EVENTS_ENDED_AT: i64 = 300;

/// What a row seeded in `state` records beside it.
fn seeded<'a>(state: RowState, request: &'a TerminationRequest) -> MoveFacts<'a> {
    MoveFacts {
        worker_id: Some(BEFORE),
        assigned_at: Some(100),
        ended_at: state.ended_by.map(|_| SEEDED_ENDED_AT),
        request: state.requested.then_some(request),
    }
}

async fn store() -> (ControlPlaneStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = ControlPlaneStore::open(&dir.path().join("control-plane.db"))
        .await
        .unwrap();
    (store, dir)
}

#[tokio::test]
async fn every_event_from_every_state_answers_and_moves_as_decided() {
    let (store, _dir) = store().await;
    let mut cases = 0;
    for state in states() {
        for what in OwnershipEvent::ALL {
            cases += 1;
            let id = format!("inv-{cases}");
            let first = seeded_request();
            if let Some(state) = state {
                assert!(
                    store
                        .compare_and_set_ownership(&id, None, state, seeded(state, &first))
                        .await
                        .unwrap()
                );
            }
            let before = store.get_invocation_owner(&id).await.unwrap();
            let second = the_events_request();
            let applied = apply(
                &store,
                &id,
                what,
                OwnerFacts {
                    worker_id: EVENT,
                    at_ms: 200,
                    ended_at_ms: THE_EVENTS_ENDED_AT,
                    request: Some(&second),
                },
            )
            .await
            .unwrap();
            let after = store.get_invocation_owner(&id).await.unwrap();
            let context = format!("{what:?} from {state:?}");
            let (want, moved) = match (transition(state, what), state) {
                (Decision::Ignore, _) => (Applied::Ignored, None),
                (Decision::Duplicate, Some(at)) => (Applied::Duplicate { at }, None),
                (Decision::Refused, Some(recorded)) => (Applied::Refused { recorded }, None),
                (Decision::Conflict { then: None }, Some(recorded)) => (
                    Applied::Conflict {
                        recorded,
                        moved_to: None,
                    },
                    None,
                ),
                (
                    Decision::Conflict {
                        then: Some((to, owner)),
                    },
                    Some(recorded),
                ) => (
                    Applied::Conflict {
                        recorded,
                        moved_to: Some(to),
                    },
                    Some((to, owner)),
                ),
                (Decision::Move { to, owner }, from) => {
                    (Applied::Moved { from, to }, Some((to, owner)))
                }
                (decision, None) => panic!("{context}: {decision:?} with no row"),
            };
            assert_eq!(applied, want, "{context}");
            match moved {
                None => assert_eq!(after, before, "{context}: the row is left as it was"),
                Some((to, owner)) => {
                    let row = after.expect("a move leaves a row");
                    assert_eq!(row.state(), to, "{context}");
                    let (worker, at) = match (state, owner) {
                        (None, _) | (Some(_), Owner::TakeBoth) => (EVENT, 200),
                        (Some(_), Owner::TakeWorker) => (EVENT, 100),
                        (Some(_), Owner::Keep) => (BEFORE, 100),
                    };
                    assert_eq!(
                        (row.worker_id.as_str(), row.assigned_at),
                        (worker, at),
                        "{context}: who the row names"
                    );
                    // The record: a new ender takes the event's ending time,
                    // and the first request is the one kept.
                    let ender = state.and_then(|state| state.ended_by);
                    let ended_at = if to.ended_by != ender {
                        Some(THE_EVENTS_ENDED_AT)
                    } else {
                        before.as_ref().and_then(|row| row.ended_at)
                    };
                    let termination = match before.as_ref().and_then(|row| row.termination.clone())
                    {
                        Some(kept) => Some(kept),
                        None => to.requested.then(the_events_request),
                    };
                    assert_eq!(
                        (row.ended_at, row.termination),
                        (ended_at, termination),
                        "{context}: the termination record"
                    );
                }
            }
        }
    }
    assert_eq!(cases, 29 * 13);
}

/// **A durable start writes the row its run lacks, and leaves every other
/// alone.** With no row, it writes `in_flight` under the worker and time
/// it names. A row in flight already answers as a duplicate and keeps its
/// own worker and time. An `ambiguous` or ended row answers as refused,
/// with the row as it found it, and stays exactly as it was: status,
/// archive, worker and time, and its termination record.
#[tokio::test]
async fn a_durable_start_writes_a_missing_row_and_refuses_the_rest() {
    let (store, _dir) = store().await;
    // A start brings no ending and no request; the time is not read.
    let start = OwnerFacts {
        worker_id: EVENT,
        at_ms: 200,
        ended_at_ms: THE_EVENTS_ENDED_AT,
        request: None,
    };

    let applied = apply(&store, "inv-new", OwnershipEvent::DurablyStarted, start)
        .await
        .unwrap();
    assert_eq!(
        applied,
        Applied::Moved {
            from: None,
            to: RowState::IN_FLIGHT
        }
    );
    let row = store
        .get_invocation_owner("inv-new")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            row.state(),
            row.worker_id.as_str(),
            row.assigned_at,
            row.ended_at,
            row.termination
        ),
        (RowState::IN_FLIGHT, EVENT, 200, None, None)
    );

    for (n, state) in states().into_iter().flatten().enumerate() {
        let id = format!("inv-{n}");
        let first = seeded_request();
        assert!(
            store
                .compare_and_set_ownership(&id, None, state, seeded(state, &first))
                .await
                .unwrap()
        );
        let before = store.get_invocation_owner(&id).await.unwrap();
        let applied = apply(&store, &id, OwnershipEvent::DurablyStarted, start)
            .await
            .unwrap();
        let want = if state == RowState::IN_FLIGHT {
            Applied::Duplicate { at: state }
        } else {
            Applied::Refused { recorded: state }
        };
        assert_eq!(applied, want, "{}", state.name());
        let after = store.get_invocation_owner(&id).await.unwrap();
        assert_eq!(after, before, "{}: the row is left as it was", state.name());
        let after = after.unwrap();
        assert_eq!(
            (after.worker_id.as_str(), after.assigned_at),
            (BEFORE, 100),
            "{}: who and when the row names",
            state.name()
        );
    }
}

/// The ambiguous move of a row recovery restored keeps the worker it
/// names: the agent id does not take its place.
#[tokio::test]
async fn ambiguous_keeps_a_restored_rows_worker_and_time() {
    let (store, _dir) = store().await;
    store
        .upsert_invocation_ownership("inv", "worker-real", 50, OwnerStatus::InFlight)
        .await
        .unwrap();
    let applied = apply(
        &store,
        "inv",
        OwnershipEvent::Ambiguous,
        OwnerFacts {
            worker_id: "agent",
            at_ms: 60,
            ended_at_ms: 60,
            request: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        applied,
        Applied::Moved {
            from: Some(RowState::IN_FLIGHT),
            to: RowState::AMBIGUOUS
        }
    );
    let row = store.get_invocation_owner("inv").await.unwrap().unwrap();
    assert_eq!(
        (row.worker_id.as_str(), row.assigned_at, row.status),
        ("worker-real", 50, OwnerStatus::Ambiguous)
    );
}

/// A status this build does not know: nothing is written, and the
/// caller is told the row cannot be read.
#[tokio::test]
async fn a_row_this_build_cannot_read_is_unreadable_and_left_alone() {
    let (store, dir) = store().await;
    store
        .upsert_invocation_ownership("inv", "w", 1, OwnerStatus::Failed)
        .await
        .unwrap();
    // A second handle on the same file, as a hand edit would be.
    let by_hand = sqlx::SqlitePool::connect(&format!(
        "sqlite://{}",
        dir.path().join("control-plane.db").display()
    ))
    .await
    .unwrap();
    sqlx::query("UPDATE coordination_invocation_owner SET status = 'weird'")
        .execute(&by_hand)
        .await
        .unwrap();
    let err = apply(
        &store,
        "inv",
        OwnershipEvent::Ended(Ending::Completed),
        OwnerFacts {
            worker_id: "agent",
            at_ms: 2,
            ended_at_ms: 2,
            request: None,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(err, OwnershipError::Unreadable { .. }), "{err:?}");
    let status: String = sqlx::query_scalar("SELECT status FROM coordination_invocation_owner")
        .fetch_one(&by_hand)
        .await
        .unwrap();
    assert_eq!(status, "weird");
}

/// A store whose row reads as `rows` give it, one read at a time (the
/// last repeated), and whose compare-and-set fails the first `lose`
/// times.
struct Racing {
    rows: Vec<Option<RowState>>,
    reads: AtomicUsize,
    lose: usize,
    writes: AtomicUsize,
    written: Mutex<Vec<(Option<RowState>, RowState)>>,
}

impl Racing {
    fn new(rows: Vec<Option<RowState>>, lose: usize) -> Self {
        Self {
            rows,
            reads: AtomicUsize::new(0),
            lose,
            writes: AtomicUsize::new(0),
            written: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl OwnershipStore for Racing {
    async fn ownership(
        &self,
        invocation_id: &str,
    ) -> Result<Option<OwnerRow>, ControlPlaneStoreError> {
        let read = self.reads.fetch_add(1, Ordering::SeqCst);
        let state = self.rows[read.min(self.rows.len() - 1)];
        Ok(state.map(|state| OwnerRow {
            invocation_id: invocation_id.to_string(),
            worker_id: BEFORE.to_string(),
            assigned_at: 100,
            status: state.status,
            archive_state: state.archive,
            ended_by: state.ended_by,
            ended_at: state.ended_by.map(|_| SEEDED_ENDED_AT),
            termination: state.requested.then(seeded_request),
        }))
    }

    async fn compare_and_set_ownership(
        &self,
        _invocation_id: &str,
        expected: Option<RowState>,
        to: RowState,
        _facts: MoveFacts<'_>,
    ) -> Result<bool, ControlPlaneStoreError> {
        let write = self.writes.fetch_add(1, Ordering::SeqCst);
        if write < self.lose {
            return Ok(false);
        }
        self.written.lock().unwrap().push((expected, to));
        Ok(true)
    }
}

const FACTS: OwnerFacts<'static> = OwnerFacts {
    worker_id: EVENT,
    at_ms: 200,
    ended_at_ms: 200,
    request: None,
};

/// A write that lost its race is decided again from the row as it then
/// reads: here a run whose `failed` was folded between the read and the
/// write, so the late report is refused rather than written over it.
#[tokio::test]
async fn a_lost_race_is_decided_again_from_the_row_as_it_then_is() {
    let failed = RowState::ended(Ending::Failed, ArchiveState::Pending);
    let store = Racing::new(vec![Some(RowState::IN_FLIGHT), Some(failed)], 1);
    let applied = apply(&store, "inv", OwnershipEvent::Ambiguous, FACTS)
        .await
        .unwrap();
    assert_eq!(applied, Applied::Refused { recorded: failed });
    assert_eq!(store.writes.load(Ordering::SeqCst), 1, "one write, lost");
    assert!(store.written.lock().unwrap().is_empty());
}

/// A row that changes under every attempt: the attempts are spent, and
/// nothing is written.
#[tokio::test]
async fn a_row_that_never_settles_is_contended_after_the_attempts() {
    let store = Racing::new(vec![Some(RowState::IN_FLIGHT)], usize::MAX);
    let err = apply(
        &store,
        "inv",
        OwnershipEvent::Ended(Ending::Completed),
        FACTS,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, OwnershipError::Contended { attempts, .. } if attempts == APPLY_ATTEMPTS),
        "{err:?}"
    );
    assert_eq!(store.writes.load(Ordering::SeqCst), APPLY_ATTEMPTS);
    assert!(store.written.lock().unwrap().is_empty());
}

/// The same for a row created between the read and the insert: the
/// insert writes nothing, and the event is decided again on the row that
/// is there now.
#[tokio::test]
async fn a_lost_insert_is_decided_again_on_the_row_that_won() {
    let store = Racing::new(vec![None, Some(RowState::IN_FLIGHT)], 1);
    let applied = apply(&store, "inv", OwnershipEvent::Ambiguous, FACTS)
        .await
        .unwrap();
    assert_eq!(
        applied,
        Applied::Moved {
            from: Some(RowState::IN_FLIGHT),
            to: RowState::AMBIGUOUS
        }
    );
    assert_eq!(
        *store.written.lock().unwrap(),
        vec![(Some(RowState::IN_FLIGHT), RowState::AMBIGUOUS)]
    );
}

/// **The states a row can hold are the ones the store admits** (issue
/// 1070). `RowState::is_valid` says which status, archive state, ender and
/// request go together, and the store's checks say the same of every row
/// written; the table moves only to valid states, so the two agreeing is
/// what keeps the consumer from attempting a write the store refuses.
/// Every state is written straight into the table, its ender with the
/// ending's time and its request with the request's: the store takes
/// exactly the valid ones, `failed·none` and a drop's `failed` among them,
/// and refuses the rest.
#[tokio::test]
async fn the_pairs_a_row_can_hold_are_the_ones_the_store_admits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    ControlPlaneStore::open(&path).await.unwrap();
    let raw = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    let mut admitted = 0;
    for (n, state) in every_state().into_iter().enumerate() {
        let written = sqlx::query(
            "INSERT INTO coordination_invocation_owner \
             (invocation_id, worker_id, assigned_at, status, archive_state, ended_by, ended_at, \
              termination_requested_at, termination_requested_by, termination_reason) \
             VALUES (?, 'w', 1, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(format!("inv-{n}"))
        .bind(state.status.as_str())
        .bind(state.archive.as_str())
        .bind(state.ended_by.map(EndedBy::as_str))
        .bind(state.ended_by.map(|_| 2_i64))
        .bind(state.requested.then_some(3_i64))
        .bind(state.requested.then_some("on-call"))
        .bind(state.requested.then_some("runaway"))
        .execute(&raw)
        .await;
        assert_eq!(
            written.is_ok(),
            state.is_valid(),
            "{state:?}: the store and the table disagree: {written:?}"
        );
        admitted += usize::from(written.is_ok());
    }
    assert_eq!(admitted, 28, "the states a row can hold");
}
