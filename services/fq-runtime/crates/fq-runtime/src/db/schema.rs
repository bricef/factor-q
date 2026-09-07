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
//! than a migration (its `schema` module says why).
//!
//! `fq-store` (a separate workspace) keeps its own pool and pragma
//! bootstrap in `grant_log.rs`/`index.rs`; it is a different crate with
//! a different lifecycle and is deliberately not ported onto this kit.

use sqlx::{Pool, Row, Sqlite};

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

/// Create `schema_meta` if it is not there. Idempotent.
pub async fn ensure_schema_meta(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
    for stmt in split_sql(SCHEMA_META_SQL) {
        sqlx::query(stmt).execute(pool).await?;
    }
    Ok(())
}

/// The version `schema_meta` records for `class`, or `None` when no
/// row exists yet.
pub async fn read_schema_version(
    pool: &Pool<Sqlite>,
    class: &str,
) -> Result<Option<u32>, sqlx::Error> {
    let row = sqlx::query("SELECT version FROM schema_meta WHERE class = ?")
        .bind(class)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.get::<i64, _>(0) as u32))
}

/// Record `version` for `class`, replacing any earlier row.
pub async fn write_schema_version(
    pool: &Pool<Sqlite>,
    class: &str,
    version: u32,
) -> Result<(), sqlx::Error> {
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
    .execute(pool)
    .await?;
    Ok(())
}

/// Apply every rung of `migrations` above `from` and at or below `to`,
/// in ladder order. Re-running on an up-to-date database applies
/// nothing, which is what keeps a non-idempotent rung safe.
pub async fn apply_migrations(
    pool: &Pool<Sqlite>,
    migrations: &[Migration],
    from: u32,
    to: u32,
) -> Result<(), sqlx::Error> {
    for &(version, sql) in migrations {
        if from < version && to >= version {
            for stmt in split_sql(sql) {
                sqlx::query(stmt).execute(pool).await?;
            }
        }
    }
    Ok(())
}

/// The whole open-time protocol for a `schema_meta`-versioned store:
/// ensure the version table, read `class`'s row, and act on the
/// verdict — create, leave alone, migrate, or refuse. Returns the
/// verdict so a caller can log what happened.
pub async fn bootstrap_versioned(
    pool: &Pool<Sqlite>,
    class: &str,
    binary_version: u32,
    migrations: &[Migration],
) -> Result<Compatibility, SchemaError> {
    ensure_schema_meta(pool).await?;
    let recorded = read_schema_version(pool, class).await?;
    let verdict = check_compatibility(recorded, binary_version);
    match verdict {
        Compatibility::FreshInstall => {
            apply_migrations(pool, migrations, 0, binary_version).await?;
            write_schema_version(pool, class, binary_version).await?;
        }
        Compatibility::Current => {}
        Compatibility::NeedsUpgrade { from } => {
            apply_migrations(pool, migrations, from, binary_version).await?;
            write_schema_version(pool, class, binary_version).await?;
        }
        Compatibility::BinaryTooOld { db_version } => {
            return Err(SchemaError::BinaryTooOld {
                db_version,
                binary_version,
            });
        }
    }
    Ok(verdict)
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
