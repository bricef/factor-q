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
use crate::control_plane::store::OwnerStatus;

const BEFORE: &str = "worker-before";
const EVENT: &str = "worker-of-the-event";

/// Every state a row can be in, `None` for no row.
fn states() -> Vec<Option<RowState>> {
    let mut states = vec![None, Some(RowState::IN_FLIGHT), Some(RowState::AMBIGUOUS)];
    for ending in [Ending::Completed, Ending::Failed] {
        for archive in [ArchiveState::Pending, ArchiveState::Archived] {
            states.push(Some(RowState::ended(ending, archive)));
        }
    }
    states
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
            if let Some(state) = state {
                assert!(
                    store
                        .compare_and_set_ownership(&id, None, state, Some(BEFORE), Some(100))
                        .await
                        .unwrap()
                );
            }
            let before = store.get_invocation_owner(&id).await.unwrap();
            let applied = apply(
                &store,
                &id,
                what,
                OwnerFacts {
                    worker_id: EVENT,
                    at_ms: 200,
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
                }
            }
        }
    }
    assert_eq!(cases, 7 * 13);
}

/// **A durable start writes the row its run lacks, and leaves every other
/// alone.** With no row, it writes `in_flight` under the worker and time
/// it names. A row in flight already answers as a duplicate and keeps its
/// own worker and time. An `ambiguous` or ended row answers as refused,
/// with the row as it found it, and stays exactly as it was: status,
/// archive, worker and time.
#[tokio::test]
async fn a_durable_start_writes_a_missing_row_and_refuses_the_rest() {
    let (store, _dir) = store().await;
    let start = OwnerFacts {
        worker_id: EVENT,
        at_ms: 200,
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
        (row.state(), row.worker_id.as_str(), row.assigned_at),
        (RowState::IN_FLIGHT, EVENT, 200)
    );

    for (n, state) in states().into_iter().flatten().enumerate() {
        let id = format!("inv-{n}");
        assert!(
            store
                .compare_and_set_ownership(&id, None, state, Some(BEFORE), Some(100))
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
        }))
    }

    async fn compare_and_set_ownership(
        &self,
        _invocation_id: &str,
        expected: Option<RowState>,
        to: RowState,
        _worker_id: Option<&str>,
        _assigned_at: Option<i64>,
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
