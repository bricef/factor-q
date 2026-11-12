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
//! After the ladder a store may have open-time work of its own that reads
//! the file to decide what to do and then writes: the grant log catches
//! its permission projection up with its event log. That is the
//! `after_ladder` step of [`open_store_db`], and it runs inside the same
//! transaction, for the same reasons.
//!
//! A file whose version is beyond the ladder was written by a newer
//! binary. It is refused, not opened: see "A file a newer binary wrote",
//! below.
//!
//! ## One lock, one transaction
//!
//! [`migrate`] runs on a connection of its own, inside one `BEGIN
//! IMMEDIATE` transaction taken *before* the version is read, and commits
//! only once every pending rung, the new version and the store's
//! `after_ladder` step are written. Two properties follow:
//!
//! - **Atomic.** SQLite DDL is transactional, and so is `user_version`
//!   (it lives in the file header), so the rungs and the version that
//!   describes them commit together or not at all. An upgrade that stops
//!   anywhere — a failing statement, a cancelled open, a crash, a killed
//!   process — leaves the file at its old version with nothing applied,
//!   and the next open runs the same rungs from a clean start.
//! - **Serialised.** `BEGIN IMMEDIATE` takes SQLite's write lock on the
//!   file. That is a lock between *connections*, held through the file
//!   itself (the `-shm` index in WAL mode), so it serialises two
//!   processes — two `fq-cas` commands on one root — exactly as it
//!   serialises two tasks in one. There is no in-process mutex, and
//!   nothing here relies on one. A second opener waits for the first to
//!   commit, then reads the version (and the projection cursor) the first
//!   wrote and finds nothing to do.
//!
//! The protocol this replaced committed each rung with its own version
//! bump, which made each rung atomic, but it read the version that chose
//! the rungs on the pool, outside any transaction. Two openers of a fresh
//! `grants.db` both read 0, and the second failed on `table grant_events
//! already exists`. Two openers of a fresh `index.db` failed on rung 2's
//! copy of the old `blocks` table, and two upgrading one failed on the
//! first `ADD COLUMN`. The grant log's catch-up ran after the migration,
//! in a deferred transaction on the pool: two openers of a file whose
//! projection lagged its log both read the old cursor, and the second
//! failed with `database is locked` when it came to write.
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
//! [`migrate`]. The grant log's catch-up is ordinary reads and writes.
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
//! ## A file a newer binary wrote
//!
//! Every open refuses a file whose version is beyond the ladder, with
//! [`StoreError::BinaryTooOld`]: it names the file, the version the file
//! records and the highest version this binary knows, and says the remedy,
//! an `fq-cas` at least as new as the one that wrote it. The check is made
//! where the version is read, inside the locked transaction and before any
//! rung or the `after_ladder` step runs. So the refusal rolls back a
//! transaction that has written nothing and gives the lock back: the file
//! is left exactly as the newer binary left it, a `grants.db` projection
//! that lags its log included, and an opener waiting on the lock goes on to
//! be refused in its turn rather than waiting out [`SCHEMA_LOCK_WAIT`].
//!
//! Read-only commands are refused too (`fq-cas object ls`, `grant ls`,
//! `grant check`). This crate has one open path, which every command that
//! touches either database takes, and an older binary cannot know what a
//! newer schema means even to read it: a table it reads may have changed
//! meaning, and a column it never selects may be one the newer binary's
//! rows depend on. The runtime's read-only opens refuse such a file as its
//! writable ones do. Rolling a binary back is not supported (`STATUS.md`),
//! and nothing here downgrades a file.
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

use crate::{Result, StoreError};

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

/// Where an open has got to, told to whoever is watching. Every method is
/// a no-op unless a test overrides it: production passes `()`.
///
/// It exists for the tests that make a race deterministic — pause one
/// opener after it has read what decides its work and before it acts,
/// start a second, and see what the second can do — and for the ones that
/// stop a migration part-way up its ladder.
pub(crate) trait MigrationSeam {
    /// The recorded version has been read and nothing has been applied
    /// yet. The write lock is held.
    async fn version_read(&mut self, _version: i64) {}

    /// The rung that brings the schema to `_version` has been applied,
    /// inside the migration's transaction, and not yet committed.
    async fn rung_applied(&mut self, _version: i64) {}

    /// The store's `after_ladder` step has read what decides its work and
    /// written nothing yet: for the grant log, the projection cursor. The
    /// write lock is held.
    async fn after_ladder_read(&mut self) {}
}

impl MigrationSeam for () {}

/// Open the database at `path`, creating it if it does not exist, bring
/// its schema to the end of `ladder`, and run `after_ladder` on the same
/// connection inside the same transaction ([`migrate`]).
///
/// The pool has a single connection. Within one process every operation
/// on the store is then serialised through it, so the reference-counting
/// compare-and-swaps and the multi-statement transactions linearise
/// exactly as the verified protocols assume (SQLite's single writer), and
/// WAL's `SQLITE_BUSY_SNAPSHOT`, which a busy timeout cannot retry away,
/// cannot arise between them. Two processes on one file each have a pool
/// of their own, and between them it can.
pub(crate) async fn open_store_db(
    path: &Path,
    ladder: &[&'static str],
    after_ladder: impl AsyncFnOnce(&mut SqliteConnection) -> Result<()>,
) -> Result<SqlitePool> {
    let pool = connect_store_pool(path).await?;
    migrate(&pool.connect_options(), ladder, after_ladder).await?;
    Ok(pool)
}

/// The `after_ladder` of a store with no open-time work beyond its
/// ladder.
pub(crate) async fn nothing_after_ladder(_conn: &mut SqliteConnection) -> Result<()> {
    Ok(())
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

/// Apply every rung of `ladder` the file has not yet seen, then
/// `after_ladder`, under one lock and one transaction (see the module
/// docs). `options` are the store pool's, so the migration addresses the
/// same file in the same journal mode.
async fn migrate(
    options: &SqliteConnectOptions,
    ladder: &[&'static str],
    after_ladder: impl AsyncFnOnce(&mut SqliteConnection) -> Result<()>,
) -> Result<()> {
    migrate_observed(options, ladder, after_ladder, &mut ()).await
}

/// [`migrate`], telling `seam` where it has got to.
///
/// The connection is opened from `options` with [`SCHEMA_LOCK_WAIT`] as
/// its busy timeout. It is closed afterwards, which also gives the lock
/// back on every path; a future dropped part-way drops the connection,
/// which closes it and rolls the transaction back.
async fn migrate_observed(
    options: &SqliteConnectOptions,
    ladder: &[&'static str],
    after_ladder: impl AsyncFnOnce(&mut SqliteConnection) -> Result<()>,
    seam: &mut impl MigrationSeam,
) -> Result<()> {
    let mut conn = options
        .clone()
        .busy_timeout(SCHEMA_LOCK_WAIT)
        .connect()
        .await?;
    let outcome = in_immediate_transaction(&mut conn, async |tx: &mut SqliteConnection| {
        apply_pending(tx, options.get_filename(), ladder, seam).await?;
        after_ladder(tx).await
    })
    .await;
    // By now the migration has committed or rolled back. A close that
    // fails after a commit cannot undo it, so it is reported, not
    // returned: failing the open would claim the migration did not happen.
    if let Err(err) = conn.close().await {
        tracing::warn!(error = %err, "store migration: closing its connection failed");
    }
    outcome
}

/// Run `step` on `conn` inside one `BEGIN IMMEDIATE` transaction, which
/// commits only if `step` succeeds and is rolled back otherwise.
async fn in_immediate_transaction(
    conn: &mut SqliteConnection,
    step: impl AsyncFnOnce(&mut SqliteConnection) -> Result<()>,
) -> Result<()> {
    // The busy timeout applies here: an opener that finds the lock held
    // waits for it rather than failing on the spot.
    let mut tx = conn.begin_with("BEGIN IMMEDIATE").await?;
    match step(&mut tx).await {
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

/// Read the recorded version and apply every rung of `ladder` above it,
/// then record the version the last one brought the schema to.
///
/// A version above the ladder's length is refused with
/// [`StoreError::BinaryTooOld`], naming `path`, before anything is written
/// (see the module docs).
async fn apply_pending(
    conn: &mut SqliteConnection,
    path: &Path,
    ladder: &[&'static str],
    seam: &mut impl MigrationSeam,
) -> Result<()> {
    let recorded: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut *conn)
        .await?;
    seam.version_read(recorded).await;
    let known = ladder.len() as i64;
    if recorded > known {
        return Err(StoreError::BinaryTooOld {
            path: path.to_path_buf(),
            db_version: recorded,
            binary_version: known,
        });
    }
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
