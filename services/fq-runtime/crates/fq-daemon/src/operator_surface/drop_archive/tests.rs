//! `none` only when no row will ever come and no archive is on record
//! (issue 1070), over real stores.

use super::*;

struct Stores {
    worker: WorkerStore,
    control_plane: ControlPlaneStore,
    _dir: tempfile::TempDir,
}

async fn stores() -> Stores {
    let dir = tempfile::tempdir().unwrap();
    let paths = fq_runtime::db::RuntimeDbPaths::under(dir.path());
    Stores {
        worker: WorkerStore::open(&paths.worker).await.unwrap(),
        control_plane: ControlPlaneStore::open(&paths.control_plane).await.unwrap(),
        _dir: dir,
    }
}

const ID: &str = "0199c3a8-6d5f-7b4e-9a51-3f2e1d0c9b8a";

async fn with_a_row_on_the_worker(stores: &Stores) {
    stores
        .worker
        .upsert_invocation_state(&fq_runtime::worker::InvocationStateRow {
            invocation_id: ID.to_string(),
            agent_id: "builder".to_string(),
            schema_version: 1,
            phase: "awaiting_model".to_string(),
            state_blob: b"{}".to_vec(),
            step_index: 0,
            started_at: 1,
            updated_at: 1,
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
}

async fn with_an_archive_on_record(stores: &Stores) {
    stores
        .control_plane
        .insert_archive(&fq_runtime::control_plane::store::InvocationArchiveRow {
            invocation_id: ID.to_string(),
            agent_id: "builder".to_string(),
            final_phase: "completed".to_string(),
            final_state_blob: b"{}".to_vec(),
            started_at: 1,
            terminal_at: 2,
            archived_at: 3,
        })
        .await
        .unwrap();
}

async fn archive(stores: &Stores, row: Option<RowAtHalt>) -> DropArchive {
    after_the_drop(&stores.worker, &stores.control_plane, row, ID).await
}

/// No row on the worker, none coming, no archive on record: none will
/// come, whether the run was not driven here or its halt found no row.
#[tokio::test]
async fn no_row_and_no_archive_is_none() {
    let stores = stores().await;
    assert_eq!(archive(&stores, None).await, DropArchive::WillNotCome);
    assert_eq!(
        archive(&stores, Some(RowAtHalt::NeverWritten)).await,
        DropArchive::WillNotCome
    );
}

/// A halt that found the first write under way, or the row there, is
/// pending whatever the stores say: the row is being written, or is there
/// to be handed off. The middle state is never `none`.
#[tokio::test]
async fn a_row_written_or_being_written_is_pending() {
    let stores = stores().await;
    for row in [RowAtHalt::FirstWriteUnderWay, RowAtHalt::Written] {
        assert_eq!(
            archive(&stores, Some(row)).await,
            DropArchive::Pending,
            "{row:?}"
        );
    }
}

/// A row on the worker is pending, the arm's report notwithstanding: a
/// run not driven here, or one whose drive is gone, still has its row
/// handed off.
#[tokio::test]
async fn a_row_on_the_worker_is_pending() {
    let stores = stores().await;
    with_a_row_on_the_worker(&stores).await;
    for row in [None, Some(RowAtHalt::NeverWritten)] {
        assert_eq!(archive(&stores, row).await, DropArchive::Pending, "{row:?}");
    }
}

/// An archive on record is pending: `none` would deny it.
#[tokio::test]
async fn an_archive_on_record_is_pending() {
    let stores = stores().await;
    with_an_archive_on_record(&stores).await;
    for row in [None, Some(RowAtHalt::NeverWritten)] {
        assert_eq!(archive(&stores, row).await, DropArchive::Pending, "{row:?}");
    }
}

/// Put `table` of the store at `path` out of reach, as a store this build
/// cannot read would be.
async fn out_of_reach(path: &std::path::Path, table: &str) {
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", path.display()))
        .await
        .expect("open the store");
    let rename = match table {
        "invocation_state" => {
            "ALTER TABLE invocation_state RENAME TO invocation_state_out_of_reach"
        }
        "invocation_archive" => {
            "ALTER TABLE invocation_archive RENAME TO invocation_archive_out_of_reach"
        }
        other => panic!("no table {other} here"),
    };
    sqlx::query(rename)
        .execute(&pool)
        .await
        .expect("hide the table");
}

/// **A store that cannot be read gives pending.** With the worker's rows,
/// or the control plane's archive records, out of reach, the read fails,
/// and the drop says an archive is pending: the answer that may be wrong
/// is the one that waits for an archive, never the one that denies it.
#[tokio::test]
async fn a_store_that_cannot_be_read_gives_pending() {
    for table in ["invocation_state", "invocation_archive"] {
        let dir = tempfile::tempdir().unwrap();
        let paths = fq_runtime::db::RuntimeDbPaths::under(dir.path());
        let worker = WorkerStore::open(&paths.worker).await.unwrap();
        let control_plane = ControlPlaneStore::open(&paths.control_plane).await.unwrap();
        let path = if table == "invocation_state" {
            &paths.worker
        } else {
            &paths.control_plane
        };
        out_of_reach(path, table).await;
        assert_eq!(
            after_the_drop(&worker, &control_plane, None, ID).await,
            DropArchive::Pending,
            "{table} out of reach"
        );
    }
}
