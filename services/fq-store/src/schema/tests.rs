//! The migration lock's two properties
//! (<https://github.com/bricef/factor-q/issues/975>), on both of the
//! store's real ladders:
//!
//! - **Serialised**: a second opener that arrives while the first is
//!   between reading the version and applying the ladder waits, then
//!   finds nothing to do — in this process and in another.
//! - **Atomic**: a ladder that stops part-way — an injected failure on a
//!   later rung, or an open that is cancelled — leaves the file at its old
//!   version with nothing applied and the lock free, and the next open
//!   succeeds.
//!
//! And the step before the lock: a database another connection is still
//! creating, and has not yet switched into WAL mode, opens once that
//! connection lets go.
//!
//! The races are staged by [`RivalAtVersionRead`], which pauses the first
//! opener at the moment it has read the version — the window two
//! unserialised openers used to share — starts the second there, and gives
//! it a bounded head start before letting the first go on. Without the
//! lock the second reads the same old version and runs the whole ladder
//! inside its head start, so the first then applies rungs that are already
//! there and fails. With the lock the second cannot begin until the first
//! commits, and the head start runs out with it still waiting.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection};
use tempfile::tempdir;
use tokio::task::JoinHandle;

use super::*;
use crate::{SqliteGrantLog, SqliteNameIndex};

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

    /// Open the file at `path` the way the store does, as a second opener
    /// would, and close it again.
    async fn open(self, path: PathBuf) -> std::result::Result<(), String> {
        let opened = match self {
            Store::Index => SqliteNameIndex::open(&path).await.map(drop),
            Store::Grants => SqliteGrantLog::open(&path).await.map(drop),
        };
        opened.map_err(|err| err.to_string())
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

/// A file's `user_version`, and every schema object in it as
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
    let pool = connect_store_pool(path)
        .await
        .expect("open the store pool");
    if at > 0 {
        migrate(&pool, &store.ladder()[..at])
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
/// the lock a rival ladder finishes in milliseconds, far inside this; with
/// it the rival cannot finish until the first opener commits, so every race
/// test spends exactly this long here. Well under [`SCHEMA_LOCK_WAIT`],
/// which is how long the rival will wait.
const RIVAL_HEAD_START: Duration = Duration::from_secs(2);

/// How long a test waits for the rival once the first opener has
/// committed.
const RIVAL_FINISH: Duration = Duration::from_secs(60);

type Rival = Pin<Box<dyn Future<Output = std::result::Result<(), String>> + Send>>;

/// Starts a rival opener when the watched migration reads the version; see
/// the module docs.
struct RivalAtVersionRead {
    db: PathBuf,
    pending: Option<Rival>,
    running: Option<JoinHandle<std::result::Result<(), String>>>,
    /// The rival's outcome, when it finished inside its head start.
    finished: Option<std::result::Result<(), String>>,
    /// What the paused migration read.
    version: Option<i64>,
    /// Whether some connection held the write lock while the migration was
    /// paused: the migration's own, if it took one.
    lock_held: Option<bool>,
}

impl RivalAtVersionRead {
    fn new(
        db: &Path,
        rival: impl Future<Output = std::result::Result<(), String>> + Send + 'static,
    ) -> Self {
        RivalAtVersionRead {
            db: db.to_path_buf(),
            pending: Some(Box::pin(rival)),
            running: None,
            finished: None,
            version: None,
            lock_held: None,
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
            .expect("the watched migration never read a version, so the rival never started");
        tokio::time::timeout(RIVAL_FINISH, rival)
            .await
            .expect("the rival opener did not finish within a minute of the first committing")
            .expect("the rival opener panicked")
    }
}

impl MigrationSeam for RivalAtVersionRead {
    async fn version_read(&mut self, version: i64) {
        self.version = Some(version);
        let Some(rival) = self.pending.take() else {
            return;
        };
        self.lock_held = Some(write_lock_held(&self.db).await);
        let mut running = tokio::spawn(rival);
        // Elapsing is the expected outcome under the lock, not a failure:
        // the rival is still queued behind it and finishes later.
        match tokio::time::timeout(RIVAL_HEAD_START, &mut running).await {
            Ok(joined) => self.finished = Some(joined.expect("the rival opener panicked")),
            Err(_) => self.running = Some(running),
        }
    }
}

/// Two openers of the file at `path`, which is at version `at`: the first
/// paused at its version read, the second the store's own `open`. Both
/// must succeed and leave the schema a lone open leaves.
async fn race_two_openers(store: Store, at: usize) {
    let dir = tempdir().unwrap();
    let path = dir.path().join(store.file());
    let pool = pool_at(store, &path, at).await;
    let mut seam = RivalAtVersionRead::new(&path, store.open(path.clone()));

    let first = migrate_observed(&pool, store.ladder(), &mut seam).await;
    let rival = seam.rival_outcome().await;
    pool.close().await;

    if let Err(err) = first {
        panic!("the first opener must succeed: {err}");
    }
    if let Err(err) = rival {
        panic!("the second opener must succeed: {err}");
    }
    assert_eq!(seam.version, Some(at as i64), "the first opener's read");
    assert_eq!(
        seam.lock_held,
        Some(true),
        "the first opener read the version without holding the write lock"
    );
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
    let failing: Vec<&'static str> = store.ladder().iter().copied().chain([FAILING_RUNG]).collect();

    let err = migrate(&pool, &failing)
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

/// Never lets the migration past a rung: it can only end by being
/// dropped.
struct HangAfterRung(i64);

impl MigrationSeam for HangAfterRung {
    async fn rung_applied(&mut self, version: i64) {
        if version == self.0 {
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

    // Held after rung 4 — rungs 3 and 4 applied, uncommitted — until the
    // timeout drops it.
    let cancelled = tokio::time::timeout(
        Duration::from_millis(500),
        migrate_observed(&pool, Store::Index.ladder(), &mut HangAfterRung(4)),
    )
    .await;
    assert!(
        cancelled.is_err(),
        "the migration should still have been held after rung 4: {cancelled:?}"
    );

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

/// A file a newer binary has migrated past this ladder is opened as it is.
/// This crate has never refused one, and this pins that the open still
/// changes nothing and gives the lock back.
#[tokio::test]
async fn a_file_newer_than_the_binary_is_left_as_it_is() {
    let dir = tempdir().unwrap();
    let path = dir.path().join(Store::Index.file());
    let pool = pool_at(Store::Index, &path, Store::Index.ladder().len()).await;
    let newer = Store::Index.ladder().len() + 1;
    sqlx::query(sqlx::AssertSqlSafe(format!("PRAGMA user_version = {newer}")))
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let before = on_disk(&path).await;

    let index = SqliteNameIndex::open(&path)
        .await
        .expect("a newer file opens");
    assert!(
        !write_lock_held(&path).await,
        "an open with nothing to do must give the lock back"
    );
    drop(index);
    assert_eq!(on_disk(&path).await, before);
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

/// The rival is this test binary run again, filtered to `opener_process`
/// below. What makes this cross-process is that the parent's lock and the
/// child's are two processes' locks on one file — the lock SQLite takes
/// for `BEGIN IMMEDIATE`; an in-process mutex would not show up here.
#[cfg(unix)]
mod across_processes {
    use std::process::Stdio;

    use super::*;

    const OPENER_PROCESS: &str = "schema::tests::across_processes::opener_process";
    const CHILD_DB: &str = "FQ_STORE_SCHEMA_TEST_DB";
    /// Every line the child means the parent to read starts with this.
    const SAYS: &str = "store-schema-test-child: ";

    /// The child process's body: open `grants.db` through the store and say
    /// how it went. A no-op in an ordinary run; it acts only when the test
    /// below has started it.
    #[tokio::test]
    async fn opener_process() {
        let Ok(db) = std::env::var(CHILD_DB) else {
            return;
        };
        match Store::Grants.open(PathBuf::from(db)).await {
            Ok(()) => println!("{SAYS}opened"),
            Err(err) => {
                println!("{SAYS}error {err}");
                panic!("the child's open failed: {err}");
            }
        }
    }

    /// Starts the child opener when the parent's migration has read the
    /// version, and gives it the same bounded head start the in-process
    /// rival gets.
    struct ChildAtVersionRead {
        db: PathBuf,
        child: Option<fq_test_support::TestChild>,
        /// Taken at once: a child that exits inside its head start is
        /// reaped by the poll, and its pipe goes with it.
        stdout: Option<std::process::ChildStdout>,
        lock_held: Option<bool>,
    }

    impl MigrationSeam for ChildAtVersionRead {
        async fn version_read(&mut self, _version: i64) {
            self.lock_held = Some(write_lock_held(&self.db).await);
            let mut child =
                fq_test_support::TestChild::builder(std::env::current_exe().expect("this binary"))
                    .args(["--exact", OPENER_PROCESS, "--nocapture"])
                    .env(CHILD_DB, &self.db)
                    .stdout(Stdio::piped())
                    .spawn();
            self.stdout = child.take_stdout();
            const POLL: Duration = Duration::from_millis(25);
            let polls = RIVAL_HEAD_START.as_millis() / POLL.as_millis();
            for _ in 0..polls {
                if child.try_wait().expect("poll the child").is_some() {
                    break;
                }
                tokio::time::sleep(POLL).await;
            }
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
        };

        let first = migrate_observed(&pool, Store::Grants.ladder(), &mut seam).await;
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
        assert_eq!(on_disk(&path).await, Store::Grants.reference().await);
    }
}
