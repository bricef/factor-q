//! The migration lock's two properties
//! (<https://github.com/bricef/factor-q/issues/975>), on both of the
//! store's real ladders:
//!
//! - **Serialised**: a second opener that arrives while the first is
//!   between reading what decides its work — the version, or the grant
//!   log's projection cursor — and acting on it waits, then finds nothing
//!   to do, in this process and in another.
//! - **Atomic**: a ladder that stops part-way — an injected failure on a
//!   later rung, an open that is cancelled, a process that is killed —
//!   leaves the file at its old version with nothing applied and the lock
//!   free, and the next open succeeds.
//!
//! And the step before the lock: a database another connection is still
//! creating, and has not yet switched into WAL mode, opens once that
//! connection lets go.
//!
//! And the one file no open may touch: one a newer binary wrote, which is
//! refused by name before any rung or the grant log's catch-up runs, and
//! left as it was with the lock free
//! (<https://github.com/bricef/factor-q/issues/984>).
//!
//! The races are staged by [`RivalAt`], which pauses the first opener at a
//! [`Moment`] where it has read what decides its work — the window two
//! unserialised openers used to share — starts the second there, and gives
//! it a bounded head start before letting the first go on. Without the
//! lock the second reads the same old state and does the whole job inside
//! its head start, so the first then repeats work that is already done and
//! fails. With the lock the second cannot begin until the first commits,
//! and the head start runs out with it still waiting; every race test
//! asserts that it did.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection};
use tempfile::tempdir;
use tokio::task::JoinHandle;

use super::*;
use crate::grant_log::catch_up;
use crate::{
    Grantor, LiveGrant, Principal, Scope, SqliteGrantLog, SqliteNameIndex, StoreError, Verb,
};

/// The two databases the store keeps, each opened through its own store's
/// `open`.
#[derive(Debug, Clone, Copy)]
enum Store {
    Index,
    Grants,
}

impl Store {
    fn file(self) -> &'static str {
        match self {
            Store::Index => "index.db",
            Store::Grants => "grants.db",
        }
    }

    fn ladder(self) -> &'static [&'static str] {
        match self {
            Store::Index => crate::index::MIGRATIONS,
            Store::Grants => crate::grant_log::MIGRATIONS,
        }
    }

    /// Run `ladder` against the file `options` address as this store's
    /// `open` does, its `after_ladder` step included, telling `seam` where
    /// it has got to.
    async fn migrate(
        self,
        options: &SqliteConnectOptions,
        ladder: &[&'static str],
        seam: &mut impl MigrationSeam,
    ) -> Result<()> {
        match self {
            Store::Index => migrate_observed(options, ladder, nothing_after_ladder, seam).await,
            Store::Grants => {
                let after_ladder =
                    async |conn: &mut SqliteConnection| catch_up(conn, &mut ()).await;
                migrate_observed(options, ladder, after_ladder, seam).await
            }
        }
    }

    /// Open the file at `path` through the store's own `open`, and close it
    /// again.
    async fn try_open(self, path: &Path) -> Result<()> {
        match self {
            Store::Index => SqliteNameIndex::open(path).await.map(drop),
            Store::Grants => SqliteGrantLog::open(path).await.map(drop),
        }
    }

    /// Open the file at `path` the way the store does, as a second opener
    /// would, and close it again.
    async fn open(self, path: PathBuf) -> std::result::Result<(), String> {
        self.try_open(&path).await.map_err(|err| err.to_string())
    }

    /// What a file this store opened on its own, with nobody racing it,
    /// holds: the schema every test below must end at.
    async fn reference(self) -> OnDisk {
        let dir = tempdir().unwrap();
        let path = dir.path().join(self.file());
        self.open(path.clone()).await.expect("a lone open succeeds");
        on_disk(&path).await
    }
}

/// A file's `user_version`, and every row of `sqlite_master` as
/// `type name sql`, sorted.
type OnDisk = (i64, Vec<String>);

/// What the file at `path` holds, read on a fresh connection so nothing a
/// test holds can colour it.
async fn on_disk(path: &Path) -> OnDisk {
    let mut conn = SqliteConnectOptions::new()
        .filename(path)
        .connect()
        .await
        .expect("open for inspection");
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    let schema: Vec<String> = sqlx::query_scalar(
        "SELECT type || ' ' || name || ' ' || COALESCE(sql, '') FROM sqlite_master ORDER BY 1",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();
    (version, schema)
}

/// A pool on `path` opened the way the stores open theirs, with the file
/// already brought to version `at` — an older binary's file — or left new
/// and empty when `at` is 0.
async fn pool_at(store: Store, path: &Path, at: usize) -> SqlitePool {
    let pool = connect_store_pool(path).await.expect("open the store pool");
    if at > 0 {
        store
            .migrate(&pool.connect_options(), &store.ladder()[..at], &mut ())
            .await
            .expect("bring the file to its old version");
    }
    pool
}

/// Whether some connection holds the write lock on the database at `path`
/// right now. The probe has no busy timeout, so it asks once: it tries to
/// take the lock, and lets go at once if it got it.
async fn write_lock_held(path: &Path) -> bool {
    let mut probe = SqliteConnectOptions::new()
        .filename(path)
        .busy_timeout(Duration::ZERO)
        .connect()
        .await
        .expect("open the lock probe");
    let held = match sqlx::query("BEGIN IMMEDIATE").execute(&mut probe).await {
        Ok(_) => {
            sqlx::query("ROLLBACK")
                .execute(&mut probe)
                .await
                .expect("release the probe's lock");
            false
        }
        Err(err) if is_busy(&err) => true,
        Err(err) => panic!("the lock probe failed for another reason: {err}"),
    };
    probe.close().await.expect("close the lock probe");
    held
}

/// Waits for the write lock on `path` to come free. Dropping a migration
/// hands its connection back to SQLite on sqlx's worker thread, so the lock
/// goes a moment later rather than at once. Bounded by count and by time:
/// 100 polls, 50 ms apart.
async fn lock_released(path: &Path) -> bool {
    for _ in 0..100 {
        if !write_lock_held(path).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

// ----- The race, staged -----

/// How long the rival is given while the first opener is paused. Without
/// the lock a rival finishes in milliseconds, far inside this; with it the
/// rival cannot finish until the first opener commits, so every race test
/// spends exactly this long here. Well under [`SCHEMA_LOCK_WAIT`], which is
/// how long the rival will wait.
const RIVAL_HEAD_START: Duration = Duration::from_secs(2);

/// How long a test waits for the rival once the first opener has
/// committed.
const RIVAL_FINISH: Duration = Duration::from_secs(60);

type Rival = Pin<Box<dyn Future<Output = std::result::Result<(), String>> + Send>>;

/// Where [`RivalAt`] pauses the first opener and starts the second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Moment {
    /// The migration has read the recorded version.
    VersionRead,
    /// The grant log's catch-up has read the projection cursor.
    AfterLadderRead,
}

/// Starts a rival opener when the watched open reaches a [`Moment`]; see
/// the module docs.
struct RivalAt {
    moment: Moment,
    db: PathBuf,
    pending: Option<Rival>,
    running: Option<JoinHandle<std::result::Result<(), String>>>,
    /// The rival's outcome, when it finished inside its head start.
    finished: Option<std::result::Result<(), String>>,
    /// The version the paused migration read, if it reported one.
    version: Option<i64>,
    /// Whether some connection held the write lock while the first opener
    /// was paused: the migration's own, if it took one.
    lock_held: Option<bool>,
    /// Whether the rival was still unfinished when its head start ran out:
    /// it waited for the first opener rather than racing it.
    rival_waited: Option<bool>,
}

impl RivalAt {
    fn new(
        moment: Moment,
        db: &Path,
        rival: impl Future<Output = std::result::Result<(), String>> + Send + 'static,
    ) -> Self {
        RivalAt {
            moment,
            db: db.to_path_buf(),
            pending: Some(Box::pin(rival)),
            running: None,
            finished: None,
            version: None,
            lock_held: None,
            rival_waited: None,
        }
    }

    async fn start_rival(&mut self) {
        let Some(rival) = self.pending.take() else {
            return;
        };
        self.lock_held = Some(write_lock_held(&self.db).await);
        let mut running = tokio::spawn(rival);
        // Elapsing is the expected outcome under the lock, not a failure:
        // the rival is still queued behind it and finishes later.
        match tokio::time::timeout(RIVAL_HEAD_START, &mut running).await {
            Ok(joined) => {
                self.rival_waited = Some(false);
                self.finished = Some(joined.expect("the rival opener panicked"));
            }
            Err(_) => {
                self.rival_waited = Some(true);
                self.running = Some(running);
            }
        }
    }

    /// What the rival came to, waiting a bounded time for it to finish.
    async fn rival_outcome(&mut self) -> std::result::Result<(), String> {
        if let Some(outcome) = self.finished.take() {
            return outcome;
        }
        let rival = self
            .running
            .take()
            .expect("the watched open never reached its moment, so the rival never started");
        tokio::time::timeout(RIVAL_FINISH, rival)
            .await
            .expect("the rival opener did not finish within a minute of the first committing")
            .expect("the rival opener panicked")
    }

    /// Both openers succeeded, and the second waited for the first.
    fn assert_serialised(&self, first: Result<()>, rival: std::result::Result<(), String>) {
        if let Err(err) = first {
            panic!("the first opener must succeed: {err}");
        }
        if let Err(err) = rival {
            panic!("the second opener must succeed: {err}");
        }
        assert_eq!(
            self.lock_held,
            Some(true),
            "the first opener read what decides its work without holding the write lock"
        );
        assert_eq!(
            self.rival_waited,
            Some(true),
            "the second opener finished while the first was still paused"
        );
    }
}

impl MigrationSeam for RivalAt {
    async fn version_read(&mut self, version: i64) {
        self.version = Some(version);
        if self.moment == Moment::VersionRead {
            self.start_rival().await;
        }
    }

    async fn after_ladder_read(&mut self) {
        if self.moment == Moment::AfterLadderRead {
            self.start_rival().await;
        }
    }
}

/// Two openers of the file at `path`, which is at version `at`: the first
/// paused at its version read, the second the store's own `open`. Both
/// must succeed, the second must have waited, and the file must hold what
/// a lone open leaves.
async fn race_two_openers(store: Store, at: usize) {
    let dir = tempdir().unwrap();
    let path = dir.path().join(store.file());
    let pool = pool_at(store, &path, at).await;
    let mut seam = RivalAt::new(Moment::VersionRead, &path, store.open(path.clone()));

    let first = store
        .migrate(&pool.connect_options(), store.ladder(), &mut seam)
        .await;
    let rival = seam.rival_outcome().await;
    pool.close().await;

    seam.assert_serialised(first, rival);
    assert_eq!(seam.version, Some(at as i64), "the first opener's read");
    assert_eq!(on_disk(&path).await, store.reference().await);
}

#[tokio::test]
async fn two_openers_of_a_fresh_index_db_both_succeed_and_leave_one_schema() {
    race_two_openers(Store::Index, 0).await;
}

#[tokio::test]
async fn two_openers_of_a_fresh_grants_db_both_succeed_and_leave_one_schema() {
    race_two_openers(Store::Grants, 0).await;
}

/// From version 2 every remaining rung of the index is an `ADD COLUMN`,
/// the kind that fails when it runs a second time.
#[tokio::test]
async fn two_openers_upgrading_an_index_db_both_succeed() {
    race_two_openers(Store::Index, 2).await;
}

// ----- The grant log's catch-up -----

/// Every agent's live grants, in a fixed order: the answer the projection
/// gives.
async fn live_grants(log: &SqliteGrantLog) -> Vec<Vec<LiveGrant>> {
    let mut grants = Vec::new();
    for agent in ["alice", "bob", "carol"] {
        grants.push(
            log.live_grants_for(&Principal::Agent(agent.into()))
                .await
                .unwrap(),
        );
    }
    grants
}

/// A `grants.db` at `path` whose projection lags its log, built the way
/// `open_catches_up_a_stale_projection` builds one: append, then wipe the
/// projection and reset its cursor. Returns the live grants before the
/// wipe.
async fn lagging_grants_db(path: &Path) -> Vec<Vec<LiveGrant>> {
    let log = SqliteGrantLog::open(path).await.unwrap();
    let alice = Principal::Agent("alice".into());
    let bob = Principal::Agent("bob".into());
    let carol = Principal::Agent("carol".into());
    let read = BTreeSet::from([Verb::Read]);
    log.append_granted(
        &Grantor::Operator,
        &alice,
        &BTreeSet::from([Verb::Read, Verb::Grant]),
        &Scope::Namespace("research".into()),
    )
    .await
    .unwrap();
    log.append_granted(
        &Grantor::Agent("alice".into()),
        &bob,
        &read,
        &Scope::Namespace("research.papers".into()),
    )
    .await
    .unwrap();
    let revoked = log
        .append_granted(
            &Grantor::Operator,
            &carol,
            &read,
            &Scope::Name("docs.readme".into()),
        )
        .await
        .unwrap();
    log.append_revoked(revoked).await.unwrap();
    let before = live_grants(&log).await;
    assert!(
        before.iter().any(|grants| !grants.is_empty()),
        "the log must leave something live for the projection to lose"
    );
    drop(log);

    let mut conn = SqliteConnectOptions::new()
        .filename(path)
        .connect()
        .await
        .unwrap();
    sqlx::raw_sql(
        "DELETE FROM projected_grants;
         DELETE FROM projected_revocations;
         UPDATE projection_cursor SET applied_seq = 0;",
    )
    .execute(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();
    before
}

/// Two openers of a `grants.db` whose projection lags its log: the first
/// paused once its catch-up has read the cursor, the second the store's own
/// `open`. Without the lock both read the old cursor, and the first, coming
/// to write after the second has committed, fails with `database is
/// locked`.
#[tokio::test]
async fn two_openers_of_a_grants_db_with_a_lagging_projection_both_succeed() {
    let dir = tempdir().unwrap();
    let path = dir.path().join(Store::Grants.file());
    let before = lagging_grants_db(&path).await;
    let pool = connect_store_pool(&path)
        .await
        .expect("open the store pool");
    let mut seam = RivalAt::new(
        Moment::AfterLadderRead,
        &path,
        Store::Grants.open(path.clone()),
    );

    let after_ladder = async |conn: &mut SqliteConnection| catch_up(conn, &mut seam).await;
    let first = migrate_observed(
        &pool.connect_options(),
        Store::Grants.ladder(),
        after_ladder,
        &mut (),
    )
    .await;
    let rival = seam.rival_outcome().await;
    pool.close().await;

    seam.assert_serialised(first, rival);
    let log = SqliteGrantLog::open(&path).await.unwrap();
    assert_eq!(
        live_grants(&log).await,
        before,
        "the caught-up projection must answer as it did before the wipe"
    );
}

// ----- A ladder that stops part-way -----

/// A rung that fails, appended after every real rung, so everything the
/// ladder does has been done when it fails.
const FAILING_RUNG: &str = "SELECT no_such_column FROM sqlite_master";

/// The file at version `at` meets a ladder that fails after its last real
/// rung. It must be left exactly as it was, with the lock free, and the
/// store's own open must then succeed.
async fn interrupted_ladder(store: Store, at: usize) {
    let dir = tempdir().unwrap();
    let path = dir.path().join(store.file());
    let pool = pool_at(store, &path, at).await;
    let before = on_disk(&path).await;
    let failing: Vec<&'static str> = store
        .ladder()
        .iter()
        .copied()
        .chain([FAILING_RUNG])
        .collect();

    let err = store
        .migrate(&pool.connect_options(), &failing, &mut ())
        .await
        .expect_err("the injected rung must fail the migration");
    assert!(
        err.to_string().contains("no_such_column"),
        "failed for the wrong reason: {err}"
    );
    assert_eq!(
        on_disk(&path).await,
        before,
        "the rungs before the failure must roll back with it"
    );
    assert!(
        !write_lock_held(&path).await,
        "a failed migration must give the lock back"
    );
    pool.close().await;

    store
        .open(path.clone())
        .await
        .expect("the next open after a failed migration must succeed");
    assert_eq!(on_disk(&path).await, store.reference().await);
}

#[tokio::test]
async fn an_interrupted_ladder_leaves_a_fresh_index_db_empty() {
    interrupted_ladder(Store::Index, 0).await;
}

#[tokio::test]
async fn an_interrupted_ladder_leaves_a_fresh_grants_db_empty() {
    interrupted_ladder(Store::Grants, 0).await;
}

#[tokio::test]
async fn an_interrupted_index_db_upgrade_stays_at_its_old_version() {
    interrupted_ladder(Store::Index, 2).await;
}

/// Holds the migration once a rung has been applied, and says so: from
/// then on the migration can only end by being dropped.
struct HoldAfterRung {
    rung: i64,
    reached: Option<tokio::sync::oneshot::Sender<()>>,
}

impl MigrationSeam for HoldAfterRung {
    async fn rung_applied(&mut self, version: i64) {
        if version == self.rung {
            if let Some(reached) = self.reached.take() {
                let _ = reached.send(());
            }
            std::future::pending::<()>().await;
        }
    }
}

#[tokio::test]
async fn a_cancelled_open_leaves_the_file_unchanged_and_the_lock_free() {
    let dir = tempdir().unwrap();
    let path = dir.path().join(Store::Index.file());
    let pool = pool_at(Store::Index, &path, 2).await;
    let before = on_disk(&path).await;

    let (reached, held) = tokio::sync::oneshot::channel();
    let mut seam = HoldAfterRung {
        rung: 4,
        reached: Some(reached),
    };
    {
        let options = pool.connect_options();
        let migration = Store::Index.migrate(&options, Store::Index.ladder(), &mut seam);
        let mut migration = std::pin::pin!(migration);
        tokio::select! {
            outcome = &mut migration => {
                panic!("the migration ended instead of holding after rung 4: {outcome:?}")
            }
            held = held => held.expect("the seam was dropped before rung 4"),
            () = tokio::time::sleep(RIVAL_FINISH) => {
                panic!("the migration did not reach rung 4 within a minute")
            }
        }
        // Dropped here, held after rung 4: rungs 3 and 4 are applied and
        // uncommitted.
    }

    assert!(
        lock_released(&path).await,
        "the cancelled migration never gave the lock back"
    );
    assert_eq!(
        on_disk(&path).await,
        before,
        "rungs 3 and 4 must roll back with the cancelled migration"
    );
    pool.close().await;

    Store::Index
        .open(path.clone())
        .await
        .expect("the next open after a cancelled one must succeed");
    assert_eq!(on_disk(&path).await, Store::Index.reference().await);
}

// ----- A file a newer binary wrote -----

/// The highest version `store`'s ladder brings a file to: this binary's.
fn binary_version(store: Store) -> i64 {
    store.ladder().len() as i64
}

/// One version past this binary's: the least a newer binary's ladder can
/// have left.
fn newer_version(store: Store) -> i64 {
    binary_version(store) + 1
}

/// Records the version the file at `path` as a newer binary's ladder would
/// have left it.
async fn stamp(path: &Path, version: i64) {
    let mut conn = SqliteConnectOptions::new()
        .filename(path)
        .connect()
        .await
        .expect("open to stamp");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "PRAGMA user_version = {version}"
    )))
    .execute(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();
}

/// The file at `path`, which records [`newer_version`], meets the store's
/// own `open`. It must be refused by name — the file, the version it
/// records, this binary's — and left exactly as it was, with the lock free.
async fn assert_refused_and_left_as_it_is(store: Store, path: &Path) {
    let before = on_disk(path).await;
    assert_eq!(before.0, newer_version(store), "the fixture's stamp");

    let err = store
        .try_open(path)
        .await
        .expect_err("a file a newer binary wrote must be refused");
    let StoreError::BinaryTooOld {
        path: refused,
        db_version,
        binary_version: supported,
    } = &err
    else {
        panic!("refused for the wrong reason: {err}");
    };
    assert_eq!(refused, path, "the refusal names the file");
    assert_eq!(*db_version, newer_version(store), "the file's version");
    assert_eq!(*supported, binary_version(store), "this binary's version");
    let message = err.to_string();
    for part in [
        path.display().to_string(),
        format!("version {}", newer_version(store)),
        format!("supports up to {}", binary_version(store)),
        "written by a newer fq-cas".to_string(),
        "at least as new".to_string(),
    ] {
        assert!(message.contains(&part), "{part:?} is not in: {message}");
    }

    assert!(
        !write_lock_held(path).await,
        "a refused open must give the lock back"
    );
    assert_eq!(
        on_disk(path).await,
        before,
        "a refused open must leave the file as it was"
    );
}

#[tokio::test]
async fn an_index_db_a_newer_binary_wrote_is_refused_and_left_as_it_is() {
    let dir = tempdir().unwrap();
    let path = dir.path().join(Store::Index.file());
    pool_at(Store::Index, &path, Store::Index.ladder().len())
        .await
        .close()
        .await;
    stamp(&path, newer_version(Store::Index)).await;

    assert_refused_and_left_as_it_is(Store::Index, &path).await;
}

/// How far a `grants.db`'s projection has got: its cursor and the rows it
/// holds, beside the last sequence in its log.
#[derive(Debug, PartialEq, Eq)]
struct Projection {
    cursor: i64,
    grants: i64,
    revocations: i64,
    last_event: i64,
}

async fn projection(path: &Path) -> Projection {
    async fn scalar(conn: &mut SqliteConnection, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(conn).await.unwrap()
    }
    let mut conn = SqliteConnectOptions::new()
        .filename(path)
        .connect()
        .await
        .expect("open for inspection");
    let projection = Projection {
        cursor: scalar(
            &mut conn,
            "SELECT applied_seq FROM projection_cursor WHERE id = 1",
        )
        .await,
        grants: scalar(&mut conn, "SELECT COUNT(*) FROM projected_grants").await,
        revocations: scalar(&mut conn, "SELECT COUNT(*) FROM projected_revocations").await,
        last_event: scalar(&mut conn, "SELECT COALESCE(MAX(seq), 0) FROM grant_events").await,
    };
    conn.close().await.unwrap();
    projection
}

/// Which moments of an open it reached.
#[derive(Debug, Default)]
struct Reached {
    version: Option<i64>,
    rungs: Vec<i64>,
    after_ladder_read: bool,
}

impl MigrationSeam for Reached {
    async fn version_read(&mut self, version: i64) {
        self.version = Some(version);
    }

    async fn rung_applied(&mut self, version: i64) {
        self.rungs.push(version);
    }

    async fn after_ladder_read(&mut self) {
        self.after_ladder_read = true;
    }
}

/// A newer `grants.db` whose projection lags its log is refused before the
/// catch-up runs. The file shows it — the projection is as far behind
/// afterwards as before — but a rollback would undo a catch-up that ran
/// before the refusal too, so the seam shows it directly: the catch-up
/// never read its cursor.
#[tokio::test]
async fn a_grants_db_a_newer_binary_wrote_is_refused_before_its_projection_catches_up() {
    let dir = tempdir().unwrap();
    let path = dir.path().join(Store::Grants.file());
    lagging_grants_db(&path).await;
    stamp(&path, newer_version(Store::Grants)).await;
    let lagging = projection(&path).await;
    assert!(
        lagging.cursor < lagging.last_event && lagging.grants == 0,
        "the fixture's projection must lag its log: {lagging:?}"
    );

    assert_refused_and_left_as_it_is(Store::Grants, &path).await;
    assert_eq!(
        projection(&path).await,
        lagging,
        "a refused open must leave the projection where it was"
    );

    let pool = connect_store_pool(&path)
        .await
        .expect("open the store pool");
    let mut migration = Reached::default();
    let mut catching_up = Reached::default();
    let after_ladder = async |conn: &mut SqliteConnection| catch_up(conn, &mut catching_up).await;
    let outcome = migrate_observed(
        &pool.connect_options(),
        Store::Grants.ladder(),
        after_ladder,
        &mut migration,
    )
    .await;
    pool.close().await;

    assert!(
        matches!(outcome, Err(StoreError::BinaryTooOld { .. })),
        "a file a newer binary wrote must be refused: {outcome:?}"
    );
    assert_eq!(migration.version, Some(newer_version(Store::Grants)));
    assert_eq!(migration.rungs, Vec::<i64>::new(), "no rung may run");
    assert!(
        !catching_up.after_ladder_read,
        "the catch-up read its cursor: it ran before the refusal"
    );
    assert_eq!(projection(&path).await, lagging);
}

// ----- The step before the lock: a file another connection is creating -----

/// How long the creating connection keeps the file locked. The open under
/// test must still be waiting when it lets go.
const CREATOR_HOLDS: Duration = Duration::from_millis(500);

/// What an opener meets when another process created the database an
/// instant earlier and has not yet switched it into WAL mode: a file in
/// SQLite's default journal mode, with that process's write lock on it.
/// The opener's own switch into WAL mode needs that lock, and SQLite will
/// not wait for it. Opens `store` against such a file and lets the creator
/// go after [`CREATOR_HOLDS`].
async fn open_while_another_connection_creates(store: Store) {
    let dir = tempdir().unwrap();
    let path = dir.path().join(store.file());
    let mut creator = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .connect()
        .await
        .expect("create the file as the other process");
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut creator)
        .await
        .expect("take the creator's lock");

    let opening = tokio::spawn(store.open(path.clone()));
    tokio::time::sleep(CREATOR_HOLDS).await;
    let finished_while_held = opening.is_finished();
    sqlx::query("ROLLBACK")
        .execute(&mut creator)
        .await
        .expect("release the creator's lock");
    creator.close().await.expect("close the creator");

    let opened = tokio::time::timeout(Duration::from_secs(60), opening)
        .await
        .expect("the open did not finish within a minute of the creator letting go")
        .expect("the open panicked");
    if let Err(err) = opened {
        panic!("the open must wait for the creator, then succeed: {err}");
    }
    assert!(
        !finished_while_held,
        "the open finished while the creator still held the file"
    );
    assert_eq!(on_disk(&path).await, store.reference().await);
}

#[tokio::test]
async fn an_index_db_another_connection_is_creating_opens_once_it_lets_go() {
    open_while_another_connection_creates(Store::Index).await;
}

#[tokio::test]
async fn a_grants_db_another_connection_is_creating_opens_once_it_lets_go() {
    open_while_another_connection_creates(Store::Grants).await;
}

/// The other process is this test binary run again, filtered to
/// `opener_process` below. What makes these cross-process is that the
/// parent's lock and the child's are two processes' locks on one file — the
/// lock SQLite takes for `BEGIN IMMEDIATE`; an in-process mutex would not
/// show up here, and a killed process takes nothing with it but its own
/// uncommitted transaction.
#[cfg(unix)]
mod across_processes {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;

    use fq_test_support::TestChild;

    use super::*;

    const OPENER_PROCESS: &str = "schema::tests::across_processes::opener_process";
    const CHILD_DB: &str = "FQ_STORE_SCHEMA_TEST_DB";
    const CHILD_ROLE: &str = "FQ_STORE_SCHEMA_TEST_ROLE";
    /// Open `grants.db` through the store and say how it went.
    const ROLE_OPEN: &str = "open-grants";
    /// Upgrade `index.db` and stop after rung 4, transaction open, to be
    /// killed there.
    const ROLE_STALL: &str = "stall-index-after-rung-4";
    /// Every line the child means the parent to read starts with this.
    const SAYS: &str = "store-schema-test-child: ";
    /// How long a stalled child waits to be killed before giving up on its
    /// parent. Bounds the child if the parent dies first.
    const STALL: Duration = Duration::from_secs(60);
    /// POSIX gives `SIGKILL` the number 9 on every Unix; spelled out here
    /// rather than taking `libc` on as a dependency for one constant.
    const SIGKILL: i32 = 9;

    fn opener_child(db: &Path, role: &str) -> TestChild {
        TestChild::builder(std::env::current_exe().expect("this test binary"))
            .args(["--exact", OPENER_PROCESS, "--nocapture"])
            .env(CHILD_DB, db)
            .env(CHILD_ROLE, role)
            .stdout(Stdio::piped())
            .spawn()
    }

    /// Stops the ladder after a rung, with the transaction still open, and
    /// says so.
    struct StallAfterRung(i64);

    impl MigrationSeam for StallAfterRung {
        async fn rung_applied(&mut self, version: i64) {
            if version == self.0 {
                println!("{SAYS}rung-applied {version}");
                tokio::time::sleep(STALL).await;
            }
        }
    }

    /// The child process's body. A no-op in an ordinary run: it acts only
    /// when a test below has started it.
    #[tokio::test]
    async fn opener_process() {
        let Ok(db) = std::env::var(CHILD_DB) else {
            return;
        };
        let db = PathBuf::from(db);
        let role = std::env::var(CHILD_ROLE).expect("a child is always given a role");
        match role.as_str() {
            ROLE_OPEN => match Store::Grants.open(db).await {
                Ok(()) => println!("{SAYS}opened"),
                Err(err) => {
                    println!("{SAYS}error {err}");
                    panic!("the child's open failed: {err}");
                }
            },
            ROLE_STALL => {
                let pool = connect_store_pool(&db).await.expect("open the store pool");
                let outcome = Store::Index
                    .migrate(
                        &pool.connect_options(),
                        Store::Index.ladder(),
                        &mut StallAfterRung(4),
                    )
                    .await;
                panic!("the child was not killed mid-ladder (outcome: {outcome:?})");
            }
            other => panic!("unknown child role {other:?}"),
        }
    }

    /// Starts the child opener when the parent's migration has read the
    /// version, and gives it the same bounded head start the in-process
    /// rival gets.
    struct ChildAtVersionRead {
        db: PathBuf,
        child: Option<TestChild>,
        /// Taken at once: a child that exits inside its head start is
        /// reaped by the poll, and its pipe goes with it.
        stdout: Option<std::process::ChildStdout>,
        lock_held: Option<bool>,
        /// Whether the child exited before its head start ran out, rather
        /// than waiting for the parent.
        exited_during_head_start: Option<bool>,
    }

    impl MigrationSeam for ChildAtVersionRead {
        async fn version_read(&mut self, _version: i64) {
            self.lock_held = Some(write_lock_held(&self.db).await);
            let mut child = opener_child(&self.db, ROLE_OPEN);
            self.stdout = child.take_stdout();
            const POLL: Duration = Duration::from_millis(25);
            let polls = RIVAL_HEAD_START.as_millis() / POLL.as_millis();
            let mut exited = false;
            for _ in 0..polls {
                if child.try_wait().expect("poll the child").is_some() {
                    exited = true;
                    break;
                }
                tokio::time::sleep(POLL).await;
            }
            self.exited_during_head_start = Some(exited);
            self.child = Some(child);
        }
    }

    #[tokio::test]
    async fn a_second_process_waits_for_the_first_ones_migration() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(Store::Grants.file());
        let pool = pool_at(Store::Grants, &path, 0).await;
        let mut seam = ChildAtVersionRead {
            db: path.clone(),
            child: None,
            stdout: None,
            lock_held: None,
            exited_during_head_start: None,
        };

        let first = Store::Grants
            .migrate(&pool.connect_options(), Store::Grants.ladder(), &mut seam)
            .await;
        pool.close().await;

        let mut child = seam.child.take().expect("the child was started");
        let mut stdout = seam.stdout.take().expect("the child's stdout is piped");
        let status = child
            .wait_timeout(Duration::from_secs(60))
            .expect("the child opener did not finish within a minute of the parent committing");
        let mut said = String::new();
        std::io::Read::read_to_string(&mut stdout, &mut said).unwrap();

        if let Err(err) = first {
            panic!("the first process's open must succeed: {err}\nthe child said:\n{said}");
        }
        assert!(
            status.success(),
            "the second process's open must succeed: {status:?}\n{said}"
        );
        assert!(said.contains(&format!("{SAYS}opened")), "{said}");
        assert_eq!(seam.lock_held, Some(true));
        assert_eq!(
            seam.exited_during_head_start,
            Some(false),
            "the second process finished while the first was still paused:\n{said}"
        );
        assert_eq!(on_disk(&path).await, Store::Grants.reference().await);
    }

    /// A refusal gives the lock back as a failure does. The first process
    /// is refused a `grants.db` a newer binary wrote while a second waits
    /// on its lock; the second must then be refused by name too, not left
    /// to fail with `database is locked` once its wait runs out.
    #[tokio::test]
    async fn a_second_process_waits_for_the_first_ones_refusal_and_is_refused_too() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(Store::Grants.file());
        pool_at(Store::Grants, &path, Store::Grants.ladder().len())
            .await
            .close()
            .await;
        stamp(&path, newer_version(Store::Grants)).await;
        let before = on_disk(&path).await;
        let pool = connect_store_pool(&path)
            .await
            .expect("open the store pool");
        let mut seam = ChildAtVersionRead {
            db: path.clone(),
            child: None,
            stdout: None,
            lock_held: None,
            exited_during_head_start: None,
        };

        let first = Store::Grants
            .migrate(&pool.connect_options(), Store::Grants.ladder(), &mut seam)
            .await;
        pool.close().await;

        let mut child = seam.child.take().expect("the child was started");
        let mut stdout = seam.stdout.take().expect("the child's stdout is piped");
        let status = child
            .wait_timeout(Duration::from_secs(60))
            .expect("the child opener did not finish within a minute of the parent's refusal");
        let mut said = String::new();
        std::io::Read::read_to_string(&mut stdout, &mut said).unwrap();

        assert!(
            matches!(first, Err(StoreError::BinaryTooOld { .. })),
            "the first process must be refused: {first:?}\nthe child said:\n{said}"
        );
        assert!(
            !status.success(),
            "the second process must be refused too: {status:?}\n{said}"
        );
        assert!(
            said.contains(&format!("{SAYS}error incompatible schema: "))
                && said.contains("written by a newer fq-cas"),
            "the second process must be refused by name:\n{said}"
        );
        assert_eq!(seam.lock_held, Some(true));
        assert_eq!(
            seam.exited_during_head_start,
            Some(false),
            "the second process finished while the first was still paused:\n{said}"
        );
        assert!(
            !write_lock_held(&path).await,
            "both refusals give the lock back"
        );
        assert_eq!(on_disk(&path).await, before);
    }

    #[tokio::test]
    async fn a_process_killed_mid_upgrade_leaves_the_file_at_its_old_version() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(Store::Index.file());
        pool_at(Store::Index, &path, 2).await.close().await;
        let before = on_disk(&path).await;

        let mut child = opener_child(&path, ROLE_STALL);
        let stdout = child.take_stdout().expect("the child's stdout is piped");
        let (lines, said) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout)
                .lines()
                .map_while(std::result::Result::ok)
            {
                if lines.send(line).is_err() {
                    return;
                }
            }
        });
        // Bounded by count and by time: the child prints a handful of
        // harness lines before its own.
        let marker = format!("{SAYS}rung-applied 4");
        let mut stalled = false;
        for _ in 0..100 {
            match said.recv_timeout(Duration::from_secs(60)) {
                Ok(line) if line.contains(&marker) => {
                    stalled = true;
                    break;
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        assert!(stalled, "the child never reported rung 4 applied");
        assert!(
            write_lock_held(&path).await,
            "the stalled child should be holding the write lock"
        );

        child.signal(SIGKILL).expect("kill the child mid-ladder");
        child
            .wait_timeout(Duration::from_secs(10))
            .expect("the killed child was not reaped");

        assert_eq!(
            on_disk(&path).await,
            before,
            "rungs 3 and 4 of the killed upgrade must not have reached the file"
        );
        assert!(
            !write_lock_held(&path).await,
            "the killed process's lock must have gone with it"
        );
        Store::Index
            .open(path.clone())
            .await
            .expect("the next open after the kill must succeed");
        assert_eq!(on_disk(&path).await, Store::Index.reference().await);
    }
}
