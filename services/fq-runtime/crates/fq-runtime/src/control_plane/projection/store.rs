//! SQLite-backed event projection store.
//!
//! Opens a SQLite database in WAL mode with four indexes tuned for
//! the queries we actually run. Inserts are idempotent (`INSERT OR
//! IGNORE ON event_id`) so at-least-once delivery from the NATS
//! consumer does not produce duplicates on re-delivery.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Pool, Row, Sqlite};

use crate::agent::AgentId;
use crate::events::{Event, EventPayload};
use serde::Serialize;

/// Schema — migrations live inline for phase 1. When the schema
/// evolves beyond trivial additions, switch to `sqlx::migrate!`.
const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS events (
    event_id        TEXT PRIMARY KEY,
    timestamp       TEXT NOT NULL,
    agent_id        TEXT NOT NULL,
    invocation_id   TEXT NOT NULL,
    event_type      TEXT NOT NULL,
    model           TEXT,
    input_tokens    INTEGER,
    output_tokens   INTEGER,
    cache_read_tokens INTEGER,
    cache_write_tokens INTEGER,
    total_cost      REAL,
    error_kind      TEXT,
    error_message   TEXT,
    duration_ms     INTEGER
);

CREATE INDEX IF NOT EXISTS idx_events_agent_time ON events(agent_id, timestamp);
CREATE INDEX IF NOT EXISTS idx_events_invocation ON events(invocation_id);
CREATE INDEX IF NOT EXISTS idx_events_type_time ON events(event_type, timestamp);
CREATE INDEX IF NOT EXISTS idx_events_time ON events(timestamp);

-- One-line operator-facing status per invocation (#216), projected
-- from `invocation.summary` events (last write wins). Derived data:
-- a reprojection replays the summary events without re-calling the
-- LLM. (No semicolons in these comments -- the schema runner splits
-- statements on them.)
CREATE TABLE IF NOT EXISTS invocation_summary (
    invocation_id   TEXT PRIMARY KEY,
    summary         TEXT NOT NULL,
    kind            TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);
"#;

/// SQLite projection store. Cheap to clone (the underlying
/// connection pool is `Arc`-reference-counted inside `sqlx`).
#[derive(Debug, Clone)]
pub struct ProjectionStore {
    pool: Pool<Sqlite>,
}

impl ProjectionStore {
    /// Open (or create) a projection database at the given path.
    ///
    /// Runs schema migrations after connecting. WAL mode is enabled
    /// so concurrent readers (the CLI's query commands) can run
    /// alongside the projection consumer's writes.
    pub async fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(StoreError::CreateDir)?;
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
        store.run_migrations().await?;
        Ok(store)
    }

    /// Open a read-only connection to an existing projection database.
    /// Used by the CLI query commands. Does not create the file; if
    /// the database doesn't exist, returns an error indicating the
    /// projector has not run yet.
    pub async fn open_read_only(path: &Path) -> Result<Self, StoreError> {
        if !path.exists() {
            return Err(StoreError::NotInitialised(path.to_path_buf()));
        }
        let url = format!("sqlite://{}?mode=ro", path.display());
        let options = SqliteConnectOptions::from_str(&url)?;
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await?;
        Ok(Self { pool })
    }

    async fn run_migrations(&self) -> Result<(), StoreError> {
        // sqlx executes one statement per call; split the schema
        // string so `CREATE TABLE` and each `CREATE INDEX` are
        // applied individually.
        for statement in SCHEMA_SQL
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            sqlx::query(statement).execute(&self.pool).await?;
        }
        // `CREATE TABLE IF NOT EXISTS` cannot add a column to an existing
        // table, so add these additively. Existence-checked via
        // `pragma_table_info` (deterministic and idempotent) rather than
        // matching driver error text.
        //
        // FORWARD-ONLY: the projection is not reprojected here, so rows
        // written before this migration read NULL (0 through the
        // `COALESCE(SUM(...))` aggregation) even though the source
        // `llm.response` events carry the counts. `fq costs` therefore
        // reports cache usage only from this migration forward. A proper
        // projection-versioning + reproject story backfills history —
        // tracked in #139 (the phase-1 inline-schema comment above is
        // now overdue).
        let columns: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('events')")
                .fetch_all(&self.pool)
                .await?;
        for (column, ty) in [
            ("cache_read_tokens", "INTEGER"),
            ("cache_write_tokens", "INTEGER"),
            ("error_message", "TEXT"),
        ] {
            if !columns.iter().any(|c| c == column) {
                sqlx::query(&format!("ALTER TABLE events ADD COLUMN {column} {ty}"))
                    .execute(&self.pool)
                    .await?;
            }
        }
        // One-time sweep (idempotent, cheap once empty via the
        // type index): heartbeats stopped being projected — see
        // `insert_event` — and this evicts the rows older builds
        // accumulated so the events surface reads as history again.
        sqlx::query("DELETE FROM events WHERE event_type = 'worker_heartbeat'")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Delete projected events older than `cutoff_ms` (Unix epoch milliseconds).
    /// Returns the number of rows deleted.
    ///
    /// Deletes in batches: the first sweep after an upgrade can face
    /// months of backlog, and one unbounded DELETE would hold the
    /// write lock against the projection consumer for the duration.
    pub async fn sweep_events(&self, cutoff_ms: i64) -> Result<u64, StoreError> {
        const SWEEP_BATCH_ROWS: i64 = 10_000;
        self.sweep_events_batched(cutoff_ms, SWEEP_BATCH_ROWS).await
    }

    async fn sweep_events_batched(&self, cutoff_ms: i64, batch: i64) -> Result<u64, StoreError> {
        let cutoff = chrono::DateTime::from_timestamp_millis(cutoff_ms)
            .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC)
            .to_rfc3339();
        let mut total = 0u64;
        loop {
            let result = sqlx::query(
                "DELETE FROM events WHERE rowid IN \
                 (SELECT rowid FROM events WHERE timestamp < ? LIMIT ?)",
            )
            .bind(&cutoff)
            .bind(batch)
            .execute(&self.pool)
            .await?;
            total += result.rows_affected();
            if result.rows_affected() < batch as u64 {
                return Ok(total);
            }
        }
    }

    /// Insert an event into the store. Idempotent on `event_id` —
    /// re-delivery from a durable consumer is a no-op.
    ///
    /// Worker heartbeats are NOT projected: a heartbeat is an
    /// operational liveness signal that goes stale the moment the next
    /// one lands (every 10s — ~13k rows/day of noise that buried the
    /// events surface), not history. Liveness lives where it is
    /// consumed: the control-plane worker table's `last_heartbeat`.
    pub async fn insert_event(&self, event: &Event) -> Result<(), StoreError> {
        if matches!(event.payload, EventPayload::WorkerHeartbeat(_)) {
            return Ok(());
        }
        let fields = extract_fields(event);
        let event_type = event.payload.event_type();

        sqlx::query(
            r#"
            INSERT OR IGNORE INTO events
                (event_id, timestamp, agent_id, invocation_id, event_type,
                 model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, total_cost, error_kind, error_message, duration_ms)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(event.envelope.event_id.to_string())
        .bind(event.envelope.timestamp.to_rfc3339())
        .bind(event.envelope.agent_id.as_str())
        .bind(event.envelope.invocation_id.to_string())
        .bind(event_type)
        .bind(fields.model)
        .bind(fields.input_tokens)
        .bind(fields.output_tokens)
        .bind(fields.cache_read_tokens)
        .bind(fields.cache_write_tokens)
        .bind(fields.total_cost)
        .bind(fields.error_kind)
        .bind(fields.error_message)
        .bind(fields.duration_ms)
        .execute(&self.pool)
        .await?;

        // Summary events additionally maintain the per-invocation
        // current line (#216). Last write wins; `Outcome` lines are
        // final because no later summary event is emitted for the
        // invocation.
        if let EventPayload::InvocationSummary(p) = &event.payload {
            sqlx::query(
                "INSERT INTO invocation_summary (invocation_id, summary, kind, updated_at)
                 VALUES (?, ?, ?, ?)
                 ON CONFLICT(invocation_id) DO UPDATE SET
                     summary = excluded.summary,
                     kind = excluded.kind,
                     updated_at = excluded.updated_at",
            )
            .bind(event.envelope.invocation_id.to_string())
            .bind(&p.summary)
            .bind(summary_kind_name(p.kind))
            .bind(event.envelope.timestamp.to_rfc3339())
            .execute(&self.pool)
            .await?;
        }

        Ok(())
    }

    /// The current summary line per invocation (#216) for a set of
    /// ids — the views layer joins these onto its invocation lists.
    /// Missing ids simply have no line yet.
    pub async fn summaries_for(
        &self,
        invocation_ids: &[String],
    ) -> Result<std::collections::HashMap<String, String>, StoreError> {
        let mut out = std::collections::HashMap::new();
        // Ids arrive from our own store reads (bounded by the view's
        // limit), so a simple per-id lookup keeps the SQL static.
        for id in invocation_ids {
            if let Some(row) =
                sqlx::query("SELECT summary FROM invocation_summary WHERE invocation_id = ?")
                    .bind(id)
                    .fetch_optional(&self.pool)
                    .await?
            {
                out.insert(id.clone(), row.get::<String, _>(0));
            }
        }
        Ok(out)
    }

    /// Return the number of events in the store.
    pub async fn count(&self) -> Result<i64, StoreError> {
        let row = sqlx::query("SELECT COUNT(*) FROM events")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<i64, _>(0))
    }

    /// Query events with optional filters. Returns up to `limit`
    /// rows ordered by timestamp descending (most recent first).
    pub async fn query_events(
        &self,
        filter: &EventFilter<'_>,
        limit: i64,
    ) -> Result<Vec<EventRow>, StoreError> {
        // Build the WHERE clause dynamically but safely — each
        // condition uses a placeholder.
        let mut sql = String::from(
            "SELECT event_id, timestamp, agent_id, invocation_id, event_type, \
             model, total_cost, error_kind, error_message, duration_ms \
             FROM events",
        );
        let mut clauses: Vec<&str> = Vec::new();
        if filter.agent.is_some() {
            clauses.push("agent_id = ?");
        }
        if filter.event_type.is_some() {
            clauses.push("event_type = ?");
        }
        if filter.since.is_some() {
            clauses.push("timestamp >= ?");
        }
        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY timestamp DESC LIMIT ?");

        let mut q = sqlx::query(&sql);
        if let Some(agent) = filter.agent {
            q = q.bind(agent);
        }
        if let Some(ty) = filter.event_type {
            q = q.bind(ty);
        }
        if let Some(since) = filter.since {
            q = q.bind(since);
        }
        q = q.bind(limit);

        let rows = q.fetch_all(&self.pool).await?;
        let events = rows
            .into_iter()
            .map(|row| EventRow {
                event_id: row.get::<String, _>(0),
                timestamp: row.get::<String, _>(1),
                agent_id: row.get::<String, _>(2),
                invocation_id: row.get::<String, _>(3),
                event_type: row.get::<String, _>(4),
                model: row.get::<Option<String>, _>(5),
                total_cost: row.get::<Option<f64>, _>(6),
                error_kind: row.get::<Option<String>, _>(7),
                error_message: row.get::<Option<String>, _>(8),
                duration_ms: row.get::<Option<i64>, _>(9),
            })
            .collect();
        Ok(events)
    }

    /// Look up the `agent_id` for an invocation. Returns `None` if
    /// no projected event references the invocation. Used by the
    /// operator CLI to address `fq.agent.<id>.*` subjects when only
    /// the invocation id is known.
    pub async fn agent_id_for_invocation(
        &self,
        invocation_id: &str,
    ) -> Result<Option<String>, StoreError> {
        let query = format!(
            "SELECT agent_id FROM events WHERE invocation_id = ? \
             AND agent_id NOT IN ('{}', '{}', '{}') ORDER BY timestamp LIMIT 1",
            AgentId::SYSTEM_STR,
            AgentId::SUMMARY_STR,
            AgentId::OPERATOR_STR,
        );
        let row = sqlx::query(&query)
            .bind(invocation_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<String, _>(0)))
    }

    /// Aggregate cost-bearing events into per-agent totals. Cost
    /// now rides on `llm.response` envelopes (envelope-refactor
    /// plan step 3), so the filter is `total_cost IS NOT NULL`
    /// instead of `event_type = 'cost'`. The event-type allowlist
    /// covers per-call cost carriers only — `llm_response` and the
    /// summariser's `invocation_summary` (#216) — because terminal
    /// events (`completed`/`failed`) carry invocation *totals* and
    /// would double-count.
    pub async fn cost_summary(
        &self,
        agent: Option<&str>,
        since: Option<&str>,
    ) -> Result<Vec<CostSummary>, StoreError> {
        let mut sql = String::from(
            "SELECT agent_id, \
             COUNT(*) AS event_count, \
             COALESCE(SUM(total_cost), 0.0) AS total_cost, \
             COALESCE(SUM(input_tokens), 0) AS total_input_tokens, \
             COALESCE(SUM(output_tokens), 0) AS total_output_tokens, \
             COALESCE(SUM(cache_read_tokens), 0) AS total_cache_read_tokens, \
             COALESCE(SUM(cache_write_tokens), 0) AS total_cache_write_tokens, \
             COUNT(DISTINCT invocation_id) AS invocation_count \
             FROM events \
             WHERE event_type IN ('llm_response', 'invocation_summary') AND total_cost IS NOT NULL",
        );
        if agent.is_some() {
            sql.push_str(" AND agent_id = ?");
        }
        if since.is_some() {
            sql.push_str(" AND timestamp >= ?");
        }
        sql.push_str(" GROUP BY agent_id ORDER BY total_cost DESC");

        let mut q = sqlx::query(&sql);
        if let Some(a) = agent {
            q = q.bind(a);
        }
        if let Some(s) = since {
            q = q.bind(s);
        }
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|row| CostSummary {
                agent_id: row.get::<String, _>(0),
                event_count: row.get::<i64, _>(1),
                total_cost: row.get::<f64, _>(2),
                total_input_tokens: row.get::<i64, _>(3),
                total_output_tokens: row.get::<i64, _>(4),
                total_cache_read_tokens: row.get::<i64, _>(5),
                total_cache_write_tokens: row.get::<i64, _>(6),
                invocation_count: row.get::<i64, _>(7),
            })
            .collect())
    }

    /// One agent's cost-bearing events grouped per invocation, newest
    /// first (by each invocation's first cost event), capped at
    /// `limit`. Same row filter as [`Self::cost_summary`]; the columns
    /// it groups on (`invocation_id`, and `model` for
    /// [`Self::cost_by_model`]) have been on every event row since the
    /// original schema — no new columns, only new GROUP BYs.
    pub async fn cost_by_invocation(
        &self,
        agent: &str,
        since: Option<&str>,
        limit: i64,
    ) -> Result<Vec<InvocationCostSummary>, StoreError> {
        let mut sql = String::from(
            "SELECT invocation_id, \
             MIN(timestamp) AS first_timestamp, \
             COUNT(*) AS event_count, \
             COALESCE(SUM(total_cost), 0.0) AS total_cost, \
             COALESCE(SUM(input_tokens), 0) AS total_input_tokens, \
             COALESCE(SUM(output_tokens), 0) AS total_output_tokens, \
             COALESCE(SUM(cache_read_tokens), 0) AS total_cache_read_tokens, \
             COALESCE(SUM(cache_write_tokens), 0) AS total_cache_write_tokens \
             FROM events \
             WHERE event_type IN ('llm_response', 'invocation_summary') AND total_cost IS NOT NULL \
             AND agent_id = ?",
        );
        if since.is_some() {
            sql.push_str(" AND timestamp >= ?");
        }
        sql.push_str(" GROUP BY invocation_id ORDER BY first_timestamp DESC LIMIT ?");

        let mut q = sqlx::query(&sql).bind(agent);
        if let Some(s) = since {
            q = q.bind(s);
        }
        q = q.bind(limit);
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|row| InvocationCostSummary {
                invocation_id: row.get::<String, _>(0),
                first_timestamp: row.get::<String, _>(1),
                event_count: row.get::<i64, _>(2),
                total_cost: row.get::<f64, _>(3),
                total_input_tokens: row.get::<i64, _>(4),
                total_output_tokens: row.get::<i64, _>(5),
                total_cache_read_tokens: row.get::<i64, _>(6),
                total_cache_write_tokens: row.get::<i64, _>(7),
            })
            .collect())
    }

    /// One invocation's cost aggregate — the same row filter as
    /// [`Self::cost_by_invocation`], for a single id. `None` when the
    /// invocation has no cost-bearing events yet.
    pub async fn cost_of_invocation(
        &self,
        invocation_id: &str,
    ) -> Result<Option<InvocationCostSummary>, StoreError> {
        let row = sqlx::query(
            "SELECT invocation_id, \
             MIN(timestamp) AS first_timestamp, \
             COUNT(*) AS event_count, \
             COALESCE(SUM(total_cost), 0.0) AS total_cost, \
             COALESCE(SUM(input_tokens), 0) AS total_input_tokens, \
             COALESCE(SUM(output_tokens), 0) AS total_output_tokens, \
             COALESCE(SUM(cache_read_tokens), 0) AS total_cache_read_tokens, \
             COALESCE(SUM(cache_write_tokens), 0) AS total_cache_write_tokens \
             FROM events \
             WHERE event_type = 'llm_response' AND total_cost IS NOT NULL \
             AND invocation_id = ? \
             GROUP BY invocation_id",
        )
        .bind(invocation_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| InvocationCostSummary {
            invocation_id: row.get::<String, _>(0),
            first_timestamp: row.get::<String, _>(1),
            event_count: row.get::<i64, _>(2),
            total_cost: row.get::<f64, _>(3),
            total_input_tokens: row.get::<i64, _>(4),
            total_output_tokens: row.get::<i64, _>(5),
            total_cache_read_tokens: row.get::<i64, _>(6),
            total_cache_write_tokens: row.get::<i64, _>(7),
        }))
    }

    /// Cost-bearing events summed per time bucket, oldest first. The
    /// bucket key is a fixed-width prefix of the RFC3339 UTC timestamp
    /// — `substr` instead of SQLite's date functions, which cannot
    /// parse our nanosecond fractions: 10 chars = `YYYY-MM-DD` (day),
    /// 13 chars = `YYYY-MM-DDTHH` (hour). Buckets with no cost events
    /// simply don't appear; the caller fills gaps for display.
    pub async fn cost_by_time_bucket(
        &self,
        hourly: bool,
        since: Option<&str>,
    ) -> Result<Vec<CostBucketSummary>, StoreError> {
        let prefix_len = if hourly { 13 } else { 10 };
        let mut sql = format!(
            "SELECT substr(timestamp, 1, {prefix_len}) AS bucket, \
             COALESCE(SUM(total_cost), 0.0) AS total_cost \
             FROM events \
             WHERE event_type = 'llm_response' AND total_cost IS NOT NULL",
        );
        if since.is_some() {
            sql.push_str(" AND timestamp >= ?");
        }
        sql.push_str(" GROUP BY bucket ORDER BY bucket ASC");

        let mut q = sqlx::query(&sql);
        if let Some(s) = since {
            q = q.bind(s);
        }
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|row| CostBucketSummary {
                bucket: row.get::<String, _>(0),
                total_cost: row.get::<f64, _>(1),
            })
            .collect())
    }

    /// Cost-bearing events grouped per model, biggest spender first —
    /// across every agent, or one agent when `agent` is set. See
    /// [`Self::cost_by_invocation`] for the shared filter rationale.
    pub async fn cost_by_model(
        &self,
        agent: Option<&str>,
        since: Option<&str>,
    ) -> Result<Vec<ModelCostSummary>, StoreError> {
        let mut sql = String::from(
            "SELECT COALESCE(model, 'unknown') AS model, \
             COUNT(*) AS event_count, \
             COALESCE(SUM(total_cost), 0.0) AS total_cost, \
             COALESCE(SUM(input_tokens), 0) AS total_input_tokens, \
             COALESCE(SUM(output_tokens), 0) AS total_output_tokens \
             FROM events \
             WHERE event_type IN ('llm_response', 'invocation_summary') AND total_cost IS NOT NULL",
        );
        if agent.is_some() {
            sql.push_str(" AND agent_id = ?");
        }
        if since.is_some() {
            sql.push_str(" AND timestamp >= ?");
        }
        sql.push_str(" GROUP BY model ORDER BY total_cost DESC");

        let mut q = sqlx::query(&sql);
        if let Some(a) = agent {
            q = q.bind(a);
        }
        if let Some(s) = since {
            q = q.bind(s);
        }
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|row| ModelCostSummary {
                model: row.get::<String, _>(0),
                event_count: row.get::<i64, _>(1),
                total_cost: row.get::<f64, _>(2),
                total_input_tokens: row.get::<i64, _>(3),
                total_output_tokens: row.get::<i64, _>(4),
            })
            .collect())
    }

    /// Aggregate terminal `failed` events into per-`FailureKind`
    /// counts. Symmetric with [`Self::cost_summary`]: the DB stores
    /// the failure kind in the denormalised `error_kind` column
    /// (the serde snake_case form, e.g. `budget_exceeded`),
    /// so this groups by that column for a stable typed-ish shape the
    /// `fq doctor` command can render without re-reading payloads.
    pub async fn failure_summary(&self) -> Result<Vec<FailureSummary>, StoreError> {
        let rows = sqlx::query(
            "SELECT COALESCE(error_kind, 'unknown') AS kind, COUNT(*) AS n \
             FROM events \
             WHERE event_type = 'failed' \
             GROUP BY kind ORDER BY n DESC, kind",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| FailureSummary {
                error_kind: row.get::<String, _>(0),
                count: row.get::<i64, _>(1),
            })
            .collect())
    }
}

/// One row from a [`ProjectionStore::query_events`] call.
#[derive(Debug, Clone)]
pub struct EventRow {
    pub event_id: String,
    pub timestamp: String,
    pub agent_id: String,
    pub invocation_id: String,
    pub event_type: String,
    pub model: Option<String>,
    pub total_cost: Option<f64>,
    pub error_kind: Option<String>,
    pub error_message: Option<String>,
    pub duration_ms: Option<i64>,
}

/// One row of a cost summary.
#[derive(Debug, Clone)]
pub struct CostSummary {
    pub agent_id: String,
    pub event_count: i64,
    pub total_cost: f64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    pub total_cache_write_tokens: i64,
    /// Distinct invocations behind the aggregate — "how many runs did
    /// this spend buy".
    pub invocation_count: i64,
}

/// One invocation's share of an agent's spend — a row from
/// [`ProjectionStore::cost_by_invocation`].
#[derive(Debug, Clone)]
pub struct InvocationCostSummary {
    pub invocation_id: String,
    /// RFC3339 timestamp of the invocation's first cost event — its
    /// effective start, as far as the projection knows.
    pub first_timestamp: String,
    pub event_count: i64,
    pub total_cost: f64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    pub total_cache_write_tokens: i64,
}

/// One time bucket's cost sum — a row from
/// [`ProjectionStore::cost_by_time_bucket`].
#[derive(Debug, Clone, PartialEq)]
pub struct CostBucketSummary {
    /// `YYYY-MM-DD` (daily) or `YYYY-MM-DDTHH` (hourly), UTC.
    pub bucket: String,
    pub total_cost: f64,
}

/// One model's share of an agent's spend — a row from
/// [`ProjectionStore::cost_by_model`].
#[derive(Debug, Clone)]
pub struct ModelCostSummary {
    /// Model name as recorded on the event; `unknown` for rows written
    /// before the model column was populated.
    pub model: String,
    pub event_count: i64,
    pub total_cost: f64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
}

/// One row of a failure summary: a terminal `FailureKind` and the
/// number of `failed` events carrying it. Produced by
/// [`ProjectionStore::failure_summary`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureSummary {
    /// Lowercased failure kind as stored in the projection
    /// (`budget_exceeded`, `llm_error`, `max_iterations`, `tool_error`,
    /// `sandbox_violation`, `runtime_error`), or `unknown` for a
    /// `failed` row with no recorded kind.
    pub error_kind: String,
    pub count: i64,
}

/// Filter options for [`ProjectionStore::query_events`].
#[derive(Debug, Default, Clone, Copy)]
pub struct EventFilter<'a> {
    pub agent: Option<&'a str>,
    pub event_type: Option<&'a str>,
    pub since: Option<&'a str>,
}

/// Errors from the projection store.
///
/// `Backend` carries a `String` rather than a backend-specific
/// error type so swapping the underlying storage (today: SQLite
/// via sqlx) does not break downstream consumers' match arms.
/// Internal code uses `From<sqlx::Error>` for ergonomic
/// propagation; the public variant only exposes a message.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("projection store backend error: {0}")]
    Backend(String),

    #[error("failed to create database directory: {0}")]
    CreateDir(std::io::Error),

    #[error("projection database not initialised at {0} (has `fq run` been started?)")]
    NotInitialised(PathBuf),
}

impl From<sqlx::Error> for StoreError {
    fn from(err: sqlx::Error) -> Self {
        StoreError::Backend(err.to_string())
    }
}

/// Denormalised fields extracted from an event for indexing.
#[derive(Default)]
struct Fields {
    model: Option<String>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
    cache_write_tokens: Option<i64>,
    total_cost: Option<f64>,
    error_kind: Option<String>,
    error_message: Option<String>,
    duration_ms: Option<i64>,
}

fn serialized_name<T: Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .expect("failure kinds serialize")
        .as_str()
        .expect("failure kinds serialize as strings")
        .to_owned()
}

fn extract_fields(event: &Event) -> Fields {
    match &event.payload {
        EventPayload::Triggered(p) => Fields {
            model: Some(p.config_snapshot.model.clone()),
            ..Default::default()
        },
        EventPayload::LlmRequest(p) => Fields {
            model: Some(p.model.clone()),
            ..Default::default()
        },
        // Cost now rides on the envelope (envelope-refactor plan
        // step 3); pull from envelope.cost when present so the
        // existing total_cost / input_tokens / output_tokens
        // columns stay populated.
        EventPayload::LlmResponse(p) => {
            let mut f = Fields {
                input_tokens: Some(p.usage.input_tokens as i64),
                output_tokens: Some(p.usage.output_tokens as i64),
                cache_read_tokens: Some(p.usage.cache_read_tokens as i64),
                cache_write_tokens: Some(p.usage.cache_write_tokens as i64),
                ..Default::default()
            };
            if let Some(cost) = &event.envelope.cost {
                f.model = Some(cost.model.clone());
                f.total_cost = Some(cost.total_cost);
            }
            f
        }
        // The summariser's own spend (#216): everything lives on
        // envelope.cost (the llm_response pattern), emitted under the
        // reserved `summary` agent id — `fq costs` reports it as its
        // own row with no changes to the cost queries.
        EventPayload::InvocationSummary(_) => {
            let mut f = Fields::default();
            if let Some(cost) = &event.envelope.cost {
                f.model = Some(cost.model.clone());
                f.input_tokens = Some(cost.input_tokens as i64);
                f.output_tokens = Some(cost.output_tokens as i64);
                f.cache_read_tokens = Some(cost.cache_read_tokens as i64);
                f.cache_write_tokens = Some(cost.cache_write_tokens as i64);
                f.total_cost = Some(cost.total_cost);
            }
            f
        }
        EventPayload::ToolCall(_) => Fields::default(),
        EventPayload::ToolDispatched(_) => Fields::default(),
        EventPayload::LlmDispatched(_) => Fields::default(),
        EventPayload::HostNotice(_) => Fields::default(),
        EventPayload::InvocationAmbiguous(_) => Fields::default(),
        EventPayload::InvocationArchived(_) => Fields::default(),
        EventPayload::InvocationArchiveAcked(_) => Fields::default(),
        EventPayload::ToolResult(p) => Fields {
            error_kind: p.error_kind.map(serialized_name),
            duration_ms: Some(p.duration_ms as i64),
            ..Default::default()
        },
        EventPayload::Completed(p) => Fields {
            total_cost: Some(p.total_cost),
            duration_ms: Some(p.total_duration_ms as i64),
            ..Default::default()
        },
        EventPayload::Failed(p) => Fields {
            error_kind: Some(serialized_name(p.error_kind)),
            error_message: Some(p.error_message.clone()),
            duration_ms: Some(p.partial_totals.total_duration_ms as i64),
            total_cost: Some(p.partial_totals.total_cost),
            ..Default::default()
        },
        // System events carry no agent metadata. The projection
        // still records them for visibility (useful for "when did
        // the daemon restart" queries), but every denormalised
        // column is NULL. WorkerHeartbeat never reaches this point —
        // `insert_event` drops it (operational signal, not data).
        EventPayload::SystemStartup(_)
        | EventPayload::SystemShutdown(_)
        | EventPayload::SystemTaskFailed(_)
        | EventPayload::SystemRecovery(_)
        | EventPayload::WorkerHeartbeat(_)
        | EventPayload::WorkerOrphaned(_)
        | EventPayload::McpServerLog(_)
        | EventPayload::InvocationOperatorRecovered(_) => Fields::default(),
    }
}

fn summary_kind_name(kind: crate::events::SummaryKind) -> &'static str {
    match kind {
        crate::events::SummaryKind::Start => "start",
        crate::events::SummaryKind::Progress => "progress",
        crate::events::SummaryKind::Outcome => "outcome",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentId;
    use crate::events::{
        CompletedPayload, ConfigSnapshot, CostMetadata, Event, EventPayload, FailedPayload,
        FailureKind, FailurePhase, InvocationTotals, LlmRequestPayload, LlmResponsePayload,
        Message, MessageRole, RequestParams, SandboxSnapshot, StopReason, TokenUsage,
        TriggerSource, TriggeredPayload,
    };
    use serde_json::json;
    use tempfile::tempdir;
    use uuid::Uuid;

    /// Tiny helper for fixtures: `AgentId::new(s).unwrap()` would be
    /// noise at every call site. Panics on invalid input — only used
    /// in test code where the inputs are hardcoded by us.
    fn aid(s: &str) -> AgentId {
        AgentId::new(s).expect("test agent id must be valid")
    }

    fn summary_event(inv: Uuid, kind: crate::events::SummaryKind, line: &str) -> Event {
        Event::new(
            AgentId::summary(),
            inv,
            EventPayload::InvocationSummary(crate::events::InvocationSummaryPayload {
                kind,
                summary: line.to_string(),
            }),
        )
        .with_cost(CostMetadata {
            call_id: Uuid::now_v7(),
            model: "cheap-model".to_string(),
            input_tokens: 400,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            input_cost: 0.0004,
            output_cost: 0.0001,
            total_cost: 0.0005,
            cumulative_invocation_cost: 0.0005,
            cumulative_agent_cost: 0.0005,
            origin: Default::default(),
        })
    }

    #[tokio::test]
    async fn sweep_events_deletes_old_rows_and_keeps_fresh_rows() {
        let dir = tempdir().unwrap();
        let store = ProjectionStore::open(&dir.path().join("projection.db"))
            .await
            .unwrap();
        let old = sample_triggered("old", Uuid::now_v7());
        let fresh = sample_triggered("fresh", Uuid::now_v7());
        store.insert_event(&old).await.unwrap();
        store.insert_event(&fresh).await.unwrap();
        sqlx::query("UPDATE events SET timestamp = ? WHERE event_id = ?")
            .bind("2020-01-01T00:00:00+00:00")
            .bind(old.envelope.event_id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();

        let cutoff = chrono::DateTime::parse_from_rfc3339("2021-01-01T00:00:00Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(store.sweep_events(cutoff).await.unwrap(), 1);
        assert_eq!(store.count().await.unwrap(), 1);
        let remaining: String = sqlx::query_scalar("SELECT event_id FROM events")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(remaining, fresh.envelope.event_id.to_string());
    }

    #[tokio::test]
    async fn sweep_events_batches_until_backlog_clear() {
        let dir = tempdir().unwrap();
        let store = ProjectionStore::open(&dir.path().join("projection.db"))
            .await
            .unwrap();
        for name in ["old-a", "old-b", "old-c"] {
            let event = sample_triggered(name, Uuid::now_v7());
            store.insert_event(&event).await.unwrap();
            sqlx::query("UPDATE events SET timestamp = ? WHERE event_id = ?")
                .bind("2020-01-01T00:00:00+00:00")
                .bind(event.envelope.event_id.to_string())
                .execute(&store.pool)
                .await
                .unwrap();
        }
        let fresh = sample_triggered("fresh", Uuid::now_v7());
        store.insert_event(&fresh).await.unwrap();

        let cutoff = chrono::DateTime::parse_from_rfc3339("2021-01-01T00:00:00Z")
            .unwrap()
            .timestamp_millis();
        // Batch size 1 forces one delete round per backlog row (plus
        // the terminating short round): the loop must clear the whole
        // backlog, count it accurately, and leave fresh rows alone.
        assert_eq!(store.sweep_events_batched(cutoff, 1).await.unwrap(), 3);
        assert_eq!(store.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn agent_id_for_invocation_ignores_operator_only_tombstone() {
        let dir = tempdir().unwrap();
        let store = ProjectionStore::open(&dir.path().join("projection.db"))
            .await
            .unwrap();
        let inv = Uuid::now_v7();
        let event = Event::new(
            AgentId::operator(),
            inv,
            EventPayload::InvocationOperatorRecovered(
                crate::events::InvocationOperatorRecoveredPayload {
                    action: "drop".to_string(),
                    final_phase: "failed".to_string(),
                    reason: None,
                },
            ),
        );

        store.insert_event(&event).await.unwrap();
        assert_eq!(
            store
                .agent_id_for_invocation(&inv.to_string())
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn agent_id_for_invocation_uses_first_real_agent_not_summary() {
        let dir = tempdir().unwrap();
        let store = ProjectionStore::open(&dir.path().join("projection.db"))
            .await
            .unwrap();
        let inv = Uuid::now_v7();
        let summary = summary_event(inv, crate::events::SummaryKind::Start, "starting");
        let mut triggered = sample_triggered("builder", inv);
        triggered.envelope.timestamp = summary.envelope.timestamp + chrono::Duration::seconds(1);

        // Insert the real event first while giving the sentinel row an earlier
        // timestamp, pinning both sentinel exclusion and timestamp ordering.
        store.insert_event(&triggered).await.unwrap();
        store.insert_event(&summary).await.unwrap();
        assert_eq!(
            store
                .agent_id_for_invocation(&inv.to_string())
                .await
                .unwrap(),
            Some("builder".to_string())
        );
    }

    /// #216: a summary event lands twice — as a costed events row
    /// under the reserved `summary` agent (the operator-overhead
    /// accounting), and as the per-invocation current line (last
    /// write wins).
    #[tokio::test]
    async fn summary_events_are_costed_and_upsert_the_current_line() {
        let dir = tempdir().unwrap();
        let store = ProjectionStore::open(&dir.path().join("projection.db"))
            .await
            .unwrap();
        let inv = Uuid::now_v7();

        store
            .insert_event(&summary_event(
                inv,
                crate::events::SummaryKind::Start,
                "Fixing #7: starting",
            ))
            .await
            .unwrap();
        store
            .insert_event(&summary_event(
                inv,
                crate::events::SummaryKind::Progress,
                "Fixing #7: editing widget.rs",
            ))
            .await
            .unwrap();

        // The current line: last write wins.
        let summaries = store.summaries_for(&[inv.to_string()]).await.unwrap();
        assert_eq!(
            summaries.get(&inv.to_string()).map(String::as_str),
            Some("Fixing #7: editing widget.rs")
        );
        assert!(
            store
                .summaries_for(&["no-such".to_string()])
                .await
                .unwrap()
                .is_empty()
        );

        // The cost accounting: events rows under agent `summary` carry
        // model/tokens/cost from the envelope, so `fq costs` reports
        // the summariser as its own row.
        let rows = store
            .query_events(
                &EventFilter {
                    agent: Some("summary"),
                    event_type: None,
                    since: None,
                },
                10,
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.event_type == "invocation_summary"));
        assert!(rows.iter().all(|r| r.total_cost == Some(0.0005)));
        assert!(
            rows.iter()
                .all(|r| r.model.as_deref() == Some("cheap-model"))
        );

        // Reported, not just recorded (#216's operator-costed
        // guarantee): the summariser appears in the cost aggregations
        // `fq costs` renders — per-agent and per-model.
        let agents = store.cost_summary(None, None).await.unwrap();
        let summary_row = agents
            .iter()
            .find(|c| c.agent_id == "summary")
            .expect("summary agent row in cost_summary");
        assert!((summary_row.total_cost - 0.001).abs() < 1e-9);
        let models = store.cost_by_model(None, None).await.unwrap();
        assert!(
            models.iter().any(|m| m.model == "cheap-model"),
            "summariser model in the per-model split"
        );
    }

    fn sample_triggered(agent: &str, inv: Uuid) -> Event {
        Event::new(
            aid(agent),
            inv,
            EventPayload::Triggered(TriggeredPayload {
                trigger_source: TriggerSource::Manual,
                trigger_subject: None,
                trigger_payload: json!({}),
                config_snapshot: ConfigSnapshot {
                    name: agent.to_string(),
                    model: "claude-haiku-4-5".to_string(),
                    system_prompt: "You are a test.".to_string(),
                    tools: vec![],
                    sandbox: SandboxSnapshot::default(),
                    budget: None,
                    ..Default::default()
                },
            }),
        )
    }

    /// LLM response with cost attached via envelope. After step 3
    /// of the envelope-refactor plan, cost rides on the
    /// `llm.response` envelope rather than as its own event.
    fn sample_llm_response_with_cost(agent: &str, inv: Uuid, cost: f64) -> Event {
        Event::new(
            aid(agent),
            inv,
            EventPayload::LlmResponse(LlmResponsePayload {
                origin: crate::events::LlmCallOrigin::AgentTurn,
                call_id: Uuid::now_v7(),
                content: Some("ok".to_string()),
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                    cache_read_tokens: 20,
                    cache_write_tokens: 10,
                },
            }),
        )
        .with_cost(CostMetadata {
            call_id: Uuid::now_v7(),
            model: "claude-haiku-4-5".to_string(),
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 20,
            cache_write_tokens: 10,
            input_cost: 0.0001,
            output_cost: 0.00025,
            total_cost: cost,
            cumulative_invocation_cost: cost,
            cumulative_agent_cost: cost,
            origin: crate::events::LlmCallOrigin::AgentTurn,
        })
    }

    fn sample_completed(agent: &str, inv: Uuid) -> Event {
        Event::new(
            aid(agent),
            inv,
            EventPayload::Completed(CompletedPayload {
                task_status: crate::events::TaskStatus::default(),
                result_summary: Some("done".to_string()),
                total_llm_calls: 1,
                total_tool_calls: 0,
                total_cost: 0.0011,
                total_duration_ms: 123,
            }),
        )
    }

    fn sample_failed(agent: &str, inv: Uuid) -> Event {
        Event::new(
            aid(agent),
            inv,
            EventPayload::Failed(FailedPayload {
                error_kind: FailureKind::BudgetExceeded,
                error_message: "blew the budget".to_string(),
                phase: FailurePhase::LlmResponse,
                partial_totals: InvocationTotals {
                    total_llm_calls: 1,
                    total_tool_calls: 0,
                    total_cost: 0.5,
                    total_duration_ms: 99,
                    sampling_cost: 0.0,
                    elicitation_cost: 0.0,
                },
            }),
        )
    }

    fn sample_llm_request(agent: &str, inv: Uuid) -> Event {
        Event::new(
            aid(agent),
            inv,
            EventPayload::LlmRequest(LlmRequestPayload {
                origin: crate::events::LlmCallOrigin::AgentTurn,
                call_id: Uuid::now_v7(),
                model: "claude-haiku-4-5".to_string(),
                messages: vec![Message {
                    role: MessageRole::System,
                    content: Some("hi".to_string()),
                    tool_calls: vec![],
                    tool_call_id: None,
                }],
                tools_available: vec![],
                request_params: RequestParams {
                    effort: None,
                    temperature: None,
                    max_tokens: Some(1024),
                },
            }),
        )
    }

    fn sample_llm_response(agent: &str, inv: Uuid) -> Event {
        Event::new(
            aid(agent),
            inv,
            EventPayload::LlmResponse(LlmResponsePayload {
                origin: crate::events::LlmCallOrigin::AgentTurn,
                call_id: Uuid::now_v7(),
                content: Some("hi".to_string()),
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 5,
                    output_tokens: 3,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
            }),
        )
    }

    async fn open_store() -> (ProjectionStore, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("projection.db");
        let store = ProjectionStore::open(&path).await.unwrap();
        (store, dir)
    }

    #[tokio::test]
    async fn opens_and_creates_schema() {
        let (store, _dir) = open_store().await;
        assert_eq!(store.count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn migrates_existing_projection_with_cache_columns() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("projection.db");
        std::fs::File::create(&path).unwrap();
        let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE events (event_id TEXT PRIMARY KEY, timestamp TEXT NOT NULL, \
             agent_id TEXT NOT NULL, invocation_id TEXT NOT NULL, event_type TEXT NOT NULL, \
             model TEXT, input_tokens INTEGER, output_tokens INTEGER, total_cost REAL, \
             error_kind TEXT, duration_ms INTEGER)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let store = ProjectionStore::open(&path).await.unwrap();
        store
            .insert_event(&sample_llm_response_with_cost(
                "alpha",
                Uuid::now_v7(),
                0.01,
            ))
            .await
            .unwrap();
        let summary = store.cost_summary(None, None).await.unwrap();
        assert_eq!(summary[0].total_cache_read_tokens, 20);
        assert_eq!(summary[0].total_cache_write_tokens, 10);
    }

    #[tokio::test]
    async fn inserts_and_counts_events() {
        let (store, _dir) = open_store().await;
        let inv = Uuid::now_v7();
        store
            .insert_event(&sample_triggered("alpha", inv))
            .await
            .unwrap();
        store
            .insert_event(&sample_llm_response_with_cost("alpha", inv, 0.0011))
            .await
            .unwrap();
        store
            .insert_event(&sample_completed("alpha", inv))
            .await
            .unwrap();

        assert_eq!(store.count().await.unwrap(), 3);
    }

    /// Heartbeats are an operational signal, not data: `insert_event`
    /// drops them, and the migration sweep evicts rows older builds
    /// accumulated.
    #[tokio::test]
    async fn heartbeats_are_not_projected_and_legacy_rows_are_swept() {
        use crate::events::WorkerHeartbeatPayload;
        use crate::worker::WorkerId;

        let (store, dir) = open_store().await;
        let heartbeat = Event::system(
            Uuid::now_v7(),
            EventPayload::WorkerHeartbeat(WorkerHeartbeatPayload {
                worker_id: WorkerId::new("w1".to_string()).unwrap(),
            }),
        );
        store.insert_event(&heartbeat).await.unwrap();
        // A real event still lands; the heartbeat never did.
        store
            .insert_event(&sample_triggered("alpha", Uuid::now_v7()))
            .await
            .unwrap();
        assert_eq!(store.count().await.unwrap(), 1);
        let rows = store
            .query_events(&EventFilter::default(), 10)
            .await
            .unwrap();
        assert!(rows.iter().all(|r| r.event_type != "worker_heartbeat"));

        // A row written by an older build (heartbeats were projected
        // until 2026-07-15) is deleted by the reopen migration.
        sqlx::query(
            "INSERT INTO events (event_id, timestamp, agent_id, invocation_id, event_type) \
             VALUES ('legacy-hb', '2026-07-14T00:00:00+00:00', 'system', 'inv', 'worker_heartbeat')",
        )
        .execute(&store.pool)
        .await
        .unwrap();
        assert_eq!(store.count().await.unwrap(), 2);
        drop(store);
        let reopened = ProjectionStore::open(&dir.path().join("projection.db"))
            .await
            .unwrap();
        assert_eq!(reopened.count().await.unwrap(), 1, "legacy heartbeat swept");
    }

    #[tokio::test]
    async fn insert_is_idempotent_by_event_id() {
        let (store, _dir) = open_store().await;
        let inv = Uuid::now_v7();
        let event = sample_triggered("alpha", inv);
        store.insert_event(&event).await.unwrap();
        store.insert_event(&event).await.unwrap(); // re-delivery
        store.insert_event(&event).await.unwrap();
        assert_eq!(store.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn queries_filter_by_agent() {
        let (store, _dir) = open_store().await;
        let inv = Uuid::now_v7();
        store
            .insert_event(&sample_triggered("alpha", inv))
            .await
            .unwrap();
        store
            .insert_event(&sample_triggered("beta", Uuid::now_v7()))
            .await
            .unwrap();

        let filter = EventFilter {
            agent: Some("alpha"),
            ..Default::default()
        };
        let rows = store.query_events(&filter, 100).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].agent_id, "alpha");
    }

    #[tokio::test]
    async fn queries_filter_by_event_type() {
        let (store, _dir) = open_store().await;
        let inv = Uuid::now_v7();
        store
            .insert_event(&sample_triggered("alpha", inv))
            .await
            .unwrap();
        store
            .insert_event(&sample_llm_response_with_cost("alpha", inv, 0.01))
            .await
            .unwrap();
        store
            .insert_event(&sample_completed("alpha", inv))
            .await
            .unwrap();

        // After step 3 of the envelope-refactor plan, cost rides on
        // `llm.response` envelopes; filter by the response event
        // type and check the cost denormalised onto the row.
        let filter = EventFilter {
            event_type: Some("llm_response"),
            ..Default::default()
        };
        let rows = store.query_events(&filter, 100).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_type, "llm_response");
        assert_eq!(rows[0].total_cost, Some(0.01));
    }

    #[tokio::test]
    async fn queries_respect_limit() {
        let (store, _dir) = open_store().await;
        for _ in 0..5 {
            store
                .insert_event(&sample_triggered("alpha", Uuid::now_v7()))
                .await
                .unwrap();
        }
        let filter = EventFilter::default();
        let rows = store.query_events(&filter, 3).await.unwrap();
        assert_eq!(rows.len(), 3);
    }

    #[tokio::test]
    async fn cost_summary_aggregates_by_agent() {
        let (store, _dir) = open_store().await;
        store
            .insert_event(&sample_llm_response_with_cost(
                "alpha",
                Uuid::now_v7(),
                0.10,
            ))
            .await
            .unwrap();
        store
            .insert_event(&sample_llm_response_with_cost(
                "alpha",
                Uuid::now_v7(),
                0.05,
            ))
            .await
            .unwrap();
        store
            .insert_event(&sample_llm_response_with_cost("beta", Uuid::now_v7(), 0.20))
            .await
            .unwrap();

        let summary = store.cost_summary(None, None).await.unwrap();
        assert_eq!(summary.len(), 2);

        let beta = summary.iter().find(|s| s.agent_id == "beta").unwrap();
        assert!((beta.total_cost - 0.20).abs() < 1e-9);
        assert_eq!(beta.event_count, 1);

        let alpha = summary.iter().find(|s| s.agent_id == "alpha").unwrap();
        assert!((alpha.total_cost - 0.15).abs() < 1e-9);
        assert_eq!(alpha.event_count, 2);
        assert_eq!(alpha.total_input_tokens, 200);
        assert_eq!(alpha.total_output_tokens, 100);
        assert_eq!(alpha.total_cache_read_tokens, 40);
        assert_eq!(alpha.total_cache_write_tokens, 20);
        // Two events on two distinct invocations.
        assert_eq!(alpha.invocation_count, 2);
    }

    /// The drill-down queries group the same cost-bearing rows by
    /// invocation and by model — no new columns, only new GROUP BYs.
    #[tokio::test]
    async fn cost_detail_groups_by_invocation_and_model() {
        let (store, _dir) = open_store().await;
        let inv1 = Uuid::now_v7();
        let inv2 = Uuid::now_v7();
        for (inv, cost) in [(inv1, 0.10), (inv1, 0.05), (inv2, 0.20)] {
            store
                .insert_event(&sample_llm_response_with_cost("alpha", inv, cost))
                .await
                .unwrap();
        }
        // Another agent's spend must not leak into alpha's drill-down.
        store
            .insert_event(&sample_llm_response_with_cost("beta", Uuid::now_v7(), 9.0))
            .await
            .unwrap();

        let invs = store.cost_by_invocation("alpha", None, 10).await.unwrap();
        assert_eq!(invs.len(), 2);
        // Newest first by each invocation's first cost event.
        assert!(
            invs[0].first_timestamp >= invs[1].first_timestamp,
            "rows must be newest-first: {invs:?}"
        );
        let one = invs
            .iter()
            .find(|r| r.invocation_id == inv1.to_string())
            .unwrap();
        assert_eq!(one.event_count, 2);
        assert!((one.total_cost - 0.15).abs() < 1e-9);
        assert_eq!(one.total_input_tokens, 200);
        assert_eq!(one.total_cache_read_tokens, 40);
        let two = invs
            .iter()
            .find(|r| r.invocation_id == inv2.to_string())
            .unwrap();
        assert_eq!(two.event_count, 1);
        assert!((two.total_cost - 0.20).abs() < 1e-9);

        // The cap holds.
        assert_eq!(
            store
                .cost_by_invocation("alpha", None, 1)
                .await
                .unwrap()
                .len(),
            1
        );

        // The single-invocation aggregate matches the grouped rows.
        let one = store
            .cost_of_invocation(&inv1.to_string())
            .await
            .unwrap()
            .expect("inv1 has cost events");
        assert_eq!(one.event_count, 2);
        assert!((one.total_cost - 0.15).abs() < 1e-9);
        assert!(
            store
                .cost_of_invocation("no-such-id")
                .await
                .unwrap()
                .is_none()
        );

        // All fixture events carry the same model → one row, summed.
        let models = store.cost_by_model(Some("alpha"), None).await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model, "claude-haiku-4-5");
        assert_eq!(models[0].event_count, 3);
        assert!((models[0].total_cost - 0.35).abs() < 1e-9);

        // Unfiltered, the same GROUP BY spans every agent — the
        // top-level costs page's by-model split.
        let all_models = store.cost_by_model(None, None).await.unwrap();
        assert_eq!(all_models.len(), 1);
        assert_eq!(all_models[0].event_count, 4);
        assert!((all_models[0].total_cost - 9.35).abs() < 1e-9);
    }

    /// Bucketing invariants that hold whatever the wall clock says:
    /// every cost event lands in exactly one bucket, bucket sums equal
    /// the grand total, hourly refines daily, and keys carry the
    /// fixed-width UTC prefix shape.
    #[tokio::test]
    async fn cost_buckets_partition_the_spend() {
        let (store, _dir) = open_store().await;
        for cost in [0.10, 0.05, 0.20] {
            store
                .insert_event(&sample_llm_response_with_cost(
                    "alpha",
                    Uuid::now_v7(),
                    cost,
                ))
                .await
                .unwrap();
        }
        let daily = store.cost_by_time_bucket(false, None).await.unwrap();
        let hourly = store.cost_by_time_bucket(true, None).await.unwrap();
        let day_sum: f64 = daily.iter().map(|b| b.total_cost).sum();
        let hour_sum: f64 = hourly.iter().map(|b| b.total_cost).sum();
        assert!((day_sum - 0.35).abs() < 1e-9, "{daily:?}");
        assert!((hour_sum - 0.35).abs() < 1e-9, "{hourly:?}");
        assert!(!daily.is_empty() && daily.len() <= hourly.len());
        for b in &daily {
            assert_eq!(b.bucket.len(), 10, "day key shape: {}", b.bucket);
        }
        for b in &hourly {
            assert_eq!(b.bucket.len(), 13, "hour key shape: {}", b.bucket);
            assert_eq!(b.bucket.as_bytes()[10], b'T');
        }
        // A `since` beyond every event excludes all buckets.
        let none = store
            .cost_by_time_bucket(false, Some("9999-01-01"))
            .await
            .unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn cost_summary_filters_by_agent() {
        let (store, _dir) = open_store().await;
        store
            .insert_event(&sample_llm_response_with_cost(
                "alpha",
                Uuid::now_v7(),
                0.10,
            ))
            .await
            .unwrap();
        store
            .insert_event(&sample_llm_response_with_cost("beta", Uuid::now_v7(), 0.20))
            .await
            .unwrap();

        let summary = store.cost_summary(Some("alpha"), None).await.unwrap();
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].agent_id, "alpha");
    }

    fn sample_failed_kind(agent: &str, inv: Uuid, kind: FailureKind) -> Event {
        Event::new(
            aid(agent),
            inv,
            EventPayload::Failed(FailedPayload {
                error_kind: kind,
                error_message: "boom".to_string(),
                phase: FailurePhase::LlmResponse,
                partial_totals: InvocationTotals::default(),
            }),
        )
    }

    #[tokio::test]
    async fn projected_failure_kinds_match_wire_serialization() {
        let (store, _dir) = open_store().await;
        let kinds = [
            FailureKind::BudgetExceeded,
            FailureKind::LlmError,
            FailureKind::MaxIterations,
            FailureKind::ToolError,
            FailureKind::SandboxViolation,
            FailureKind::RuntimeError,
            FailureKind::TriggerExhausted,
        ];
        for kind in kinds {
            let event = sample_failed_kind("a", Uuid::now_v7(), kind);
            let wire = serde_json::to_value(kind)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned();
            store.insert_event(&event).await.unwrap();
            let projected = store
                .query_events(&EventFilter::default(), 100)
                .await
                .unwrap();
            assert!(
                projected
                    .iter()
                    .any(|row| row.error_kind.as_deref() == Some(&wire))
            );
        }
    }

    #[tokio::test]
    async fn failure_summary_groups_by_kind() {
        let (store, _dir) = open_store().await;
        store
            .insert_event(&sample_failed_kind(
                "a",
                Uuid::now_v7(),
                FailureKind::BudgetExceeded,
            ))
            .await
            .unwrap();
        store
            .insert_event(&sample_failed_kind(
                "a",
                Uuid::now_v7(),
                FailureKind::BudgetExceeded,
            ))
            .await
            .unwrap();
        store
            .insert_event(&sample_failed_kind(
                "b",
                Uuid::now_v7(),
                FailureKind::ToolError,
            ))
            .await
            .unwrap();
        // A non-failed event must not be counted.
        store
            .insert_event(&sample_completed("a", Uuid::now_v7()))
            .await
            .unwrap();

        let summary = store.failure_summary().await.unwrap();
        let total: i64 = summary.iter().map(|s| s.count).sum();
        assert_eq!(total, 3);
        let budget = summary
            .iter()
            .find(|s| s.error_kind == "budget_exceeded")
            .unwrap();
        assert_eq!(budget.count, 2);
        let tool = summary
            .iter()
            .find(|s| s.error_kind == "tool_error")
            .unwrap();
        assert_eq!(tool.count, 1);
    }

    #[tokio::test]
    async fn failure_summary_empty_when_no_failures() {
        let (store, _dir) = open_store().await;
        store
            .insert_event(&sample_completed("a", Uuid::now_v7()))
            .await
            .unwrap();
        assert!(store.failure_summary().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn extract_fields_covers_all_event_types() {
        let (store, _dir) = open_store().await;
        let inv = Uuid::now_v7();
        store
            .insert_event(&sample_triggered("alpha", inv))
            .await
            .unwrap();
        store
            .insert_event(&sample_llm_request("alpha", inv))
            .await
            .unwrap();
        store
            .insert_event(&sample_llm_response("alpha", inv))
            .await
            .unwrap();
        store
            .insert_event(&sample_llm_response_with_cost("alpha", inv, 0.01))
            .await
            .unwrap();
        store
            .insert_event(&sample_completed("alpha", inv))
            .await
            .unwrap();
        store
            .insert_event(&sample_failed("alpha", Uuid::now_v7()))
            .await
            .unwrap();
        // No panic, all inserts succeed.
        assert_eq!(store.count().await.unwrap(), 6);
    }

    #[tokio::test]
    async fn failed_event_error_message_is_projected_and_returned() {
        let (store, _dir) = open_store().await;
        let invocation_id = Uuid::now_v7();
        store
            .insert_event(&sample_failed("alpha", invocation_id))
            .await
            .unwrap();

        let rows = store
            .query_events(&EventFilter::default(), 10)
            .await
            .unwrap();
        assert_eq!(rows[0].error_kind.as_deref(), Some("budget_exceeded"));
        assert_eq!(rows[0].error_message.as_deref(), Some("blew the budget"));
    }

    #[tokio::test]
    async fn read_only_open_fails_if_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent.db");
        let err = ProjectionStore::open_read_only(&path).await.unwrap_err();
        assert!(matches!(err, StoreError::NotInitialised(_)));
    }

    #[tokio::test]
    async fn read_only_open_succeeds_after_write_open() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("projection.db");
        {
            let writer = ProjectionStore::open(&path).await.unwrap();
            writer
                .insert_event(&sample_triggered("alpha", Uuid::now_v7()))
                .await
                .unwrap();
        }
        let reader = ProjectionStore::open_read_only(&path).await.unwrap();
        assert_eq!(reader.count().await.unwrap(), 1);
    }
}
