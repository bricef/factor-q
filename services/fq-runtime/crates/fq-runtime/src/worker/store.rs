//! Worker-side SQLite store: in-flight invocation state and the
//! three-state WAL for tool and LLM dispatches.
//!
//! Per `docs/design/data-architecture.md` §3 and §9.1, this is
//! the worker's source-of-truth for invocations it currently
//! owns. Each row is non-rebuildable from NATS — losing this
//! file means losing in-flight state.
//!
//! In v1 this store opens the same SQLite file as the
//! [`crate::control_plane::projection::ProjectionStore`]. The two
//! stores manage disjoint tables, share the connection pool's
//! locking, and coordinate version-ing through a shared
//! `schema_meta` table. When v2 splits the deployment, each
//! store moves to its own file with no schema redesign.
//!
//! ## Schema versioning
//!
//! The `schema_meta` table tracks one row per *schema class*
//! (`worker`, `projection`, ...). Each store reads its row on
//! open and:
//!
//! - If the row is missing → fresh schema; create tables, insert
//!   the row with the binary's expected version.
//! - If the row matches the binary's version → up-to-date.
//! - If the row's version is *higher* than the binary → refuse
//!   to start, per the §5.6 refuse-and-flag contract.
//! - If the row's version is *lower* → migrate forward (for now
//!   only additive migrations land, so this is a no-op past
//!   `CREATE TABLE IF NOT EXISTS`).
//!
//! ## What this module does NOT do yet
//!
//! - The reducer-state persistence integration (planned in
//!   step 5 of the data-architecture-v1 plan).
//! - The actual three-state WAL writes from the
//!   [`crate::worker::ReducerRunner`] (step 4).
//! - The recovery-categorisation queries (step 6).
//!
//! Step 2 lands the schema, the migration mechanism, the
//! version-compatibility check, and the basic CRUD that the
//! later steps will build on.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::Utc;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Pool, Row, Sqlite};

/// Schema class name used in the shared `schema_meta` table.
pub const SCHEMA_CLASS: &str = "worker";

/// Schema version this binary expects. Bump on incompatible
/// schema changes; additive migrations between versions belong
/// in [`run_migrations`].
///
/// Versions:
/// - **v1** — initial worker tables (`invocation_state`,
///   `tool_dispatch`, `llm_dispatch`).
/// - **v2** — adds `is_error INTEGER` to `llm_dispatch` so an
///   LLM call that fails has a non-ambiguous final state
///   (`completed` with `is_error=1`) rather than being stuck
///   in `dispatched` and surfacing as ambiguous on recovery.
/// - **v3** — adds `workspace_ref TEXT NULL` to
///   `invocation_state`. The column is currently unused (always
///   `NULL`); it reserves the slot for a future
///   workspace-storage layer (likely content-addressed) without
///   forcing a schema change at that point. See
///   data-architecture.md §3.3.
pub const WORKER_SCHEMA_VERSION: u32 = 3;

/// Soft warning threshold for the `state_blob` size, in bytes.
/// At this size, a write logs a warning to give the operator
/// data on whether the inline-in-SQLite assumption is holding.
/// If the threshold is regularly crossed, the architectural
/// next step is to move blobs to a filesystem layer with the
/// `state_blob` column becoming a reference. See
/// data-architecture.md §6 and the step-5 design discussion.
pub const STATE_BLOB_WARN_THRESHOLD_BYTES: usize = 10 * 1024 * 1024;

const SCHEMA_META_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_meta (
    class       TEXT PRIMARY KEY,
    version     INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
"#;

const WORKER_TABLES_V1_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS invocation_state (
    invocation_id   TEXT PRIMARY KEY,
    agent_id        TEXT NOT NULL,
    schema_version  INTEGER NOT NULL,
    phase           TEXT NOT NULL,
    state_blob      BLOB NOT NULL,
    iteration       INTEGER NOT NULL DEFAULT 0,
    started_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    terminal_at     INTEGER
);
CREATE INDEX IF NOT EXISTS idx_invocation_state_agent ON invocation_state(agent_id);
CREATE INDEX IF NOT EXISTS idx_invocation_state_terminal ON invocation_state(terminal_at);

CREATE TABLE IF NOT EXISTS tool_dispatch (
    invocation_id   TEXT NOT NULL,
    tool_call_id    TEXT NOT NULL,
    tool_name       TEXT NOT NULL,
    status          TEXT NOT NULL,
    parameters      TEXT NOT NULL,
    result          TEXT,
    is_error        INTEGER,
    intent_at       INTEGER NOT NULL,
    dispatched_at   INTEGER,
    completed_at    INTEGER,
    PRIMARY KEY (invocation_id, tool_call_id)
);
CREATE INDEX IF NOT EXISTS idx_tool_dispatch_status ON tool_dispatch(status, dispatched_at);

CREATE TABLE IF NOT EXISTS llm_dispatch (
    invocation_id   TEXT NOT NULL,
    request_id      TEXT NOT NULL,
    model           TEXT NOT NULL,
    status          TEXT NOT NULL,
    request_payload TEXT NOT NULL,
    response        TEXT,
    cost_usd        REAL,
    intent_at       INTEGER NOT NULL,
    dispatched_at   INTEGER,
    completed_at    INTEGER,
    PRIMARY KEY (invocation_id, request_id)
);
CREATE INDEX IF NOT EXISTS idx_llm_dispatch_status ON llm_dispatch(status, dispatched_at);
"#;

/// v2 migration: add `is_error` to `llm_dispatch`.
///
/// `ALTER TABLE ... ADD COLUMN` is idempotent in SQLite *only*
/// guarded by a check; we run this conditionally based on the
/// recorded schema version, so re-running is safe.
const WORKER_MIGRATION_V2_SQL: &str = r#"
ALTER TABLE llm_dispatch ADD COLUMN is_error INTEGER;
"#;

/// v3 migration: add `workspace_ref` to `invocation_state`.
///
/// Reserves the column for a future workspace-storage layer.
/// Currently always populated as NULL.
const WORKER_MIGRATION_V3_SQL: &str = r#"
ALTER TABLE invocation_state ADD COLUMN workspace_ref TEXT;
"#;

/// One of the three WAL states a dispatch can be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchStatus {
    Intent,
    Dispatched,
    Completed,
}

impl DispatchStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            DispatchStatus::Intent => "intent",
            DispatchStatus::Dispatched => "dispatched",
            DispatchStatus::Completed => "completed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "intent" => Some(DispatchStatus::Intent),
            "dispatched" => Some(DispatchStatus::Dispatched),
            "completed" => Some(DispatchStatus::Completed),
            _ => None,
        }
    }
}

/// One in-flight tool dispatch row, as queried back from the WAL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDispatchRow {
    pub invocation_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub status: DispatchStatus,
    pub parameters: String,
    pub result: Option<String>,
    pub is_error: Option<bool>,
    pub intent_at: i64,
    pub dispatched_at: Option<i64>,
    pub completed_at: Option<i64>,
}

/// One in-flight LLM-dispatch row.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmDispatchRow {
    pub invocation_id: String,
    pub request_id: String,
    pub model: String,
    pub status: DispatchStatus,
    pub request_payload: String,
    pub response: Option<String>,
    pub cost_usd: Option<f64>,
    /// `Some(true)` if the LLM call returned an error;
    /// `Some(false)` for a successful response;
    /// `None` until the dispatch reaches `completed`.
    pub is_error: Option<bool>,
    pub intent_at: i64,
    pub dispatched_at: Option<i64>,
    pub completed_at: Option<i64>,
}

/// One in-flight invocation row.
///
/// `state_blob` holds the reducer's conversation state only —
/// not the agent's filesystem state, which is a separate
/// future concern (see `workspace_ref`). See data-architecture.md
/// §3.3 and the step-5 design discussion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvocationStateRow {
    pub invocation_id: String,
    pub agent_id: String,
    pub schema_version: u32,
    pub phase: String,
    pub state_blob: Vec<u8>,
    pub iteration: u32,
    pub started_at: i64,
    pub updated_at: i64,
    pub terminal_at: Option<i64>,
    /// Reference to the agent's workspace state at the time
    /// this row was last written. Currently always `None`;
    /// reserved for the future workspace-storage layer
    /// (likely content-addressed).
    pub workspace_ref: Option<String>,
}

/// Worker-side store. Cheap to clone (the underlying connection
/// pool is `Arc`-reference-counted inside `sqlx`).
#[derive(Debug, Clone)]
pub struct WorkerStore {
    pool: Pool<Sqlite>,
}

impl WorkerStore {
    /// Open (or create) the worker store at the given path.
    ///
    /// Runs schema migrations as needed. Refuses to open if the
    /// file's recorded schema version is *higher* than this
    /// binary's [`WORKER_SCHEMA_VERSION`].
    pub async fn open(path: &Path) -> Result<Self, WorkerStoreError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(WorkerStoreError::CreateDir)?;
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

    /// Open a read-only handle. Used by inspection commands; does
    /// not run migrations.
    pub async fn open_read_only(path: &Path) -> Result<Self, WorkerStoreError> {
        if !path.exists() {
            return Err(WorkerStoreError::NotInitialised(path.to_path_buf()));
        }
        let url = format!("sqlite://{}?mode=ro", path.display());
        let options = SqliteConnectOptions::from_str(&url)?;
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await?;
        Ok(Self { pool })
    }

    /// Initialise schema_meta and run worker migrations. Idempotent.
    async fn bootstrap_schema(&self) -> Result<(), WorkerStoreError> {
        // schema_meta is shared by both stores in v1; create it
        // unconditionally with `IF NOT EXISTS` so racing with the
        // projection store's bootstrap is safe.
        for stmt in split_sql(SCHEMA_META_SQL) {
            sqlx::query(&stmt).execute(&self.pool).await?;
        }

        let recorded = self.read_schema_version().await?;
        match check_compatibility(recorded, WORKER_SCHEMA_VERSION) {
            Compatibility::FreshInstall => {
                self.run_migrations(0, WORKER_SCHEMA_VERSION).await?;
                self.write_schema_version(WORKER_SCHEMA_VERSION).await?;
            }
            Compatibility::Current => {
                // Recorded version matches the binary; nothing
                // to do. Migrations are NOT re-run because not
                // every migration is idempotent (e.g.
                // `ALTER TABLE ADD COLUMN` errors on a second
                // run with "duplicate column").
            }
            Compatibility::NeedsUpgrade { from } => {
                self.run_migrations(from, WORKER_SCHEMA_VERSION).await?;
                self.write_schema_version(WORKER_SCHEMA_VERSION).await?;
            }
            Compatibility::BinaryTooOld { db_version } => {
                return Err(WorkerStoreError::IncompatibleSchema {
                    db_version,
                    binary_version: WORKER_SCHEMA_VERSION,
                });
            }
        }
        Ok(())
    }

    /// Read the recorded version of the worker schema, or `None`
    /// if no row exists yet.
    async fn read_schema_version(&self) -> Result<Option<u32>, WorkerStoreError> {
        let row = sqlx::query("SELECT version FROM schema_meta WHERE class = ?")
            .bind(SCHEMA_CLASS)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<i64, _>(0) as u32))
    }

    async fn write_schema_version(&self, version: u32) -> Result<(), WorkerStoreError> {
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

    /// Apply the migrations needed to advance from `from` to
    /// `to`. Migrations are additive and gated on the recorded
    /// version; re-running on an up-to-date DB is a no-op past
    /// `IF NOT EXISTS`.
    async fn run_migrations(&self, from: u32, to: u32) -> Result<(), WorkerStoreError> {
        if from < 1 && to >= 1 {
            for stmt in split_sql(WORKER_TABLES_V1_SQL) {
                sqlx::query(&stmt).execute(&self.pool).await?;
            }
        }
        if from < 2 && to >= 2 {
            for stmt in split_sql(WORKER_MIGRATION_V2_SQL) {
                sqlx::query(&stmt).execute(&self.pool).await?;
            }
        }
        if from < 3 && to >= 3 {
            for stmt in split_sql(WORKER_MIGRATION_V3_SQL) {
                sqlx::query(&stmt).execute(&self.pool).await?;
            }
        }
        // Future migrations: 3 → 4, 4 → 5, etc.
        Ok(())
    }

    // -----------------------------------------------------------
    // Tool-dispatch WAL operations.
    // -----------------------------------------------------------

    /// Record `intent` for a tool dispatch.
    ///
    /// Idempotent on `(invocation_id, tool_call_id)` via
    /// `INSERT OR REPLACE`: re-issuing intent during recovery
    /// (when a stale row exists from a crash) succeeds. The
    /// stale row is overwritten with fresh `intent_at`. Safe
    /// because the row's later transitions
    /// (`dispatched`/`completed`) are also tied to the same
    /// PK, so concurrent transitions can't race.
    pub async fn write_tool_intent(
        &self,
        invocation_id: &str,
        tool_call_id: &str,
        tool_name: &str,
        parameters: &str,
        intent_at: i64,
    ) -> Result<(), WorkerStoreError> {
        sqlx::query(
            r#"
            INSERT OR REPLACE INTO tool_dispatch
                (invocation_id, tool_call_id, tool_name, status, parameters, intent_at)
            VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(invocation_id)
        .bind(tool_call_id)
        .bind(tool_name)
        .bind(DispatchStatus::Intent.as_str())
        .bind(parameters)
        .bind(intent_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Transition a tool dispatch from `intent` to `dispatched`.
    pub async fn write_tool_dispatched(
        &self,
        invocation_id: &str,
        tool_call_id: &str,
        dispatched_at: i64,
    ) -> Result<(), WorkerStoreError> {
        let res = sqlx::query(
            r#"
            UPDATE tool_dispatch
            SET status = ?, dispatched_at = ?
            WHERE invocation_id = ? AND tool_call_id = ? AND status = ?
            "#,
        )
        .bind(DispatchStatus::Dispatched.as_str())
        .bind(dispatched_at)
        .bind(invocation_id)
        .bind(tool_call_id)
        .bind(DispatchStatus::Intent.as_str())
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(WorkerStoreError::WalTransitionFailed {
                entity: "tool_dispatch",
                invocation_id: invocation_id.to_string(),
                call_id: tool_call_id.to_string(),
                reason: "no row in `intent` state".to_string(),
            });
        }
        Ok(())
    }

    /// Finalise a tool dispatch with its result. Transitions
    /// from `dispatched` to `completed`.
    pub async fn write_tool_completed(
        &self,
        invocation_id: &str,
        tool_call_id: &str,
        result: &str,
        is_error: bool,
        completed_at: i64,
    ) -> Result<(), WorkerStoreError> {
        let res = sqlx::query(
            r#"
            UPDATE tool_dispatch
            SET status = ?, result = ?, is_error = ?, completed_at = ?
            WHERE invocation_id = ? AND tool_call_id = ? AND status = ?
            "#,
        )
        .bind(DispatchStatus::Completed.as_str())
        .bind(result)
        .bind(is_error as i64)
        .bind(completed_at)
        .bind(invocation_id)
        .bind(tool_call_id)
        .bind(DispatchStatus::Dispatched.as_str())
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(WorkerStoreError::WalTransitionFailed {
                entity: "tool_dispatch",
                invocation_id: invocation_id.to_string(),
                call_id: tool_call_id.to_string(),
                reason: "no row in `dispatched` state".to_string(),
            });
        }
        Ok(())
    }

    /// Fetch a single tool-dispatch row by primary key.
    pub async fn get_tool_dispatch(
        &self,
        invocation_id: &str,
        tool_call_id: &str,
    ) -> Result<Option<ToolDispatchRow>, WorkerStoreError> {
        let row = sqlx::query(
            r#"
            SELECT invocation_id, tool_call_id, tool_name, status, parameters,
                   result, is_error, intent_at, dispatched_at, completed_at
            FROM tool_dispatch
            WHERE invocation_id = ? AND tool_call_id = ?
            "#,
        )
        .bind(invocation_id)
        .bind(tool_call_id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(r) => Ok(Some(row_to_tool_dispatch(r)?)),
            None => Ok(None),
        }
    }

    /// Find tool dispatches stuck in `dispatched` without a
    /// matching `completed` — the recovery-time ambiguous set.
    pub async fn find_ambiguous_tool_dispatches(
        &self,
    ) -> Result<Vec<ToolDispatchRow>, WorkerStoreError> {
        let rows = sqlx::query(
            r#"
            SELECT invocation_id, tool_call_id, tool_name, status, parameters,
                   result, is_error, intent_at, dispatched_at, completed_at
            FROM tool_dispatch
            WHERE status = ?
            ORDER BY dispatched_at
            "#,
        )
        .bind(DispatchStatus::Dispatched.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_tool_dispatch).collect()
    }

    // -----------------------------------------------------------
    // LLM-dispatch WAL operations. Same three-state shape as the
    // tool-dispatch side; symmetry is intentional per the §3.2
    // contract that LLM calls and tool calls share recovery
    // semantics.
    // -----------------------------------------------------------

    /// Record `intent` for an LLM dispatch. Idempotent via
    /// `INSERT OR REPLACE`; same reasoning as
    /// [`Self::write_tool_intent`].
    pub async fn write_llm_intent(
        &self,
        invocation_id: &str,
        request_id: &str,
        model: &str,
        request_payload: &str,
        intent_at: i64,
    ) -> Result<(), WorkerStoreError> {
        sqlx::query(
            r#"
            INSERT OR REPLACE INTO llm_dispatch
                (invocation_id, request_id, model, status, request_payload, intent_at)
            VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(invocation_id)
        .bind(request_id)
        .bind(model)
        .bind(DispatchStatus::Intent.as_str())
        .bind(request_payload)
        .bind(intent_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn write_llm_dispatched(
        &self,
        invocation_id: &str,
        request_id: &str,
        dispatched_at: i64,
    ) -> Result<(), WorkerStoreError> {
        let res = sqlx::query(
            r#"
            UPDATE llm_dispatch
            SET status = ?, dispatched_at = ?
            WHERE invocation_id = ? AND request_id = ? AND status = ?
            "#,
        )
        .bind(DispatchStatus::Dispatched.as_str())
        .bind(dispatched_at)
        .bind(invocation_id)
        .bind(request_id)
        .bind(DispatchStatus::Intent.as_str())
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(WorkerStoreError::WalTransitionFailed {
                entity: "llm_dispatch",
                invocation_id: invocation_id.to_string(),
                call_id: request_id.to_string(),
                reason: "no row in `intent` state".to_string(),
            });
        }
        Ok(())
    }

    pub async fn write_llm_completed(
        &self,
        invocation_id: &str,
        request_id: &str,
        response: &str,
        is_error: bool,
        cost_usd: f64,
        completed_at: i64,
    ) -> Result<(), WorkerStoreError> {
        let res = sqlx::query(
            r#"
            UPDATE llm_dispatch
            SET status = ?, response = ?, is_error = ?, cost_usd = ?, completed_at = ?
            WHERE invocation_id = ? AND request_id = ? AND status = ?
            "#,
        )
        .bind(DispatchStatus::Completed.as_str())
        .bind(response)
        .bind(is_error as i64)
        .bind(cost_usd)
        .bind(completed_at)
        .bind(invocation_id)
        .bind(request_id)
        .bind(DispatchStatus::Dispatched.as_str())
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(WorkerStoreError::WalTransitionFailed {
                entity: "llm_dispatch",
                invocation_id: invocation_id.to_string(),
                call_id: request_id.to_string(),
                reason: "no row in `dispatched` state".to_string(),
            });
        }
        Ok(())
    }

    pub async fn get_llm_dispatch(
        &self,
        invocation_id: &str,
        request_id: &str,
    ) -> Result<Option<LlmDispatchRow>, WorkerStoreError> {
        let row = sqlx::query(
            r#"
            SELECT invocation_id, request_id, model, status, request_payload,
                   response, cost_usd, is_error, intent_at, dispatched_at, completed_at
            FROM llm_dispatch
            WHERE invocation_id = ? AND request_id = ?
            "#,
        )
        .bind(invocation_id)
        .bind(request_id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(r) => Ok(Some(row_to_llm_dispatch(r)?)),
            None => Ok(None),
        }
    }

    pub async fn find_ambiguous_llm_dispatches(
        &self,
    ) -> Result<Vec<LlmDispatchRow>, WorkerStoreError> {
        let rows = sqlx::query(
            r#"
            SELECT invocation_id, request_id, model, status, request_payload,
                   response, cost_usd, is_error, intent_at, dispatched_at, completed_at
            FROM llm_dispatch
            WHERE status = ?
            ORDER BY dispatched_at
            "#,
        )
        .bind(DispatchStatus::Dispatched.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_llm_dispatch).collect()
    }

    // -----------------------------------------------------------
    // Invocation-state operations.
    // -----------------------------------------------------------

    /// Insert or update an invocation's persisted state.
    ///
    /// Logs a warning at [`STATE_BLOB_WARN_THRESHOLD_BYTES`] —
    /// useful telemetry on whether the inline-in-SQLite
    /// assumption is holding for the operator's workload.
    pub async fn upsert_invocation_state(
        &self,
        row: &InvocationStateRow,
    ) -> Result<(), WorkerStoreError> {
        if row.state_blob.len() > STATE_BLOB_WARN_THRESHOLD_BYTES {
            tracing::warn!(
                invocation_id = %row.invocation_id,
                agent_id = %row.agent_id,
                blob_size_bytes = row.state_blob.len(),
                threshold_bytes = STATE_BLOB_WARN_THRESHOLD_BYTES,
                "state_blob exceeds soft threshold; consider moving to filesystem-backed storage"
            );
        }
        sqlx::query(
            r#"
            INSERT INTO invocation_state
                (invocation_id, agent_id, schema_version, phase, state_blob,
                 iteration, started_at, updated_at, terminal_at, workspace_ref)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(invocation_id) DO UPDATE SET
                phase = excluded.phase,
                state_blob = excluded.state_blob,
                iteration = excluded.iteration,
                updated_at = excluded.updated_at,
                terminal_at = excluded.terminal_at,
                workspace_ref = excluded.workspace_ref
            "#,
        )
        .bind(&row.invocation_id)
        .bind(&row.agent_id)
        .bind(row.schema_version as i64)
        .bind(&row.phase)
        .bind(&row.state_blob)
        .bind(row.iteration as i64)
        .bind(row.started_at)
        .bind(row.updated_at)
        .bind(row.terminal_at)
        .bind(&row.workspace_ref)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch one invocation's persisted state by id.
    pub async fn get_invocation_state(
        &self,
        invocation_id: &str,
    ) -> Result<Option<InvocationStateRow>, WorkerStoreError> {
        let row = sqlx::query(
            r#"
            SELECT invocation_id, agent_id, schema_version, phase, state_blob,
                   iteration, started_at, updated_at, terminal_at, workspace_ref
            FROM invocation_state
            WHERE invocation_id = ?
            "#,
        )
        .bind(invocation_id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(r) => Ok(Some(row_to_invocation_state(r)?)),
            None => Ok(None),
        }
    }

    /// All invocations that have not reached a terminal status
    /// (`terminal_at IS NULL`). The shape recovery uses on
    /// startup.
    pub async fn find_in_flight_invocations(
        &self,
    ) -> Result<Vec<InvocationStateRow>, WorkerStoreError> {
        let rows = sqlx::query(
            r#"
            SELECT invocation_id, agent_id, schema_version, phase, state_blob,
                   iteration, started_at, updated_at, terminal_at, workspace_ref
            FROM invocation_state
            WHERE terminal_at IS NULL
            ORDER BY started_at
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_invocation_state).collect()
    }

    /// Delete an invocation's state row. Used after the
    /// completed-invocation hand-off ack from the control-plane
    /// (step 8 in the data-architecture-v1 plan).
    pub async fn delete_invocation_state(
        &self,
        invocation_id: &str,
    ) -> Result<u64, WorkerStoreError> {
        let res = sqlx::query("DELETE FROM invocation_state WHERE invocation_id = ?")
            .bind(invocation_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    /// All tool-dispatch rows for one invocation, ordered by
    /// `intent_at`. Used by the recovery categorisation logic
    /// (step 6) which needs to inspect every dispatch row to
    /// decide safe-resume / safe-replay / ambiguous.
    pub async fn list_tool_dispatches_for_invocation(
        &self,
        invocation_id: &str,
    ) -> Result<Vec<ToolDispatchRow>, WorkerStoreError> {
        let rows = sqlx::query(
            r#"
            SELECT invocation_id, tool_call_id, tool_name, status, parameters,
                   result, is_error, intent_at, dispatched_at, completed_at
            FROM tool_dispatch
            WHERE invocation_id = ?
            ORDER BY intent_at
            "#,
        )
        .bind(invocation_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_tool_dispatch).collect()
    }

    /// Symmetric to [`list_tool_dispatches_for_invocation`] for
    /// the LLM dispatch table.
    pub async fn list_llm_dispatches_for_invocation(
        &self,
        invocation_id: &str,
    ) -> Result<Vec<LlmDispatchRow>, WorkerStoreError> {
        let rows = sqlx::query(
            r#"
            SELECT invocation_id, request_id, model, status, request_payload,
                   response, cost_usd, is_error, intent_at, dispatched_at, completed_at
            FROM llm_dispatch
            WHERE invocation_id = ?
            ORDER BY intent_at
            "#,
        )
        .bind(invocation_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_llm_dispatch).collect()
    }
}

/// Outcome of comparing the binary's expected schema version
/// against what the database has recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compatibility {
    /// No schema_meta row for this class — first time we've
    /// touched this DB.
    FreshInstall,
    /// Recorded version equals the binary's expected version.
    Current,
    /// Recorded version is older than the binary's. Run
    /// migrations forward.
    NeedsUpgrade { from: u32 },
    /// Recorded version is newer than the binary supports.
    /// Refuse and surface the case to the operator.
    BinaryTooOld { db_version: u32 },
}

/// Pure compatibility check, exposed for unit testing without
/// needing a database.
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

fn row_to_tool_dispatch(row: sqlx::sqlite::SqliteRow) -> Result<ToolDispatchRow, WorkerStoreError> {
    let status_str: String = row.get("status");
    let status = DispatchStatus::parse(&status_str)
        .ok_or_else(|| WorkerStoreError::Malformed(format!("unknown status `{status_str}`")))?;
    let is_error: Option<i64> = row.get("is_error");
    Ok(ToolDispatchRow {
        invocation_id: row.get("invocation_id"),
        tool_call_id: row.get("tool_call_id"),
        tool_name: row.get("tool_name"),
        status,
        parameters: row.get("parameters"),
        result: row.get("result"),
        is_error: is_error.map(|x| x != 0),
        intent_at: row.get("intent_at"),
        dispatched_at: row.get("dispatched_at"),
        completed_at: row.get("completed_at"),
    })
}

fn row_to_llm_dispatch(row: sqlx::sqlite::SqliteRow) -> Result<LlmDispatchRow, WorkerStoreError> {
    let status_str: String = row.get("status");
    let status = DispatchStatus::parse(&status_str)
        .ok_or_else(|| WorkerStoreError::Malformed(format!("unknown status `{status_str}`")))?;
    let is_error: Option<i64> = row.get("is_error");
    Ok(LlmDispatchRow {
        invocation_id: row.get("invocation_id"),
        request_id: row.get("request_id"),
        model: row.get("model"),
        status,
        request_payload: row.get("request_payload"),
        response: row.get("response"),
        cost_usd: row.get("cost_usd"),
        is_error: is_error.map(|x| x != 0),
        intent_at: row.get("intent_at"),
        dispatched_at: row.get("dispatched_at"),
        completed_at: row.get("completed_at"),
    })
}

fn row_to_invocation_state(
    row: sqlx::sqlite::SqliteRow,
) -> Result<InvocationStateRow, WorkerStoreError> {
    Ok(InvocationStateRow {
        invocation_id: row.get("invocation_id"),
        agent_id: row.get("agent_id"),
        schema_version: row.get::<i64, _>("schema_version") as u32,
        phase: row.get("phase"),
        state_blob: row.get("state_blob"),
        iteration: row.get::<i64, _>("iteration") as u32,
        started_at: row.get("started_at"),
        updated_at: row.get("updated_at"),
        terminal_at: row.get("terminal_at"),
        workspace_ref: row.get("workspace_ref"),
    })
}

/// Errors from the worker store.
///
/// The `Backend` variant deliberately carries a `String` rather
/// than a backend-specific error type, so swapping the
/// underlying storage (today: SQLite via sqlx) does not break
/// downstream consumers' match arms. Internal code uses the
/// `From<sqlx::Error>` impl below for ergonomic propagation;
/// the public variant only exposes a message.
#[derive(Debug, thiserror::Error)]
pub enum WorkerStoreError {
    #[error("worker store backend error: {0}")]
    Backend(String),

    #[error("failed to create database directory: {0}")]
    CreateDir(std::io::Error),

    #[error("worker store not initialised at {0}")]
    NotInitialised(PathBuf),

    #[error(
        "incompatible schema: db is at version {db_version}, this binary supports {binary_version}. \
         Roll back the runtime or use `fq invocation drop --schema-mismatch` to abandon in-flight state."
    )]
    IncompatibleSchema {
        db_version: u32,
        binary_version: u32,
    },

    #[error("WAL transition failed for {entity} ({invocation_id}/{call_id}): {reason}")]
    WalTransitionFailed {
        /// Domain name of the entity whose transition failed
        /// (currently `tool_dispatch` or `llm_dispatch`). Named
        /// to avoid baking in "I am a relational table" — the
        /// value is the domain concept, not the storage row.
        entity: &'static str,
        invocation_id: String,
        call_id: String,
        reason: String,
    },

    #[error("malformed row from worker store: {0}")]
    Malformed(String),
}

impl From<sqlx::Error> for WorkerStoreError {
    fn from(err: sqlx::Error) -> Self {
        WorkerStoreError::Backend(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    //! Tiered tests:
    //! - **unit**: pure functions over `Compatibility` and
    //!   `DispatchStatus`. No I/O.
    //! - **integration**: in-memory or tempdir SQLite. Fast,
    //!   no env vars required.
    //!
    //! Live `fq run` acceptance — the daemon coming up cleanly
    //! against an empty cache dir — is exercised by the existing
    //! NATS-gated startup tests once the daemon construction in
    //! `fq-cli/src/main.rs` is updated to call `WorkerStore::open`
    //! (a step 3/4 follow-up).

    use super::*;
    use tempfile::tempdir;

    // ----- Unit -----

    #[test]
    fn check_compatibility_fresh_install_when_no_row() {
        assert_eq!(check_compatibility(None, 1), Compatibility::FreshInstall);
    }

    #[test]
    fn check_compatibility_current_when_matched() {
        assert_eq!(check_compatibility(Some(1), 1), Compatibility::Current);
        assert_eq!(check_compatibility(Some(7), 7), Compatibility::Current);
    }

    #[test]
    fn check_compatibility_needs_upgrade_when_db_older() {
        assert_eq!(
            check_compatibility(Some(1), 3),
            Compatibility::NeedsUpgrade { from: 1 }
        );
    }

    #[test]
    fn check_compatibility_binary_too_old_when_db_newer() {
        assert_eq!(
            check_compatibility(Some(2), 1),
            Compatibility::BinaryTooOld { db_version: 2 }
        );
    }

    #[test]
    fn dispatch_status_round_trip() {
        for s in [
            DispatchStatus::Intent,
            DispatchStatus::Dispatched,
            DispatchStatus::Completed,
        ] {
            assert_eq!(DispatchStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(DispatchStatus::parse("garbage"), None);
    }

    // ----- Integration -----

    async fn open_fresh() -> (WorkerStore, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.db");
        let store = WorkerStore::open(&path).await.expect("open fresh");
        (store, dir)
    }

    #[tokio::test]
    async fn open_creates_tables_and_records_version() {
        let (store, _dir) = open_fresh().await;

        let v = store.read_schema_version().await.expect("read version");
        assert_eq!(v, Some(WORKER_SCHEMA_VERSION));

        // Verify each expected table exists by selecting its
        // column list (sqlite_master is the metadata table).
        for table in ["invocation_state", "tool_dispatch", "llm_dispatch"] {
            let row = sqlx::query("SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?")
                .bind(table)
                .fetch_optional(&store.pool)
                .await
                .unwrap();
            assert!(row.is_some(), "missing table {table}");
        }
    }

    #[tokio::test]
    async fn open_against_existing_db_is_idempotent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.db");

        let _ = WorkerStore::open(&path).await.expect("first open");
        // Second open should not fail and should not re-run migrations.
        let store = WorkerStore::open(&path).await.expect("second open");
        let v = store.read_schema_version().await.unwrap();
        assert_eq!(v, Some(WORKER_SCHEMA_VERSION));
    }

    #[tokio::test]
    async fn open_refuses_when_db_version_higher_than_binary() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.db");

        // Bring the DB up to current version, then bump it
        // beyond what the binary supports.
        let store = WorkerStore::open(&path).await.unwrap();
        let future_version = WORKER_SCHEMA_VERSION + 1;
        store.write_schema_version(future_version).await.unwrap();
        drop(store);

        let err = WorkerStore::open(&path)
            .await
            .expect_err("should refuse newer DB");
        match err {
            WorkerStoreError::IncompatibleSchema {
                db_version,
                binary_version,
            } => {
                assert_eq!(db_version, future_version);
                assert_eq!(binary_version, WORKER_SCHEMA_VERSION);
            }
            other => panic!("expected IncompatibleSchema, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn open_against_v0_db_applies_migration_without_disturbing_other_tables() {
        // Simulate a pre-Step-2 database: only the projection
        // tables exist and there's no schema_meta row for
        // `worker`. Opening WorkerStore should add the worker
        // tables and stamp the version row, leaving the
        // projection tables intact.
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.db");

        // Create an existing-but-empty SQLite file with a
        // pre-existing unrelated table to stand in for a
        // pre-Step-2 layout.
        {
            let url = format!("sqlite://{}?mode=rwc", path.display());
            let opts = SqliteConnectOptions::from_str(&url).unwrap();
            let pool = SqlitePoolOptions::new().connect_with(opts).await.unwrap();
            sqlx::query("CREATE TABLE pretend_projection (id INTEGER PRIMARY KEY, note TEXT)")
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("INSERT INTO pretend_projection (note) VALUES ('preserved')")
                .execute(&pool)
                .await
                .unwrap();
            pool.close().await;
        }

        let store = WorkerStore::open(&path).await.expect("migrate v0 -> v1");
        assert_eq!(
            store.read_schema_version().await.unwrap(),
            Some(WORKER_SCHEMA_VERSION)
        );

        // The pretend-projection table is untouched.
        let row = sqlx::query("SELECT note FROM pretend_projection")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(row.get::<String, _>(0), "preserved");
    }

    #[tokio::test]
    async fn wal_intent_dispatched_completed_round_trip() {
        let (store, _dir) = open_fresh().await;
        let inv = "inv_test_1";
        let call = "tc_a";

        store
            .write_tool_intent(inv, call, "echo", r#"{"x":1}"#, 100)
            .await
            .unwrap();
        let r = store.get_tool_dispatch(inv, call).await.unwrap().unwrap();
        assert_eq!(r.status, DispatchStatus::Intent);
        assert_eq!(r.intent_at, 100);
        assert!(r.dispatched_at.is_none());
        assert!(r.completed_at.is_none());
        assert!(r.result.is_none());

        store.write_tool_dispatched(inv, call, 200).await.unwrap();
        let r = store.get_tool_dispatch(inv, call).await.unwrap().unwrap();
        assert_eq!(r.status, DispatchStatus::Dispatched);
        assert_eq!(r.dispatched_at, Some(200));
        assert!(r.completed_at.is_none());

        store
            .write_tool_completed(inv, call, r#"{"out":"ok"}"#, false, 300)
            .await
            .unwrap();
        let r = store.get_tool_dispatch(inv, call).await.unwrap().unwrap();
        assert_eq!(r.status, DispatchStatus::Completed);
        assert_eq!(r.completed_at, Some(300));
        assert_eq!(r.is_error, Some(false));
        assert_eq!(r.result.as_deref(), Some(r#"{"out":"ok"}"#));
    }

    #[tokio::test]
    async fn wal_dispatched_without_intent_fails() {
        let (store, _dir) = open_fresh().await;
        let err = store
            .write_tool_dispatched("missing_inv", "missing_call", 1)
            .await
            .expect_err("transition should fail");
        assert!(matches!(err, WorkerStoreError::WalTransitionFailed { .. }));
    }

    #[tokio::test]
    async fn wal_completed_without_dispatched_fails() {
        let (store, _dir) = open_fresh().await;
        store
            .write_tool_intent("inv1", "tc1", "shell", "{}", 1)
            .await
            .unwrap();
        // Skip the `dispatched` step.
        let err = store
            .write_tool_completed("inv1", "tc1", "{}", false, 5)
            .await
            .expect_err("must fail without dispatched");
        assert!(matches!(err, WorkerStoreError::WalTransitionFailed { .. }));
    }

    #[tokio::test]
    async fn find_ambiguous_returns_only_dispatched() {
        let (store, _dir) = open_fresh().await;

        // intent only — not ambiguous (safe-resume).
        store
            .write_tool_intent("inv1", "a", "shell", "{}", 1)
            .await
            .unwrap();

        // dispatched without completed — ambiguous.
        store
            .write_tool_intent("inv2", "b", "shell", "{}", 2)
            .await
            .unwrap();
        store.write_tool_dispatched("inv2", "b", 3).await.unwrap();

        // fully completed — safe-replay.
        store
            .write_tool_intent("inv3", "c", "shell", "{}", 4)
            .await
            .unwrap();
        store.write_tool_dispatched("inv3", "c", 5).await.unwrap();
        store
            .write_tool_completed("inv3", "c", "{}", false, 6)
            .await
            .unwrap();

        let ambiguous = store.find_ambiguous_tool_dispatches().await.unwrap();
        assert_eq!(ambiguous.len(), 1);
        assert_eq!(ambiguous[0].invocation_id, "inv2");
        assert_eq!(ambiguous[0].tool_call_id, "b");
    }

    #[tokio::test]
    async fn invocation_state_upsert_round_trip() {
        let (store, _dir) = open_fresh().await;
        let row = InvocationStateRow {
            invocation_id: "inv-x".to_string(),
            agent_id: "agent-y".to_string(),
            schema_version: 1,
            phase: "awaiting_model".to_string(),
            state_blob: b"{\"phase\":\"awaiting_model\"}".to_vec(),
            iteration: 2,
            started_at: 1_000,
            updated_at: 1_010,
            terminal_at: None,
            workspace_ref: None,
        };
        store.upsert_invocation_state(&row).await.unwrap();
        let back = store.get_invocation_state("inv-x").await.unwrap().unwrap();
        assert_eq!(back, row);

        // Update — same key, different phase + updated_at.
        let mut updated = row.clone();
        updated.phase = "dispatching_tools".to_string();
        updated.iteration = 3;
        updated.updated_at = 1_050;
        store.upsert_invocation_state(&updated).await.unwrap();
        let back2 = store.get_invocation_state("inv-x").await.unwrap().unwrap();
        assert_eq!(back2, updated);
    }

    #[tokio::test]
    async fn find_in_flight_excludes_terminal_rows() {
        let (store, _dir) = open_fresh().await;
        let alive = InvocationStateRow {
            invocation_id: "alive".to_string(),
            agent_id: "a".to_string(),
            schema_version: 1,
            phase: "awaiting_model".to_string(),
            state_blob: vec![],
            iteration: 0,
            started_at: 1,
            updated_at: 1,
            terminal_at: None,
            workspace_ref: None,
        };
        let mut done = alive.clone();
        done.invocation_id = "done".to_string();
        done.phase = "done".to_string();
        done.terminal_at = Some(2);

        store.upsert_invocation_state(&alive).await.unwrap();
        store.upsert_invocation_state(&done).await.unwrap();

        let in_flight = store.find_in_flight_invocations().await.unwrap();
        let ids: Vec<_> = in_flight.iter().map(|r| r.invocation_id.as_str()).collect();
        assert_eq!(ids, vec!["alive"]);
    }

    #[tokio::test]
    async fn delete_invocation_state_removes_row() {
        let (store, _dir) = open_fresh().await;
        let row = InvocationStateRow {
            invocation_id: "to-delete".to_string(),
            agent_id: "a".to_string(),
            schema_version: 1,
            phase: "awaiting_model".to_string(),
            state_blob: vec![],
            iteration: 0,
            started_at: 1,
            updated_at: 1,
            terminal_at: Some(2),
            workspace_ref: None,
        };
        store.upsert_invocation_state(&row).await.unwrap();
        let n = store.delete_invocation_state("to-delete").await.unwrap();
        assert_eq!(n, 1);
        assert!(
            store
                .get_invocation_state("to-delete")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn llm_wal_intent_dispatched_completed_round_trip() {
        let (store, _dir) = open_fresh().await;
        let inv = "inv_llm_1";
        let req = "req_a";

        store
            .write_llm_intent(inv, req, "claude-haiku", r#"{"messages":[]}"#, 100)
            .await
            .unwrap();
        let r = store.get_llm_dispatch(inv, req).await.unwrap().unwrap();
        assert_eq!(r.status, DispatchStatus::Intent);
        assert_eq!(r.model, "claude-haiku");
        assert!(r.dispatched_at.is_none());
        assert!(r.response.is_none());

        store.write_llm_dispatched(inv, req, 200).await.unwrap();
        let r = store.get_llm_dispatch(inv, req).await.unwrap().unwrap();
        assert_eq!(r.status, DispatchStatus::Dispatched);

        store
            .write_llm_completed(inv, req, r#"{"content":"hi"}"#, false, 0.0011, 300)
            .await
            .unwrap();
        let r = store.get_llm_dispatch(inv, req).await.unwrap().unwrap();
        assert_eq!(r.status, DispatchStatus::Completed);
        assert_eq!(r.cost_usd, Some(0.0011));
        assert_eq!(r.is_error, Some(false));
        assert_eq!(r.response.as_deref(), Some(r#"{"content":"hi"}"#));
    }

    #[tokio::test]
    async fn llm_completed_with_error_round_trip() {
        let (store, _dir) = open_fresh().await;
        store
            .write_llm_intent("inv-err", "r-err", "haiku", "{}", 1)
            .await
            .unwrap();
        store
            .write_llm_dispatched("inv-err", "r-err", 2)
            .await
            .unwrap();
        store
            .write_llm_completed("inv-err", "r-err", "rate limited", true, 0.0, 3)
            .await
            .unwrap();
        let r = store
            .get_llm_dispatch("inv-err", "r-err")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.status, DispatchStatus::Completed);
        assert_eq!(r.is_error, Some(true));
        assert_eq!(r.cost_usd, Some(0.0));
    }

    #[tokio::test]
    async fn v1_to_v2_migration_adds_is_error_column() {
        // Build a DB at schema v1 (the worker tables without
        // the `is_error` column on `llm_dispatch`), then open
        // it with the current binary and verify the migration
        // adds the column without disturbing existing rows.
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.db");

        // Manually construct a v1 DB.
        {
            let url = format!("sqlite://{}?mode=rwc", path.display());
            let opts = SqliteConnectOptions::from_str(&url).unwrap();
            let pool = SqlitePoolOptions::new().connect_with(opts).await.unwrap();
            for stmt in split_sql(SCHEMA_META_SQL) {
                sqlx::query(&stmt).execute(&pool).await.unwrap();
            }
            for stmt in split_sql(WORKER_TABLES_V1_SQL) {
                sqlx::query(&stmt).execute(&pool).await.unwrap();
            }
            sqlx::query("INSERT INTO schema_meta (class, version, updated_at) VALUES (?, ?, ?)")
                .bind(SCHEMA_CLASS)
                .bind(1_i64)
                .bind(0_i64)
                .execute(&pool)
                .await
                .unwrap();
            // Pre-existing v1 row to ensure migration preserves data.
            sqlx::query(
                "INSERT INTO llm_dispatch (invocation_id, request_id, model, status, request_payload, intent_at) VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind("legacy-inv")
            .bind("legacy-req")
            .bind("claude-haiku")
            .bind("intent")
            .bind("{}")
            .bind(1_i64)
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }

        // Open with current binary — runs v1 → v2 migration.
        let store = WorkerStore::open(&path).await.expect("migrate v1 -> v2");
        assert_eq!(
            store.read_schema_version().await.unwrap(),
            Some(WORKER_SCHEMA_VERSION)
        );

        // Existing row preserved.
        let pre = store
            .get_llm_dispatch("legacy-inv", "legacy-req")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pre.status, DispatchStatus::Intent);
        assert_eq!(pre.is_error, None);

        // New writes can use the is_error column.
        store
            .write_llm_dispatched("legacy-inv", "legacy-req", 10)
            .await
            .unwrap();
        store
            .write_llm_completed("legacy-inv", "legacy-req", "ok", false, 0.001, 20)
            .await
            .unwrap();
        let post = store
            .get_llm_dispatch("legacy-inv", "legacy-req")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(post.is_error, Some(false));
    }

    #[tokio::test]
    async fn find_ambiguous_llm_returns_only_dispatched() {
        let (store, _dir) = open_fresh().await;

        // intent only — safe-resume.
        store
            .write_llm_intent("inv1", "r1", "haiku", "{}", 1)
            .await
            .unwrap();

        // dispatched without completed — ambiguous.
        store
            .write_llm_intent("inv2", "r2", "haiku", "{}", 2)
            .await
            .unwrap();
        store.write_llm_dispatched("inv2", "r2", 3).await.unwrap();

        // fully completed — safe-replay.
        store
            .write_llm_intent("inv3", "r3", "haiku", "{}", 4)
            .await
            .unwrap();
        store.write_llm_dispatched("inv3", "r3", 5).await.unwrap();
        store
            .write_llm_completed("inv3", "r3", "{}", false, 0.0, 6)
            .await
            .unwrap();

        let ambiguous = store.find_ambiguous_llm_dispatches().await.unwrap();
        assert_eq!(ambiguous.len(), 1);
        assert_eq!(ambiguous[0].invocation_id, "inv2");
        assert_eq!(ambiguous[0].request_id, "r2");
    }

    #[tokio::test]
    async fn v2_to_v3_migration_adds_workspace_ref_column() {
        // Pre-populate a v2 DB (initial tables + the v2
        // is_error column on llm_dispatch, but no workspace_ref
        // on invocation_state). Open with current binary;
        // verify the v3 migration adds workspace_ref without
        // disturbing existing rows.
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.db");

        {
            let url = format!("sqlite://{}?mode=rwc", path.display());
            let opts = SqliteConnectOptions::from_str(&url).unwrap();
            let pool = SqlitePoolOptions::new().connect_with(opts).await.unwrap();
            for stmt in split_sql(SCHEMA_META_SQL) {
                sqlx::query(&stmt).execute(&pool).await.unwrap();
            }
            for stmt in split_sql(WORKER_TABLES_V1_SQL) {
                sqlx::query(&stmt).execute(&pool).await.unwrap();
            }
            for stmt in split_sql(WORKER_MIGRATION_V2_SQL) {
                sqlx::query(&stmt).execute(&pool).await.unwrap();
            }
            sqlx::query("INSERT INTO schema_meta (class, version, updated_at) VALUES (?, ?, ?)")
                .bind(SCHEMA_CLASS)
                .bind(2_i64)
                .bind(0_i64)
                .execute(&pool)
                .await
                .unwrap();
            // Pre-existing v2 row.
            sqlx::query(
                "INSERT INTO invocation_state (invocation_id, agent_id, schema_version, phase, state_blob, iteration, started_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind("legacy-inv")
            .bind("a")
            .bind(1_i64)
            .bind("awaiting_model")
            .bind(b"".as_slice())
            .bind(0_i64)
            .bind(1_i64)
            .bind(1_i64)
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }

        let store = WorkerStore::open(&path).await.expect("migrate v2 -> v3");
        assert_eq!(
            store.read_schema_version().await.unwrap(),
            Some(WORKER_SCHEMA_VERSION)
        );

        // Existing row preserved; workspace_ref reads as None.
        let pre = store
            .get_invocation_state("legacy-inv")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pre.workspace_ref, None);

        // Future writes can populate workspace_ref.
        let mut updated = pre.clone();
        updated.workspace_ref = Some("placeholder-ref".to_string());
        store.upsert_invocation_state(&updated).await.unwrap();
        let post = store
            .get_invocation_state("legacy-inv")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(post.workspace_ref, Some("placeholder-ref".to_string()));
    }

    #[tokio::test]
    async fn open_read_only_refuses_missing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist.db");
        let err = WorkerStore::open_read_only(&path)
            .await
            .expect_err("missing file");
        assert!(matches!(err, WorkerStoreError::NotInitialised(_)));
    }
}
