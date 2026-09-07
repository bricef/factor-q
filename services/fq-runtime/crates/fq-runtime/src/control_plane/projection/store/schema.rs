//! The projection's SQL schema, its version, and what a bump does.
//!
//! ## Versioned by `user_version`; a bump is a rebuild, never a migration
//!
//! The projection is derived: every row is a fold of an event the
//! stream holds, or held. So its schema is **one `CREATE TABLE` block at
//! [`PROJECTION_SCHEMA_VERSION`]**, stamped into SQLite's `user_version`
//! pragma, and there is no migration ladder. When the version a file
//! records is older than this binary's, [`ProjectionStore::open`]
//! **rebuilds**: the projection tables are dropped and recreated at the
//! current version, the rows the retention sweep exempts are carried
//! across (below), and the durable consumer is marked for a reset so the
//! stream replays from the start of retention (`DeliverAll`) and
//! re-derives every row it still can. The verdict — fresh, current,
//! older, newer — is the shared kit's ([`crate::db::schema`]); a file
//! written by a *newer* binary is refused, as the other two stores
//! refuse theirs.
//!
//! A rebuild backfills where a migration cannot. `ALTER TABLE ADD
//! COLUMN` leaves every existing row NULL even when the event that row
//! indexes carries the value; the replay writes the value. That is the
//! whole reason a derived store evolves by re-deriving.
//!
//! ## The forward-only path, kept for one case
//!
//! [`ADDED_EVENT_COLUMNS`] and [`ADDED_TRIGGER_COLUMNS`] still add a
//! column by `ALTER` — but only to a database **whose version already
//! matches**. That is the right tool for a column whose history is
//! genuinely absent from the events (nothing to backfill, so a replay
//! would buy nothing); a column the events do carry gets a version bump
//! instead. Adding a column to the `CREATE TABLE` block without doing
//! one or the other leaves it missing from existing files, and
//! `verify_readable` names it.
//!
//! ## What a rebuild keeps
//!
//! Three kinds of row outlive the log they were folded from, by design:
//! cost-bearing `events` rows (`total_cost IS NOT NULL` — spend is kept
//! indefinitely, and past stream retention the projection is its only
//! copy), every `invocation_summary` line, and every `triggers` record.
//! A rebuild carries all three across into the recreated tables before
//! the replay starts. The replay then **refreshes** whichever of them
//! the stream still holds — `insert_event` is an upsert on `event_id` —
//! so history inside retention is re-derived whole, and history past it
//! keeps the shape it had. Nothing the sweep would have kept is lost to
//! a rebuild.
//!
//! ## Bumping the version
//!
//! Change the `CREATE TABLE` block, bump [`PROJECTION_SCHEMA_VERSION`],
//! and add a line to its version log. Every daemon rebuilds on its next
//! start; `fq status` reports the replay's progress. `fq projection
//! rebuild` performs the same rebuild on demand.

use std::path::Path;

use sqlx::{QueryBuilder, Sqlite};

use super::{ProjectionStore, StoreError};
use crate::db::schema::{Compatibility, check_compatibility, read_user_version, split_sql};

/// The projection schema version this binary writes and expects.
///
/// Versions:
/// - **v1** — the first stamped version: `events` with the `seq`,
///   cache, `error_message` and `reasoning_tokens` columns,
///   `invocation_summary`, and `triggers` with `requeued_from`. A file
///   from before versioning reads `user_version` 0 with its tables
///   present and is rebuilt on first open — which is what backfills
///   `reasoning_tokens` and the cache columns for every event the
///   stream still holds.
pub const PROJECTION_SCHEMA_VERSION: u32 = 1;

/// The tables a rebuild drops and recreates — the projection proper.
/// `projection_meta` is deliberately not among them: it records the
/// rebuild.
pub(super) const PROJECTION_TABLES: [&str; 3] = ["events", "invocation_summary", "triggers"];

/// The schema, whole, at [`PROJECTION_SCHEMA_VERSION`]. Run on a fresh
/// file and on a rebuild; `IF NOT EXISTS` keeps it idempotent on a file
/// that already matches.
pub(super) const SCHEMA_SQL: &str = r#"
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
    reasoning_tokens INTEGER,
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
-- aged out. A rebuild carries every row across for the same reason.
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
/// [`SCHEMA_SQL`]: on a current-version database created before the
/// column existed, `CREATE UNIQUE INDEX ... ON triggers(requeued_from)`
/// names a column that is not there yet, and the schema block runs
/// before the additive step that adds it.
pub(super) const TRIGGER_REQUEUE_INDEX_SQL: &str =
    "CREATE UNIQUE INDEX IF NOT EXISTS idx_triggers_requeued_from ON triggers(requeued_from)";

/// The columns `events` has gained by the forward-only path — added by
/// `ALTER` to a database whose version already matches, never by a
/// rebuild (see the module docs for when each is the right tool).
///
/// `CREATE TABLE` above already names every one of them, so a database
/// created by this build has them from the start. They are listed here
/// for the databases that were not: `CREATE TABLE IF NOT EXISTS` cannot
/// widen a table that already exists, so a same-version file that
/// predates a column needs it added.
///
/// One list, two consumers, and they must not drift.
/// [`ProjectionStore::ensure_schema`] adds whichever are absent, and
/// [`ProjectionStore::verify_readable`] checks for the same set on a
/// handle that cannot add anything. A column added to the table but
/// not to this list — and not covered by a version bump — would be
/// missing from old databases and unnoticed by both.
///
/// Every entry here predates versioning: the v1 stamp happened after
/// all five had shipped, so they are the columns a `user_version` 0
/// file may lack. Such a file is rebuilt, not altered, and gets them
/// from the `CREATE TABLE` block; the list still guards a v1 file that
/// somehow lost one.
const ADDED_EVENT_COLUMNS: [(&str, &str); 5] = [
    ("cache_read_tokens", "INTEGER"),
    ("cache_write_tokens", "INTEGER"),
    ("error_message", "TEXT"),
    // The log position this row indexes — where `event.get` reads the
    // payload once the identity has resolved here. Rows projected
    // before this column existed read NULL until a rebuild replays
    // them, which is why "we do not know where its payload is" is a
    // state `event.get` names rather than rounds down to "no such
    // event".
    ("seq", "INTEGER"),
    // The thought-versus-spoken split, where the provider reported one
    // (#536). NULL is the column's own meaning — "no split reported",
    // which is every Anthropic call. Rows from before the column read
    // NULL until a rebuild replays their events (the v1 rebuild is what
    // backfills them); a value the provider reported is never coalesced
    // to 0 on the way out.
    ("reasoning_tokens", "INTEGER"),
];

/// The same story for `triggers`: columns the table has gained since
/// step B created it, added to same-version databases that predate
/// them.
///
/// Two consumers, as above — [`ProjectionStore::ensure_schema`] adds
/// them, [`ProjectionStore::verify_readable`] checks for them — because
/// `TRIGGER_COLUMNS` selects every one of them on a handle that cannot
/// migrate.
const ADDED_TRIGGER_COLUMNS: [(&str, &str); 1] = [
    // The trigger a requeue re-ran. Forward-only: rows written before
    // requeues were recorded read NULL, which is exactly right — they
    // were not requeues.
    ("requeued_from", "TEXT"),
];

impl ProjectionStore {
    /// The version this file records, or `None` for a file that has no
    /// projection tables yet. `user_version` reads 0 on a file that was
    /// never stamped, so the tables are what tell a fresh file from a
    /// pre-versioning one — the latter has tables and needs a rebuild,
    /// the former needs a schema.
    pub(super) async fn recorded_version(&self) -> Result<Option<u32>, StoreError> {
        let tables: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'events'",
        )
        .fetch_one(&self.pool)
        .await?;
        if tables == 0 {
            return Ok(None);
        }
        Ok(Some(read_user_version(&self.pool).await?))
    }

    /// The `user_version` this file carries — what a test reads to
    /// prove a rebuild stamped it.
    pub async fn schema_version(&self) -> Result<u32, StoreError> {
        Ok(read_user_version(&self.pool).await?)
    }

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

    /// Bring the file to [`PROJECTION_SCHEMA_VERSION`] on the way in:
    /// create the schema on a fresh file, leave a current one alone
    /// (bar the forward-only columns), rebuild an older one, refuse a
    /// newer one. The module docs are the contract.
    pub(super) async fn ensure_schema(&self, path: &Path) -> Result<(), StoreError> {
        self.ensure_meta_table().await?;
        let recorded = self.recorded_version().await?;
        match check_compatibility(recorded, PROJECTION_SCHEMA_VERSION) {
            Compatibility::FreshInstall => {
                self.create_schema().await?;
                // A file that did not exist a moment ago must not
                // inherit a durable's position: if the durable is
                // still there from an earlier life of this store, its
                // acked floor would leave everything before it out of
                // the new file for good. The consumer resets it before
                // it reads (see `rebuild::reset_projection_consumer`).
                self.request_consumer_reset(super::rebuild::FRESH_FILE_REASON)
                    .await?;
            }
            Compatibility::Current => {
                // Idempotent on a file that already matches, and what
                // gives a same-version file a table it lacks: the
                // legacy split copies only the tables the v1 file had,
                // so a copy stamped current can still be short of
                // `triggers`.
                for statement in split_sql(SCHEMA_SQL) {
                    sqlx::query(statement).execute(&self.pool).await?;
                }
                // The forward-only path: same version, a column added
                // by `ALTER`, history NULL. See the module docs for
                // when that is the right tool.
                self.add_missing_columns("events", &ADDED_EVENT_COLUMNS)
                    .await?;
                self.add_missing_columns("triggers", &ADDED_TRIGGER_COLUMNS)
                    .await?;
                // Only now, with the column guaranteed present on old
                // databases as well as new ones — see
                // [`TRIGGER_REQUEUE_INDEX_SQL`].
                sqlx::query(TRIGGER_REQUEUE_INDEX_SQL)
                    .execute(&self.pool)
                    .await?;
                // Sweep the transients (cheap once empty via the type
                // index): they stopped being projected — see
                // `insert_event` — and this evicts what older builds
                // accumulated. Derived from `events::transient`, so
                // adding a type there needs no edit here.
                for event_type in crate::events::transient::types() {
                    sqlx::query("DELETE FROM events WHERE event_type = ?")
                        .bind(event_type)
                        .execute(&self.pool)
                        .await?;
                }
            }
            Compatibility::NeedsUpgrade { from } => {
                self.rebuild_tables(super::rebuild::RebuildReason::SchemaUpgrade { from })
                    .await?;
            }
            Compatibility::BinaryTooOld { db_version } => {
                return Err(StoreError::IncompatibleSchema {
                    path: path.to_path_buf(),
                    db_version,
                    binary_version: PROJECTION_SCHEMA_VERSION,
                });
            }
        }
        Ok(())
    }

    /// The schema at the current version on an empty file, stamped.
    async fn create_schema(&self) -> Result<(), StoreError> {
        for statement in split_sql(SCHEMA_SQL) {
            sqlx::query(statement).execute(&self.pool).await?;
        }
        sqlx::query(TRIGGER_REQUEUE_INDEX_SQL)
            .execute(&self.pool)
            .await?;
        crate::db::schema::write_user_version(&self.pool, PROJECTION_SCHEMA_VERSION).await?;
        Ok(())
    }

    /// Check that this database is one the read path can serve, on a
    /// handle that cannot change it.
    ///
    /// [`ProjectionStore::open`] brings a file forward on the way in;
    /// [`ProjectionStore::open_read_only`] cannot, and should not — it
    /// exists so a file can be read while a daemon owns it, and
    /// rebuilding under that daemon is the opposite of what it is for.
    /// So it checks instead, and fails while it still has the context
    /// to say what is wrong: first the version — a newer binary's file
    /// is refused, an older one is named as needing the daemon's
    /// rebuild — then the forward-only columns.
    ///
    /// Without the check the failure still happens, just later and
    /// further from the cause: the first query naming a missing column
    /// returns a driver error about SQL the operator never wrote, from
    /// whichever verb happened to ask first. `fq costs` selects the
    /// cache and reasoning columns and `event.get` selects `seq`, so
    /// which error you get depends on what you ran.
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

        let recorded = read_user_version(&self.pool).await?;
        match check_compatibility(Some(recorded), PROJECTION_SCHEMA_VERSION) {
            Compatibility::Current | Compatibility::FreshInstall => {}
            Compatibility::NeedsUpgrade { from } => {
                return Err(StoreError::SchemaOutdated {
                    path: path.to_path_buf(),
                    missing: format!(
                        "schema version {PROJECTION_SCHEMA_VERSION} (the file records version \
                         {from}; the daemon rebuilds it from the event stream on start)"
                    ),
                });
            }
            Compatibility::BinaryTooOld { db_version } => {
                return Err(StoreError::IncompatibleSchema {
                    path: path.to_path_buf(),
                    db_version,
                    binary_version: PROJECTION_SCHEMA_VERSION,
                });
            }
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
