//! What [`ControlPlaneStore::mark_invocation_ambiguous`] answers, and what
//! it leaves the row as, from every status the row can hold.
//!
//! The answer is the seam the coordination consumer logs from: `Marked`
//! and `AlreadyAmbiguous` quietly, `Refused` as a warning naming the
//! status kept. A wrong answer would log a refusal as a mark, or warn on
//! every redelivery, with the row itself still right; so each case below
//! pins the answer and the row together.

use super::*;
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
