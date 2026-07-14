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

use crate::events::{Event, EventPayload};

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
    duration_ms     INTEGER
);

CREATE INDEX IF NOT EXISTS idx_events_agent_time ON events(agent_id, timestamp);
CREATE INDEX IF NOT EXISTS idx_events_invocation ON events(invocation_id);
CREATE INDEX IF NOT EXISTS idx_events_type_time ON events(event_type, timestamp);
CREATE INDEX IF NOT EXISTS idx_events_time ON events(timestamp);
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
        for column in ["cache_read_tokens", "cache_write_tokens"] {
            if !columns.iter().any(|c| c == column) {
                sqlx::query(&format!("ALTER TABLE events ADD COLUMN {column} INTEGER"))
                    .execute(&self.pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Insert an event into the store. Idempotent on `event_id` —
    /// re-delivery from a durable consumer is a no-op.
    pub async fn insert_event(&self, event: &Event) -> Result<(), StoreError> {
        let fields = extract_fields(event);
        let event_type = event_type_name(&event.payload);

        sqlx::query(
            r#"
            INSERT OR IGNORE INTO events
                (event_id, timestamp, agent_id, invocation_id, event_type,
                 model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, total_cost, error_kind, duration_ms)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
        .bind(fields.duration_ms)
        .execute(&self.pool)
        .await?;

        Ok(())
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
             model, total_cost, error_kind, duration_ms \
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
                duration_ms: row.get::<Option<i64>, _>(8),
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
        let row = sqlx::query("SELECT agent_id FROM events WHERE invocation_id = ? LIMIT 1")
            .bind(invocation_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<String, _>(0)))
    }

    /// Aggregate cost-bearing events into per-agent totals. Cost
    /// now rides on `llm.response` envelopes (envelope-refactor
    /// plan step 3), so the filter is `total_cost IS NOT NULL`
    /// instead of `event_type = 'cost'`.
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
             WHERE event_type = 'llm_response' AND total_cost IS NOT NULL",
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
             WHERE event_type = 'llm_response' AND total_cost IS NOT NULL \
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

    /// One agent's cost-bearing events grouped per model, biggest
    /// spender first. See [`Self::cost_by_invocation`] for the shared
    /// filter rationale.
    pub async fn cost_by_model(
        &self,
        agent: &str,
        since: Option<&str>,
    ) -> Result<Vec<ModelCostSummary>, StoreError> {
        let mut sql = String::from(
            "SELECT COALESCE(model, 'unknown') AS model, \
             COUNT(*) AS event_count, \
             COALESCE(SUM(total_cost), 0.0) AS total_cost, \
             COALESCE(SUM(input_tokens), 0) AS total_input_tokens, \
             COALESCE(SUM(output_tokens), 0) AS total_output_tokens \
             FROM events \
             WHERE event_type = 'llm_response' AND total_cost IS NOT NULL \
             AND agent_id = ?",
        );
        if since.is_some() {
            sql.push_str(" AND timestamp >= ?");
        }
        sql.push_str(" GROUP BY model ORDER BY total_cost DESC");

        let mut q = sqlx::query(&sql).bind(agent);
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
    /// (lowercased `Debug` of the `FailureKind`, e.g. `budgetexceeded`),
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
    /// (`budgetexceeded`, `llmerror`, `maxiterations`, `toolerror`,
    /// `sandboxviolation`, `runtimeerror`), or `unknown` for a
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
    duration_ms: Option<i64>,
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
        EventPayload::ToolCall(_) => Fields::default(),
        EventPayload::ToolDispatched(_) => Fields::default(),
        EventPayload::LlmDispatched(_) => Fields::default(),
        EventPayload::HostNotice(_) => Fields::default(),
        EventPayload::InvocationAmbiguous(_) => Fields::default(),
        EventPayload::InvocationArchived(_) => Fields::default(),
        EventPayload::InvocationArchiveAcked(_) => Fields::default(),
        EventPayload::ToolResult(p) => Fields {
            error_kind: p.error_kind.map(|k| format!("{k:?}").to_lowercase()),
            duration_ms: Some(p.duration_ms as i64),
            ..Default::default()
        },
        EventPayload::Completed(p) => Fields {
            total_cost: Some(p.total_cost),
            duration_ms: Some(p.total_duration_ms as i64),
            ..Default::default()
        },
        EventPayload::Failed(p) => Fields {
            error_kind: Some(format!("{:?}", p.error_kind).to_lowercase()),
            duration_ms: Some(p.partial_totals.total_duration_ms as i64),
            total_cost: Some(p.partial_totals.total_cost),
            ..Default::default()
        },
        // System events carry no agent metadata. The projection
        // still records them for visibility (useful for "when did
        // the daemon restart" queries), but every denormalised
        // column is NULL.
        EventPayload::SystemStartup(_)
        | EventPayload::SystemShutdown(_)
        | EventPayload::SystemTaskFailed(_)
        | EventPayload::SystemRecovery(_)
        | EventPayload::WorkerHeartbeat(_)
        | EventPayload::McpServerLog(_)
        | EventPayload::InvocationOperatorRecovered(_) => Fields::default(),
    }
}

fn event_type_name(payload: &EventPayload) -> &'static str {
    match payload {
        EventPayload::Triggered(_) => "triggered",
        EventPayload::LlmRequest(_) => "llm_request",
        EventPayload::LlmDispatched(_) => "llm_dispatched",
        EventPayload::LlmResponse(_) => "llm_response",
        EventPayload::ToolCall(_) => "tool_call",
        EventPayload::ToolDispatched(_) => "tool_dispatched",
        EventPayload::ToolResult(_) => "tool_result",
        EventPayload::Completed(_) => "completed",
        EventPayload::Failed(_) => "failed",
        EventPayload::HostNotice(_) => "host_notice",
        EventPayload::InvocationAmbiguous(_) => "invocation_ambiguous",
        EventPayload::InvocationArchived(_) => "invocation_archived",
        EventPayload::InvocationArchiveAcked(_) => "invocation_archive_acked",
        EventPayload::InvocationOperatorRecovered(_) => "invocation_operator_recovered",
        EventPayload::SystemStartup(_) => "system_startup",
        EventPayload::SystemShutdown(_) => "system_shutdown",
        EventPayload::SystemTaskFailed(_) => "system_task_failed",
        EventPayload::SystemRecovery(_) => "system_recovery",
        EventPayload::WorkerHeartbeat(_) => "worker_heartbeat",
        EventPayload::McpServerLog(_) => "mcp_server_log",
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
        let path = dir.path().join("events.db");
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
        let path = dir.path().join("events.db");
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

        // All fixture events carry the same model → one row, summed.
        let models = store.cost_by_model("alpha", None).await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model, "claude-haiku-4-5");
        assert_eq!(models[0].event_count, 3);
        assert!((models[0].total_cost - 0.35).abs() < 1e-9);
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
            .find(|s| s.error_kind == "budgetexceeded")
            .unwrap();
        assert_eq!(budget.count, 2);
        let tool = summary
            .iter()
            .find(|s| s.error_kind == "toolerror")
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
    async fn read_only_open_fails_if_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent.db");
        let err = ProjectionStore::open_read_only(&path).await.unwrap_err();
        assert!(matches!(err, StoreError::NotInitialised(_)));
    }

    #[tokio::test]
    async fn read_only_open_succeeds_after_write_open() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.db");
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
