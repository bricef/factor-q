//! The SQLite schema-versioning kit every versioned store is built on.
//!
//! Three stores, one contract. [`crate::worker::WorkerStore`] and
//! [`crate::control_plane::ControlPlaneStore`] record their version in a
//! `schema_meta` row keyed by *schema class* and migrate forward on open;
//! [`crate::control_plane::projection::ProjectionStore`] records its
//! version in SQLite's `user_version` pragma and rebuilds from the event
//! stream on a bump. The pieces they share — the compatibility verdict,
//! the statement splitter, the `schema_meta` handling and the migration
//! ladder — used to be copy-pasted between the first two, which meant a
//! fix to the refuse-if-newer check needed two edits and the third store
//! could not adopt it without a third copy. They live here now.
//!
//! ## The contract
//!
//! On open, a store reads the version its file records and compares it
//! with the version the binary was built for ([`check_compatibility`]):
//!
//! - **No recorded version** — a fresh file. Create the schema at the
//!   binary's version and record it.
//! - **Recorded equals binary** — up to date. Nothing runs: not every
//!   migration is idempotent (`ALTER TABLE ADD COLUMN` fails on a second
//!   run), so a current store is left exactly as it is.
//! - **Recorded is older** — migrate forward, one ladder rung at a
//!   time, then record the binary's version.
//! - **Recorded is newer** — **refuse to open** ([`SchemaError::BinaryTooOld`]).
//!   A newer binary wrote this file, and an older one cannot know what
//!   it would be misreading or dropping. This is the refuse-and-flag
//!   contract of `data-architecture.md` §5.6: a deploy that rolls a
//!   binary *back* across a schema bump has to roll the store back too,
//!   or delete it, and the store's own error says which.
//!
//! ## One lock, one transaction
//!
//! The whole of that — read the version, act on the verdict, record the
//! new version — runs under [`with_schema_lock`]: on one connection of
//! its own, inside one `BEGIN IMMEDIATE` transaction taken *before* the
//! version is read. Two properties follow, and every schema step in
//! this crate relies on both:
//!
//! - **Atomic.** SQLite DDL is transactional, so the rungs and the
//!   version that describes them commit together or not at all. An
//!   upgrade interrupted anywhere — an error, a crash, a kill — leaves
//!   the file at its old version with nothing applied, and the next open
//!   runs the same rungs again from a clean start. Before this, each
//!   statement committed on its own: a process that died after a rung's
//!   `ALTER TABLE ADD COLUMN` and before the version write left a file
//!   that failed every later open with `duplicate column name`.
//! - **Serialised.** `BEGIN IMMEDIATE` takes SQLite's write lock on the
//!   file, which is a lock between *connections*, so it holds across
//!   processes (two daemons, a daemon and a tool) as well as within one.
//!   A second opener waits for the first to commit, then reads the
//!   version the first wrote and finds nothing to do. Before this, two
//!   openers of a fresh file both read "no version", both ran the
//!   ladder, and the second failed on the first `ADD COLUMN`.
//!
//! The wait is bounded by [`SCHEMA_LOCK_WAIT`], set on the step's own
//! connection rather than inherited from the store's pool, whose busy
//! timeout is sized for ordinary writes. An opener that cannot get the
//! lock in that time fails with SQLite's `database is locked`.
//!
//! Nothing a schema step runs is refused or mishandled inside a
//! transaction. The one statement that would be — `PRAGMA journal_mode`,
//! which cannot change into or out of WAL mid-transaction — is issued by
//! sqlx when each connection is established, before any transaction
//! begins, and no ladder or schema block repeats it. `user_version` is
//! transactional (it lives in the file header), so the projection's
//! stamp commits with its schema. No rung uses `VACUUM`, `ATTACH`, a
//! `PRAGMA` or a transaction-control statement; a rung that needed one
//! could not run inside the step's transaction, and would have to be
//! designed as a step of its own rather than appended to a ladder.
//!
//! That leaves one window outside the lock: the switch into WAL mode
//! when a file is first created. It needs an exclusive lock that a busy
//! timeout cannot wait for, so two processes creating the same file in
//! the same instant can still see one fail that switch with `database
//! is locked`. Once the file exists in WAL mode the switch only reads
//! the header and never contends.
//!
//! ## Adding a migration to a `schema_meta` store
//!
//! Bump the store's version constant, append a `(version, SQL)` rung to
//! its migration ladder, and say in the constant's doc what the rung
//! changes. [`apply_migrations`] runs every rung above the recorded
//! version and at or below the binary's, in order, so a file two
//! versions behind takes both rungs. A rung's SQL may hold several
//! statements; [`split_sql`] runs them one at a time so a failure names
//! the statement.
//!
//! The projection has no ladder: its schema is one `CREATE TABLE` block
//! at the current version, and a bump is answered by a rebuild rather
//! than a migration (its `schema` module says why). Its open-time step
//! and its on-demand rebuild run under [`with_schema_lock`] all the
//! same.
//!
//! `fq-store` (a separate workspace) keeps its own pool and pragma
//! bootstrap in `grant_log.rs`/`index.rs`; it is a different crate with
//! a different lifecycle and is deliberately not ported onto this kit.

use std::time::Duration;

use sqlx::sqlite::SqliteConnection;
use sqlx::{ConnectOptions, Connection, Executor, Pool, Row, Sqlite};

#[cfg(test)]
pub(crate) mod test_seam;
#[cfg(test)]
mod tests;

/// How long a schema step waits for SQLite's write lock before it gives
/// up — and so how long a second opener waits for a first one's
/// bootstrap to commit.
///
/// A ladder applies in milliseconds; the longest step that can hold the
/// lock is a projection rebuild, which carries every row of a large file
/// across and can take seconds. Thirty seconds outlasts that on a loaded
/// machine, and is still short enough that a holder which never lets go
/// (a process stopped mid-transaction) surfaces as a failed open naming
/// the lock rather than a daemon that hangs at startup.
pub const SCHEMA_LOCK_WAIT: Duration = Duration::from_secs(30);

/// The version table shared by the `schema_meta`-versioned stores: one
/// row per schema class. Created with `IF NOT EXISTS` on every open, so
/// two stores bootstrapping the same file (the pre-split `events.db`)
/// never raced on it.
pub const SCHEMA_META_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_meta (
    class       TEXT PRIMARY KEY,
    version     INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
"#;

/// One rung of a migration ladder: the version it brings the schema to,
/// and the SQL that does it.
pub type Migration = (u32, &'static str);

/// Outcome of comparing the binary's expected schema version against
/// what the database has recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compatibility {
    /// No recorded version — first time this binary has touched the
    /// file.
    FreshInstall,
    /// Recorded version equals the binary's expected version.
    Current,
    /// Recorded version is older than the binary's. Bring it forward.
    NeedsUpgrade { from: u32 },
    /// Recorded version is newer than the binary supports. Refuse and
    /// surface the case to the operator.
    BinaryTooOld { db_version: u32 },
}

/// Pure compatibility check, exposed for unit testing without needing
/// a database.
pub fn check_compatibility(recorded: Option<u32>, binary: u32) -> Compatibility {
    match recorded {
        None => Compatibility::FreshInstall,
        Some(v) if v == binary => Compatibility::Current,
        Some(v) if v < binary => Compatibility::NeedsUpgrade { from: v },
        Some(v) => Compatibility::BinaryTooOld { db_version: v },
    }
}

/// One statement at a time, so a failure names the statement. Slices of
/// the `'static` script rather than copies: each is still compile-time
/// SQL, which is what `sqlx::query` accepts without an audit marker.
///
/// Splits on `;`, so a schema script's comments must not contain one.
pub fn split_sql(sql: &'static str) -> impl Iterator<Item = &'static str> {
    sql.split(';').map(str::trim).filter(|s| !s.is_empty())
}

/// Why a versioned store could not be brought to the binary's version.
#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    #[error("{0}")]
    Backend(#[from] sqlx::Error),

    /// The file was written by a newer binary. The store maps this onto
    /// its own `IncompatibleSchema` variant, whose message names the
    /// store and what abandoning it costs.
    #[error(
        "incompatible schema: db is at version {db_version}, this binary supports {binary_version}"
    )]
    BinaryTooOld {
        db_version: u32,
        binary_version: u32,
    },
}

/// Where a schema step has got to, told to whoever is watching. Every
/// method is a no-op unless a test overrides it: production passes `()`.
///
/// The seam exists for the tests that make a race deterministic — pause
/// one opener after it has read the version and before it acts, start a
/// second, and see what the second can do — and for the ones that kill
/// a process part-way up a ladder.
pub(crate) trait SchemaSeam {
    /// The recorded version has been read and judged, and nothing has
    /// been applied yet. Under [`with_schema_lock`], the lock is held.
    async fn version_read(&mut self, _verdict: Compatibility) {}

    /// The rung that brings the schema to `_version` has been applied,
    /// inside the step's transaction, and not yet committed.
    async fn rung_applied(&mut self, _version: u32) {}
}

impl SchemaSeam for () {}

/// Run one schema step under the schema lock: on a connection of its
/// own, inside one `BEGIN IMMEDIATE` transaction that commits only if
/// `step` succeeds. See the module docs for what that guarantees.
///
/// The connection is opened from the pool's options, so it addresses
/// the same file with the same journal mode, but with
/// [`SCHEMA_LOCK_WAIT`] as its busy timeout; it is closed afterwards,
/// which also gives the lock back on every path. `step` must do all of
/// its reading and writing through the connection it is handed —
/// anything sent through the pool runs outside the transaction, and a
/// write there would wait on the very lock this step holds.
pub async fn with_schema_lock<T, E>(
    pool: &Pool<Sqlite>,
    step: impl AsyncFnOnce(&mut SqliteConnection) -> Result<T, E>,
) -> Result<T, E>
where
    E: From<sqlx::Error>,
{
    let options = pool
        .connect_options()
        .as_ref()
        .clone()
        .busy_timeout(SCHEMA_LOCK_WAIT);
    let mut conn = options.connect().await?;
    let outcome = in_immediate_transaction(&mut conn, step).await;
    let closed = conn.close().await;
    let value = outcome?;
    closed?;
    Ok(value)
}

async fn in_immediate_transaction<T, E>(
    conn: &mut SqliteConnection,
    step: impl AsyncFnOnce(&mut SqliteConnection) -> Result<T, E>,
) -> Result<T, E>
where
    E: From<sqlx::Error>,
{
    // The busy timeout applies here: an opener that finds the lock held
    // waits for it rather than failing on the spot.
    let mut tx = conn.begin_with("BEGIN IMMEDIATE").await?;
    match step(&mut tx).await {
        Ok(value) => {
            tx.commit().await?;
            Ok(value)
        }
        Err(err) => {
            // Said rather than left to `Drop`, so an interrupted step is
            // visibly one that leaves nothing behind. Closing the
            // connection would roll back too, so a failure here changes
            // nothing and the step's own error is the one to report.
            if let Err(rollback) = tx.rollback().await {
                tracing::debug!(error = %rollback, "schema step: rollback failed; closing rolls back");
            }
            Err(err)
        }
    }
}

/// Create `schema_meta` if it is not there. Idempotent.
pub async fn ensure_schema_meta(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    for stmt in split_sql(SCHEMA_META_SQL) {
        sqlx::query(stmt).execute(&mut *conn).await?;
    }
    Ok(())
}

/// The version `schema_meta` records for `class`, or `None` when no
/// row exists yet.
pub async fn read_schema_version<'e, E>(
    executor: E,
    class: &str,
) -> Result<Option<u32>, sqlx::Error>
where
    E: Executor<'e, Database = Sqlite>,
{
    let row = sqlx::query("SELECT version FROM schema_meta WHERE class = ?")
        .bind(class)
        .fetch_optional(executor)
        .await?;
    Ok(row.map(|r| r.get::<i64, _>(0) as u32))
}

/// Inspect a versioned store without creating or migrating anything.
///
/// A missing `schema_meta` table is a fresh/uninitialised file rather
/// than a backend failure. Read-only callers use that distinction to
/// tell operators whether to initialise or migrate the store.
pub async fn inspect_readable_versioned(
    pool: &Pool<Sqlite>,
    class: &str,
    binary_version: u32,
) -> Result<Compatibility, sqlx::Error> {
    let has_schema_meta: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_meta')",
    )
    .fetch_one(pool)
    .await?;
    if !has_schema_meta {
        return Ok(Compatibility::FreshInstall);
    }

    let recorded = read_schema_version(pool, class).await?;
    Ok(check_compatibility(recorded, binary_version))
}

/// Record `version` for `class`, replacing any earlier row.
pub async fn write_schema_version<'e, E>(
    executor: E,
    class: &str,
    version: u32,
) -> Result<(), sqlx::Error>
where
    E: Executor<'e, Database = Sqlite>,
{
    let now = chrono::Utc::now().timestamp_millis();
    sqlx::query(
        r#"
        INSERT INTO schema_meta (class, version, updated_at) VALUES (?, ?, ?)
        ON CONFLICT(class) DO UPDATE SET version = excluded.version, updated_at = excluded.updated_at
        "#,
    )
    .bind(class)
    .bind(version as i64)
    .bind(now)
    .execute(executor)
    .await?;
    Ok(())
}

/// Apply every rung of `migrations` above `from` and at or below `to`,
/// in ladder order, on `conn`.
///
/// Running a rung twice is not safe — `ALTER TABLE ADD COLUMN` fails if
/// the column is already there — and nothing in this function prevents
/// it. What does is [`bootstrap_versioned`], which runs the rungs and
/// the version write that records them in one transaction under the
/// schema lock: no other opener can read the old version while the
/// rungs are applied, and no interrupted run can leave the rungs
/// without the version. Called on its own, outside that, it has neither
/// property; the migration tests use it that way to build a file at an
/// old rung.
pub async fn apply_migrations(
    conn: &mut SqliteConnection,
    migrations: &[Migration],
    from: u32,
    to: u32,
) -> Result<(), sqlx::Error> {
    apply_ladder(conn, migrations, from, to, &mut ()).await
}

async fn apply_ladder(
    conn: &mut SqliteConnection,
    migrations: &[Migration],
    from: u32,
    to: u32,
    seam: &mut impl SchemaSeam,
) -> Result<(), sqlx::Error> {
    for &(version, sql) in migrations {
        if from < version && to >= version {
            for stmt in split_sql(sql) {
                sqlx::query(stmt).execute(&mut *conn).await?;
            }
            seam.rung_applied(version).await;
        }
    }
    Ok(())
}

/// The whole open-time protocol for a `schema_meta`-versioned store:
/// ensure the version table, read `class`'s row, and act on the
/// verdict — create, leave alone, migrate, or refuse — all of it under
/// [`with_schema_lock`]. Returns the verdict so a caller can log what
/// happened.
///
/// A refusal ([`SchemaError::BinaryTooOld`]) rolls the transaction
/// back, so a file this binary cannot read is left exactly as it was.
pub async fn bootstrap_versioned(
    pool: &Pool<Sqlite>,
    class: &str,
    binary_version: u32,
    migrations: &[Migration],
) -> Result<Compatibility, SchemaError> {
    bootstrap_observed(pool, class, binary_version, migrations, &mut ()).await
}

/// [`bootstrap_versioned`], telling `seam` where it has got to.
pub(crate) async fn bootstrap_observed(
    pool: &Pool<Sqlite>,
    class: &str,
    binary_version: u32,
    migrations: &[Migration],
    seam: &mut impl SchemaSeam,
) -> Result<Compatibility, SchemaError> {
    with_schema_lock(
        pool,
        async |conn: &mut SqliteConnection| -> Result<Compatibility, SchemaError> {
            ensure_schema_meta(conn).await?;
            let recorded = read_schema_version(&mut *conn, class).await?;
            let verdict = check_compatibility(recorded, binary_version);
            seam.version_read(verdict).await;
            let from = match verdict {
                Compatibility::FreshInstall => 0,
                Compatibility::NeedsUpgrade { from } => from,
                Compatibility::Current => return Ok(verdict),
                Compatibility::BinaryTooOld { db_version } => {
                    return Err(SchemaError::BinaryTooOld {
                        db_version,
                        binary_version,
                    });
                }
            };
            apply_ladder(conn, migrations, from, binary_version, seam).await?;
            write_schema_version(&mut *conn, class, binary_version).await?;
            Ok(verdict)
        },
    )
    .await
}

/// SQLite's `user_version` pragma — the version slot the projection
/// uses. `0` is what a file that was never stamped reads, so a caller
/// that needs to tell "never stamped" from "version 0" checks for its
/// tables first.
pub async fn read_user_version<'e, E>(executor: E) -> Result<u32, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(executor)
        .await?;
    Ok(version as u32)
}

/// Stamp `user_version`. A pragma takes no bound parameter, so the
/// number is interpolated — a `u32`, which is why that is safe.
pub async fn write_user_version<'e, E>(executor: E, version: u32) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "PRAGMA user_version = {version}"
    )))
    .execute(executor)
    .await?;
    Ok(())
}
