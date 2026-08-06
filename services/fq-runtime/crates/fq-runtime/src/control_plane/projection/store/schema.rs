//! The projection's SQL schema and its migration runner.
//!
//! Split out of the parent module for file size. The `impl
//! ProjectionStore` block below is part of the same inherent impl, so
//! `open` calls `run_migrations` exactly as it did when both lived in
//! one file.

use super::{ProjectionStore, StoreError};

/// Schema — migrations live inline for phase 1. When the schema
/// evolves beyond trivial additions, switch to `sqlx::migrate!`.
const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS events (
    event_id        TEXT PRIMARY KEY,
    seq             INTEGER,
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

impl ProjectionStore {
    pub(super) async fn run_migrations(&self) -> Result<(), StoreError> {
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
            // The log position this row indexes — where `event.get`
            // reads the payload once the identity has resolved here.
            // Forward-only like the rest: rows projected before this
            // column existed read NULL, which is why "we do not know
            // where its payload is" is a state `event.get` names
            // rather than rounds down to "no such event".
            ("seq", "INTEGER"),
        ] {
            if !columns.iter().any(|c| c == column) {
                sqlx::query(&format!("ALTER TABLE events ADD COLUMN {column} {ty}"))
                    .execute(&self.pool)
                    .await?;
            }
        }
        // Sweep the transients (cheap once empty via the type index):
        // they stopped being projected — see `insert_event` — and this
        // evicts what older builds accumulated. Derived from
        // `events::transient`, so adding a type there needs no edit here.
        for event_type in crate::events::transient::types() {
            sqlx::query("DELETE FROM events WHERE event_type = ?")
                .bind(event_type)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }
}
