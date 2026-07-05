//! Control-plane SQLite store: coordination state, schedules,
//! pending waits, and the completed-invocation archive.
//!
//! Per `docs/design/committed/data-architecture.md` §3 and §9.2, this is
//! the control-plane's source-of-truth for the data that is
//! *not* a projection of the audit log. The `events_*` tables
//! managed by [`crate::control_plane::projection::ProjectionStore`]
//! remain rebuildable from NATS; the rows in this store are not.
//!
//! In v1 this store opens the same SQLite file as the projection
//! store and the [`crate::worker::WorkerStore`]. The three
//! stores manage disjoint tables and coordinate version-ing
//! through the shared `schema_meta` table. v2 splits the
//! file with no schema redesign.
//!
//! ## Schema versioning
//!
//! The `control_plane` class in `schema_meta` tracks this
//! store's schema version. Same refuse-and-flag semantics as
//! [`crate::worker::WorkerStore`].
//!
//! ## What this module does NOT do yet
//!
//! - Wiring into `fq run` startup beyond opening the file.
//!   Self-registration (the local worker registers with the
//!   local control-plane on `fq run` start) is wired in
//!   `fq-cli/src/main.rs` as part of step 3.
//! - Subscribing to `invocation.ambiguous` /
//!   `invocation.archived` events on NATS — that's step 7.
//! - The `fq recover` and `fq workers` commands — that's step 9.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::Utc;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Pool, Row, Sqlite};

/// Schema class name used in the shared `schema_meta` table.
pub const SCHEMA_CLASS: &str = "control_plane";

/// Schema version this binary expects for the control-plane
/// tables. Bump on incompatible schema changes.
pub const CONTROL_PLANE_SCHEMA_VERSION: u32 = 1;

const SCHEMA_META_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_meta (
    class       TEXT PRIMARY KEY,
    version     INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
"#;

const CONTROL_PLANE_TABLES_V1_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS coordination_worker (
    worker_id       TEXT PRIMARY KEY,
    host            TEXT NOT NULL,
    registered_at   INTEGER NOT NULL,
    last_heartbeat  INTEGER NOT NULL,
    status          TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS coordination_invocation_owner (
    invocation_id   TEXT PRIMARY KEY,
    worker_id       TEXT NOT NULL,
    assigned_at     INTEGER NOT NULL,
    status          TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_owner_worker_status ON coordination_invocation_owner(worker_id, status);

CREATE TABLE IF NOT EXISTS pending_wait (
    invocation_id   TEXT PRIMARY KEY,
    kind            TEXT NOT NULL,
    descriptor      TEXT NOT NULL,
    expires_at      INTEGER,
    created_at      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS schedule_entry (
    id              TEXT PRIMARY KEY,
    kind            TEXT NOT NULL,
    fire_at         INTEGER NOT NULL,
    payload         TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_schedule_fire_at ON schedule_entry(fire_at);

CREATE TABLE IF NOT EXISTS invocation_archive (
    invocation_id    TEXT PRIMARY KEY,
    agent_id         TEXT NOT NULL,
    final_phase      TEXT NOT NULL,
    final_state_blob BLOB NOT NULL,
    started_at       INTEGER NOT NULL,
    terminal_at      INTEGER NOT NULL,
    archived_at      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_archive_agent ON invocation_archive(agent_id, terminal_at);
CREATE INDEX IF NOT EXISTS idx_archive_archived_at ON invocation_archive(archived_at);
"#;

// ---------------------------------------------------------------
// Domain enums — typed wrappers over the string-valued status
// columns so callers don't sprinkle string literals through
// the codebase.
// ---------------------------------------------------------------

/// Worker membership status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerStatus {
    Alive,
    Stale,
    Shutdown,
}

impl WorkerStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkerStatus::Alive => "alive",
            WorkerStatus::Stale => "stale",
            WorkerStatus::Shutdown => "shutdown",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "alive" => Some(WorkerStatus::Alive),
            "stale" => Some(WorkerStatus::Stale),
            "shutdown" => Some(WorkerStatus::Shutdown),
            _ => None,
        }
    }
}

/// Invocation ownership status, as seen by the control-plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerStatus {
    InFlight,
    Completed,
    /// Terminal failure. Reached either via worker-emitted
    /// `invocation.archived` with `final_phase = "failed"`,
    /// or operator-issued
    /// [`crate::events::EventPayload::InvocationOperatorRecovered`].
    Failed,
    Ambiguous,
}

impl OwnerStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            OwnerStatus::InFlight => "in_flight",
            OwnerStatus::Completed => "completed",
            OwnerStatus::Failed => "failed",
            OwnerStatus::Ambiguous => "ambiguous",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "in_flight" => Some(OwnerStatus::InFlight),
            "completed" => Some(OwnerStatus::Completed),
            "failed" => Some(OwnerStatus::Failed),
            "ambiguous" => Some(OwnerStatus::Ambiguous),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------
// Row types — plain data, decoupled from sqlx.
// ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerRow {
    pub worker_id: String,
    pub host: String,
    pub registered_at: i64,
    pub last_heartbeat: i64,
    pub status: WorkerStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerRow {
    pub invocation_id: String,
    pub worker_id: String,
    pub assigned_at: i64,
    pub status: OwnerStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWaitRow {
    pub invocation_id: String,
    pub kind: String,
    pub descriptor: String,
    pub expires_at: Option<i64>,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleEntryRow {
    pub id: String,
    pub kind: String,
    pub fire_at: i64,
    pub payload: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvocationArchiveRow {
    pub invocation_id: String,
    pub agent_id: String,
    pub final_phase: String,
    pub final_state_blob: Vec<u8>,
    pub started_at: i64,
    pub terminal_at: i64,
    pub archived_at: i64,
}

// ---------------------------------------------------------------
// Pure predicates. Exposed as free functions so unit tests can
// exercise the policy decisions without a database.
// ---------------------------------------------------------------

/// Is a worker stale? `now - last_heartbeat > threshold`.
pub fn is_stale(last_heartbeat_ms: i64, now_ms: i64, threshold_ms: i64) -> bool {
    now_ms.saturating_sub(last_heartbeat_ms) > threshold_ms
}

/// Is a scheduled entry due? `fire_at <= now`.
pub fn is_due(fire_at_ms: i64, now_ms: i64) -> bool {
    fire_at_ms <= now_ms
}

/// Retention cutoff for archive sweep: rows older than this
/// timestamp can be deleted. `retention_days` is operator-set
/// (default 7 per data-architecture.md §5.3).
pub fn retention_cutoff_ms(now_ms: i64, retention_days: i64) -> i64 {
    now_ms.saturating_sub(retention_days.saturating_mul(86_400_000))
}

// ---------------------------------------------------------------
// Store
// ---------------------------------------------------------------

/// Control-plane store. Cheap to clone (the underlying
/// connection pool is `Arc`-reference-counted inside `sqlx`).
#[derive(Debug, Clone)]
pub struct ControlPlaneStore {
    pool: Pool<Sqlite>,
}

impl ControlPlaneStore {
    /// Open (or create) the control-plane store at the given
    /// path. Refuses to open if the file's recorded schema
    /// version is higher than this binary supports.
    pub async fn open(path: &Path) -> Result<Self, ControlPlaneStoreError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(ControlPlaneStoreError::CreateDir)?;
        }

        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;

        let store = Self { pool };
        store.bootstrap_schema().await?;
        Ok(store)
    }

    /// Open a read-only handle. Used by inspection commands.
    pub async fn open_read_only(path: &Path) -> Result<Self, ControlPlaneStoreError> {
        if !path.exists() {
            return Err(ControlPlaneStoreError::NotInitialised(path.to_path_buf()));
        }
        let url = format!("sqlite://{}?mode=ro", path.display());
        let options = SqliteConnectOptions::from_str(&url)?;
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await?;
        Ok(Self { pool })
    }

    async fn bootstrap_schema(&self) -> Result<(), ControlPlaneStoreError> {
        for stmt in split_sql(SCHEMA_META_SQL) {
            sqlx::query(&stmt).execute(&self.pool).await?;
        }

        let recorded = self.read_schema_version().await?;
        match check_compatibility(recorded, CONTROL_PLANE_SCHEMA_VERSION) {
            Compatibility::FreshInstall => {
                self.run_migrations(0, CONTROL_PLANE_SCHEMA_VERSION).await?;
                self.write_schema_version(CONTROL_PLANE_SCHEMA_VERSION)
                    .await?;
            }
            Compatibility::Current => {
                // Recorded version matches the binary; nothing
                // to do. Same reasoning as in the worker store:
                // not every migration is idempotent.
            }
            Compatibility::NeedsUpgrade { from } => {
                self.run_migrations(from, CONTROL_PLANE_SCHEMA_VERSION)
                    .await?;
                self.write_schema_version(CONTROL_PLANE_SCHEMA_VERSION)
                    .await?;
            }
            Compatibility::BinaryTooOld { db_version } => {
                return Err(ControlPlaneStoreError::IncompatibleSchema {
                    db_version,
                    binary_version: CONTROL_PLANE_SCHEMA_VERSION,
                });
            }
        }
        Ok(())
    }

    async fn read_schema_version(&self) -> Result<Option<u32>, ControlPlaneStoreError> {
        let row = sqlx::query("SELECT version FROM schema_meta WHERE class = ?")
            .bind(SCHEMA_CLASS)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<i64, _>(0) as u32))
    }

    async fn write_schema_version(&self, version: u32) -> Result<(), ControlPlaneStoreError> {
        let now = Utc::now().timestamp_millis();
        sqlx::query(
            r#"
            INSERT INTO schema_meta (class, version, updated_at) VALUES (?, ?, ?)
            ON CONFLICT(class) DO UPDATE SET version = excluded.version, updated_at = excluded.updated_at
            "#,
        )
        .bind(SCHEMA_CLASS)
        .bind(version as i64)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn run_migrations(&self, from: u32, to: u32) -> Result<(), ControlPlaneStoreError> {
        if from < 1 && to >= 1 {
            for stmt in split_sql(CONTROL_PLANE_TABLES_V1_SQL) {
                sqlx::query(&stmt).execute(&self.pool).await?;
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------
    // Worker membership
    // -----------------------------------------------------------

    /// Register a worker. Idempotent on `worker_id`: an existing
    /// row's `host`, `registered_at`, `last_heartbeat`, and
    /// `status` are overwritten so a restarting worker reuses
    /// its id without manual cleanup.
    pub async fn register_worker(
        &self,
        worker_id: &str,
        host: &str,
        now_ms: i64,
    ) -> Result<(), ControlPlaneStoreError> {
        sqlx::query(
            r#"
            INSERT INTO coordination_worker (worker_id, host, registered_at, last_heartbeat, status)
            VALUES (?, ?, ?, ?, ?)
            ON CONFLICT(worker_id) DO UPDATE SET
                host = excluded.host,
                registered_at = excluded.registered_at,
                last_heartbeat = excluded.last_heartbeat,
                status = excluded.status
            "#,
        )
        .bind(worker_id)
        .bind(host)
        .bind(now_ms)
        .bind(now_ms)
        .bind(WorkerStatus::Alive.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Update a worker's last heartbeat. No-op if the worker_id
    /// doesn't exist (returns Ok with rows_affected=0 in
    /// `Result`, but we collapse to Ok(()) — the caller cares
    /// about the eventual store state, not which write reset it).
    pub async fn heartbeat_worker(
        &self,
        worker_id: &str,
        now_ms: i64,
    ) -> Result<(), ControlPlaneStoreError> {
        sqlx::query(
            "UPDATE coordination_worker SET last_heartbeat = ?, status = ? WHERE worker_id = ?",
        )
        .bind(now_ms)
        .bind(WorkerStatus::Alive.as_str())
        .bind(worker_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark a worker as gracefully shut down.
    pub async fn mark_worker_shutdown(
        &self,
        worker_id: &str,
    ) -> Result<(), ControlPlaneStoreError> {
        sqlx::query("UPDATE coordination_worker SET status = ? WHERE worker_id = ?")
            .bind(WorkerStatus::Shutdown.as_str())
            .bind(worker_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Mark a worker as stale (heartbeat lapsed). Distinct
    /// from shutdown — stale means the worker may still be
    /// running but isn't reporting; shutdown means a clean
    /// exit. The coordination-consumer's stale-worker sweep
    /// promotes alive→stale; only an explicit operator
    /// action or graceful exit moves to shutdown.
    pub async fn mark_worker_stale(&self, worker_id: &str) -> Result<(), ControlPlaneStoreError> {
        sqlx::query("UPDATE coordination_worker SET status = ? WHERE worker_id = ? AND status = ?")
            .bind(WorkerStatus::Stale.as_str())
            .bind(worker_id)
            .bind(WorkerStatus::Alive.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_worker(
        &self,
        worker_id: &str,
    ) -> Result<Option<WorkerRow>, ControlPlaneStoreError> {
        let row = sqlx::query(
            r#"
            SELECT worker_id, host, registered_at, last_heartbeat, status
            FROM coordination_worker WHERE worker_id = ?
            "#,
        )
        .bind(worker_id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(r) => Ok(Some(row_to_worker(r)?)),
            None => Ok(None),
        }
    }

    pub async fn list_workers(&self) -> Result<Vec<WorkerRow>, ControlPlaneStoreError> {
        let rows = sqlx::query(
            r#"
            SELECT worker_id, host, registered_at, last_heartbeat, status
            FROM coordination_worker ORDER BY worker_id
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_worker).collect()
    }

    /// Workers whose heartbeat is older than `now_ms - threshold_ms`
    /// AND not already marked shutdown. Uses the pure
    /// [`is_stale`] predicate for the cutoff calculation.
    pub async fn list_stale_workers(
        &self,
        now_ms: i64,
        threshold_ms: i64,
    ) -> Result<Vec<WorkerRow>, ControlPlaneStoreError> {
        let cutoff = now_ms.saturating_sub(threshold_ms);
        let rows = sqlx::query(
            r#"
            SELECT worker_id, host, registered_at, last_heartbeat, status
            FROM coordination_worker
            WHERE last_heartbeat < ? AND status != ?
            ORDER BY last_heartbeat
            "#,
        )
        .bind(cutoff)
        .bind(WorkerStatus::Shutdown.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_worker).collect()
    }

    // -----------------------------------------------------------
    // Invocation ownership
    // -----------------------------------------------------------

    pub async fn assign_invocation(
        &self,
        invocation_id: &str,
        worker_id: &str,
        now_ms: i64,
    ) -> Result<(), ControlPlaneStoreError> {
        sqlx::query(
            r#"
            INSERT INTO coordination_invocation_owner
                (invocation_id, worker_id, assigned_at, status)
            VALUES (?, ?, ?, ?)
            "#,
        )
        .bind(invocation_id)
        .bind(worker_id)
        .bind(now_ms)
        .bind(OwnerStatus::InFlight.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn update_invocation_status(
        &self,
        invocation_id: &str,
        status: OwnerStatus,
    ) -> Result<u64, ControlPlaneStoreError> {
        let res = sqlx::query(
            "UPDATE coordination_invocation_owner SET status = ? WHERE invocation_id = ?",
        )
        .bind(status.as_str())
        .bind(invocation_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Upsert the ownership row. Used by recovery: an
    /// invocation may not have a pre-existing coordination
    /// row (the trigger-dispatch path doesn't populate it
    /// yet — that wiring lands later in the plan). The
    /// recovery path needs to be able to record an
    /// ambiguous status without depending on prior insert.
    pub async fn upsert_invocation_ownership(
        &self,
        invocation_id: &str,
        worker_id: &str,
        assigned_at: i64,
        status: OwnerStatus,
    ) -> Result<(), ControlPlaneStoreError> {
        sqlx::query(
            r#"
            INSERT INTO coordination_invocation_owner
                (invocation_id, worker_id, assigned_at, status)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(invocation_id) DO UPDATE SET
                worker_id = excluded.worker_id,
                assigned_at = excluded.assigned_at,
                status = excluded.status
            "#,
        )
        .bind(invocation_id)
        .bind(worker_id)
        .bind(assigned_at)
        .bind(status.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_invocation_owner(
        &self,
        invocation_id: &str,
    ) -> Result<Option<OwnerRow>, ControlPlaneStoreError> {
        let row = sqlx::query(
            r#"
            SELECT invocation_id, worker_id, assigned_at, status
            FROM coordination_invocation_owner WHERE invocation_id = ?
            "#,
        )
        .bind(invocation_id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(r) => Ok(Some(row_to_owner(r)?)),
            None => Ok(None),
        }
    }

    pub async fn list_invocations_for_worker(
        &self,
        worker_id: &str,
    ) -> Result<Vec<OwnerRow>, ControlPlaneStoreError> {
        let rows = sqlx::query(
            r#"
            SELECT invocation_id, worker_id, assigned_at, status
            FROM coordination_invocation_owner WHERE worker_id = ?
            ORDER BY assigned_at
            "#,
        )
        .bind(worker_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_owner).collect()
    }

    pub async fn list_invocations_with_status(
        &self,
        status: OwnerStatus,
    ) -> Result<Vec<OwnerRow>, ControlPlaneStoreError> {
        let rows = sqlx::query(
            r#"
            SELECT invocation_id, worker_id, assigned_at, status
            FROM coordination_invocation_owner WHERE status = ?
            ORDER BY assigned_at
            "#,
        )
        .bind(status.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_owner).collect()
    }

    // -----------------------------------------------------------
    // Pending waits
    // -----------------------------------------------------------

    pub async fn insert_wait(&self, row: &PendingWaitRow) -> Result<(), ControlPlaneStoreError> {
        sqlx::query(
            r#"
            INSERT INTO pending_wait (invocation_id, kind, descriptor, expires_at, created_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(&row.invocation_id)
        .bind(&row.kind)
        .bind(&row.descriptor)
        .bind(row.expires_at)
        .bind(row.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_wait(
        &self,
        invocation_id: &str,
    ) -> Result<Option<PendingWaitRow>, ControlPlaneStoreError> {
        let row = sqlx::query(
            r#"
            SELECT invocation_id, kind, descriptor, expires_at, created_at
            FROM pending_wait WHERE invocation_id = ?
            "#,
        )
        .bind(invocation_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_wait))
    }

    /// Remove a pending wait — i.e. signal it. Returns the row
    /// count (0 if no wait existed; 1 if one was removed).
    pub async fn signal_wait(&self, invocation_id: &str) -> Result<u64, ControlPlaneStoreError> {
        let res = sqlx::query("DELETE FROM pending_wait WHERE invocation_id = ?")
            .bind(invocation_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    /// Waits whose `expires_at` is in the past. Used by the
    /// schedule poller to fire timed waits.
    pub async fn list_expired_waits(
        &self,
        now_ms: i64,
    ) -> Result<Vec<PendingWaitRow>, ControlPlaneStoreError> {
        let rows = sqlx::query(
            r#"
            SELECT invocation_id, kind, descriptor, expires_at, created_at
            FROM pending_wait
            WHERE expires_at IS NOT NULL AND expires_at <= ?
            ORDER BY expires_at
            "#,
        )
        .bind(now_ms)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_wait).collect())
    }

    // -----------------------------------------------------------
    // Schedule entries
    // -----------------------------------------------------------

    pub async fn insert_schedule(
        &self,
        row: &ScheduleEntryRow,
    ) -> Result<(), ControlPlaneStoreError> {
        sqlx::query(
            r#"
            INSERT INTO schedule_entry (id, kind, fire_at, payload) VALUES (?, ?, ?, ?)
            "#,
        )
        .bind(&row.id)
        .bind(&row.kind)
        .bind(row.fire_at)
        .bind(&row.payload)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_schedule(
        &self,
        id: &str,
    ) -> Result<Option<ScheduleEntryRow>, ControlPlaneStoreError> {
        let row = sqlx::query("SELECT id, kind, fire_at, payload FROM schedule_entry WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_schedule))
    }

    pub async fn list_due_schedules(
        &self,
        now_ms: i64,
    ) -> Result<Vec<ScheduleEntryRow>, ControlPlaneStoreError> {
        let rows = sqlx::query(
            r#"
            SELECT id, kind, fire_at, payload FROM schedule_entry
            WHERE fire_at <= ? ORDER BY fire_at
            "#,
        )
        .bind(now_ms)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_schedule).collect())
    }

    pub async fn delete_schedule(&self, id: &str) -> Result<u64, ControlPlaneStoreError> {
        let res = sqlx::query("DELETE FROM schedule_entry WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    // -----------------------------------------------------------
    // Invocation archive
    // -----------------------------------------------------------

    pub async fn insert_archive(
        &self,
        row: &InvocationArchiveRow,
    ) -> Result<(), ControlPlaneStoreError> {
        sqlx::query(
            r#"
            INSERT INTO invocation_archive
                (invocation_id, agent_id, final_phase, final_state_blob,
                 started_at, terminal_at, archived_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(invocation_id) DO NOTHING
            "#,
        )
        .bind(&row.invocation_id)
        .bind(&row.agent_id)
        .bind(&row.final_phase)
        .bind(&row.final_state_blob)
        .bind(row.started_at)
        .bind(row.terminal_at)
        .bind(row.archived_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_archive(
        &self,
        invocation_id: &str,
    ) -> Result<Option<InvocationArchiveRow>, ControlPlaneStoreError> {
        let row = sqlx::query(
            r#"
            SELECT invocation_id, agent_id, final_phase, final_state_blob,
                   started_at, terminal_at, archived_at
            FROM invocation_archive WHERE invocation_id = ?
            "#,
        )
        .bind(invocation_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_archive))
    }

    /// Bulk-delete archive rows whose `archived_at` is older
    /// than `cutoff_ms`. Used by the retention sweep in step 10.
    /// Returns the number of rows deleted.
    pub async fn sweep_archive(&self, cutoff_ms: i64) -> Result<u64, ControlPlaneStoreError> {
        let res = sqlx::query("DELETE FROM invocation_archive WHERE archived_at < ?")
            .bind(cutoff_ms)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    pub async fn list_archive_for_agent(
        &self,
        agent_id: &str,
    ) -> Result<Vec<InvocationArchiveRow>, ControlPlaneStoreError> {
        let rows = sqlx::query(
            r#"
            SELECT invocation_id, agent_id, final_phase, final_state_blob,
                   started_at, terminal_at, archived_at
            FROM invocation_archive WHERE agent_id = ?
            ORDER BY terminal_at DESC
            "#,
        )
        .bind(agent_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_archive).collect())
    }

    /// Most-recent `n` rows from `invocation_archive`, ordered by
    /// `archived_at` DESC. Used by `fq invocation list
    /// --include-archived`.
    pub async fn list_archives_recent(
        &self,
        limit: i64,
    ) -> Result<Vec<InvocationArchiveRow>, ControlPlaneStoreError> {
        let rows = sqlx::query(
            r#"
            SELECT invocation_id, agent_id, final_phase, final_state_blob,
                   started_at, terminal_at, archived_at
            FROM invocation_archive
            ORDER BY archived_at DESC
            LIMIT ?
            "#,
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_archive).collect())
    }

    /// List coordination ownership rows, optionally filtered by
    /// status, capped to `limit`, ordered by `assigned_at` DESC.
    /// Used by `fq invocation list`.
    pub async fn list_invocations(
        &self,
        status: Option<OwnerStatus>,
        limit: i64,
    ) -> Result<Vec<OwnerRow>, ControlPlaneStoreError> {
        let rows = match status {
            Some(s) => {
                sqlx::query(
                    r#"
                    SELECT invocation_id, worker_id, assigned_at, status
                    FROM coordination_invocation_owner
                    WHERE status = ?
                    ORDER BY assigned_at DESC
                    LIMIT ?
                    "#,
                )
                .bind(s.as_str())
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    r#"
                    SELECT invocation_id, worker_id, assigned_at, status
                    FROM coordination_invocation_owner
                    ORDER BY assigned_at DESC
                    LIMIT ?
                    "#,
                )
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
        };
        rows.into_iter().map(row_to_owner).collect()
    }
}

// ---------------------------------------------------------------
// Schema-version compatibility (mirrors the worker store).
// ---------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compatibility {
    FreshInstall,
    Current,
    NeedsUpgrade { from: u32 },
    BinaryTooOld { db_version: u32 },
}

pub fn check_compatibility(recorded: Option<u32>, binary: u32) -> Compatibility {
    match recorded {
        None => Compatibility::FreshInstall,
        Some(v) if v == binary => Compatibility::Current,
        Some(v) if v < binary => Compatibility::NeedsUpgrade { from: v },
        Some(v) => Compatibility::BinaryTooOld { db_version: v },
    }
}

fn split_sql(sql: &str) -> Vec<String> {
    sql.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn row_to_worker(row: sqlx::sqlite::SqliteRow) -> Result<WorkerRow, ControlPlaneStoreError> {
    let status_str: String = row.get("status");
    let status = WorkerStatus::parse(&status_str).ok_or_else(|| {
        ControlPlaneStoreError::Malformed(format!("unknown worker status `{status_str}`"))
    })?;
    Ok(WorkerRow {
        worker_id: row.get("worker_id"),
        host: row.get("host"),
        registered_at: row.get("registered_at"),
        last_heartbeat: row.get("last_heartbeat"),
        status,
    })
}

fn row_to_owner(row: sqlx::sqlite::SqliteRow) -> Result<OwnerRow, ControlPlaneStoreError> {
    let status_str: String = row.get("status");
    let status = OwnerStatus::parse(&status_str).ok_or_else(|| {
        ControlPlaneStoreError::Malformed(format!("unknown owner status `{status_str}`"))
    })?;
    Ok(OwnerRow {
        invocation_id: row.get("invocation_id"),
        worker_id: row.get("worker_id"),
        assigned_at: row.get("assigned_at"),
        status,
    })
}

fn row_to_wait(row: sqlx::sqlite::SqliteRow) -> PendingWaitRow {
    PendingWaitRow {
        invocation_id: row.get("invocation_id"),
        kind: row.get("kind"),
        descriptor: row.get("descriptor"),
        expires_at: row.get("expires_at"),
        created_at: row.get("created_at"),
    }
}

fn row_to_schedule(row: sqlx::sqlite::SqliteRow) -> ScheduleEntryRow {
    ScheduleEntryRow {
        id: row.get("id"),
        kind: row.get("kind"),
        fire_at: row.get("fire_at"),
        payload: row.get("payload"),
    }
}

fn row_to_archive(row: sqlx::sqlite::SqliteRow) -> InvocationArchiveRow {
    InvocationArchiveRow {
        invocation_id: row.get("invocation_id"),
        agent_id: row.get("agent_id"),
        final_phase: row.get("final_phase"),
        final_state_blob: row.get("final_state_blob"),
        started_at: row.get("started_at"),
        terminal_at: row.get("terminal_at"),
        archived_at: row.get("archived_at"),
    }
}

/// Errors from the control-plane store.
///
/// `Backend` carries a `String` rather than a backend-specific
/// error type so swapping the underlying storage does not break
/// downstream consumers' match arms. Internal code uses
/// `From<sqlx::Error>` for ergonomic propagation; the public
/// variant only exposes a message.
#[derive(Debug, thiserror::Error)]
pub enum ControlPlaneStoreError {
    #[error("control-plane store backend error: {0}")]
    Backend(String),

    #[error("failed to create database directory: {0}")]
    CreateDir(std::io::Error),

    #[error("control-plane store not initialised at {0}")]
    NotInitialised(PathBuf),

    #[error(
        "incompatible schema: db is at version {db_version}, this binary supports {binary_version}. \
         Roll back the runtime or use `fq invocation drop --schema-mismatch` to abandon coordination state."
    )]
    IncompatibleSchema {
        db_version: u32,
        binary_version: u32,
    },

    #[error("malformed row from control-plane store: {0}")]
    Malformed(String),
}

impl From<sqlx::Error> for ControlPlaneStoreError {
    fn from(err: sqlx::Error) -> Self {
        ControlPlaneStoreError::Backend(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // ----- Unit -----

    #[test]
    fn is_stale_within_threshold_returns_false() {
        // last_heartbeat 1000ms ago, threshold 5s
        assert!(!is_stale(9_000, 10_000, 5_000));
    }

    #[test]
    fn is_stale_past_threshold_returns_true() {
        // last_heartbeat 6s ago, threshold 5s
        assert!(is_stale(4_000, 10_000, 5_000));
    }

    #[test]
    fn is_stale_handles_zero_and_negative_clock_skew() {
        assert!(!is_stale(10_000, 10_000, 5_000)); // exactly now
        assert!(!is_stale(10_000, 10_000, 0)); // threshold zero, not stale at exactly now
        assert!(!is_stale(11_000, 10_000, 5_000)); // future heartbeat: not stale
    }

    #[test]
    fn is_due_when_fire_at_is_past_or_now() {
        assert!(is_due(99, 100));
        assert!(is_due(100, 100));
        assert!(!is_due(101, 100));
    }

    #[test]
    fn retention_cutoff_subtracts_days() {
        let now = 86_400_000 * 10; // day 10
        assert_eq!(retention_cutoff_ms(now, 7), 86_400_000 * 3); // day 3
        assert_eq!(retention_cutoff_ms(now, 0), now);
    }

    #[test]
    fn check_compatibility_classifies_correctly() {
        assert_eq!(check_compatibility(None, 1), Compatibility::FreshInstall);
        assert_eq!(check_compatibility(Some(1), 1), Compatibility::Current);
        assert_eq!(
            check_compatibility(Some(1), 2),
            Compatibility::NeedsUpgrade { from: 1 }
        );
        assert_eq!(
            check_compatibility(Some(3), 2),
            Compatibility::BinaryTooOld { db_version: 3 }
        );
    }

    #[test]
    fn worker_status_round_trip() {
        for s in [
            WorkerStatus::Alive,
            WorkerStatus::Stale,
            WorkerStatus::Shutdown,
        ] {
            assert_eq!(WorkerStatus::parse(s.as_str()), Some(s));
        }
        assert!(WorkerStatus::parse("garbage").is_none());
    }

    #[test]
    fn owner_status_round_trip() {
        for s in [
            OwnerStatus::InFlight,
            OwnerStatus::Completed,
            OwnerStatus::Failed,
            OwnerStatus::Ambiguous,
        ] {
            assert_eq!(OwnerStatus::parse(s.as_str()), Some(s));
        }
    }

    // ----- Integration -----

    async fn open_fresh() -> (ControlPlaneStore, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.db");
        let store = ControlPlaneStore::open(&path).await.expect("open fresh");
        (store, dir)
    }

    #[tokio::test]
    async fn open_creates_tables_and_records_version() {
        let (store, _dir) = open_fresh().await;
        assert_eq!(
            store.read_schema_version().await.unwrap(),
            Some(CONTROL_PLANE_SCHEMA_VERSION)
        );

        for table in [
            "coordination_worker",
            "coordination_invocation_owner",
            "pending_wait",
            "schedule_entry",
            "invocation_archive",
        ] {
            let row = sqlx::query("SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?")
                .bind(table)
                .fetch_optional(&store.pool)
                .await
                .unwrap();
            assert!(row.is_some(), "missing table {table}");
        }
    }

    #[tokio::test]
    async fn open_refuses_when_db_version_higher_than_binary() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.db");
        let store = ControlPlaneStore::open(&path).await.unwrap();
        store
            .write_schema_version(CONTROL_PLANE_SCHEMA_VERSION + 1)
            .await
            .unwrap();
        drop(store);
        let err = ControlPlaneStore::open(&path)
            .await
            .expect_err("should refuse newer DB");
        assert!(matches!(
            err,
            ControlPlaneStoreError::IncompatibleSchema { .. }
        ));
    }

    #[tokio::test]
    async fn worker_registration_round_trip() {
        let (store, _dir) = open_fresh().await;
        store
            .register_worker("w-001", "prod-1", 1_000)
            .await
            .unwrap();
        let w = store.get_worker("w-001").await.unwrap().unwrap();
        assert_eq!(w.worker_id, "w-001");
        assert_eq!(w.host, "prod-1");
        assert_eq!(w.registered_at, 1_000);
        assert_eq!(w.last_heartbeat, 1_000);
        assert_eq!(w.status, WorkerStatus::Alive);

        // Re-register with a different host: row updated in place.
        store
            .register_worker("w-001", "prod-2", 2_000)
            .await
            .unwrap();
        let w = store.get_worker("w-001").await.unwrap().unwrap();
        assert_eq!(w.host, "prod-2");
        assert_eq!(w.registered_at, 2_000);
    }

    #[tokio::test]
    async fn worker_heartbeat_updates_last_heartbeat() {
        let (store, _dir) = open_fresh().await;
        store.register_worker("w-002", "host", 100).await.unwrap();
        store.heartbeat_worker("w-002", 200).await.unwrap();
        let w = store.get_worker("w-002").await.unwrap().unwrap();
        assert_eq!(w.last_heartbeat, 200);
        store.heartbeat_worker("w-002", 300).await.unwrap();
        let w = store.get_worker("w-002").await.unwrap().unwrap();
        assert_eq!(w.last_heartbeat, 300);
    }

    #[tokio::test]
    async fn worker_marked_stale_after_heartbeat_lapse() {
        let (store, _dir) = open_fresh().await;
        store.register_worker("alive", "h", 10_000).await.unwrap();
        store.register_worker("stale", "h", 1_000).await.unwrap();
        store.register_worker("gone", "h", 500).await.unwrap();
        store.mark_worker_shutdown("gone").await.unwrap();

        let now = 12_000;
        let threshold = 5_000; // 5s
        let stale = store.list_stale_workers(now, threshold).await.unwrap();
        let ids: Vec<_> = stale.iter().map(|w| w.worker_id.as_str()).collect();
        // alive is within threshold, gone is shutdown — both excluded.
        assert_eq!(ids, vec!["stale"]);
    }

    #[tokio::test]
    async fn invocation_ownership_round_trip() {
        let (store, _dir) = open_fresh().await;
        store.register_worker("w-1", "h", 1).await.unwrap();
        store.assign_invocation("inv-A", "w-1", 100).await.unwrap();
        store.assign_invocation("inv-B", "w-1", 200).await.unwrap();

        let owner = store.get_invocation_owner("inv-A").await.unwrap().unwrap();
        assert_eq!(owner.worker_id, "w-1");
        assert_eq!(owner.assigned_at, 100);
        assert_eq!(owner.status, OwnerStatus::InFlight);

        let listed = store.list_invocations_for_worker("w-1").await.unwrap();
        let ids: Vec<_> = listed.iter().map(|o| o.invocation_id.as_str()).collect();
        assert_eq!(ids, vec!["inv-A", "inv-B"]);

        let updated = store
            .update_invocation_status("inv-A", OwnerStatus::Ambiguous)
            .await
            .unwrap();
        assert_eq!(updated, 1);
        let amb = store
            .list_invocations_with_status(OwnerStatus::Ambiguous)
            .await
            .unwrap();
        assert_eq!(amb.len(), 1);
        assert_eq!(amb[0].invocation_id, "inv-A");
    }

    #[tokio::test]
    async fn pending_wait_insert_and_signal() {
        let (store, _dir) = open_fresh().await;
        let w = PendingWaitRow {
            invocation_id: "inv-x".to_string(),
            kind: "approval".to_string(),
            descriptor: r#"{"approver":"alice"}"#.to_string(),
            expires_at: Some(2_000),
            created_at: 1_000,
        };
        store.insert_wait(&w).await.unwrap();

        let back = store.get_wait("inv-x").await.unwrap().unwrap();
        assert_eq!(back, w);

        let n = store.signal_wait("inv-x").await.unwrap();
        assert_eq!(n, 1);
        assert!(store.get_wait("inv-x").await.unwrap().is_none());

        // Signalling a non-existent wait returns 0.
        let n = store.signal_wait("inv-x").await.unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn pending_wait_list_expired() {
        let (store, _dir) = open_fresh().await;
        let mk = |id: &str, expires: Option<i64>| PendingWaitRow {
            invocation_id: id.to_string(),
            kind: "time".to_string(),
            descriptor: "{}".to_string(),
            expires_at: expires,
            created_at: 0,
        };
        store.insert_wait(&mk("expired", Some(50))).await.unwrap();
        store.insert_wait(&mk("future", Some(150))).await.unwrap();
        store.insert_wait(&mk("no-expiry", None)).await.unwrap();

        let now = 100;
        let expired = store.list_expired_waits(now).await.unwrap();
        let ids: Vec<_> = expired.iter().map(|w| w.invocation_id.as_str()).collect();
        assert_eq!(ids, vec!["expired"]);
    }

    #[tokio::test]
    async fn schedule_entry_due_query() {
        let (store, _dir) = open_fresh().await;
        let mk = |id: &str, fire_at: i64| ScheduleEntryRow {
            id: id.to_string(),
            kind: "trigger".to_string(),
            fire_at,
            payload: "{}".to_string(),
        };
        store.insert_schedule(&mk("a", 100)).await.unwrap();
        store.insert_schedule(&mk("b", 200)).await.unwrap();
        store.insert_schedule(&mk("c", 50)).await.unwrap();

        let due = store.list_due_schedules(150).await.unwrap();
        let ids: Vec<_> = due.iter().map(|s| s.id.as_str()).collect();
        // Sorted by fire_at ascending.
        assert_eq!(ids, vec!["c", "a"]);

        let n = store.delete_schedule("a").await.unwrap();
        assert_eq!(n, 1);
        assert!(store.get_schedule("a").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn archive_insert_and_retention_query() {
        let (store, _dir) = open_fresh().await;
        let mk = |id: &str, archived_at: i64| InvocationArchiveRow {
            invocation_id: id.to_string(),
            agent_id: "agent-a".to_string(),
            final_phase: "completed".to_string(),
            final_state_blob: vec![1, 2, 3],
            started_at: 0,
            terminal_at: archived_at - 1,
            archived_at,
        };
        store.insert_archive(&mk("old1", 1_000)).await.unwrap();
        store.insert_archive(&mk("old2", 2_000)).await.unwrap();
        store.insert_archive(&mk("recent", 5_000)).await.unwrap();

        // Cutoff at 3_000 — old1 and old2 should be swept.
        let n = store.sweep_archive(3_000).await.unwrap();
        assert_eq!(n, 2);

        assert!(store.get_archive("old1").await.unwrap().is_none());
        assert!(store.get_archive("old2").await.unwrap().is_none());
        assert!(store.get_archive("recent").await.unwrap().is_some());

        let by_agent = store.list_archive_for_agent("agent-a").await.unwrap();
        assert_eq!(by_agent.len(), 1);
        assert_eq!(by_agent[0].invocation_id, "recent");
    }

    #[tokio::test]
    async fn archive_insert_is_idempotent_on_invocation_id() {
        let (store, _dir) = open_fresh().await;
        let row = InvocationArchiveRow {
            invocation_id: "dup".to_string(),
            agent_id: "a".to_string(),
            final_phase: "completed".to_string(),
            final_state_blob: vec![],
            started_at: 0,
            terminal_at: 1,
            archived_at: 2,
        };
        store.insert_archive(&row).await.unwrap();

        // Second insert for the same invocation_id is a no-op
        // (DO NOTHING). A redelivered `invocation.archived`
        // event must not produce a duplicate row or fail.
        let mut second = row.clone();
        second.archived_at = 999;
        store.insert_archive(&second).await.unwrap();

        // Stored row is the original, not the second.
        let back = store.get_archive("dup").await.unwrap().unwrap();
        assert_eq!(back.archived_at, 2);
    }

    #[tokio::test]
    async fn coexists_with_worker_store_in_same_file() {
        // Both stores should be able to share the same SQLite
        // file in v1 — they manage disjoint tables and use
        // their own `schema_meta` row classes.
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.db");

        let cp = ControlPlaneStore::open(&path).await.unwrap();
        let worker = crate::worker::WorkerStore::open(&path).await.unwrap();

        cp.register_worker("w-coexist", "h", 1).await.unwrap();

        // Worker store can write its own tables without
        // disturbing the control-plane data.
        worker
            .write_tool_intent("inv-cox", "tc", "echo", "{}", 100)
            .await
            .unwrap();

        assert!(cp.get_worker("w-coexist").await.unwrap().is_some());
        assert!(
            worker
                .get_tool_dispatch("inv-cox", "tc")
                .await
                .unwrap()
                .is_some()
        );
    }
}
