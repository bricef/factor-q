//! The termination record's rung (issue 1070): the five columns it adds
//! beside the ownership row's status, the checks it adds across them, and
//! what it does to a control plane that already holds rows.
//!
//! The rung rebuilds `coordination_invocation_owner`, since SQLite cannot
//! add a table constraint to a table that exists. So these tests hold it
//! to what a rebuild must not do: lose or alter a row or a column, leave
//! a half-built table behind when it is cut short, or run a second time.
//! The rung is found by what its SQL adds and the store it upgrades is
//! built from the ladder up to the archive-state rung, so no test names a
//! version number: a rung another change puts before this one moves
//! nothing here.

use std::time::Duration;

use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use tempfile::tempdir;

use super::super::{CONTROL_PLANE_SCHEMA_VERSION, MIGRATIONS, SCHEMA_CLASS};
use super::*;
use crate::control_plane::ownership::{Decision, OwnershipEvent, transition};
use crate::db::schema::{self, SchemaSeam};

/// The statuses a control plane writes.
const STATUSES: [&str; 4] = ["in_flight", "ambiguous", "completed", "failed"];

/// The archive states the column admits, the record's `none` among them.
const ARCHIVE_STATES: [&str; 4] = ["not_due", "pending", "archived", "none"];

/// The pairs a row may hold: a run that has not ended has no archive due,
/// a completed run's archive is pending or written, and a failed run's
/// may also be `none`, the archive a dropped run that never wrote a
/// worker row will never have.
const VALID_PAIRS: [(&str, &str); 7] = [
    ("in_flight", "not_due"),
    ("ambiguous", "not_due"),
    ("completed", "pending"),
    ("completed", "archived"),
    ("failed", "pending"),
    ("failed", "archived"),
    ("failed", "none"),
];

/// One row as the rung found it: id, worker, time, status, archive state.
type OldColumns = (String, String, i64, String, String);

/// The rows a control plane holds before the rung: one of each pair a
/// writer of that build wrote, each with its own worker and time, and a
/// row whose status only a hand edit or a newer build writes (#1082).
const ROWS_BEFORE: [(&str, &str, i64, &str, &str); 7] = [
    ("run-in-flight", "worker-a", 1_001, "in_flight", "not_due"),
    ("run-ambiguous", "agent-b", 1_002, "ambiguous", "not_due"),
    (
        "run-completed-pending",
        "agent-c",
        1_003,
        "completed",
        "pending",
    ),
    (
        "run-completed-archived",
        "worker-d",
        1_004,
        "completed",
        "archived",
    ),
    ("run-failed-pending", "agent-e", 1_005, "failed", "pending"),
    (
        "run-failed-archived",
        "worker-f",
        1_006,
        "failed",
        "archived",
    ),
    ("run-hand-edited", "worker-g", 1_007, "weird", "not_due"),
];

/// The record's five columns.
const RECORD_COLUMNS: [&str; 5] = [
    "ended_by",
    "ended_at",
    "termination_requested_at",
    "termination_requested_by",
    "termination_reason",
];

/// Where in the ladder the termination record's rung is: the rung whose
/// SQL adds the request's time.
fn the_record_rung() -> usize {
    MIGRATIONS
        .iter()
        .position(|(_, sql)| sql.contains("termination_requested_at"))
        .expect("the ladder holds the termination record's rung")
}

/// A raw connection to the store's file, as the ladder's tests take one.
async fn raw_pool(path: &std::path::Path) -> sqlx::Pool<sqlx::Sqlite> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    schema::connect_store_pool(options, 1).await.unwrap()
}

/// A control plane as the build before the record left it: the ladder up
/// to the archive-state rung, holding [`ROWS_BEFORE`]. Returns the rows
/// as read back.
async fn a_store_before_the_record(path: &std::path::Path) -> Vec<OldColumns> {
    let pool = raw_pool(path).await;
    let archive_rung = MIGRATIONS
        .iter()
        .position(|(_, sql)| *sql == ARCHIVE_STATE_SQL)
        .expect("the ladder holds the archive-state rung");
    schema::bootstrap_versioned(
        &pool,
        SCHEMA_CLASS,
        MIGRATIONS[archive_rung].0,
        &MIGRATIONS[..=archive_rung],
    )
    .await
    .expect("a store at the archive-state rung's version");
    for (id, worker, at, status, archive) in ROWS_BEFORE {
        sqlx::query(
            "INSERT INTO coordination_invocation_owner \
             (invocation_id, worker_id, assigned_at, status, archive_state) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(worker)
        .bind(at)
        .bind(status)
        .bind(archive)
        .execute(&pool)
        .await
        .unwrap();
    }
    let rows = old_columns(&pool).await;
    pool.close().await;
    rows
}

/// Every row's columns from before the rung, by id.
async fn old_columns(pool: &sqlx::Pool<sqlx::Sqlite>) -> Vec<OldColumns> {
    sqlx::query(
        "SELECT invocation_id, worker_id, assigned_at, status, archive_state \
         FROM coordination_invocation_owner ORDER BY invocation_id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|row| {
        (
            row.get("invocation_id"),
            row.get("worker_id"),
            row.get("assigned_at"),
            row.get("status"),
            row.get("archive_state"),
        )
    })
    .collect()
}

/// How many rows hold a value in any of the record's columns.
async fn rows_with_a_record(pool: &sqlx::Pool<sqlx::Sqlite>) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM coordination_invocation_owner \
         WHERE ended_by IS NOT NULL OR ended_at IS NOT NULL \
            OR termination_requested_at IS NOT NULL \
            OR termination_requested_by IS NOT NULL OR termination_reason IS NOT NULL",
    )
    .fetch_one(pool)
    .await
}

/// The ownership table's columns, as SQLite lists them.
async fn owner_columns(pool: &sqlx::Pool<sqlx::Sqlite>) -> Vec<String> {
    sqlx::query("SELECT name FROM pragma_table_info('coordination_invocation_owner')")
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get("name"))
        .collect()
}

/// The schema objects of the ownership table: the table, its primary
/// key's index, its own index, and anything a rebuild left behind.
async fn owner_objects(pool: &sqlx::Pool<sqlx::Sqlite>) -> Vec<(String, String)> {
    sqlx::query(
        "SELECT type, name FROM sqlite_master \
         WHERE name LIKE '%invocation_owner%' OR name = 'idx_owner_worker_status' \
         ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|row| (row.get("type"), row.get("name")))
    .collect()
}

/// **A control plane the build before the record wrote comes through the
/// rung whole.** Every row keeps its id, worker, time, status and archive
/// state, the hand-edited one included; the record's five columns are
/// there, empty on every row, which reads "not recorded"; the table keeps
/// its index; and the rebuild leaves nothing of itself behind.
#[tokio::test]
async fn a_control_plane_from_before_the_record_keeps_every_row_through_its_rung() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    let before = a_store_before_the_record(&path).await;
    assert_eq!(before.len(), ROWS_BEFORE.len());
    let pool = raw_pool(&path).await;
    let objects = owner_objects(&pool).await;
    assert!(objects.contains(&("index".to_string(), "idx_owner_worker_status".to_string())));
    pool.close().await;

    let store = ControlPlaneStore::open(&path).await.expect("migrates");
    assert_eq!(
        store.read_schema_version().await.unwrap(),
        Some(CONTROL_PLANE_SCHEMA_VERSION)
    );
    assert_eq!(
        old_columns(&store.pool).await,
        before,
        "every row keeps every column it had"
    );
    let columns = owner_columns(&store.pool).await;
    for column in RECORD_COLUMNS {
        assert!(
            columns.iter().any(|c| c == column),
            "the rung adds `{column}`: {columns:?}"
        );
    }
    assert_eq!(
        rows_with_a_record(&store.pool).await.unwrap(),
        0,
        "no row from before the rung has a record: each reads not recorded"
    );
    assert_eq!(
        owner_objects(&store.pool).await,
        objects,
        "the table keeps its indexes, and the rebuild leaves nothing behind"
    );
}

/// Never lets the upgrade past the rung that brings the schema to `.0`:
/// the step can only end by being dropped, as a cancelled open or a
/// process killed there ends it.
struct HoldAfterRung(u32);

impl SchemaSeam for HoldAfterRung {
    async fn rung_applied(&mut self, version: u32) {
        if version == self.0 {
            std::future::pending::<()>().await;
        }
    }
}

/// **The rung cut short leaves the store as it was.** An upgrade held
/// after the rung's table rebuild, inside its transaction, and dropped
/// there leaves the file at its old version with the old table, every
/// row and nothing of the rebuild; the next open runs the rung from the
/// start and keeps every row.
#[tokio::test]
async fn the_record_rung_cut_short_leaves_the_store_as_it_was() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    let before = a_store_before_the_record(&path).await;
    let pool = raw_pool(&path).await;
    let old_version = schema::read_schema_version(&pool, SCHEMA_CLASS)
        .await
        .unwrap();
    let old_columns_list = owner_columns(&pool).await;
    let objects = owner_objects(&pool).await;
    let rung_version = MIGRATIONS[the_record_rung()].0;

    let cut = tokio::time::timeout(
        Duration::from_millis(500),
        schema::bootstrap_observed(
            &pool,
            SCHEMA_CLASS,
            CONTROL_PLANE_SCHEMA_VERSION,
            MIGRATIONS,
            &mut HoldAfterRung(rung_version),
        ),
    )
    .await;
    assert!(cut.is_err(), "the upgrade was held after the rung: {cut:?}");
    assert_eq!(
        schema::read_schema_version(&pool, SCHEMA_CLASS)
            .await
            .unwrap(),
        old_version,
        "the version is the old one"
    );
    assert_eq!(
        owner_columns(&pool).await,
        old_columns_list,
        "the old table"
    );
    assert_eq!(old_columns(&pool).await, before, "with every row");
    assert_eq!(
        owner_objects(&pool).await,
        objects,
        "and nothing of the rebuild"
    );
    pool.close().await;

    // The store's own open waits for the dropped step's lock to go.
    let store = ControlPlaneStore::open(&path)
        .await
        .expect("the next open runs the rung again, from the start");
    assert_eq!(
        store.read_schema_version().await.unwrap(),
        Some(CONTROL_PLANE_SCHEMA_VERSION)
    );
    assert_eq!(old_columns(&store.pool).await, before);
    assert_eq!(rows_with_a_record(&store.pool).await.unwrap(), 0);
    assert_eq!(owner_objects(&store.pool).await, objects);
}

/// **The rung runs once.** Once a store is past it, a record written in
/// its columns survives every later open, two at once included: the
/// ladder never runs a rung its version already records, and the
/// rebuild, which copies only the columns from before it, would lose the
/// record if it ran again.
#[tokio::test]
async fn the_record_rung_runs_once() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    a_store_before_the_record(&path).await;
    let store = ControlPlaneStore::open(&path).await.expect("migrates");
    sqlx::query(
        "UPDATE coordination_invocation_owner \
         SET status = 'failed', archive_state = 'pending', ended_by = 'drop', ended_at = 2000, \
             termination_requested_at = 2000, termination_requested_by = 'operator', \
             termination_reason = 'stuck' \
         WHERE invocation_id = 'run-in-flight'",
    )
    .execute(&store.pool)
    .await
    .expect("a record written as the fold will write it");
    let written = old_columns(&store.pool).await;
    drop(store);

    let (first, second) = tokio::join!(
        ControlPlaneStore::open(&path),
        ControlPlaneStore::open(&path)
    );
    for store in [first.expect("opens"), second.expect("opens")] {
        assert_eq!(old_columns(&store.pool).await, written);
        assert_eq!(rows_with_a_record(&store.pool).await.unwrap(), 1);
    }
    let store = ControlPlaneStore::open(&path).await.expect("opens");
    let kept: (String, i64, i64, String, String) = sqlx::query_as(
        "SELECT ended_by, ended_at, termination_requested_at, termination_requested_by, \
                termination_reason \
         FROM coordination_invocation_owner WHERE invocation_id = 'run-in-flight'",
    )
    .fetch_one(&store.pool)
    .await
    .unwrap();
    assert_eq!(
        kept,
        (
            "drop".to_string(),
            2000,
            2000,
            "operator".to_string(),
            "stuck".to_string()
        )
    );
}

/// Insert a row of `status` and `archive`, with the record's columns as
/// `record` sets them (SQL assignments after the pair), as a hand edit
/// would: what the table itself accepts.
async fn insert_raw(
    store: &ControlPlaneStore,
    id: &str,
    status: &str,
    archive: &str,
    record: &'static str,
) -> Result<(), sqlx::Error> {
    let (columns, values) = match record {
        "" => (String::new(), String::new()),
        record => {
            let (columns, values): (Vec<&str>, Vec<&str>) = record
                .split(", ")
                .map(|assignment| assignment.split_once(" = ").expect("`column = value`"))
                .unzip();
            (
                format!(", {}", columns.join(", ")),
                format!(", {}", values.join(", ")),
            )
        }
    };
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO coordination_invocation_owner \
         (invocation_id, worker_id, assigned_at, status, archive_state{columns}) \
         VALUES (?, 'w', 1, ?, ?{values})"
    )))
    .bind(id)
    .bind(status)
    .bind(archive)
    .execute(&store.pool)
    .await
    .map(|_| ())
}

/// **The pair check refuses every pair no writer writes, and only those.**
/// Each known status with each archive state: a pair a row may hold is
/// accepted, any other refused. A status this build does not know is
/// left to its readers (#1082): the check constrains only the statuses it
/// knows, so a hand-edited row comes through the rung as it is.
#[tokio::test]
async fn the_pair_check_refuses_every_pair_a_row_may_not_hold() {
    let dir = tempdir().unwrap();
    let store = ControlPlaneStore::open(&dir.path().join("control-plane.db"))
        .await
        .unwrap();
    for status in STATUSES {
        for archive in ARCHIVE_STATES {
            let inserted =
                insert_raw(&store, &format!("{status}-{archive}"), status, archive, "").await;
            let valid = VALID_PAIRS.contains(&(status, archive));
            assert_eq!(
                inserted.is_ok(),
                valid,
                "{status}·{archive} is {} by the table: {inserted:?}",
                if valid { "accepted" } else { "refused" }
            );
        }
    }
    for archive in ARCHIVE_STATES {
        insert_raw(&store, &format!("weird-{archive}"), "weird", archive, "")
            .await
            .expect("a status this build does not know is the readers' to refuse");
    }
    assert!(
        insert_raw(&store, "weird-held", "weird", "held", "")
            .await
            .is_err(),
        "an archive state outside the column's values is refused whatever the status"
    );
}

/// **Nothing a writer writes is refused.** Every state the transition
/// moves a row to, from no row and from every state a row may hold, its
/// ender and request included, written by the one production writer, the
/// compare-and-set, with the termination record a move writes; and every
/// row the seeding helpers write. With the rung in place, none of it
/// breaks a check.
#[tokio::test]
async fn every_row_a_writer_writes_passes_the_checks() {
    let dir = tempdir().unwrap();
    let store = ControlPlaneStore::open(&dir.path().join("control-plane.db"))
        .await
        .unwrap();
    let mut states: Vec<Option<crate::control_plane::ownership::RowState>> = vec![None];
    for (status, archive) in VALID_PAIRS {
        let (Some(status), Some(archive)) =
            (OwnerStatus::parse(status), ArchiveState::parse(archive))
        else {
            continue;
        };
        for ended_by in [None, Some(EndedBy::Run), Some(EndedBy::Drop)] {
            for requested in [false, true] {
                let state = crate::control_plane::ownership::RowState {
                    status,
                    archive,
                    ended_by,
                    requested,
                };
                if state.is_valid() {
                    states.push(Some(state));
                }
            }
        }
    }
    let request = TerminationRequest {
        requested_at: 1,
        requested_by: Some("on-call".to_string()),
        reason: Some("runaway".to_string()),
    };
    let mut n = 0;
    for from in &states {
        for event in OwnershipEvent::ALL {
            let to = match transition(*from, event) {
                Decision::Move { to, .. }
                | Decision::Conflict {
                    then: Some((to, _)),
                } => to,
                _ => continue,
            };
            n += 1;
            let id = format!("written-{n}");
            if let Some(from) = from {
                assert!(
                    store
                        .compare_and_set_ownership(
                            &id,
                            None,
                            *from,
                            MoveFacts {
                                worker_id: Some("w"),
                                assigned_at: Some(1),
                                ended_at: from.ended_by.map(|_| 1),
                                request: from.requested.then_some(&request),
                            },
                        )
                        .await
                        .expect("the state it moves from is written"),
                );
            }
            let (ender, requested) =
                from.map_or((None, false), |from| (from.ended_by, from.requested));
            let moved = store
                .compare_and_set_ownership(
                    &id,
                    *from,
                    to,
                    MoveFacts {
                        worker_id: Some("w"),
                        assigned_at: Some(2),
                        ended_at: (to.ended_by != ender).then_some(2),
                        request: (to.requested && !requested).then_some(&request),
                    },
                )
                .await;
            assert!(
                moved.as_ref().is_ok_and(|landed| *landed),
                "{event:?} from {from:?} to {to:?} is written: {moved:?}"
            );
        }
    }
    for status in [
        OwnerStatus::InFlight,
        OwnerStatus::Ambiguous,
        OwnerStatus::Completed,
        OwnerStatus::Failed,
    ] {
        store
            .upsert_invocation_ownership(&format!("seeded-{}", status.as_str()), "w", 1, status)
            .await
            .expect("the seeding helper writes a row the checks accept");
    }
    store
        .assign_invocation("assigned", "w", 1)
        .await
        .expect("the in-flight seeding helper too");
}

/// **The record's columns take what the record writes, and refuse the
/// rest.** A run's ending names who ended it and when, together, on a row
/// that has ended; a drop ends a run as `failed`; the request's caller and
/// reason come only with its time, and only on a row that has ended, since
/// a drop of a run in progress ends it.
#[tokio::test]
async fn the_record_columns_take_what_the_record_writes_and_refuse_the_rest() {
    let dir = tempdir().unwrap();
    let store = ControlPlaneStore::open(&dir.path().join("control-plane.db"))
        .await
        .unwrap();
    let accepted: [(&str, &str, &str, &'static str); 7] = [
        (
            "dropped",
            "failed",
            "pending",
            "ended_by = 'drop', ended_at = 2, termination_requested_at = 2, \
             termination_requested_by = 'operator', termination_reason = 'stuck'",
        ),
        (
            "dropped-never-archived",
            "failed",
            "none",
            "ended_by = 'drop', ended_at = 2, termination_requested_at = 2",
        ),
        (
            "completed-then-requested",
            "completed",
            "archived",
            "ended_by = 'run', ended_at = 1, termination_requested_at = 2",
        ),
        (
            "failed-by-itself",
            "failed",
            "archived",
            "ended_by = 'run', ended_at = 1",
        ),
        ("not-recorded", "completed", "archived", ""),
        ("in-flight", "in_flight", "not_due", ""),
        (
            "a-record-on-a-status-this-build-does-not-know",
            "weird",
            "not_due",
            "ended_by = 'drop', ended_at = 2, termination_requested_at = 2, \
             termination_requested_by = 'operator', termination_reason = 'stuck'",
        ),
    ];
    for (id, status, archive, record) in accepted {
        let inserted = insert_raw(&store, id, status, archive, record).await;
        assert!(inserted.is_ok(), "{id} is accepted: {inserted:?}");
    }
    let refused: [(&str, &str, &str, &'static str); 9] = [
        (
            "an-unknown-ender",
            "failed",
            "pending",
            "ended_by = 'operator', ended_at = 2",
        ),
        (
            "an-ender-with-no-time",
            "failed",
            "pending",
            "ended_by = 'run'",
        ),
        ("a-time-with-no-ender", "failed", "pending", "ended_at = 2"),
        (
            "an-ender-in-flight",
            "in_flight",
            "not_due",
            "ended_by = 'run', ended_at = 2",
        ),
        (
            "a-drop-that-completed",
            "completed",
            "pending",
            "ended_by = 'drop', ended_at = 2",
        ),
        (
            "a-caller-with-no-request",
            "failed",
            "archived",
            "termination_requested_by = 'operator'",
        ),
        (
            "a-reason-with-no-request",
            "failed",
            "archived",
            "termination_reason = 'stuck'",
        ),
        (
            "a-request-in-flight",
            "in_flight",
            "not_due",
            "termination_requested_at = 2",
        ),
        (
            "a-request-ambiguous",
            "ambiguous",
            "not_due",
            "termination_requested_at = 2",
        ),
    ];
    for (id, status, archive, record) in refused {
        let inserted = insert_raw(&store, id, status, archive, record).await;
        assert!(inserted.is_err(), "{id} is refused: {inserted:?}");
    }
}

/// The pairs no writer ever wrote, each with the archive state the rung
/// gives it: the one its status implies, as the archive-state rung gave
/// rows from before its column.
const PAIRS_CARRIED_THROUGH: [(&str, &str, &str); 6] = [
    ("in_flight", "pending", "not_due"),
    ("in_flight", "archived", "not_due"),
    ("ambiguous", "pending", "not_due"),
    ("ambiguous", "archived", "not_due"),
    ("completed", "not_due", "archived"),
    ("failed", "not_due", "archived"),
];

/// Add a row of `status` and `archive` to the store at `path`, before the
/// rung, as a hand edit of the status alone leaves one.
async fn a_hand_edited_row(path: &std::path::Path, status: &str, archive: &str) {
    let pool = raw_pool(path).await;
    sqlx::query(
        "INSERT INTO coordination_invocation_owner \
         (invocation_id, worker_id, assigned_at, status, archive_state) \
         VALUES ('run-hand-edited-pair', 'worker-h', 1008, ?, ?)",
    )
    .bind(status)
    .bind(archive)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}

/// **A row whose archive state does not go with its status is carried
/// through, not refused.** No writer ever wrote one, but a hand edit of
/// the status alone can, and one such row must not keep the daemon down.
/// For each such pair, the store opens; that row comes out with the
/// archive state its status implies and every other column as it was; and
/// every other row is exactly as it was.
#[tokio::test]
async fn a_row_whose_archive_state_does_not_go_with_its_status_is_carried_through() {
    for (status, before, after) in PAIRS_CARRIED_THROUGH {
        let dir = tempdir().unwrap();
        let path = dir.path().join("control-plane.db");
        let others = a_store_before_the_record(&path).await;
        a_hand_edited_row(&path, status, before).await;

        let store = ControlPlaneStore::open(&path)
            .await
            .unwrap_or_else(|err| panic!("{status}·{before} stops the store's open: {err}"));
        let mut expected = others;
        expected.push((
            "run-hand-edited-pair".to_string(),
            "worker-h".to_string(),
            1008,
            status.to_string(),
            after.to_string(),
        ));
        expected.sort();
        assert_eq!(
            old_columns(&store.pool).await,
            expected,
            "{status}·{before} becomes {status}·{after}, and nothing else changes"
        );
    }
}

/// Bring the store at `path` back to the version before the record's
/// rung, as a build that numbered another rung the same would leave it.
async fn record_rung_forgotten(path: &std::path::Path) {
    let pool = raw_pool(path).await;
    schema::write_schema_version(&pool, SCHEMA_CLASS, MIGRATIONS[the_record_rung() - 1].0)
        .await
        .unwrap();
    pool.close().await;
}

/// **A second run of the rung stops the store's open, loudly, and keeps
/// every record.** The rebuild copies only the columns from before it, so
/// a second run would erase every record; a store whose version says the
/// rung has not run, when it has, is refused instead, with nothing
/// applied.
#[tokio::test]
async fn a_second_run_of_the_record_rung_stops_the_open_and_keeps_every_record() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    a_store_before_the_record(&path).await;
    let store = ControlPlaneStore::open(&path).await.expect("migrates");
    sqlx::query(
        "UPDATE coordination_invocation_owner \
         SET ended_by = 'run', ended_at = 1500, termination_requested_at = 2000 \
         WHERE invocation_id = 'run-completed-archived'",
    )
    .execute(&store.pool)
    .await
    .unwrap();
    let written = (
        old_columns(&store.pool).await,
        rows_with_a_record(&store.pool).await.unwrap(),
    );
    drop(store);
    record_rung_forgotten(&path).await;

    let refused = ControlPlaneStore::open(&path).await;
    let Err(err) = refused else {
        panic!("a second run of the rung must not open the store");
    };
    assert!(
        err.to_string().contains("duplicate column name: ended_by"),
        "it says why: {err}"
    );
    let pool = raw_pool(&path).await;
    assert_eq!(
        (
            old_columns(&pool).await,
            rows_with_a_record(&pool).await.unwrap()
        ),
        written,
        "every row and its record are as they were"
    );
}

/// The table's columns as SQLite lists them: name, type, not null,
/// default, place in the key.
async fn column_definitions(
    pool: &sqlx::Pool<sqlx::Sqlite>,
) -> Vec<(String, String, i64, Option<String>, i64)> {
    sqlx::query_as(
        "SELECT name, type, \"notnull\", dflt_value, pk \
         FROM pragma_table_info('coordination_invocation_owner') ORDER BY cid",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The ownership index's columns, in order.
async fn index_columns(pool: &sqlx::Pool<sqlx::Sqlite>) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT name FROM pragma_index_info('idx_owner_worker_status') ORDER BY seqno",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The schema text of the ownership table and its index.
async fn schema_text(pool: &sqlx::Pool<sqlx::Sqlite>) -> Vec<(String, Option<String>)> {
    sqlx::query_as(
        "SELECT name, sql FROM sqlite_master \
         WHERE name IN ('coordination_invocation_owner', 'idx_owner_worker_status') \
         ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// **The rebuilt table is the table it replaced, with the record added.**
/// Every column keeps its type, its not-null, its default and its place in
/// the key; the five new columns are nullable with no default; the index
/// is on the same columns; and the upgraded schema's text is the text a
/// fresh store's ladder writes.
#[tokio::test]
async fn the_rebuilt_table_is_the_table_it_replaced_with_the_record_added() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    a_store_before_the_record(&path).await;
    let pool = raw_pool(&path).await;
    let mut expected = column_definitions(&pool).await;
    let index = index_columns(&pool).await;
    pool.close().await;
    for (name, kind) in [
        ("ended_by", "TEXT"),
        ("ended_at", "INTEGER"),
        ("termination_requested_at", "INTEGER"),
        ("termination_requested_by", "TEXT"),
        ("termination_reason", "TEXT"),
    ] {
        expected.push((name.to_string(), kind.to_string(), 0, None, 0));
    }

    let upgraded = ControlPlaneStore::open(&path).await.expect("migrates");
    assert_eq!(column_definitions(&upgraded.pool).await, expected);
    assert_eq!(
        index_columns(&upgraded.pool).await,
        index,
        "the index is on the columns it was"
    );
    assert_eq!(index, ["worker_id", "status"]);
    let fresh = ControlPlaneStore::open(&dir.path().join("fresh.db"))
        .await
        .unwrap();
    assert_eq!(
        schema_text(&upgraded.pool).await,
        schema_text(&fresh.pool).await,
        "an upgraded store's schema is a fresh one's"
    );
}

/// **Every reader of the row carries the record.** A row with a record
/// reads back with it, through the lookup and each listing, and a row
/// without one reads not recorded: no ender, no time, no request.
#[tokio::test]
async fn every_reader_of_the_row_carries_the_record() {
    let dir = tempdir().unwrap();
    let store = ControlPlaneStore::open(&dir.path().join("control-plane.db"))
        .await
        .unwrap();
    insert_raw(
        &store,
        "dropped",
        "failed",
        "pending",
        "ended_by = 'drop', ended_at = 20, termination_requested_at = 20, \
         termination_requested_by = 'operator', termination_reason = 'stuck'",
    )
    .await
    .unwrap();
    insert_raw(
        &store,
        "requested-after",
        "completed",
        "archived",
        "ended_by = 'run', ended_at = 10, termination_requested_at = 30",
    )
    .await
    .unwrap();
    insert_raw(&store, "not-recorded", "failed", "archived", "")
        .await
        .unwrap();

    let expected = |id: &str| -> (Option<EndedBy>, Option<i64>, Option<TerminationRequest>) {
        match id {
            "dropped" => (
                Some(EndedBy::Drop),
                Some(20),
                Some(TerminationRequest {
                    requested_at: 20,
                    requested_by: Some("operator".to_string()),
                    reason: Some("stuck".to_string()),
                }),
            ),
            "requested-after" => (
                Some(EndedBy::Run),
                Some(10),
                Some(TerminationRequest {
                    requested_at: 30,
                    requested_by: None,
                    reason: None,
                }),
            ),
            _ => (None, None, None),
        }
    };
    let record = |row: &OwnerRow| (row.ended_by, row.ended_at, row.termination.clone());

    for id in ["dropped", "requested-after", "not-recorded"] {
        let row = store.get_invocation_owner(id).await.unwrap().unwrap();
        assert_eq!(record(&row), expected(id), "{id}, looked up");
    }
    let listings = [
        store.list_invocations(None, 10).await.unwrap(),
        store
            .list_invocations(Some(OwnerStatus::Failed), 10)
            .await
            .unwrap(),
        store.list_invocations_for_worker("w").await.unwrap(),
        store
            .list_invocations_with_status(OwnerStatus::Completed)
            .await
            .unwrap(),
    ];
    for rows in listings {
        assert!(!rows.is_empty());
        for row in rows {
            assert_eq!(
                record(&row),
                expected(&row.invocation_id),
                "{}, listed",
                row.invocation_id
            );
        }
    }
}

#[test]
fn an_ender_round_trips() {
    for ender in [EndedBy::Run, EndedBy::Drop] {
        assert_eq!(EndedBy::parse(ender.as_str()), Some(ender));
    }
    assert_eq!(EndedBy::parse("operator"), None);
}

/// **The rows the rung corrects are read before it runs, and none after.**
/// The store's open reads them before the ladder, and logs each once the
/// rung has landed, naming the invocation, its status and the archive
/// state before and after; the same query is the pre-flight check of a
/// copy of a live file. A store from before the archive-state column, a
/// fresh store and one past the rung have none.
#[tokio::test]
async fn the_rows_the_rung_corrects_are_read_before_it_and_none_after() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    a_store_before_the_record(&path).await;
    a_hand_edited_row(&path, "failed", "not_due").await;
    let pool = raw_pool(&path).await;
    assert_eq!(
        pairs_the_record_rung_corrects(&pool).await.unwrap(),
        [PairCorrected {
            invocation_id: "run-hand-edited-pair".to_string(),
            status: "failed".to_string(),
            before: "not_due".to_string(),
            after: "archived".to_string(),
        }]
    );
    pool.close().await;
    let store = ControlPlaneStore::open(&path).await.expect("migrates");
    assert!(
        pairs_the_record_rung_corrects(&store.pool)
            .await
            .unwrap()
            .is_empty()
    );

    let fresh = ControlPlaneStore::open(&dir.path().join("fresh.db"))
        .await
        .unwrap();
    assert!(
        pairs_the_record_rung_corrects(&fresh.pool)
            .await
            .unwrap()
            .is_empty()
    );
    let before_the_column = raw_pool(&dir.path().join("v2.db")).await;
    let archive_rung = MIGRATIONS
        .iter()
        .position(|(_, sql)| *sql == ARCHIVE_STATE_SQL)
        .unwrap();
    schema::bootstrap_versioned(
        &before_the_column,
        SCHEMA_CLASS,
        MIGRATIONS[archive_rung - 1].0,
        &MIGRATIONS[..archive_rung],
    )
    .await
    .unwrap();
    assert!(
        pairs_the_record_rung_corrects(&before_the_column)
            .await
            .unwrap()
            .is_empty()
    );
}

/// **A row the rung carries is read without panicking, whatever its
/// storage.** An id stored as a blob, which no writer stores, on a row
/// whose archive state does not go with its status: the read before the
/// ladder reports it as text, and the store opens with the row corrected.
#[tokio::test]
async fn a_row_whose_id_is_a_blob_is_read_and_carried_through() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    a_store_before_the_record(&path).await;
    let pool = raw_pool(&path).await;
    sqlx::query(
        "INSERT INTO coordination_invocation_owner \
         (invocation_id, worker_id, assigned_at, status, archive_state) \
         VALUES (X'626C6F62', 'w', 1, 'failed', 'not_due')",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        pairs_the_record_rung_corrects(&pool).await.unwrap(),
        [PairCorrected {
            invocation_id: "blob".to_string(),
            status: "failed".to_string(),
            before: "not_due".to_string(),
            after: "archived".to_string(),
        }]
    );
    pool.close().await;

    let store = ControlPlaneStore::open(&path)
        .await
        .expect("a blob id does not stop the open");
    let archive: String = sqlx::query_scalar(
        "SELECT archive_state FROM coordination_invocation_owner WHERE typeof(invocation_id) = 'blob'",
    )
    .fetch_one(&store.pool)
    .await
    .unwrap();
    assert_eq!(archive, "archived");
}

/// Apply `set`, SQL assignments, to `id`'s ownership row by hand, as the
/// operating guide's edit does. The number of rows changed.
async fn hand_edit(store: &ControlPlaneStore, id: &str, set: &str) -> Result<u64, sqlx::Error> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE coordination_invocation_owner SET {set} WHERE invocation_id = ?"
    )))
    .bind(id)
    .execute(&store.pool)
    .await
    .map(|done| done.rows_affected())
}

/// Whether `id`'s ownership row reads as one this build cannot read.
async fn unreadable(store: &ControlPlaneStore, id: &str) -> bool {
    matches!(
        store.get_invocation_owner(id).await,
        Err(ControlPlaneStoreError::Malformed(_))
    )
}

/// **The hand edit the operating guide gives for a row this build cannot
/// read clears it, and the checks hold the record to its status.** A row
/// whose status a newer build wrote, with an ender and a request on
/// record, reads as malformed. Set to a run in progress with its record
/// left on it, it is refused and stays as it was; with the record cleared
/// too, it is admitted and reads. Set to a run that ended, its record may
/// stay, but a `completed` run's ender may not be the drop.
#[tokio::test]
async fn the_guides_hand_edit_of_an_unreadable_row_goes_through_the_checks() {
    let dir = tempdir().unwrap();
    let store = ControlPlaneStore::open(&dir.path().join("control-plane.db"))
        .await
        .unwrap();
    let record = "ended_by = 'drop', ended_at = 1500, termination_requested_at = 1400, \
                  termination_requested_by = 'on-call', termination_reason = 'stuck'";
    for id in ["to-in-flight", "to-completed", "to-failed"] {
        insert_raw(&store, id, "weird", "not_due", "")
            .await
            .unwrap();
        hand_edit(&store, id, record)
            .await
            .expect("a status this build does not know holds any record");
        assert!(unreadable(&store, id).await, "{id} reads as malformed");
    }

    assert!(
        hand_edit(
            &store,
            "to-in-flight",
            "status = 'in_flight', archive_state = 'not_due'"
        )
        .await
        .is_err(),
        "a run in progress with a record on it is refused"
    );
    assert!(
        unreadable(&store, "to-in-flight").await,
        "the refused edit leaves the row as it was"
    );
    let cleared = hand_edit(
        &store,
        "to-in-flight",
        "status = 'in_flight', archive_state = 'not_due', ended_by = NULL, ended_at = NULL, \
         termination_requested_at = NULL, termination_requested_by = NULL, \
         termination_reason = NULL",
    )
    .await;
    assert_eq!(cleared.ok(), Some(1), "the record cleared with it");
    assert_eq!(
        store
            .get_invocation_owner("to-in-flight")
            .await
            .unwrap()
            .map(|row| (row.status, row.ended_by, row.termination)),
        Some((OwnerStatus::InFlight, None, None))
    );

    assert!(
        hand_edit(
            &store,
            "to-completed",
            "status = 'completed', archive_state = 'archived'"
        )
        .await
        .is_err(),
        "a completed run's ender is not the drop"
    );
    hand_edit(
        &store,
        "to-completed",
        "status = 'completed', archive_state = 'archived', ended_by = 'run'",
    )
    .await
    .expect("a completed run ended by the run, its request kept");
    hand_edit(
        &store,
        "to-failed",
        "status = 'failed', archive_state = 'archived'",
    )
    .await
    .expect("a failed run keeps the drop's record");
    for (id, status, ended_by) in [
        ("to-completed", OwnerStatus::Completed, EndedBy::Run),
        ("to-failed", OwnerStatus::Failed, EndedBy::Drop),
    ] {
        let row = store.get_invocation_owner(id).await.unwrap().unwrap();
        assert_eq!(
            (row.status, row.archive_state, row.ended_by, row.ended_at),
            (status, ArchiveState::Archived, Some(ended_by), Some(1500)),
            "{id}"
        );
        assert!(row.termination.is_some(), "{id} keeps the request");
    }
}
