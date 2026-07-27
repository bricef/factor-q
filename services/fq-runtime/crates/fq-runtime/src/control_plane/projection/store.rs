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

    /// Delete projected events older than `cutoff_ms` (Unix epoch
    /// milliseconds), except cost-bearing rows. Returns the number of
    /// rows deleted.
    ///
    /// Cost accounting is a primary platform concern: rows with
    /// `total_cost` set (`llm_response`, `invocation_summary`) are
    /// retained indefinitely so all-time spend figures and
    /// per-invocation cost display survive retention. Everything the
    /// cost queries read filters on `total_cost IS NOT NULL`, so the
    /// exemption preserves them exactly.
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
                 (SELECT rowid FROM events \
                  WHERE timestamp < ? AND total_cost IS NULL LIMIT ?)",
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
        | EventPayload::InvocationOperatorRecovered(_)
        | EventPayload::InvocationOperatorResumed(_) => Fields::default(),
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
mod tests;
