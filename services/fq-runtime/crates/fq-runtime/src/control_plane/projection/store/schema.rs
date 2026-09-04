//! The projection's SQL schema and its migration runner.
//!
//! Split out of the parent module for file size. The `impl
//! ProjectionStore` block below is part of the same inherent impl, so
//! `open` calls `run_migrations` exactly as it did when both lived in
//! one file.

use std::path::Path;

use sqlx::{QueryBuilder, Sqlite};

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

-- A trigger's permanent home. Projected from the two events that name
-- one (see `Trigger::from_event`) and, unlike `events`, NEVER SWEPT: a
-- trigger is a key domain event and its retention is indefinite. The
-- exemption is structural rather than a predicate -- `sweep_events`
-- only ever deletes from `events` -- which is the same way
-- `invocation_summary` above survives, and the same intent as the
-- `total_cost IS NOT NULL` exemption that keeps spend after its log has
-- aged out.
--
-- `payload` holds the trigger body verbatim, so a Get needs no second
-- hop and nothing can be listed and then found missing. It is bounded
-- at accept time by `MAX_TRIGGER_PAYLOAD_BYTES`. THE SEAM: when the CAS
-- object store lands, this column becomes a content address and the
-- body moves there -- the row shape and every query below are otherwise
-- unchanged, because nothing here reads inside the payload.
--
-- `seq` is the log position of the record that named the trigger -- the
-- universal cursor (P5), what `trigger.stream` resumes from. NULL when
-- the delivery carried no JetStream metadata.
--
-- `requeued_from` names the trigger this one was requeued from, and is
-- NULL for every trigger that is not a requeue. It is `dead_letter.
-- requeue`'s idempotency key -- see the UNIQUE index below, which is
-- created after the column migration rather than here. (No semicolons
-- in these comments -- the schema runner splits statements on them.)
CREATE TABLE IF NOT EXISTS triggers (
    trigger_id      TEXT PRIMARY KEY,
    recorded_at     TEXT NOT NULL,
    agent_id        TEXT NOT NULL,
    source          TEXT NOT NULL,
    subject         TEXT,
    payload         TEXT NOT NULL,
    seq             INTEGER,
    requeued_from   TEXT
);

CREATE INDEX IF NOT EXISTS idx_triggers_agent_time ON triggers(agent_id, recorded_at);
CREATE INDEX IF NOT EXISTS idx_triggers_time ON triggers(recorded_at);
CREATE INDEX IF NOT EXISTS idx_triggers_seq ON triggers(seq);
"#;

/// The index that makes "a dead letter is requeued at most once" a
/// property of the database rather than of a check the caller
/// remembered to run.
///
/// UNIQUE, and SQLite lets any number of rows hold NULL in a unique
/// index — so this constrains requeues alone and every ordinary trigger
/// is untouched. `ProjectionStore::reserve_requeue` inserts against it
/// and reads its own success as the claim.
///
/// It is applied **after** [`ADDED_TRIGGER_COLUMNS`] rather than inside
/// [`SCHEMA_SQL`]: on a database created before the column existed,
/// `CREATE UNIQUE INDEX ... ON triggers(requeued_from)` names a column
/// that is not there yet, and the schema block runs before the
/// additive migration that adds it.
const TRIGGER_REQUEUE_INDEX_SQL: &str =
    "CREATE UNIQUE INDEX IF NOT EXISTS idx_triggers_requeued_from ON triggers(requeued_from)";

/// The columns `events` has gained since its first shape.
///
/// `CREATE TABLE` above already names all four, so a database created
/// by this build has them from the start. They are listed here for the
/// databases that were not: `CREATE TABLE IF NOT EXISTS` cannot widen
/// a table that already exists, so an older file needs them added.
///
/// One list, two consumers, and they must not drift.
/// [`ProjectionStore::run_migrations`] adds whichever are absent, and
/// [`ProjectionStore::verify_readable`] checks for the same set on a
/// handle that cannot add anything. A column added to the table but
/// not to this list would be missing from old databases and unnoticed
/// by both.
const ADDED_EVENT_COLUMNS: [(&str, &str); 4] = [
    ("cache_read_tokens", "INTEGER"),
    ("cache_write_tokens", "INTEGER"),
    ("error_message", "TEXT"),
    // The log position this row indexes — where `event.get` reads the
    // payload once the identity has resolved here. Forward-only like
    // the rest: rows projected before this column existed read NULL,
    // which is why "we do not know where its payload is" is a state
    // `event.get` names rather than rounds down to "no such event".
    ("seq", "INTEGER"),
];

/// The same story for `triggers`: columns the table has gained since
/// step B created it, added to databases that predate them.
///
/// Two consumers, as above — [`ProjectionStore::run_migrations`] adds
/// them, [`ProjectionStore::verify_readable`] checks for them — because
/// `TRIGGER_COLUMNS` selects every one of them on a handle that cannot
/// migrate.
const ADDED_TRIGGER_COLUMNS: [(&str, &str); 1] = [
    // The trigger a requeue re-ran. Forward-only like the rest: rows
    // written before requeues were recorded read NULL, which is exactly
    // right — they were not requeues.
    ("requeued_from", "TEXT"),
];

impl ProjectionStore {
    /// Add each of `columns` that `table` does not yet have. Existence-
    /// checked via `pragma_table_info` (deterministic and idempotent)
    /// rather than matching driver error text. DDL cannot take
    /// identifiers as parameters, so the statement is composed — from
    /// `'static` names only, which the signature enforces: nothing read
    /// at runtime can reach it.
    async fn add_missing_columns(
        &self,
        table: &'static str,
        columns: &[(&'static str, &'static str)],
    ) -> Result<(), StoreError> {
        let present: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info(?)")
            .bind(table)
            .fetch_all(&self.pool)
            .await?;
        for &(column, ty) in columns {
            if present.iter().any(|c| c == column) {
                continue;
            }
            let mut ddl = QueryBuilder::<Sqlite>::new("ALTER TABLE ");
            ddl.push(table)
                .push(" ADD COLUMN ")
                .push(column)
                .push(' ')
                .push(ty);
            ddl.build().execute(&self.pool).await?;
        }
        Ok(())
    }

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
        // table, so add these additively.
        //
        // FORWARD-ONLY: the projection is not reprojected here, so rows
        // written before this migration read NULL (0 through the
        // `COALESCE(SUM(...))` aggregation) even though the source
        // `llm.response` events carry the counts. `fq costs` therefore
        // reports cache usage only from this migration forward. A proper
        // projection-versioning + reproject story backfills history —
        // tracked in #139 (the phase-1 inline-schema comment above is
        // now overdue).
        self.add_missing_columns("events", &ADDED_EVENT_COLUMNS)
            .await?;
        self.add_missing_columns("triggers", &ADDED_TRIGGER_COLUMNS)
            .await?;
        // Only now, with the column guaranteed present on old databases
        // as well as new ones — see [`TRIGGER_REQUEUE_INDEX_SQL`].
        sqlx::query(TRIGGER_REQUEUE_INDEX_SQL)
            .execute(&self.pool)
            .await?;
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

    /// Check that this database has the columns the read path selects,
    /// on a handle that cannot add them.
    ///
    /// [`ProjectionStore::open`] migrates on the way in;
    /// [`ProjectionStore::open_read_only`] cannot, and should not — it
    /// exists so a file can be read while a daemon owns it, and
    /// migrating under that daemon is the opposite of what it is for.
    /// So it checks instead, and fails while it still has the context
    /// to say what is wrong.
    ///
    /// Without the check the failure still happens, just later and
    /// further from the cause: the first query naming a missing column
    /// returns a driver error about SQL the operator never wrote, from
    /// whichever verb happened to ask first. `fq costs` selects the
    /// two cache columns and `event.get` selects `seq`, so which error
    /// you get depends on what you ran.
    pub(super) async fn verify_readable(&self, path: &Path) -> Result<(), StoreError> {
        let columns: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('events')")
                .fetch_all(&self.pool)
                .await?;

        // `pragma_table_info` answers with no rows for a table that is
        // not there at all, which is a different state and already has
        // a name: the file exists but nothing was ever projected into
        // it. Reporting that as "missing columns" would send an
        // operator looking for an upgrade they do not need.
        if columns.is_empty() {
            return Err(StoreError::NotInitialised(path.to_path_buf()));
        }

        let mut missing: Vec<&str> = ADDED_EVENT_COLUMNS
            .iter()
            .map(|(column, _)| *column)
            .filter(|column| !columns.iter().any(|have| have == column))
            .collect();
        // A table rather than a column, checked in the same breath and
        // for the same reason: `trigger.get` selects from it, so a
        // handle that cannot migrate must name the upgrade it needs
        // instead of letting SQLite report an unknown table from SQL
        // the operator never wrote.
        let triggers: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('triggers')")
                .fetch_all(&self.pool)
                .await?;
        if triggers.is_empty() {
            missing.push("the triggers table");
        } else {
            // Present but older: the same forward-only story the event
            // columns have, and `TRIGGER_COLUMNS` selects these too.
            missing.extend(
                ADDED_TRIGGER_COLUMNS
                    .iter()
                    .map(|(column, _)| *column)
                    .filter(|column| !triggers.iter().any(|have| have == column)),
            );
        }
        if !missing.is_empty() {
            return Err(StoreError::SchemaOutdated {
                path: path.to_path_buf(),
                missing: missing.join(", "),
            });
        }
        Ok(())
    }
}
