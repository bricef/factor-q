//! Per-store database layout and the one-time legacy split.
//!
//! The runtime persists through three SQLite-backed stores with three
//! different data lifecycles (issue #262, data-architecture.md §11):
//!
//! - [`crate::worker::WorkerStore`] — `worker.db`, source of truth for
//!   in-flight state; non-rebuildable.
//! - [`crate::control_plane::ControlPlaneStore`] — `control-plane.db`,
//!   source of truth for coordination, schedules, and the archive;
//!   non-rebuildable.
//! - [`crate::control_plane::projection::ProjectionStore`] —
//!   `projection.db`, derived from the NATS stream; disposable
//!   (delete + replay rebuilds it).
//!
//! v1 collapsed all three into a single `events.db`. This module owns
//! the split layout ([`RuntimeDbPaths`]) and the one-time migration
//! from the collapsed file ([`split_legacy_events_db`]).
//!
//! ## Split contract
//!
//! The migration is crash-safe by construction: `events.db` remains
//! the authority until the very last step, and a marker file
//! distinguishes "migration in progress" from "conflicting files".
//!
//! 1. No `events.db` → nothing to do (a leftover marker from a crash
//!    after step 6 is cleaned up here).
//! 2. `events.db` present alongside any per-store file *without* the
//!    marker → refuse ([`SplitError::Conflict`]). This state never
//!    arises from the migration itself; it means an old (pre-split)
//!    binary ran after the split and recreated `events.db`. An
//!    operator has to pick which side wins.
//! 3. Write the marker, delete any remnants of a previous attempt
//!    (partial copies are garbage while `events.db` still exists).
//! 4. Probe `events.db` with `BEGIN IMMEDIATE`: a concurrent writer
//!    (an old daemon still running) fails the split rather than
//!    losing its writes ([`SplitError::LegacyBusy`]).
//! 5. `VACUUM INTO` a temp copy per store — transactionally
//!    consistent — then drop every table that does not belong to
//!    that store and prune `schema_meta` to the store's own class.
//! 6. Rename temp → final (same directory, atomic), then retire
//!    `events.db` → `events.db.pre-split` (kept as rollback), then
//!    remove the marker.
//!
//! A crash anywhere before step 6's legacy rename leaves `events.db`
//! and the marker in place: the next run redoes the copies from scratch. A
//! crash after the rename leaves marker-but-no-legacy: the next run
//! just removes the marker. The split is schema-version-agnostic —
//! copies carry whatever version the legacy file recorded, and each
//! store's normal open migrates its own file forward afterwards. The
//! one guarded case is a legacy file written by a *newer* binary
//! ([`SplitError::SchemaAhead`]): splitting it would silently drop
//! tables this binary does not know about.

use std::path::{Path, PathBuf};

use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};
use sqlx::{ConnectOptions, Connection, Row};

use crate::control_plane::store::{
    CONTROL_PLANE_SCHEMA_VERSION, SCHEMA_CLASS as CONTROL_PLANE_SCHEMA_CLASS,
};
use crate::worker::store::{SCHEMA_CLASS as WORKER_SCHEMA_CLASS, WORKER_SCHEMA_VERSION};

/// File name of the worker store (in-flight state and dispatch WAL).
pub const WORKER_DB_FILE: &str = "worker.db";
/// File name of the control-plane store (coordination, schedules, archive).
pub const CONTROL_PLANE_DB_FILE: &str = "control-plane.db";
/// File name of the projection store (rebuildable event read model).
pub const PROJECTION_DB_FILE: &str = "projection.db";
/// File name of the v1 single-file database.
pub const LEGACY_DB_FILE: &str = "events.db";
/// What the legacy file is renamed to once the split has committed.
/// Kept as a rollback artefact; safe to delete once the split layout
/// has proven itself.
pub const LEGACY_RETIRED_FILE: &str = "events.db.pre-split";
/// Marker present while a split is in progress. Its existence is what
/// lets a rerun distinguish "resume my own crashed migration"
/// (overwrite the partial copies) from "two layouts genuinely
/// conflict" (refuse).
const SPLIT_MARKER_FILE: &str = "events.db.splitting";

/// The v1 table inventory, frozen at the split. Tables added after
/// the split never live in `events.db`, so these lists must NOT be
/// extended when new tables land — they describe the legacy file,
/// not the live schema.
const WORKER_TABLES: &[&str] = &[
    "invocation_state",
    "tool_dispatch",
    "llm_dispatch",
    "host_notice",
    "schema_meta",
];
const CONTROL_PLANE_TABLES: &[&str] = &[
    "coordination_worker",
    "coordination_invocation_owner",
    "pending_wait",
    "schedule_entry",
    "invocation_archive",
    "schema_meta",
];
// No `schema_meta`: the projection has no version row (#139), so the
// copy drops the table entirely.
const PROJECTION_TABLES: &[&str] = &["events", "invocation_summary"];

/// Absolute paths of the three per-store database files under one
/// state directory. Pure path arithmetic — nothing is created or
/// checked at construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeDbPaths {
    pub worker: PathBuf,
    pub control_plane: PathBuf,
    pub projection: PathBuf,
}

impl RuntimeDbPaths {
    /// The split layout under `dir` (the configured cache directory).
    pub fn under(dir: &Path) -> Self {
        RuntimeDbPaths {
            worker: dir.join(WORKER_DB_FILE),
            control_plane: dir.join(CONTROL_PLANE_DB_FILE),
            projection: dir.join(PROJECTION_DB_FILE),
        }
    }

    /// All three files exist — the layout a completed `fq run`
    /// bootstrap leaves behind.
    pub fn all_exist(&self) -> bool {
        self.as_array().iter().all(|p| p.exists())
    }

    /// At least one per-store file exists.
    pub fn any_exists(&self) -> bool {
        self.as_array().iter().any(|p| p.exists())
    }

    fn as_array(&self) -> [&PathBuf; 3] {
        [&self.worker, &self.control_plane, &self.projection]
    }
}

/// Path of the v1 single-file database under `dir`.
pub fn legacy_db_path(dir: &Path) -> PathBuf {
    dir.join(LEGACY_DB_FILE)
}

/// Headline row counts of a completed split, for the operator log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitStats {
    /// Rows in `worker.db`'s `invocation_state` (in-flight WAL).
    pub worker_invocations: i64,
    /// Rows in `control-plane.db`'s `invocation_archive`.
    pub archived_invocations: i64,
    /// Rows in `projection.db`'s `events`.
    pub projected_events: i64,
}

impl std::fmt::Display for SplitStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} in-flight invocation(s), {} archived, {} projected event(s)",
            self.worker_invocations, self.archived_invocations, self.projected_events
        )
    }
}

/// What [`split_legacy_events_db`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitOutcome {
    /// No legacy file — fresh install or already split.
    NotNeeded,
    /// The legacy file was split into the per-store layout.
    Completed(SplitStats),
}

#[derive(Debug, thiserror::Error)]
pub enum SplitError {
    #[error("legacy split: filesystem error on {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("legacy split: database error: {0}")]
    Backend(#[from] sqlx::Error),

    #[error(
        "legacy split: {legacy} is in use by another process (an old fq daemon still \
         running?). Stop it and retry."
    )]
    LegacyBusy { legacy: PathBuf },

    #[error(
        "legacy split: both {legacy} and {existing} exist. The split never leaves this \
         state behind — it means a pre-split binary ran after the split and recreated \
         {legacy}. Decide which side is current: delete {legacy} to keep the split \
         stores, or delete the per-store files (worker.db, control-plane.db, \
         projection.db) to re-split {legacy}."
    )]
    Conflict { legacy: PathBuf, existing: PathBuf },

    #[error(
        "legacy split: {legacy} records schema class `{class}` at version {db_version}, \
         newer than this binary's {binary_version}. Splitting it could silently drop \
         newer tables — upgrade the binary first."
    )]
    SchemaAhead {
        legacy: PathBuf,
        class: String,
        db_version: u32,
        binary_version: u32,
    },
}

fn io_err(path: &Path, source: std::io::Error) -> SplitError {
    SplitError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Split a v1 single-file `events.db` under `dir` into the per-store
/// layout, if one is present. Idempotent and crash-safe; see the
/// module docs for the exact contract. Callers that open any store
/// for writing run this first; read-only surfaces do not (they
/// surface a "run `fq run` to migrate" hint instead), so exactly the
/// writable paths — which already assume exclusive-enough access to
/// the state directory — can trigger it.
pub async fn split_legacy_events_db(dir: &Path) -> Result<SplitOutcome, SplitError> {
    let legacy = legacy_db_path(dir);
    let marker = dir.join(SPLIT_MARKER_FILE);
    let targets = RuntimeDbPaths::under(dir);

    if !legacy.exists() {
        // A marker without a legacy file is the crash window between
        // retiring the legacy file and removing the marker: the split
        // itself completed.
        if marker.exists() {
            std::fs::remove_file(&marker).map_err(|e| io_err(&marker, e))?;
        }
        return Ok(SplitOutcome::NotNeeded);
    }

    if targets.any_exists() && !marker.exists() {
        let existing = targets
            .as_array()
            .into_iter()
            .find(|p| p.exists())
            .expect("any_exists() held")
            .clone();
        return Err(SplitError::Conflict { legacy, existing });
    }

    std::fs::write(&marker, b"factor-q legacy events.db split in progress\n")
        .map_err(|e| io_err(&marker, e))?;

    // Partial copies from a previous crashed attempt are garbage as
    // long as the legacy file is still the authority.
    for base in targets.as_array() {
        remove_db_files(base)?;
        remove_db_files(&split_tmp_path(base))?;
    }

    let mut legacy_conn = SqliteConnectOptions::new()
        .filename(&legacy)
        .busy_timeout(std::time::Duration::from_millis(500))
        .connect()
        .await?;

    // Refuse to copy a file written by a newer binary: the table
    // keep-lists below are this binary's inventory, and pruning a
    // newer schema with them would drop data.
    check_legacy_schema_not_ahead(&mut legacy_conn, &legacy).await?;

    // A concurrent writer means an old daemon is still running
    // against the legacy file; copying under it would lose its
    // writes. `BEGIN IMMEDIATE` takes the write lock (or fails with
    // SQLITE_BUSY after the busy timeout) without changing anything.
    if let Err(source) = sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut legacy_conn)
        .await
    {
        tracing::debug!(error = %source, "legacy split: BEGIN IMMEDIATE probe failed");
        return Err(SplitError::LegacyBusy { legacy });
    }
    sqlx::query("COMMIT").execute(&mut legacy_conn).await?;

    // One transactionally-consistent copy per store, pruned down to
    // the store's own tables. The copies are written to `.split-tmp`
    // names so a crash mid-copy never leaves a plausible-looking
    // final file.
    let plan: [(&PathBuf, &[&str], Option<&str>); 3] = [
        (&targets.worker, WORKER_TABLES, Some(WORKER_SCHEMA_CLASS)),
        (
            &targets.control_plane,
            CONTROL_PLANE_TABLES,
            Some(CONTROL_PLANE_SCHEMA_CLASS),
        ),
        (&targets.projection, PROJECTION_TABLES, None),
    ];
    for (base, keep, schema_class) in plan {
        let tmp = split_tmp_path(base);
        let quoted = tmp.display().to_string().replace('\'', "''");
        sqlx::query(&format!("VACUUM INTO '{quoted}'"))
            .execute(&mut legacy_conn)
            .await?;
        prune_copy(&tmp, keep, schema_class).await?;
    }
    legacy_conn.close().await?;

    // Commit point: temp → final renames (same directory, atomic),
    // then retire the legacy file. Only after `events.db` is gone is
    // the split visible as complete.
    for base in targets.as_array() {
        let tmp = split_tmp_path(base);
        std::fs::rename(&tmp, base).map_err(|e| io_err(&tmp, e))?;
    }
    let retired = dir.join(LEGACY_RETIRED_FILE);
    std::fs::rename(&legacy, &retired).map_err(|e| io_err(&legacy, e))?;
    // Sidecars are checkpointed away by the clean close above; if one
    // survives anyway, keep the byte-for-byte pairing intact under
    // the retired name (`<db>-wal` belongs to `<db>`).
    for suffix in ["-wal", "-shm"] {
        let sidecar = sidecar_path(&legacy, suffix);
        if sidecar.exists() {
            let _ = std::fs::rename(&sidecar, sidecar_path(&retired, suffix));
        }
    }
    std::fs::remove_file(&marker).map_err(|e| io_err(&marker, e))?;

    let stats = SplitStats {
        worker_invocations: count_rows(&targets.worker, "invocation_state").await?,
        archived_invocations: count_rows(&targets.control_plane, "invocation_archive").await?,
        projected_events: count_rows(&targets.projection, "events").await?,
    };
    Ok(SplitOutcome::Completed(stats))
}

/// `<base>.split-tmp` — the in-progress copy next to its final name.
fn split_tmp_path(base: &Path) -> PathBuf {
    let mut os = base.as_os_str().to_os_string();
    os.push(".split-tmp");
    PathBuf::from(os)
}

/// `<db><suffix>` — SQLite pairs sidecars by exact filename.
fn sidecar_path(base: &Path, suffix: &str) -> PathBuf {
    let mut os = base.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

/// Remove a database file and its WAL/SHM sidecars, tolerating
/// absence.
fn remove_db_files(base: &Path) -> Result<(), SplitError> {
    for path in [
        base.to_path_buf(),
        sidecar_path(base, "-wal"),
        sidecar_path(base, "-shm"),
    ] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(&path, e)),
        }
    }
    Ok(())
}

/// Error if the legacy file's `schema_meta` records a version newer
/// than this binary for either versioned class. A legacy file without
/// `schema_meta` (never written by a versioned store) passes — each
/// store's own open decides what to do with it after the split.
async fn check_legacy_schema_not_ahead(
    conn: &mut SqliteConnection,
    legacy: &Path,
) -> Result<(), SplitError> {
    let has_meta: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'schema_meta'",
    )
    .fetch_one(&mut *conn)
    .await?;
    if has_meta == 0 {
        return Ok(());
    }
    let rows = sqlx::query("SELECT class, version FROM schema_meta")
        .fetch_all(&mut *conn)
        .await?;
    for row in rows {
        let class: String = row.get(0);
        let db_version = row.get::<i64, _>(1) as u32;
        let binary_version = match class.as_str() {
            c if c == WORKER_SCHEMA_CLASS => WORKER_SCHEMA_VERSION,
            c if c == CONTROL_PLANE_SCHEMA_CLASS => CONTROL_PLANE_SCHEMA_VERSION,
            _ => continue,
        };
        if db_version > binary_version {
            return Err(SplitError::SchemaAhead {
                legacy: legacy.to_path_buf(),
                class,
                db_version,
                binary_version,
            });
        }
    }
    Ok(())
}

/// Reduce a full copy of the legacy file to one store's tables: drop
/// everything not in `keep`, prune `schema_meta` to the store's own
/// class (when the store has one), reclaim the space.
async fn prune_copy(
    path: &Path,
    keep: &[&str],
    schema_class: Option<&str>,
) -> Result<(), SplitError> {
    let mut conn = SqliteConnectOptions::new().filename(path).connect().await?;
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(&mut conn)
    .await?;
    for table in tables {
        if !keep.contains(&table.as_str()) {
            sqlx::query(&format!("DROP TABLE \"{table}\""))
                .execute(&mut conn)
                .await?;
        }
    }
    if let Some(class) = schema_class {
        sqlx::query("DELETE FROM schema_meta WHERE class != ?")
            .bind(class)
            .execute(&mut conn)
            .await?;
    }
    sqlx::query("VACUUM").execute(&mut conn).await?;
    conn.close().await?;
    Ok(())
}

/// Row count of `table` in the database at `path`, 0 when the table
/// does not exist (a legacy file predating that table's migration).
async fn count_rows(path: &Path, table: &str) -> Result<i64, SplitError> {
    let mut conn = SqliteConnectOptions::new().filename(path).connect().await?;
    let exists: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?")
            .bind(table)
            .fetch_one(&mut conn)
            .await?;
    let count = if exists == 0 {
        0
    } else {
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM \"{table}\""))
            .fetch_one(&mut conn)
            .await?
    };
    conn.close().await?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_plane::ControlPlaneStore;
    use crate::control_plane::projection::ProjectionStore;
    use crate::worker::WorkerStore;
    use tempfile::tempdir;

    /// Build a populated v1 single-file `events.db`: all three stores
    /// bootstrapped on one path, one recognisable row each.
    async fn seed_legacy(dir: &Path) -> PathBuf {
        let legacy = legacy_db_path(dir);
        let worker = WorkerStore::open(&legacy).await.unwrap();
        let cp = ControlPlaneStore::open(&legacy).await.unwrap();
        let _proj = ProjectionStore::open(&legacy).await.unwrap();

        worker
            .write_tool_intent("inv-legacy", "tc-1", "echo", "{}", 100)
            .await
            .unwrap();
        cp.register_worker("w-legacy", "host", 1).await.unwrap();

        let mut conn = SqliteConnectOptions::new()
            .filename(&legacy)
            .connect()
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO events (event_id, timestamp, agent_id, invocation_id, event_type) \
             VALUES ('ev-1', '2026-01-01T00:00:00Z', 'agent-a', 'inv-legacy', 'triggered')",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO invocation_state \
             (invocation_id, agent_id, schema_version, phase, state_blob, started_at, updated_at) \
             VALUES ('inv-legacy', 'agent-a', 1, 'running', x'00', 100, 100)",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO invocation_archive \
             (invocation_id, agent_id, final_phase, final_state_blob, started_at, terminal_at, archived_at) \
             VALUES ('inv-done', 'agent-a', 'completed', x'00', 1, 2, 3)",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        conn.close().await.unwrap();
        legacy
    }

    async fn table_names(path: &Path) -> Vec<String> {
        let mut conn = SqliteConnectOptions::new()
            .filename(path)
            .connect()
            .await
            .unwrap();
        let mut names: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        )
        .fetch_all(&mut conn)
        .await
        .unwrap();
        conn.close().await.unwrap();
        names.sort();
        names
    }

    async fn schema_meta_classes(path: &Path) -> Vec<String> {
        let mut conn = SqliteConnectOptions::new()
            .filename(path)
            .connect()
            .await
            .unwrap();
        let mut classes: Vec<String> = sqlx::query_scalar("SELECT class FROM schema_meta")
            .fetch_all(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        classes.sort();
        classes
    }

    #[tokio::test]
    async fn not_needed_without_legacy_file() {
        let dir = tempdir().unwrap();
        let outcome = split_legacy_events_db(dir.path()).await.unwrap();
        assert_eq!(outcome, SplitOutcome::NotNeeded);
        assert!(!RuntimeDbPaths::under(dir.path()).any_exists());
    }

    #[tokio::test]
    async fn splits_populated_legacy_into_three_files() {
        let dir = tempdir().unwrap();
        seed_legacy(dir.path()).await;

        let outcome = split_legacy_events_db(dir.path()).await.unwrap();
        let SplitOutcome::Completed(stats) = outcome else {
            panic!("expected Completed, got {outcome:?}");
        };
        assert_eq!(stats.worker_invocations, 1);
        assert_eq!(stats.archived_invocations, 1);
        assert_eq!(stats.projected_events, 1);

        let targets = RuntimeDbPaths::under(dir.path());
        assert!(targets.all_exist());
        assert!(!legacy_db_path(dir.path()).exists());
        assert!(dir.path().join(LEGACY_RETIRED_FILE).exists());
        assert!(!dir.path().join(SPLIT_MARKER_FILE).exists());

        // Each file holds exactly its store's tables.
        assert_eq!(
            table_names(&targets.worker).await,
            vec![
                "host_notice",
                "invocation_state",
                "llm_dispatch",
                "schema_meta",
                "tool_dispatch"
            ]
        );
        assert_eq!(
            table_names(&targets.control_plane).await,
            vec![
                "coordination_invocation_owner",
                "coordination_worker",
                "invocation_archive",
                "pending_wait",
                "schedule_entry",
                "schema_meta"
            ]
        );
        assert_eq!(
            table_names(&targets.projection).await,
            vec!["events", "invocation_summary"]
        );

        // schema_meta pruned to each store's own class.
        assert_eq!(schema_meta_classes(&targets.worker).await, vec!["worker"]);
        assert_eq!(
            schema_meta_classes(&targets.control_plane).await,
            vec!["control_plane"]
        );

        // The stores open their own files as current-version schemas
        // and the data is there.
        let worker = WorkerStore::open(&targets.worker).await.unwrap();
        assert!(
            worker
                .get_tool_dispatch("inv-legacy", "tc-1")
                .await
                .unwrap()
                .is_some()
        );
        let cp = ControlPlaneStore::open(&targets.control_plane)
            .await
            .unwrap();
        assert!(cp.get_worker("w-legacy").await.unwrap().is_some());
        assert!(cp.get_archive("inv-done").await.unwrap().is_some());
        let proj = ProjectionStore::open(&targets.projection).await.unwrap();
        assert_eq!(proj.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn second_run_is_a_no_op() {
        let dir = tempdir().unwrap();
        seed_legacy(dir.path()).await;
        let first = split_legacy_events_db(dir.path()).await.unwrap();
        assert!(matches!(first, SplitOutcome::Completed(_)));
        let second = split_legacy_events_db(dir.path()).await.unwrap();
        assert_eq!(second, SplitOutcome::NotNeeded);
    }

    #[tokio::test]
    async fn conflicting_layouts_without_marker_refuse() {
        let dir = tempdir().unwrap();
        seed_legacy(dir.path()).await;
        // A per-store file that the split did not create: the
        // downgrade-then-upgrade scenario.
        let targets = RuntimeDbPaths::under(dir.path());
        let _worker = WorkerStore::open(&targets.worker).await.unwrap();

        let err = split_legacy_events_db(dir.path()).await.unwrap_err();
        assert!(matches!(err, SplitError::Conflict { .. }), "got {err:?}");
        // Nothing was moved or deleted.
        assert!(legacy_db_path(dir.path()).exists());
        assert!(targets.worker.exists());
    }

    #[tokio::test]
    async fn crashed_attempt_resumes_with_marker() {
        let dir = tempdir().unwrap();
        seed_legacy(dir.path()).await;
        // Simulate a crash mid-copy: marker present, one partial
        // final file and one partial temp file left behind.
        std::fs::write(dir.path().join(SPLIT_MARKER_FILE), b"in progress\n").unwrap();
        std::fs::write(
            &RuntimeDbPaths::under(dir.path()).worker,
            b"partial garbage",
        )
        .unwrap();
        std::fs::write(
            split_tmp_path(&RuntimeDbPaths::under(dir.path()).projection),
            b"partial garbage",
        )
        .unwrap();

        let outcome = split_legacy_events_db(dir.path()).await.unwrap();
        assert!(matches!(outcome, SplitOutcome::Completed(_)));
        let targets = RuntimeDbPaths::under(dir.path());
        assert!(targets.all_exist());
        // The garbage was replaced by a real store file.
        let worker = WorkerStore::open(&targets.worker).await.unwrap();
        assert!(
            worker
                .get_tool_dispatch("inv-legacy", "tc-1")
                .await
                .unwrap()
                .is_some()
        );
        assert!(!dir.path().join(SPLIT_MARKER_FILE).exists());
    }

    #[tokio::test]
    async fn crash_after_retire_before_marker_removal_completes() {
        let dir = tempdir().unwrap();
        seed_legacy(dir.path()).await;
        split_legacy_events_db(dir.path()).await.unwrap();
        // Re-create the crash window: marker back, legacy already
        // retired.
        std::fs::write(dir.path().join(SPLIT_MARKER_FILE), b"in progress\n").unwrap();

        let outcome = split_legacy_events_db(dir.path()).await.unwrap();
        assert_eq!(outcome, SplitOutcome::NotNeeded);
        assert!(!dir.path().join(SPLIT_MARKER_FILE).exists());
        assert!(RuntimeDbPaths::under(dir.path()).all_exist());
    }

    #[tokio::test]
    async fn busy_legacy_writer_refuses_split() {
        let dir = tempdir().unwrap();
        let legacy = seed_legacy(dir.path()).await;

        let mut writer = SqliteConnectOptions::new()
            .filename(&legacy)
            .connect()
            .await
            .unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut writer)
            .await
            .unwrap();

        let err = split_legacy_events_db(dir.path()).await.unwrap_err();
        assert!(matches!(err, SplitError::LegacyBusy { .. }), "got {err:?}");

        sqlx::query("COMMIT").execute(&mut writer).await.unwrap();
        writer.close().await.unwrap();
        // Legacy untouched; a retry after the writer is gone works.
        assert!(legacy.exists());
        let outcome = split_legacy_events_db(dir.path()).await.unwrap();
        assert!(matches!(outcome, SplitOutcome::Completed(_)));
    }

    #[tokio::test]
    async fn legacy_from_newer_binary_refuses_split() {
        let dir = tempdir().unwrap();
        let legacy = seed_legacy(dir.path()).await;
        let mut conn = SqliteConnectOptions::new()
            .filename(&legacy)
            .connect()
            .await
            .unwrap();
        sqlx::query("UPDATE schema_meta SET version = version + 100 WHERE class = 'worker'")
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();

        let err = split_legacy_events_db(dir.path()).await.unwrap_err();
        assert!(matches!(err, SplitError::SchemaAhead { .. }), "got {err:?}");
        assert!(legacy.exists());
    }
}
