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

use super::fields::{extract_fields, summary_kind_name};
use crate::agent::AgentId;
use crate::events::{Event, EventPayload};

mod costs;
mod schema;
mod triggers;

// Explicit rather than a glob: the moved row types keep their public
// paths (`...projection::store::CostSummary`), and naming each one keeps
// this module's surface readable at its declaration site.
pub use self::costs::{
    CostBucketSummary, CostSummary, FailureSummary, InvocationCostSummary, ModelCostSummary,
};

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
    ///
    /// Unlike [`Self::open`], this cannot migrate what it finds, so it
    /// verifies instead (`verify_readable`, in the `schema` module). A
    /// file written by an older build is rejected here, by name,
    /// rather than surfacing later as a driver error from whichever
    /// query first mentions a column that is not there.
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
        let store = Self { pool };
        store.verify_readable(path).await?;
        Ok(store)
    }

    /// Delete projected events older than `cutoff_ms` (Unix epoch
    /// milliseconds), except cost-bearing rows. Returns the number of
    /// rows deleted.
    ///
    /// Cost accounting is a primary platform concern: rows with
    /// `total_cost` set (`llm_response`, `llm_failure`,
    /// `invocation_summary`) are
    /// retained indefinitely so all-time spend figures and
    /// per-invocation cost display survive retention. Everything the
    /// cost queries read filters on `total_cost IS NOT NULL`, so the
    /// exemption preserves them exactly.
    ///
    /// **Triggers are exempt too, structurally rather than by
    /// predicate**: a trigger's record lives in `triggers`, and this
    /// deletes only from `events`. Same intent as the cost exemption —
    /// a key domain fact outliving the log it was recorded on — reached
    /// without a second clause to keep in step. `invocation_summary` is
    /// untouched for the same reason.
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
    /// `seq` is the event's position in the log this row indexes — an
    /// internal locator, never an identity. `event.get` takes the
    /// `event_id` and resolves it *through* this column
    /// ([`ProjectionStore::event_location`]) to read the payload back
    /// out of the log, so the number never crosses the wire. `None`
    /// says we do not know the position — a fixture seeding the index
    /// directly, or a row from before the column existed. Such a row
    /// still lists; its payload just cannot be located.
    ///
    /// **Transient events are NOT projected**: which types, and why,
    /// is [`crate::events::transient`] — the same list `event.stream`
    /// excludes, so the surface's two reads answer one population.
    pub async fn insert_event(&self, event: &Event, seq: Option<u64>) -> Result<(), StoreError> {
        if event.payload.is_transient() {
            return Ok(());
        }
        let fields = extract_fields(event);
        let event_type = event.payload.event_type();

        sqlx::query(
            r#"
            INSERT OR IGNORE INTO events
                (event_id, seq, timestamp, agent_id, invocation_id, event_type,
                 model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, total_cost, error_kind, error_message, duration_ms)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(event.envelope.event_id.to_string())
        .bind(seq.map(|s| s as i64))
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

        // An event that names a trigger additionally writes that
        // trigger's own permanent record (`triggers`), which the sweep
        // never reaches — see the module's schema. Same shape as the
        // summary line below: one event, one row in `events`, and a
        // second table maintained beside it.
        self.insert_trigger(event, seq).await?;

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

    /// Where one event's payload sits in the log — `event.get`'s
    /// first hop, resolving an identity to a position the log can
    /// then be read at. A primary-key lookup (`event_id` is the
    /// table's key), so an identity costs no more to resolve than the
    /// raw sequence it replaced; see [`EventLocation`] for why the
    /// answer has three states.
    pub async fn event_location(&self, event_id: &str) -> Result<EventLocation, StoreError> {
        let row = sqlx::query("SELECT seq FROM events WHERE event_id = ?")
            .bind(event_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(match row.map(|r| r.get::<Option<i64>, _>(0)) {
            None => EventLocation::Unindexed,
            Some(None) => EventLocation::Unlocated,
            Some(Some(seq)) => EventLocation::At(seq as u64),
        })
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
}

/// Where the projection says one event's payload sits in the log —
/// [`ProjectionStore::event_location`]'s answer. Three states rather
/// than an `Option`, because two of them are different kinds of "you
/// cannot have the payload", and a caller that cannot tell them apart
/// cannot tell "I asked wrongly" from "the fact is no longer whole".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventLocation {
    /// No such row: the identity names no event this index has seen.
    Unindexed,
    /// The row is here, and so is its payload's log position — though
    /// whether the log still *holds* it is the log's answer, not this
    /// index's.
    At(u64),
    /// The row is here; where its payload sits was never recorded.
    /// The event is known, its payload is not addressable.
    Unlocated,
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

    #[error("projection database not initialised at {0} (has `fqd` been started?)")]
    NotInitialised(PathBuf),

    /// A database written by an older build, opened by a handle that
    /// cannot bring it forward. Named rather than left to surface as a
    /// driver error, because the fix is an action the operator takes
    /// and not something to deduce from missing SQL.
    #[error(
        "projection database at {path} was written by an older build and is missing \
         {missing}. A read-only handle cannot migrate it — start `fqd` once against \
         this state directory and it will bring the schema forward."
    )]
    SchemaOutdated { path: PathBuf, missing: String },
}

impl From<sqlx::Error> for StoreError {
    fn from(err: sqlx::Error) -> Self {
        StoreError::Backend(err.to_string())
    }
}

#[cfg(test)]
mod tests;
