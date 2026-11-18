//! What [`ControlPlaneStore::mark_invocation_ambiguous`] answers, and what
//! it leaves the row as, from every status the row can hold.
//!
//! The answer is the seam the coordination consumer logs from: `Marked`
//! and `AlreadyAmbiguous` quietly, `Refused` as a warning naming the
//! status kept. A wrong answer would log a refusal as a mark, or warn on
//! every redelivery, with the row itself still right; so each case below
//! pins the answer and the row together.
//!
//! The archive's state the row keeps beside its status, and the rung that
//! adds it, are tested below them.

use super::super::{MIGRATIONS, SCHEMA_CLASS};
use super::*;
use crate::db::schema;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use tempfile::tempdir;

const ID: &str = "019a0000-0000-7000-8000-000000001024";

async fn store() -> (ControlPlaneStore, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let store = ControlPlaneStore::open(&dir.path().join("control-plane.db"))
        .await
        .unwrap();
    (store, dir)
}

async fn row(store: &ControlPlaneStore) -> Option<super::super::OwnerRow> {
    store.get_invocation_owner(ID).await.unwrap()
}

#[tokio::test]
async fn no_row_is_marked_and_written() {
    let (store, _dir) = store().await;
    assert_eq!(
        store
            .mark_invocation_ambiguous(ID, "agent", 10)
            .await
            .unwrap(),
        AmbiguousMark::Marked
    );
    let row = row(&store).await.expect("the mark writes the row");
    assert_eq!(
        (row.worker_id.as_str(), row.assigned_at, row.status),
        ("agent", 10, OwnerStatus::Ambiguous)
    );
}

/// The worker id and time this move writes over the row's are not pinned
/// here: that is known and not intended, and the next change of issue
/// 1024 keeps the row's.
#[tokio::test]
async fn an_in_flight_row_is_marked() {
    let (store, _dir) = store().await;
    store
        .upsert_invocation_ownership(ID, "worker-real", 50, OwnerStatus::InFlight)
        .await
        .unwrap();
    assert_eq!(
        store
            .mark_invocation_ambiguous(ID, "agent", 60)
            .await
            .unwrap(),
        AmbiguousMark::Marked
    );
    assert_eq!(
        row(&store).await.map(|row| row.status),
        Some(OwnerStatus::Ambiguous)
    );
}

#[tokio::test]
async fn an_ambiguous_row_is_already_ambiguous_and_left_alone() {
    let (store, _dir) = store().await;
    store
        .upsert_invocation_ownership(ID, "agent", 10, OwnerStatus::Ambiguous)
        .await
        .unwrap();
    let before = row(&store).await;
    assert_eq!(
        store
            .mark_invocation_ambiguous(ID, "other", 20)
            .await
            .unwrap(),
        AmbiguousMark::AlreadyAmbiguous
    );
    assert_eq!(row(&store).await, before);
}

#[tokio::test]
async fn a_finished_row_is_refused_with_its_status_and_left_alone() {
    for finished in [OwnerStatus::Completed, OwnerStatus::Failed] {
        let (store, _dir) = store().await;
        store
            .upsert_invocation_ownership(ID, "worker-real", 30, finished)
            .await
            .unwrap();
        let before = row(&store).await;
        assert_eq!(
            store
                .mark_invocation_ambiguous(ID, "agent", 40)
                .await
                .unwrap(),
            AmbiguousMark::Refused(finished),
            "from {}",
            finished.as_str()
        );
        assert_eq!(row(&store).await, before, "from {}", finished.as_str());
    }
}

/// A status this build cannot read is an error, so the event is retried
/// rather than acked, and the row is left as it was.
#[tokio::test]
async fn an_unknown_status_is_an_error_and_left_alone() {
    let (store, _dir) = store().await;
    store
        .upsert_invocation_ownership(ID, "worker-real", 30, OwnerStatus::Failed)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE coordination_invocation_owner SET status = 'weird' WHERE invocation_id = ?",
    )
    .bind(ID)
    .execute(&store.pool)
    .await
    .unwrap();
    let err = store
        .mark_invocation_ambiguous(ID, "agent", 70)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ControlPlaneStoreError::Malformed(_)),
        "{err:?}"
    );
    let status: String = sqlx::query_scalar(
        "SELECT status FROM coordination_invocation_owner WHERE invocation_id = ?",
    )
    .bind(ID)
    .fetch_one(&store.pool)
    .await
    .unwrap();
    assert_eq!(status, "weird");
}

/// A control plane at schema version 2, holding one row of each status,
/// opened by this build: each row gets the archive's state its status had
/// before the column existed, and the store is at the current version.
#[tokio::test]
async fn the_v3_rung_gives_every_existing_row_its_archive_state() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = schema::connect_store_pool(options, 1).await.unwrap();
    schema::bootstrap_versioned(&pool, SCHEMA_CLASS, 2, &MIGRATIONS[..2])
        .await
        .expect("a version 2 store");
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
        assert_eq!(ArchiveState::recorded_before_v3(status), archive);
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
