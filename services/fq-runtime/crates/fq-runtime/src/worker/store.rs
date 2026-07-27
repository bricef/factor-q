//! Worker-side SQLite store: in-flight invocation state and the
//! three-state WAL for tool and LLM dispatches.
//!
//! Per `docs/design/committed/data-architecture.md` §3 and §9.1, this is
//! the worker's source-of-truth for invocations it currently
//! owns. Each row is non-rebuildable from NATS — losing this
//! file means losing in-flight state.
//!
//! This store owns its own SQLite file (`worker.db`, see
//! [`crate::db::RuntimeDbPaths`]) with its own `schema_meta`
//! version row. v1 collapsed all three runtime stores into a
//! single `events.db`; the split (#262) moved each store to its
//! own file with no schema redesign — a leftover v1 file is
//! migrated by [`crate::db::split_legacy_events_db`].
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
//! This module owns the worker's durable state: schema migrations,
//! reducer-state persistence, three-state WAL writes, and recovery queries.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::Utc;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Pool, Row, Sqlite};

/// Schema class name used in the shared `schema_meta` table.
pub const SCHEMA_CLASS: &str = "worker";

/// Schema version this binary expects. Bump on incompatible
/// schema changes; additive migrations between versions belong
/// in `run_migrations`.
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
/// - **v4** — adds `archive_status TEXT NULL` and
///   `archive_published_at INTEGER NULL` to `invocation_state`,
///   tracking the worker → control-plane archive hand-off (step
///   8 of data-architecture-v1). Values: `NULL` (no archive
///   flow yet, pre-terminal); `"pending"` (an
///   `invocation.archived` event has been published and the
///   worker is awaiting the control-plane ack). The retry
///   sweeper uses `archive_published_at` to decide when to
///   republish. On `invocation.archive_acked` the row is
///   deleted outright.
/// - **v5** — adds `trigger_source`/`trigger_subject`/
///   `trigger_payload` to `invocation_state` so resume replays
///   the original invocation input (see the v5 const's doc).
/// - **v6** — renames the `invocation_state.iteration` column to
///   `step_index`. The column always held the reducer *step*
///   counter (every model and tool step), not the model-turn
///   count that `max_iterations` gates; the old name misread as
///   turn-vs-cap progress (issue #109). Pure rename — the value
///   written and every recovery/replay path are unchanged.
/// - **v7** — adds the `host_notice` table (#155): durable host
///   messages injected into the conversation at reducer step
///   boundaries, keyed `(invocation_id, step_index, seq)` so a
///   resume replays them verbatim at the recorded positions.
/// - **v8** — adds `ambiguous_reported_at INTEGER NULL` to
///   `invocation_state` (#64): the once-per-invocation guard for
///   `invocation.ambiguous` emission. Set when the event is first
///   published (recovery scan or failed auto-resume); a restart
///   that re-classifies the same invocation as ambiguous sees the
///   stamp and does not re-fire.
/// - **v9** — adds a nullable per-invocation completion `seq` to both
///   dispatch tables, providing one total replay order across tool and
///   LLM results. Pre-v9 rows remain `NULL` and use timestamp fallback.
pub const WORKER_SCHEMA_VERSION: u32 = 9;

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

/// v4 migration: add `archive_status` and `archive_published_at`
/// to `invocation_state`, plus an index supporting the retry
/// sweeper's "pending and stale" lookup.
const WORKER_MIGRATION_V4_SQL: &str = r#"
ALTER TABLE invocation_state ADD COLUMN archive_status TEXT;
ALTER TABLE invocation_state ADD COLUMN archive_published_at INTEGER;
CREATE INDEX IF NOT EXISTS idx_invocation_state_archive
    ON invocation_state(archive_status, archive_published_at);
"#;

/// v5 migration: persist the trigger on `invocation_state`.
///
/// Found by the slice-4 resume-equivalence property (reducer
/// verification plan): `resume()` passed a null trigger on the
/// grounds that "step 0 is past us", but replay re-runs step 0 —
/// so every resumed invocation re-seeded its conversation with
/// "(no input)" instead of the original request. The trigger is
/// invocation input, and input must survive a crash like
/// everything else in the WAL. Rows written before v5 have NULLs
/// here; resume logs a warning and degrades to the old behaviour
/// for those.
const WORKER_MIGRATION_V5_SQL: &str = r#"
ALTER TABLE invocation_state ADD COLUMN trigger_source TEXT;
ALTER TABLE invocation_state ADD COLUMN trigger_subject TEXT;
ALTER TABLE invocation_state ADD COLUMN trigger_payload TEXT;
"#;

/// v6 migration: rename `invocation_state.iteration` to
/// `step_index` (issue #109). Behaviour-preserving — the column
/// always stored the reducer step counter, never the model-turn
/// count `max_iterations` gates. `ALTER TABLE ... RENAME COLUMN`
/// preserves the data; it is gated on the recorded version so it
/// runs exactly once.
const WORKER_MIGRATION_V6_SQL: &str = r#"
ALTER TABLE invocation_state RENAME COLUMN iteration TO step_index;
"#;

/// v7 migration: durable host notices injected at reducer step boundaries.
const WORKER_MIGRATION_V7_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS host_notice (
    invocation_id TEXT NOT NULL,
    step_index INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    kind TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (invocation_id, step_index, seq)
);
"#;

/// v8 migration: once-per-invocation guard for `invocation.ambiguous`
/// emission (#64). `NULL` until the first publish; stamped with the
/// publish time thereafter so restarts don't re-fire the event.
const WORKER_MIGRATION_V8_SQL: &str = r#"
ALTER TABLE invocation_state ADD COLUMN ambiguous_reported_at INTEGER;
"#;

/// v9 migration: total completion order shared by both WAL tables.
const WORKER_MIGRATION_V9_SQL: &str = r#"
ALTER TABLE tool_dispatch ADD COLUMN seq INTEGER;
ALTER TABLE llm_dispatch ADD COLUMN seq INTEGER;
"#;

/// One durable host notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostNoticeRow {
    pub invocation_id: String,
    pub step_index: u32,
    pub seq: u32,
    pub kind: String,
    pub body: String,
    pub created_at: i64,
}

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
    pub seq: Option<i64>,
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
    pub seq: Option<i64>,
}

/// Minimal fields for an open tool dispatch used by read-model views.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenToolDispatchRow {
    pub tool_name: String,
    /// The dispatch's parameters JSON — carried so read-model views
    /// can surface the command an open exec/shell is running. Open
    /// dispatches are bounded per invocation, so the extra column is
    /// cheap here (unlike the full-history queries).
    pub parameters: String,
    pub intent_at: i64,
    pub dispatched_at: Option<i64>,
}

/// Minimal fields for an open LLM dispatch used by read-model views.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenLlmDispatchRow {
    pub model: String,
    pub intent_at: i64,
    pub dispatched_at: Option<i64>,
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
    /// Reducer step counter: incremented once per reducer
    /// `step()` — every model step *and* every tool step — not
    /// the model-turn count that `max_iterations` gates (that
    /// lives inside `state_blob`). A normal turn is ~2 steps, so
    /// this is roughly `2 × model_turns`. Named `step_index` so it
    /// is not misread as turn-vs-cap progress (issue #109).
    pub step_index: u32,
    pub started_at: i64,
    pub updated_at: i64,
    pub terminal_at: Option<i64>,
    /// Reference to the agent's workspace state at the time
    /// this row was last written. Currently always `None`;
    /// reserved for the future workspace-storage layer
    /// (likely content-addressed).
    pub workspace_ref: Option<String>,
    /// Archive hand-off state. `None` while the invocation is
    /// in flight (no archive yet). `Some("pending")` once the
    /// worker has published `invocation.archived` and is
    /// awaiting the control-plane ack. There is no `acked`
    /// state on disk — receipt of the ack deletes the row.
    pub archive_status: Option<String>,
    /// When the most recent `invocation.archived` event was
    /// published, in unix ms. Used by the retry sweeper to
    /// decide when to republish.
    pub archive_published_at: Option<i64>,
    /// The trigger that started this invocation (v5): source kind
    /// (`manual` / `subject` / `schedule`), optional subject, and
    /// the payload as JSON text. Resume replays step 0, which
    /// re-seeds the conversation from the trigger — so the trigger
    /// must survive a crash like every other input. `None` on rows
    /// written before v5.
    pub trigger_source: Option<String>,
    pub trigger_subject: Option<String>,
    pub trigger_payload: Option<String>,
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
        Self::open_with_pool(path, 4).await
    }

    /// Open with an explicit connection-pool ceiling. The daemon sizes
    /// this from `worker.max_concurrent_invocations` plus headroom for
    /// the sweepers (#70) — under WAL, SQLite still serialises the
    /// actual writes, so the pool bounds *waiting* connections, not
    /// write parallelism.
    pub async fn open_with_pool(
        path: &Path,
        max_connections: u32,
    ) -> Result<Self, WorkerStoreError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(WorkerStoreError::CreateDir)?;
        }

        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            // Explicit (this is also sqlx's default): a writer blocked
            // on the WAL write lock waits up to this long before
            // surfacing SQLITE_BUSY, so concurrent invocations contend
            // on latency rather than erroring.
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
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
        const MIGRATIONS: &[(u32, &str)] = &[
            (1, WORKER_TABLES_V1_SQL),
            (2, WORKER_MIGRATION_V2_SQL),
            (3, WORKER_MIGRATION_V3_SQL),
            (4, WORKER_MIGRATION_V4_SQL),
            (5, WORKER_MIGRATION_V5_SQL),
            (6, WORKER_MIGRATION_V6_SQL),
            (7, WORKER_MIGRATION_V7_SQL),
            (8, WORKER_MIGRATION_V8_SQL),
            (9, WORKER_MIGRATION_V9_SQL),
        ];
        for &(version, sql) in MIGRATIONS {
            if from < version && to >= version {
                for stmt in split_sql(sql) {
                    sqlx::query(&stmt).execute(&self.pool).await?;
                }
            }
        }
        // Future migrations: add a `(version, SQL)` row above.
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
            SET status = ?, result = ?, is_error = ?, completed_at = ?,
                seq = 1 + MAX(
                    COALESCE((SELECT MAX(seq) FROM tool_dispatch WHERE invocation_id = ?), 0),
                    COALESCE((SELECT MAX(seq) FROM llm_dispatch WHERE invocation_id = ?), 0)
                )
            WHERE invocation_id = ? AND tool_call_id = ? AND status = ?
            "#,
        )
        .bind(DispatchStatus::Completed.as_str())
        .bind(result)
        .bind(is_error as i64)
        .bind(completed_at)
        .bind(invocation_id)
        .bind(invocation_id)
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

    /// Complete every dispatch left ambiguous by a crashed host for one invocation.
    /// The synthetic payload is rendered from each row's persisted dispatch time,
    /// never from the live clock. Returns the completed call ids.
    pub async fn inject_interrupted_results(
        &self,
        invocation_id: &str,
    ) -> Result<Vec<String>, WorkerStoreError> {
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query(
            "SELECT tool_call_id, dispatched_at FROM tool_dispatch \
             WHERE invocation_id = ? AND status = 'dispatched' ORDER BY dispatched_at, tool_call_id",
        )
        .bind(invocation_id)
        .fetch_all(&mut *tx)
        .await?;
        let mut ids = Vec::with_capacity(rows.len());
        for row in rows {
            let call_id: String = row.try_get("tool_call_id")?;
            let dispatched_at: i64 = row.try_get("dispatched_at")?;
            let payload = interrupted_result_payload(dispatched_at);
            sqlx::query(
                "UPDATE tool_dispatch SET status = 'completed', result = ?, is_error = 1, \
                 completed_at = dispatched_at, seq = 1 + MAX(\
                   COALESCE((SELECT MAX(seq) FROM tool_dispatch WHERE invocation_id = ?), 0), \
                   COALESCE((SELECT MAX(seq) FROM llm_dispatch WHERE invocation_id = ?), 0)) \
                 WHERE invocation_id = ? AND tool_call_id = ? AND status = 'dispatched'",
            )
            .bind(payload)
            .bind(invocation_id)
            .bind(invocation_id)
            .bind(invocation_id)
            .bind(&call_id)
            .execute(&mut *tx)
            .await?;
            ids.push(call_id);
        }
        tx.commit().await?;
        Ok(ids)
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
                   result, is_error, intent_at, dispatched_at, completed_at, seq
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
                   result, is_error, intent_at, dispatched_at, completed_at, seq
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
            SET status = ?, response = ?, is_error = ?, cost_usd = ?, completed_at = ?,
                seq = 1 + MAX(
                    COALESCE((SELECT MAX(seq) FROM tool_dispatch WHERE invocation_id = ?), 0),
                    COALESCE((SELECT MAX(seq) FROM llm_dispatch WHERE invocation_id = ?), 0)
                )
            WHERE invocation_id = ? AND request_id = ? AND status = ?
            "#,
        )
        .bind(DispatchStatus::Completed.as_str())
        .bind(response)
        .bind(is_error as i64)
        .bind(cost_usd)
        .bind(completed_at)
        .bind(invocation_id)
        .bind(invocation_id)
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
                   response, cost_usd, is_error, intent_at, dispatched_at, completed_at, seq
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
                   response, cost_usd, is_error, intent_at, dispatched_at, completed_at, seq
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
    // Host-notice WAL operations.
    // -----------------------------------------------------------

    pub async fn insert_host_notice(
        &self,
        invocation_id: &str,
        step_index: u32,
        seq: u32,
        kind: &str,
        body: &str,
        created_at: i64,
    ) -> Result<(), WorkerStoreError> {
        sqlx::query("INSERT INTO host_notice (invocation_id, step_index, seq, kind, body, created_at) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(invocation_id).bind(step_index as i64).bind(seq as i64).bind(kind).bind(body)
            .bind(created_at).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn list_host_notices(
        &self,
        invocation_id: &str,
    ) -> Result<Vec<HostNoticeRow>, WorkerStoreError> {
        let rows = sqlx::query("SELECT invocation_id, step_index, seq, kind, body, created_at FROM host_notice WHERE invocation_id = ? ORDER BY step_index, seq")
            .bind(invocation_id).fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|r| HostNoticeRow {
                invocation_id: r.get("invocation_id"),
                step_index: r.get::<i64, _>("step_index") as u32,
                seq: r.get::<i64, _>("seq") as u32,
                kind: r.get("kind"),
                body: r.get("body"),
                created_at: r.get("created_at"),
            })
            .collect())
    }

    // -----------------------------------------------------------
    // Invocation-state operations.
    // -----------------------------------------------------------

    /// Insert or update an invocation's persisted state.
    ///
    /// Logs a warning at [`STATE_BLOB_WARN_THRESHOLD_BYTES`] —
    /// useful telemetry on whether the inline-in-SQLite
    /// assumption is holding for the operator's workload.
    ///
    /// Does **not** write `archive_status` /
    /// `archive_published_at` — those are owned by
    /// [`Self::set_archive_pending`] and preserved across
    /// upserts. The fields on `row` are ignored.
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
                 step_index, started_at, updated_at, terminal_at, workspace_ref,
                 trigger_source, trigger_subject, trigger_payload)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(invocation_id) DO UPDATE SET
                phase = excluded.phase,
                state_blob = excluded.state_blob,
                step_index = excluded.step_index,
                updated_at = excluded.updated_at,
                terminal_at = excluded.terminal_at,
                workspace_ref = excluded.workspace_ref,
                trigger_source = excluded.trigger_source,
                trigger_subject = excluded.trigger_subject,
                trigger_payload = excluded.trigger_payload
            WHERE invocation_state.terminal_at IS NULL
            "#,
        )
        .bind(&row.invocation_id)
        .bind(&row.agent_id)
        .bind(row.schema_version as i64)
        .bind(&row.phase)
        .bind(&row.state_blob)
        .bind(row.step_index as i64)
        .bind(row.started_at)
        .bind(row.updated_at)
        .bind(row.terminal_at)
        .bind(&row.workspace_ref)
        .bind(&row.trigger_source)
        .bind(&row.trigger_subject)
        .bind(&row.trigger_payload)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark an invocation terminal after an authoritative operator transition.
    /// The conditional update is idempotent and never overwrites a terminal
    /// worker outcome; paired with the guarded upsert above, it also prevents
    /// a late live-worker write from resurrecting the invocation.
    pub async fn mark_invocation_operator_terminal(
        &self,
        invocation_id: &str,
        phase: &str,
        terminal_at: i64,
    ) -> Result<u64, WorkerStoreError> {
        let res = sqlx::query(
            r#"
            UPDATE invocation_state
            SET phase = ?, updated_at = ?, terminal_at = ?
            WHERE invocation_id = ? AND terminal_at IS NULL
            "#,
        )
        .bind(phase)
        .bind(terminal_at)
        .bind(terminal_at)
        .bind(invocation_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Mark a terminal invocation as awaiting archive ack.
    /// Called after the worker publishes `invocation.archived`;
    /// the retry sweeper uses `archive_published_at` to decide
    /// when to republish, and the ack consumer deletes the row
    /// outright on receipt of `invocation.archive_acked`.
    ///
    /// Idempotent: re-calling on a pending row simply bumps
    /// `archive_published_at`, which is what the sweeper wants
    /// when republishing.
    pub async fn set_archive_pending(
        &self,
        invocation_id: &str,
        published_at: i64,
    ) -> Result<u64, WorkerStoreError> {
        let res = sqlx::query(
            r#"
            UPDATE invocation_state
            SET archive_status = 'pending',
                archive_published_at = ?
            WHERE invocation_id = ?
              AND terminal_at IS NOT NULL
            "#,
        )
        .bind(published_at)
        .bind(invocation_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Claim the one-shot right to publish `invocation.ambiguous`
    /// for this invocation (#64). Conditional on the stamp being
    /// unset, so exactly one caller across all restarts wins:
    /// `true` means "you claimed it — publish"; `false` means the
    /// event was already reported (or the row no longer exists,
    /// i.e. the invocation is not in recovery limbo anymore).
    pub async fn mark_ambiguous_reported(
        &self,
        invocation_id: &str,
        now_ms: i64,
    ) -> Result<bool, WorkerStoreError> {
        let res = sqlx::query(
            r#"
            UPDATE invocation_state
            SET ambiguous_reported_at = ?
            WHERE invocation_id = ?
              AND ambiguous_reported_at IS NULL
            "#,
        )
        .bind(now_ms)
        .bind(invocation_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    /// All rows in archive-flow: terminal but the
    /// control-plane has not yet acked. Returned in
    /// `archive_published_at`-ascending order so the retry
    /// sweeper sees the oldest pending hand-offs first.
    /// `archive_published_at IS NULL` rows are included and
    /// sort first (terminal but the publish step has not yet
    /// run — typically a transient sliver, but the sweeper
    /// republishes them too so the flow is self-healing).
    pub async fn list_archive_pending(&self) -> Result<Vec<InvocationStateRow>, WorkerStoreError> {
        let rows = sqlx::query(
            r#"
            SELECT invocation_id, agent_id, schema_version, phase, state_blob,
                   step_index, started_at, updated_at, terminal_at, workspace_ref,
                   archive_status, archive_published_at,
                   trigger_source, trigger_subject, trigger_payload
            FROM invocation_state
            WHERE terminal_at IS NOT NULL
              AND (archive_status IS NULL OR archive_status = 'pending')
            ORDER BY archive_published_at IS NULL DESC, archive_published_at ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_invocation_state).collect()
    }

    /// Fetch one invocation's persisted state by id.
    pub async fn get_invocation_state(
        &self,
        invocation_id: &str,
    ) -> Result<Option<InvocationStateRow>, WorkerStoreError> {
        let row = sqlx::query(
            r#"
            SELECT invocation_id, agent_id, schema_version, phase, state_blob,
                   step_index, started_at, updated_at, terminal_at, workspace_ref,
                   archive_status, archive_published_at,
                   trigger_source, trigger_subject, trigger_payload
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
                   step_index, started_at, updated_at, terminal_at, workspace_ref,
                   archive_status, archive_published_at,
                   trigger_source, trigger_subject, trigger_payload
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
                   result, is_error, intent_at, dispatched_at, completed_at, seq
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

    /// Open tool dispatches with only the fields needed by read-model views.
    pub async fn open_tool_dispatches_for_invocation(
        &self,
        invocation_id: &str,
    ) -> Result<Vec<OpenToolDispatchRow>, WorkerStoreError> {
        let rows = sqlx::query(
            "SELECT tool_name, parameters, intent_at, dispatched_at FROM tool_dispatch \
             WHERE invocation_id = ? AND status != 'completed' ORDER BY intent_at",
        )
        .bind(invocation_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(OpenToolDispatchRow {
                    tool_name: row.try_get("tool_name")?,
                    parameters: row.try_get("parameters")?,
                    intent_at: row.try_get("intent_at")?,
                    dispatched_at: row.try_get("dispatched_at")?,
                })
            })
            .collect()
    }

    /// Open LLM dispatches with only the fields needed by read-model views.
    pub async fn open_llm_dispatches_for_invocation(
        &self,
        invocation_id: &str,
    ) -> Result<Vec<OpenLlmDispatchRow>, WorkerStoreError> {
        let rows = sqlx::query(
            "SELECT model, intent_at, dispatched_at FROM llm_dispatch \
             WHERE invocation_id = ? AND status != 'completed' ORDER BY intent_at",
        )
        .bind(invocation_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(OpenLlmDispatchRow {
                    model: row.try_get("model")?,
                    intent_at: row.try_get("intent_at")?,
                    dispatched_at: row.try_get("dispatched_at")?,
                })
            })
            .collect()
    }

    /// Symmetric to [`Self::list_tool_dispatches_for_invocation`] for
    /// the LLM dispatch table.
    pub async fn list_llm_dispatches_for_invocation(
        &self,
        invocation_id: &str,
    ) -> Result<Vec<LlmDispatchRow>, WorkerStoreError> {
        let rows = sqlx::query(
            r#"
            SELECT invocation_id, request_id, model, status, request_payload,
                   response, cost_usd, is_error, intent_at, dispatched_at, completed_at, seq
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

/// Render the synthetic result for one interrupted tool dispatch (#373).
/// The timestamp is the row's persisted `dispatched_at` — never the live
/// clock — so the payload is rendered once and every later replay
/// reproduces it byte-identically (the PR #143 clock bug is the
/// cautionary tale).
fn interrupted_result_payload(dispatched_at_ms: i64) -> String {
    let rendered_at = chrono::DateTime::from_timestamp_millis(dispatched_at_ms)
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| dispatched_at_ms.to_string());
    serde_json::json!({
        "interrupted": true,
        "notice": format!("HOST NOTICE: this tool call was interrupted by a runtime crash after being dispatched at {rendered_at}. Whether it executed — fully, partially, or not at all — is unknown. Verify the relevant state (files, git, external services) before building on anything; re-run it only if you have confirmed it did not take effect.")
    })
    .to_string()
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
        seq: row.get("seq"),
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
        seq: row.get("seq"),
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
        step_index: row.get::<i64, _>("step_index") as u32,
        started_at: row.get("started_at"),
        updated_at: row.get("updated_at"),
        terminal_at: row.get("terminal_at"),
        workspace_ref: row.get("workspace_ref"),
        archive_status: row.get("archive_status"),
        archive_published_at: row.get("archive_published_at"),
        trigger_source: row.get("trigger_source"),
        trigger_subject: row.get("trigger_subject"),
        trigger_payload: row.get("trigger_payload"),
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
mod tests;
