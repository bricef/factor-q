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

    /// Guard for the parallel-workers concurrency invariant (H4):
    /// concurrent invocations may interleave WAL writes only because
    /// every row is keyed by `invocation_id`. Tables are enumerated
    /// from `sqlite_master`, not hardcoded, so a *new* table added
    /// without either an invocation-id-led PK or an explicit exemption
    /// below fails here loudly before it can cross-contaminate
    /// invocations.
    #[tokio::test]
    async fn wal_tables_are_keyed_by_invocation_id() {
        // Tables that hold no per-invocation rows. Adding a table here
        // is an explicit classification decision — the point of the
        // test is that it cannot happen by omission.
        const NOT_PER_INVOCATION: &[&str] = &["schema_meta"];

        let dir = tempdir().unwrap();
        let store = WorkerStore::open(&dir.path().join("keyed.db"))
            .await
            .unwrap();
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        )
        .fetch_all(&store.pool)
        .await
        .unwrap();
        assert!(
            tables.len() > NOT_PER_INVOCATION.len(),
            "expected per-invocation tables beyond the exemptions; got {tables:?}"
        );

        for table in tables {
            if NOT_PER_INVOCATION.contains(&table.as_str()) {
                continue;
            }
            let columns: Vec<(String, i64)> = sqlx::query_as(&format!(
                "SELECT name, pk FROM pragma_table_info('{table}')"
            ))
            .fetch_all(&store.pool)
            .await
            .unwrap();
            let first_pk = columns
                .iter()
                .find(|(_, pk)| *pk == 1)
                .unwrap_or_else(|| panic!("{table} has no primary key"));
            assert_eq!(
                first_pk.0, "invocation_id",
                "{table}'s primary key must lead with invocation_id \
                 (or be explicitly exempted as not per-invocation)"
            );
        }
    }

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
        let path = dir.path().join("worker.db");
        let store = WorkerStore::open(&path).await.expect("open fresh");
        (store, dir)
    }

    /// A worker database down-projected to one historical schema version,
    /// populated from a HEAD SimWorld run with all three WAL shapes #44
    /// names (completed, crashed mid-flight, budget-failed).
    struct PopulatedDb {
        _dir: tempfile::TempDir,
        path: std::path::PathBuf,
        fixture: crate::test_support::sim::MigrationFixture,
        /// Rows projected per table, so the ladder test can assert
        /// nothing the old schema could hold is lost.
        source_counts: Vec<(&'static str, i64)>,
    }

    /// Copy the SimWorld-generated HEAD rows into the columns present at `version`.
    /// This is deliberately mechanical: the migration history remains the schema fixture.
    /// The meta bootstrap below mirrors `open()`'s fresh-install path
    /// (`SCHEMA_META_SQL`, then the ladder) — keep the two in sync.
    async fn populated_db_at(version: u32) -> PopulatedDb {
        use crate::test_support::sim::{
            MIGRATION_FIXTURE_BUDGET, SimWorld, migration_fixture_pricing,
        };

        let world = SimWorld::with_pricing(
            44 + version as u64,
            MIGRATION_FIXTURE_BUDGET,
            migration_fixture_pricing(),
        )
        .await;
        let fixture = world.populate_for_migration_test().await;
        let source = world.worker_db_path();

        let dir = tempdir().unwrap();
        let path = dir.path().join("worker.db");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        let store = WorkerStore { pool };
        for stmt in split_sql(SCHEMA_META_SQL) {
            sqlx::query(&stmt).execute(&store.pool).await.unwrap();
        }
        store.run_migrations(0, version).await.unwrap();
        store.write_schema_version(version).await.unwrap();
        sqlx::query("ATTACH DATABASE ? AS sim")
            .bind(source.to_string_lossy().as_ref())
            .execute(&store.pool)
            .await
            .unwrap();

        let mut source_counts: Vec<(&'static str, i64)> = Vec::new();
        for table in [
            "invocation_state",
            "tool_dispatch",
            "llm_dispatch",
            "host_notice",
        ] {
            let exists: Option<i64> = sqlx::query_scalar(
                "SELECT 1 FROM main.sqlite_master WHERE type='table' AND name=?",
            )
            .bind(table)
            .fetch_optional(&store.pool)
            .await
            .unwrap();
            if exists.is_none() {
                continue;
            }
            let old: Vec<String> =
                sqlx::query_scalar(&format!("SELECT name FROM pragma_table_info('{table}')"))
                    .fetch_all(&store.pool)
                    .await
                    .unwrap();
            let current: Vec<String> = sqlx::query_scalar(&format!(
                "SELECT name FROM pragma_table_info('{table}', 'sim')"
            ))
            .fetch_all(&store.pool)
            .await
            .unwrap();
            let common: Vec<&str> = old
                .iter()
                .map(String::as_str)
                .filter(|c| current.iter().any(|n| n == c))
                .collect();
            if !common.is_empty() {
                let columns = common.join(", ");
                sqlx::query(&format!(
                    "INSERT INTO main.{table} ({columns}) SELECT {columns} FROM sim.{table}"
                ))
                .execute(&store.pool)
                .await
                .unwrap();
                let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM main.{table}"))
                    .fetch_one(&store.pool)
                    .await
                    .unwrap();
                source_counts.push((table, count));
            }
        }
        // Preserve the renamed counter when projecting HEAD back before v6.
        if version < 6 {
            sqlx::query("UPDATE invocation_state SET iteration = (SELECT step_index FROM sim.invocation_state WHERE sim.invocation_state.invocation_id = invocation_state.invocation_id)")
                .execute(&store.pool).await.unwrap();
        }
        sqlx::query("DETACH DATABASE sim")
            .execute(&store.pool)
            .await
            .unwrap();
        store.pool.close().await;
        PopulatedDb {
            _dir: dir,
            path,
            fixture,
            source_counts,
        }
    }

    #[tokio::test]
    async fn every_worker_migration_upgrades_populated_sim_data() {
        for from in 1..WORKER_SCHEMA_VERSION {
            let db = populated_db_at(from).await;
            let store = WorkerStore::open(&db.path)
                .await
                .unwrap_or_else(|e| panic!("v{from} migration failed: {e}"));
            assert_eq!(
                store.read_schema_version().await.unwrap(),
                Some(WORKER_SCHEMA_VERSION)
            );

            // Row preservation: nothing the old schema could hold is lost.
            for (table, expected) in &db.source_counts {
                let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
                    .fetch_one(&store.pool)
                    .await
                    .unwrap();
                assert_eq!(count, *expected, "v{from} lost rows in {table}");
            }

            // Every WAL shape stays readable through the current readers:
            // terminal state for the completed and budget-failed rows,
            // recoverable in-flight state (with its finished LLM span)
            // for the crashed row, and the completed row's tool span.
            let ids = &db.fixture;
            for id in [&ids.completed, &ids.crashed, &ids.budget_failed] {
                assert!(
                    store.get_invocation_state(id).await.unwrap().is_some(),
                    "v{from}: state row {id} unreadable after migration"
                );
            }
            let in_flight: Vec<String> = store
                .find_in_flight_invocations()
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.invocation_id)
                .collect();
            assert!(
                in_flight.contains(&ids.crashed),
                "v{from}: crashed row no longer recoverable"
            );
            assert!(
                !in_flight.contains(&ids.completed),
                "v{from}: completed row regressed to in-flight"
            );
            assert!(
                !in_flight.contains(&ids.budget_failed),
                "v{from}: budget-failed row regressed to in-flight"
            );
            assert!(
                !store
                    .list_tool_dispatches_for_invocation(&ids.completed)
                    .await
                    .unwrap()
                    .is_empty(),
                "v{from}: completed tool WAL unreadable"
            );
            assert!(
                !store
                    .list_llm_dispatches_for_invocation(&ids.crashed)
                    .await
                    .unwrap()
                    .is_empty(),
                "v{from}: crashed LLM WAL unreadable"
            );

            let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
                .fetch_one(&store.pool)
                .await
                .unwrap();
            assert_eq!(integrity, "ok", "v{from} integrity check");
            let fk_violations = sqlx::query("PRAGMA foreign_key_check")
                .fetch_all(&store.pool)
                .await
                .unwrap();
            assert!(
                fk_violations.is_empty(),
                "v{from}: {} foreign key violation(s)",
                fk_violations.len()
            );
        }
    }

    #[tokio::test]
    async fn full_worker_ladder_preserves_populated_database() {
        // At v0 none of the worker tables exist yet, so "populated" for
        // the 0→current ladder means a foreign table the migrations must
        // leave untouched.
        let legacy_value = "pre-ladder value";
        let dir = tempdir().unwrap();
        let path = dir.path().join("worker.db");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE legacy_data (value TEXT NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO legacy_data VALUES (?)")
            .bind(legacy_value)
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        let store = WorkerStore::open(&path).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT value FROM legacy_data")
                .fetch_one(&store.pool)
                .await
                .unwrap(),
            legacy_value
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("PRAGMA integrity_check")
                .fetch_one(&store.pool)
                .await
                .unwrap(),
            "ok"
        );
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
        let path = dir.path().join("worker.db");

        let _ = WorkerStore::open(&path).await.expect("first open");
        // Second open should not fail and should not re-run migrations.
        let store = WorkerStore::open(&path).await.expect("second open");
        let v = store.read_schema_version().await.unwrap();
        assert_eq!(v, Some(WORKER_SCHEMA_VERSION));
    }

    #[tokio::test]
    async fn open_refuses_when_db_version_higher_than_binary() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("worker.db");

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
        let path = dir.path().join("worker.db");

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
            step_index: 2,
            started_at: 1_000,
            updated_at: 1_010,
            terminal_at: None,
            workspace_ref: None,
            archive_status: None,
            archive_published_at: None,
            trigger_source: Some("subject".to_string()),
            trigger_subject: Some("fq.agent.agent-y.trigger".to_string()),
            trigger_payload: Some("{\"ask\":\"review the docs\"}".to_string()),
        };
        store.upsert_invocation_state(&row).await.unwrap();
        let back = store.get_invocation_state("inv-x").await.unwrap().unwrap();
        assert_eq!(back, row);

        // Update — same key, different phase + updated_at.
        let mut updated = row.clone();
        updated.phase = "dispatching_tools".to_string();
        updated.step_index = 3;
        updated.updated_at = 1_050;
        store.upsert_invocation_state(&updated).await.unwrap();
        let back2 = store.get_invocation_state("inv-x").await.unwrap().unwrap();
        assert_eq!(back2, updated);
    }

    #[tokio::test]
    async fn mark_ambiguous_reported_claims_exactly_once() {
        let (store, _dir) = open_fresh().await;
        let row = InvocationStateRow {
            invocation_id: "inv-amb".to_string(),
            agent_id: "agent-y".to_string(),
            schema_version: 1,
            phase: "awaiting_model".to_string(),
            state_blob: b"{}".to_vec(),
            step_index: 0,
            started_at: 1_000,
            updated_at: 1_000,
            terminal_at: None,
            workspace_ref: None,
            archive_status: None,
            archive_published_at: None,
            trigger_source: None,
            trigger_subject: None,
            trigger_payload: None,
        };
        store.upsert_invocation_state(&row).await.unwrap();

        // First claim wins; the second (a restart re-classifying the
        // same invocation) must not re-fire.
        assert!(
            store
                .mark_ambiguous_reported("inv-amb", 2_000)
                .await
                .unwrap()
        );
        assert!(
            !store
                .mark_ambiguous_reported("inv-amb", 3_000)
                .await
                .unwrap()
        );

        // No row → nothing in recovery limbo → no claim.
        assert!(
            !store
                .mark_ambiguous_reported("inv-gone", 2_000)
                .await
                .unwrap()
        );
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
            step_index: 0,
            started_at: 1,
            updated_at: 1,
            terminal_at: Some(2),
            workspace_ref: None,
            archive_status: None,
            archive_published_at: None,
            trigger_source: None,
            trigger_subject: None,
            trigger_payload: None,
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

    fn terminal_state_row(id: &str, terminal_at_ms: i64) -> InvocationStateRow {
        InvocationStateRow {
            invocation_id: id.to_string(),
            agent_id: "a".to_string(),
            schema_version: 1,
            phase: "completed".to_string(),
            state_blob: vec![],
            step_index: 0,
            started_at: 1,
            updated_at: terminal_at_ms,
            terminal_at: Some(terminal_at_ms),
            workspace_ref: None,
            archive_status: None,
            archive_published_at: None,
            trigger_source: None,
            trigger_subject: None,
            trigger_payload: None,
        }
    }

    #[tokio::test]
    async fn set_archive_pending_marks_terminal_row_pending() {
        let (store, _dir) = open_fresh().await;
        store
            .upsert_invocation_state(&terminal_state_row("inv-1", 100))
            .await
            .unwrap();

        let updated = store.set_archive_pending("inv-1", 200).await.unwrap();
        assert_eq!(updated, 1);

        let back = store.get_invocation_state("inv-1").await.unwrap().unwrap();
        assert_eq!(back.archive_status.as_deref(), Some("pending"));
        assert_eq!(back.archive_published_at, Some(200));
    }

    #[tokio::test]
    async fn set_archive_pending_no_op_on_non_terminal_row() {
        // Guards the `terminal_at IS NOT NULL` WHERE clause: an
        // archive flow only makes sense after terminal. If a
        // non-terminal row somehow reaches this path it must
        // not be marked pending.
        let (store, _dir) = open_fresh().await;
        let mut row = terminal_state_row("inv-still-going", 100);
        row.terminal_at = None;
        row.phase = "awaiting_model".to_string();
        store.upsert_invocation_state(&row).await.unwrap();

        let updated = store
            .set_archive_pending("inv-still-going", 200)
            .await
            .unwrap();
        assert_eq!(updated, 0);

        let back = store
            .get_invocation_state("inv-still-going")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(back.archive_status, None);
        assert_eq!(back.archive_published_at, None);
    }

    #[tokio::test]
    async fn set_archive_pending_bumps_published_at_on_retry() {
        // Re-calling is the retry sweeper's primary action; it
        // should leave the row pending and bump the published-at
        // so the next retry-window check measures from now.
        let (store, _dir) = open_fresh().await;
        store
            .upsert_invocation_state(&terminal_state_row("inv-1", 100))
            .await
            .unwrap();

        store.set_archive_pending("inv-1", 200).await.unwrap();
        store.set_archive_pending("inv-1", 250).await.unwrap();

        let back = store.get_invocation_state("inv-1").await.unwrap().unwrap();
        assert_eq!(back.archive_status.as_deref(), Some("pending"));
        assert_eq!(back.archive_published_at, Some(250));
    }

    #[tokio::test]
    async fn list_archive_pending_returns_terminal_rows_in_published_order() {
        let (store, _dir) = open_fresh().await;

        // One in-flight row — must not appear.
        let mut alive = terminal_state_row("alive", 100);
        alive.terminal_at = None;
        alive.phase = "awaiting_model".to_string();
        store.upsert_invocation_state(&alive).await.unwrap();

        // Two terminal-pending rows with different publish times.
        store
            .upsert_invocation_state(&terminal_state_row("older", 100))
            .await
            .unwrap();
        store
            .upsert_invocation_state(&terminal_state_row("newer", 100))
            .await
            .unwrap();
        store.set_archive_pending("newer", 250).await.unwrap();
        store.set_archive_pending("older", 200).await.unwrap();

        // One terminal row that has not yet been published — the
        // transient "terminal but pre-publish" sliver. The
        // sweeper should see it and republish, so it ranks
        // before pending rows.
        store
            .upsert_invocation_state(&terminal_state_row("no-publish-yet", 100))
            .await
            .unwrap();

        let pending = store.list_archive_pending().await.unwrap();
        let ids: Vec<_> = pending.iter().map(|r| r.invocation_id.as_str()).collect();
        assert_eq!(ids, vec!["no-publish-yet", "older", "newer"]);
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
        let path = dir.path().join("worker.db");

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
        let path = dir.path().join("worker.db");

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
    async fn v3_to_v4_migration_adds_archive_columns() {
        // Pre-populate a v3 DB (initial tables + v2 is_error
        // + v3 workspace_ref, but no archive_status /
        // archive_published_at). Open with current binary;
        // verify the v4 migration adds the archive columns
        // without disturbing existing rows.
        let dir = tempdir().unwrap();
        let path = dir.path().join("worker.db");

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
            for stmt in split_sql(WORKER_MIGRATION_V3_SQL) {
                sqlx::query(&stmt).execute(&pool).await.unwrap();
            }
            sqlx::query("INSERT INTO schema_meta (class, version, updated_at) VALUES (?, ?, ?)")
                .bind(SCHEMA_CLASS)
                .bind(3_i64)
                .bind(0_i64)
                .execute(&pool)
                .await
                .unwrap();
            // Pre-existing v3 terminal row.
            sqlx::query(
                "INSERT INTO invocation_state (invocation_id, agent_id, schema_version, phase, state_blob, iteration, started_at, updated_at, terminal_at, workspace_ref) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind("legacy-terminal")
            .bind("a")
            .bind(1_i64)
            .bind("completed")
            .bind(b"".as_slice())
            .bind(0_i64)
            .bind(1_i64)
            .bind(2_i64)
            .bind(2_i64)
            .bind::<Option<String>>(None)
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }

        let store = WorkerStore::open(&path).await.expect("migrate v3 -> v4");
        assert_eq!(
            store.read_schema_version().await.unwrap(),
            Some(WORKER_SCHEMA_VERSION)
        );

        // Existing terminal row preserved; archive columns
        // read as None — pre-existing rows weren't part of the
        // hand-off flow and stay that way.
        let pre = store
            .get_invocation_state("legacy-terminal")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pre.archive_status, None);
        assert_eq!(pre.archive_published_at, None);

        // The new write path can flip the legacy terminal row
        // into archive-pending, exercising the migrated
        // columns.
        store
            .set_archive_pending("legacy-terminal", 999)
            .await
            .unwrap();
        let post = store
            .get_invocation_state("legacy-terminal")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(post.archive_status.as_deref(), Some("pending"));
        assert_eq!(post.archive_published_at, Some(999));
    }

    #[tokio::test]
    async fn v6_to_v7_migration_adds_host_notice_table() {
        // Pre-populate a v6 DB (initial tables + every migration
        // through v6, no host_notice table). Open with the current
        // binary; verify the v7 migration creates the table without
        // disturbing existing rows.
        let dir = tempdir().unwrap();
        let path = dir.path().join("worker.db");

        {
            let url = format!("sqlite://{}?mode=rwc", path.display());
            let opts = SqliteConnectOptions::from_str(&url).unwrap();
            let pool = SqlitePoolOptions::new().connect_with(opts).await.unwrap();
            for sql in [
                SCHEMA_META_SQL,
                WORKER_TABLES_V1_SQL,
                WORKER_MIGRATION_V2_SQL,
                WORKER_MIGRATION_V3_SQL,
                WORKER_MIGRATION_V4_SQL,
                WORKER_MIGRATION_V5_SQL,
                WORKER_MIGRATION_V6_SQL,
            ] {
                for stmt in split_sql(sql) {
                    sqlx::query(&stmt).execute(&pool).await.unwrap();
                }
            }
            sqlx::query("INSERT INTO schema_meta (class, version, updated_at) VALUES (?, ?, ?)")
                .bind(SCHEMA_CLASS)
                .bind(6_i64)
                .bind(0_i64)
                .execute(&pool)
                .await
                .unwrap();
            // Pre-existing v6 row (post-rename: `step_index`).
            sqlx::query(
                "INSERT INTO invocation_state (invocation_id, agent_id, schema_version, phase, state_blob, step_index, started_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
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

        let store = WorkerStore::open(&path).await.expect("migrate v6 -> v7");
        assert_eq!(
            store.read_schema_version().await.unwrap(),
            Some(WORKER_SCHEMA_VERSION)
        );

        // Existing row preserved.
        assert!(
            store
                .get_invocation_state("legacy-inv")
                .await
                .unwrap()
                .is_some()
        );

        // The migrated table serves the new write path.
        store
            .insert_host_notice(
                "legacy-inv",
                0,
                0,
                "resume",
                "<host-notice>hello</host-notice>",
                5,
            )
            .await
            .unwrap();
        let rows = store.list_host_notices("legacy-inv").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].body, "<host-notice>hello</host-notice>");
    }

    /// Insert/list round-trip: rows come back ordered by
    /// `(step_index, seq)` regardless of insertion order, with every
    /// column intact — the order a replay re-injects them in.
    #[tokio::test]
    async fn host_notice_round_trip_orders_by_step_then_seq() {
        let dir = tempdir().unwrap();
        let store = WorkerStore::open(&dir.path().join("worker.db"))
            .await
            .unwrap();

        // Deliberately inserted out of order.
        for (step, seq, kind, body) in [
            (
                3_u32,
                1_u32,
                "context_pressure",
                "<host-notice>c</host-notice>",
            ),
            (0, 0, "resume", "<host-notice>a</host-notice>"),
            (3, 0, "tools_changed", "<host-notice>b</host-notice>"),
        ] {
            store
                .insert_host_notice("inv-1", step, seq, kind, body, 42)
                .await
                .unwrap();
        }
        // A different invocation's rows must not bleed in.
        store
            .insert_host_notice("inv-2", 0, 0, "resume", "<host-notice>x</host-notice>", 42)
            .await
            .unwrap();

        let rows = store.list_host_notices("inv-1").await.unwrap();
        let summary: Vec<(u32, u32, &str, &str)> = rows
            .iter()
            .map(|r| (r.step_index, r.seq, r.kind.as_str(), r.body.as_str()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (0, 0, "resume", "<host-notice>a</host-notice>"),
                (3, 0, "tools_changed", "<host-notice>b</host-notice>"),
                (3, 1, "context_pressure", "<host-notice>c</host-notice>"),
            ]
        );
        assert!(rows.iter().all(|r| r.invocation_id == "inv-1"));
        assert!(rows.iter().all(|r| r.created_at == 42));

        // The composite key is a real constraint: re-inserting an
        // existing (invocation, step, seq) fails loudly rather than
        // silently rewriting history.
        let dup = store
            .insert_host_notice(
                "inv-1",
                0,
                0,
                "resume",
                "<host-notice>dup</host-notice>",
                43,
            )
            .await;
        assert!(dup.is_err(), "duplicate key must be rejected");
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
    #[tokio::test]
    async fn v8_to_v9_migration_preserves_rows_and_adds_shared_sequence() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("worker-v8.db");
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let opts = SqliteConnectOptions::from_str(&url).unwrap();
        let pool = SqlitePoolOptions::new().connect_with(opts).await.unwrap();
        for stmt in split_sql(SCHEMA_META_SQL) {
            sqlx::query(&stmt).execute(&pool).await.unwrap();
        }
        for stmt in split_sql(WORKER_TABLES_V1_SQL) {
            sqlx::query(&stmt).execute(&pool).await.unwrap();
        }
        for migration in [
            WORKER_MIGRATION_V2_SQL,
            WORKER_MIGRATION_V3_SQL,
            WORKER_MIGRATION_V4_SQL,
            WORKER_MIGRATION_V5_SQL,
            WORKER_MIGRATION_V6_SQL,
            WORKER_MIGRATION_V7_SQL,
            WORKER_MIGRATION_V8_SQL,
        ] {
            for stmt in split_sql(migration) {
                sqlx::query(&stmt).execute(&pool).await.unwrap();
            }
        }
        sqlx::query("INSERT INTO schema_meta (class, version, updated_at) VALUES (?, 8, 0)")
            .bind(SCHEMA_CLASS)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO tool_dispatch (invocation_id, tool_call_id, tool_name, status, parameters, intent_at) VALUES ('inv', 'old', 't', 'intent', '{}', 1)")
            .execute(&pool).await.unwrap();
        pool.close().await;

        let store = WorkerStore::open(&path).await.unwrap();
        let old = store
            .get_tool_dispatch("inv", "old")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(old.seq, None, "pre-v9 rows use timestamp fallback");
        store
            .write_llm_intent("inv", "llm", "m", "{}", 2)
            .await
            .unwrap();
        store.write_llm_dispatched("inv", "llm", 3).await.unwrap();
        store
            .write_llm_completed("inv", "llm", "{}", false, 0.0, 4)
            .await
            .unwrap();
        store.write_tool_dispatched("inv", "old", 5).await.unwrap();
        store
            .write_tool_completed("inv", "old", "{}", false, 4)
            .await
            .unwrap();
        assert_eq!(
            store
                .get_llm_dispatch("inv", "llm")
                .await
                .unwrap()
                .unwrap()
                .seq,
            Some(1)
        );
        assert_eq!(
            store
                .get_tool_dispatch("inv", "old")
                .await
                .unwrap()
                .unwrap()
                .seq,
            Some(2)
        );
    }
}
