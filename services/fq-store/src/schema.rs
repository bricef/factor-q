//! How the store opens its SQLite databases and brings them to the
//! binary's schema. The storage index (`index.db`, [`crate::index`]) and
//! the grant log (`grants.db`, [`crate::grant_log`]) both open through
//! [`open_store_db`], so the two cannot drift apart again: until
//! <https://github.com/bricef/factor-q/issues/975> each carried its own
//! copy of the pool options and of `migrate`.
//!
//! ## The ladder
//!
//! Each database has a migration ladder: a list of SQL scripts in which
//! rung *n*, counting from 1, brings the schema to version *n*. The
//! version a file has reached is SQLite's `user_version` pragma, which
//! reads 0 on a new file. Opening runs every rung above the recorded
//! version, in order, and then records the ladder's length. A rung must
//! never run twice, because not every rung can: `ALTER TABLE … ADD
//! COLUMN` fails if the column is there, and `grants.db`'s first rung is
//! a bare `CREATE TABLE`.
//!
//! A file whose version is beyond the ladder, written by a newer binary,
//! is opened as it is and nothing runs. This crate has never refused such
//! a file; the runtime's stores do (`data-architecture.md` §5.6).
//!
//! ## One lock, one transaction
//!
//! [`migrate`] runs on a connection of its own, inside one `BEGIN
//! IMMEDIATE` transaction taken *before* the version is read, and commits
//! only once every pending rung and the new version are written. Two
//! properties follow:
//!
//! - **Atomic.** SQLite DDL is transactional, and so is `user_version`
//!   (it lives in the file header), so the rungs and the version that
//!   describes them commit together or not at all. An upgrade that stops
//!   anywhere — a failing statement, a cancelled open, a crash — leaves
//!   the file at its old version with nothing applied, and the next open
//!   runs the same rungs from a clean start.
//! - **Serialised.** `BEGIN IMMEDIATE` takes SQLite's write lock on the
//!   file. That is a lock between *connections*, held through the file
//!   itself (the `-shm` index in WAL mode), so it serialises two
//!   processes — two `fq-cas` commands on one root — exactly as it
//!   serialises two tasks in one. There is no in-process mutex, and
//!   nothing here relies on one. A second opener waits for the first to
//!   commit, then reads the version the first wrote and finds nothing to
//!   do.
//!
//! The protocol this replaced committed each rung with its own version
//! bump, which made each rung atomic, but it read the version that chose
//! the rungs on the pool, outside any transaction. Two openers of a fresh
//! `grants.db` both read 0, and the second failed on `table grant_events
//! already exists`. Two openers of a fresh `index.db` failed on rung 2's
//! copy of the old `blocks` table, and two upgrading one failed on the
//! first `ADD COLUMN`.
//!
//! The wait for the lock is bounded by [`SCHEMA_LOCK_WAIT`], set on the
//! migration's own connection. The pool's busy timeout,
//! [`POOL_BUSY_TIMEOUT`], is sized for ordinary writes and is not the
//! bound here. An opener that cannot get the lock in time fails with
//! SQLite's `database is locked`.
//!
//! Nothing a rung runs is refused or mishandled inside a transaction.
//! Every rung of both ladders is `CREATE TABLE`, `CREATE INDEX`,
//! `INSERT`, `DROP TABLE` or `ALTER TABLE`, and each already ran inside a
//! transaction of its own under the protocol this replaced. No rung uses
//! `PRAGMA journal_mode` (which cannot change into or out of WAL
//! mid-transaction), `VACUUM`, `ATTACH`, another `PRAGMA` or a
//! transaction-control statement. A rung that needed one could not be
//! appended to a ladder; it would have to be a step of its own outside
//! [`migrate`].
//!
//! ## The one step before the lock: switching a new file into WAL mode
//!
//! Both databases run in WAL mode, and sqlx switches a file into it with
//! `PRAGMA journal_mode` when the pool makes its first connection —
//! before [`migrate`] can take the lock. That switch cannot wait for a
//! lock. On a file not yet in WAL mode it takes a read transaction and
//! then upgrades it to write the header, and SQLite calls the busy
//! handler only when no transaction is open, so the upgrade fails at once
//! with `database is locked` if another connection holds the file. sqlx
//! does not retry a connect on that error either. So two processes
//! creating one database in the same instant would have one fail there,
//! before the lock was ever reached. [`open_store_db`] retries that
//! connect on `database is locked` only, with a short backoff, for up to
//! [`SCHEMA_LOCK_WAIT`]. Once a file is in WAL mode the switch only reads
//! the header and never contends.
//!
//! ## The runtime's stores
//!
//! `fq-runtime`'s `db/schema.rs` holds its stores to the same rule, and
//! learned the WAL-switch retry first
//! (<https://github.com/bricef/factor-q/pull/973>). The two are separate
//! implementations of one protocol rather than one shared kit: `fq-store`
//! is a storage service that depends on nothing in the agent runtime,
//! nothing depends the other way, and the two version their files
//! differently — a positional ladder in `user_version` here, a
//! `schema_meta` row per schema class with numbered rungs there.

use std::path::Path;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{ConnectOptions, Connection, SqlitePool};

use crate::Result;

#[cfg(test)]
mod tests;

/// How long [`migrate`] waits for SQLite's write lock — and so how long a
/// second opener waits for a first one's migration to commit.
/// [`open_store_db`] spends the same budget waiting out the switch into
/// WAL mode.
///
/// A chosen bound, not a measured one, and the same one the runtime's
/// stores use. A ladder applies in milliseconds; the other holders of the
/// write lock are ordinary writes and `fq-cas gc`'s short transactions.
/// Thirty seconds is meant to be comfortably longer than any of those on
/// a loaded machine while still turning a holder that never lets go (a
/// process stopped mid-transaction) into a failed open rather than a hang.
/// When it is exceeded the waiting open fails with `database is locked`,
/// the holder carries on undisturbed, and starting the waiter again once
/// the holder has finished finds the work done.
const SCHEMA_LOCK_WAIT: Duration = Duration::from_secs(30);

/// The busy timeout of each store's pool: how long an ordinary read or
/// write waits for another connection's lock.
const POOL_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The first pause before [`open_store_db`] tries a locked connect again,
/// doubled on each retry up to [`CONNECT_RETRY_MAX_PAUSE`].
const CONNECT_RETRY_FIRST_PAUSE: Duration = Duration::from_millis(10);
const CONNECT_RETRY_MAX_PAUSE: Duration = Duration::from_millis(250);

/// Where a migration has got to, told to whoever is watching. Every method
/// is a no-op unless a test overrides it: production passes `()`.
///
/// It exists for the tests that make a race deterministic — pause one
/// opener after it has read the version and before it acts, start a
/// second, and see what the second can do — and for the ones that stop a
/// migration part-way up its ladder.
trait MigrationSeam {
    /// The recorded version has been read and nothing has been applied
    /// yet. The write lock is held.
    async fn version_read(&mut self, _version: i64) {}

    /// The rung that brings the schema to `_version` has been applied,
    /// inside the migration's transaction, and not yet committed.
    async fn rung_applied(&mut self, _version: i64) {}
}

impl MigrationSeam for () {}

/// Open the database at `path`, creating it if it does not exist, and
/// bring its schema to the end of `ladder` ([`migrate`]).
///
/// The pool has a single connection. Every operation on the store is
/// then serialised through it, so the reference-counting compare-and-swaps
/// and the multi-statement transactions linearise exactly as the verified
/// protocols assume (SQLite's single writer), and WAL's
/// `SQLITE_BUSY_SNAPSHOT`, which a busy timeout cannot retry away, cannot
/// arise.
pub(crate) async fn open_store_db(path: &Path, ladder: &[&'static str]) -> Result<SqlitePool> {
    let pool = connect_store_pool(path).await?;
    migrate(&pool, ladder).await?;
    Ok(pool)
}

/// Connect the store's pool to `path`, creating the file if it does not
/// exist, and waiting out the one lock a busy timeout cannot: the switch
/// into WAL mode (see the module docs).
///
/// A connect that fails with `database is locked` is tried again after a
/// pause that starts at 10 ms and doubles to 250 ms, until
/// [`SCHEMA_LOCK_WAIT`] has passed; then the last error is returned. Every
/// other error is returned at once.
async fn connect_store_pool(path: &Path) -> std::result::Result<SqlitePool, sqlx::Error> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(POOL_BUSY_TIMEOUT);
    let deadline = tokio::time::Instant::now() + SCHEMA_LOCK_WAIT;
    let mut pause = CONNECT_RETRY_FIRST_PAUSE;
    loop {
        let attempt = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await;
        match attempt {
            Err(err) if is_busy(&err) && tokio::time::Instant::now() + pause < deadline => {
                tracing::debug!(error = %err, ?pause, "store open: file locked, trying again");
                tokio::time::sleep(pause).await;
                pause = (pause * 2).min(CONNECT_RETRY_MAX_PAUSE);
            }
            outcome => return outcome,
        }
    }
}

/// `SQLITE_BUSY` (`database is locked`), under any of its extended codes.
fn is_busy(err: &sqlx::Error) -> bool {
    let sqlx::Error::Database(err) = err else {
        return false;
    };
    err.code()
        .and_then(|code| code.parse::<i32>().ok())
        .is_some_and(|code| code & 0xff == 5)
}

/// Apply every rung of `ladder` the file at `pool` has not yet seen,
/// under one lock and one transaction (see the module docs).
async fn migrate(pool: &SqlitePool, ladder: &[&'static str]) -> Result<()> {
    migrate_observed(pool, ladder, &mut ()).await
}

/// [`migrate`], telling `seam` where it has got to.
///
/// The connection is opened from the pool's options, so it addresses the
/// same file in the same journal mode, but with [`SCHEMA_LOCK_WAIT`] as
/// its busy timeout. It is closed afterwards, which also gives the lock
/// back on every path; a future dropped part-way drops the connection,
/// which closes it and rolls the transaction back.
async fn migrate_observed(
    pool: &SqlitePool,
    ladder: &[&'static str],
    seam: &mut impl MigrationSeam,
) -> Result<()> {
    let options = pool
        .connect_options()
        .as_ref()
        .clone()
        .busy_timeout(SCHEMA_LOCK_WAIT);
    let mut conn = options.connect().await?;
    let outcome = apply_ladder(&mut conn, ladder, seam).await;
    // By now the migration has committed or rolled back. A close that
    // fails after a commit cannot undo it, so it is reported, not
    // returned: failing the open would claim the migration did not happen.
    if let Err(err) = conn.close().await {
        tracing::warn!(error = %err, "store migration: closing its connection failed");
    }
    outcome
}

async fn apply_ladder(
    conn: &mut SqliteConnection,
    ladder: &[&'static str],
    seam: &mut impl MigrationSeam,
) -> Result<()> {
    // The busy timeout applies here: an opener that finds the lock held
    // waits for it rather than failing on the spot.
    let mut tx = conn.begin_with("BEGIN IMMEDIATE").await?;
    match apply_pending(&mut tx, ladder, seam).await {
        Ok(()) => {
            tx.commit().await?;
            Ok(())
        }
        Err(err) => {
            // Said rather than left to `Drop`, so a failed migration is
            // visibly one that leaves nothing behind. Closing the
            // connection rolls back too, so a failure here changes
            // nothing and the migration's own error is the one to report.
            if let Err(rollback) = tx.rollback().await {
                tracing::debug!(error = %rollback, "store migration: rollback failed; closing rolls back");
            }
            Err(err)
        }
    }
}

async fn apply_pending(
    conn: &mut SqliteConnection,
    ladder: &[&'static str],
    seam: &mut impl MigrationSeam,
) -> Result<()> {
    let recorded: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut *conn)
        .await?;
    seam.version_read(recorded).await;
    let mut applied = recorded;
    for (version, rung) in (1..).zip(ladder) {
        if version > recorded {
            // `raw_sql` steps through a multi-statement rung.
            sqlx::raw_sql(*rung).execute(&mut *conn).await?;
            seam.rung_applied(version).await;
            applied = version;
        }
    }
    if applied > recorded {
        // A pragma takes no bound parameter; `applied` is an integer
        // counted by this loop.
        let stamp = sqlx::AssertSqlSafe(format!("PRAGMA user_version = {applied}"));
        sqlx::query(stamp).execute(&mut *conn).await?;
    }
    Ok(())
}
