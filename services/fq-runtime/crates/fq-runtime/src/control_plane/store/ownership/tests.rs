//! Tests for the ownership row's storage: the archive's state beside the
//! status, the rung that adds it, and the compare-and-set every move goes
//! through. What [`crate::control_plane::ownership::apply`] answers, from
//! every state, is pinned beside it.

use super::super::{MIGRATIONS, SCHEMA_CLASS};
use super::*;
use crate::control_plane::ownership::{ArchiveExpected, Ending, RowState};
use crate::db::schema;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use tempfile::tempdir;

/// What a move writes of the owner facts, and nothing of the termination
/// record.
fn owner(worker_id: Option<&str>, assigned_at: Option<i64>) -> MoveFacts<'_> {
    MoveFacts {
        worker_id,
        assigned_at,
        ..MoveFacts::default()
    }
}

/// The write lands only on the state it expects, and writes nothing
/// otherwise: not over another state, not over a row that is not there,
/// and no second insert over a row that is.
#[tokio::test]
async fn a_compare_and_set_lands_only_on_the_state_it_expects() {
    let dir = tempdir().unwrap();
    let store = ControlPlaneStore::open(&dir.path().join("control-plane.db"))
        .await
        .unwrap();
    let pending = RowState::ended(Ending::Failed, ArchiveState::Pending);
    let archived = RowState::ended(Ending::Failed, ArchiveState::Archived);
    assert!(
        !store
            .compare_and_set_ownership("inv", Some(RowState::IN_FLIGHT), pending, owner(None, None))
            .await
            .unwrap(),
        "no row to update"
    );
    assert!(
        store
            .compare_and_set_ownership("inv", None, RowState::IN_FLIGHT, owner(Some("w"), Some(1)))
            .await
            .unwrap()
    );
    assert!(
        !store
            .compare_and_set_ownership("inv", None, pending, owner(Some("x"), Some(2)))
            .await
            .unwrap(),
        "no second insert"
    );
    assert!(
        !store
            .compare_and_set_ownership("inv", Some(RowState::AMBIGUOUS), pending, owner(None, None))
            .await
            .unwrap(),
        "not from a state the row is not in"
    );
    assert!(
        store
            .compare_and_set_ownership("inv", Some(RowState::IN_FLIGHT), pending, owner(None, None))
            .await
            .unwrap()
    );
    assert!(
        !store
            .compare_and_set_ownership(
                "inv",
                Some(archived),
                RowState::ended(Ending::Completed, ArchiveState::Archived),
                owner(Some("other"), Some(3))
            )
            .await
            .unwrap(),
        "not from the row's status with another archive state"
    );
    assert_eq!(
        store.get_invocation_owner("inv").await.unwrap().map(|row| (
            row.state(),
            row.worker_id,
            row.assigned_at
        )),
        Some((pending, "w".to_string(), 1)),
        "a write refused for its archive state wrote nothing"
    );
    assert!(
        store
            .compare_and_set_ownership("inv", Some(pending), archived, owner(Some("real"), None))
            .await
            .unwrap()
    );
    let row = store.get_invocation_owner("inv").await.unwrap().unwrap();
    assert_eq!(
        (row.worker_id.as_str(), row.assigned_at, row.state()),
        ("real", 1, archived),
        "the worker given replaces the row's, the time not given is kept"
    );
}

/// **The compare-and-set compares the ender and the request too, and
/// writes the termination record as a move says** (issue 1070). A move
/// lands only on a row still in the state it was decided from, its ender
/// and whether it records a request included; the ending's time is written
/// with a new ender and kept otherwise; and a request is written only
/// where none is on record, so the first one is kept.
#[tokio::test]
async fn a_compare_and_set_lands_only_on_the_ender_and_request_it_expects() {
    let dir = tempdir().unwrap();
    let store = ControlPlaneStore::open(&dir.path().join("control-plane.db"))
        .await
        .unwrap();
    let first = TerminationRequest {
        requested_at: 10,
        requested_by: Some("first".to_string()),
        reason: Some("the first request".to_string()),
    };
    let second = TerminationRequest {
        requested_at: 20,
        requested_by: Some("second".to_string()),
        reason: None,
    };
    let dropped = RowState::dropped(ArchiveExpected::Yes);
    assert!(
        store
            .compare_and_set_ownership(
                "inv",
                None,
                dropped,
                MoveFacts {
                    worker_id: Some("w"),
                    assigned_at: Some(1),
                    ended_at: Some(10),
                    request: Some(&first),
                },
            )
            .await
            .unwrap()
    );
    let ran = RowState::ended(Ending::Failed, ArchiveState::Pending)
        .by(EndedBy::Run)
        .with_request();
    assert!(
        !store
            .compare_and_set_ownership("inv", Some(ran), ran, MoveFacts::default())
            .await
            .unwrap(),
        "not from the row's pair with another ender"
    );
    let unrequested = RowState {
        requested: false,
        ..dropped
    };
    assert!(
        !store
            .compare_and_set_ownership("inv", Some(unrequested), dropped, MoveFacts::default())
            .await
            .unwrap(),
        "not from the row's pair and ender with no request on record"
    );
    let completed = RowState::ended(Ending::Completed, ArchiveState::Pending)
        .by(EndedBy::Run)
        .with_request();
    assert!(
        store
            .compare_and_set_ownership(
                "inv",
                Some(dropped),
                completed,
                MoveFacts {
                    ended_at: Some(30),
                    request: Some(&second),
                    ..MoveFacts::default()
                },
            )
            .await
            .unwrap()
    );
    let row = store.get_invocation_owner("inv").await.unwrap().unwrap();
    assert_eq!(
        (row.state(), row.ended_at, row.termination),
        (completed, Some(30), Some(first.clone())),
        "a new ender takes its time, and the first request is kept"
    );
    let archived = RowState {
        archive: ArchiveState::Archived,
        ..completed
    };
    assert!(
        store
            .compare_and_set_ownership("inv", Some(completed), archived, MoveFacts::default())
            .await
            .unwrap()
    );
    let row = store.get_invocation_owner("inv").await.unwrap().unwrap();
    assert_eq!(
        (row.state(), row.ended_at, row.termination),
        (archived, Some(30), Some(first)),
        "a move that records no new ender keeps the ending's time"
    );
}

/// A control plane at the version before the rung that adds the column,
/// holding one row of each status, opened by this build: each row gets
/// the archive's state its status had before the column existed, and the
/// store is at the current version. The rung is found by its SQL, so the
/// test names no version number.
#[tokio::test]
async fn the_archive_state_rung_gives_every_existing_row_its_archive_state() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = schema::connect_store_pool(options, 1).await.unwrap();
    let rung = MIGRATIONS
        .iter()
        .position(|(_, sql)| *sql == ARCHIVE_STATE_SQL)
        .expect("the ladder holds the rung");
    let (before, _) = MIGRATIONS[rung - 1];
    schema::bootstrap_versioned(&pool, SCHEMA_CLASS, before, &MIGRATIONS[..rung])
        .await
        .expect("a store at the version before the rung");
    for status in [
        OwnerStatus::InFlight,
        OwnerStatus::Ambiguous,
        OwnerStatus::Completed,
        OwnerStatus::Failed,
    ] {
        sqlx::query(
            "INSERT INTO coordination_invocation_owner \
             (invocation_id, worker_id, assigned_at, status) VALUES (?, 'w', 1, ?)",
        )
        .bind(status.as_str())
        .bind(status.as_str())
        .execute(&pool)
        .await
        .unwrap();
    }
    pool.close().await;

    let store = ControlPlaneStore::open(&path).await.expect("migrates");
    assert_eq!(
        store.read_schema_version().await.unwrap(),
        Some(super::super::CONTROL_PLANE_SCHEMA_VERSION)
    );
    for (status, archive) in [
        (OwnerStatus::InFlight, ArchiveState::NotDue),
        (OwnerStatus::Ambiguous, ArchiveState::NotDue),
        (OwnerStatus::Completed, ArchiveState::Archived),
        (OwnerStatus::Failed, ArchiveState::Archived),
    ] {
        let row = store
            .get_invocation_owner(status.as_str())
            .await
            .unwrap()
            .expect("the row survives the migration");
        assert_eq!((row.status, row.archive_state), (status, archive));
        assert_eq!(ArchiveState::before_the_column(status), archive);
    }
}

#[test]
fn archive_state_round_trips() {
    for state in [
        ArchiveState::NotDue,
        ArchiveState::Pending,
        ArchiveState::Archived,
    ] {
        assert_eq!(ArchiveState::parse(state.as_str()), Some(state));
    }
    assert_eq!(ArchiveState::parse("held"), None);
}

/// The column refuses a state the runtime never writes.
#[tokio::test]
async fn the_column_refuses_an_unknown_archive_state() {
    let dir = tempdir().unwrap();
    let store = ControlPlaneStore::open(&dir.path().join("control-plane.db"))
        .await
        .unwrap();
    let refused = sqlx::query(
        "INSERT INTO coordination_invocation_owner \
         (invocation_id, worker_id, assigned_at, status, archive_state) \
         VALUES ('inv', 'w', 1, 'completed', 'held')",
    )
    .execute(&store.pool)
    .await;
    assert!(refused.is_err(), "{refused:?}");
}
