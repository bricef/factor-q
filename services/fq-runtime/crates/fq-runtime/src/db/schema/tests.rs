//! The schema lock's two properties (#970), on a toy ladder whose second
//! and third rungs are `ALTER TABLE ADD COLUMN` — the kind of rung that
//! fails when it runs twice. The stores' own tests repeat the race on
//! their real ladders; these pin the kit itself, including across two
//! processes.
//!
//! - **Serialised**: a second opener that arrives while the first is
//!   between reading the version and applying the ladder waits, then
//!   finds nothing to do — in this process and in another.
//! - **Atomic**: a ladder that stops part-way — an injected failure, a
//!   cancelled step, or a process killed with `SIGKILL` after a rung —
//!   leaves the file at its old version with nothing applied, and the
//!   next open succeeds.
//!
//! And the step before the lock: a store whose file another process has
//! just created, and has not yet switched into WAL mode, waits for it
//! rather than failing.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use tempfile::tempdir;

use super::test_seam::{RivalAtVersionRead, write_lock_held};
use super::*;

const CLASS: &str = "toy";

/// The toy ladder. Rung 3 is two statements so an injected failure can
/// land part-way through a rung as well as between rungs.
const LADDER: &[Migration] = &[
    (1, "CREATE TABLE IF NOT EXISTS toy (id INTEGER PRIMARY KEY)"),
    (2, "ALTER TABLE toy ADD COLUMN b TEXT"),
    (
        3,
        "ALTER TABLE toy ADD COLUMN c TEXT; CREATE INDEX IF NOT EXISTS idx_toy_c ON toy(c)",
    ),
];

/// The same ladder with rung 3 failing after its first statement has run.
const LADDER_FAILING_AT_3: &[Migration] = &[
    LADDER[0],
    LADDER[1],
    (
        3,
        "ALTER TABLE toy ADD COLUMN c TEXT; SELECT no_such_column FROM toy",
    ),
];

/// A pool opened the way the stores open theirs.
async fn store_pool(path: &Path) -> Pool<Sqlite> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    connect_store_pool(options, 4)
        .await
        .expect("open the store pool")
}

/// The version recorded for the toy class, and the toy table's columns,
/// read on a fresh connection so nothing a test holds can colour them.
async fn on_disk(path: &Path) -> (Option<u32>, Vec<String>) {
    let mut conn = SqliteConnectOptions::new()
        .filename(path)
        .connect()
        .await
        .expect("open for inspection");
    let has_meta: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_meta')",
    )
    .fetch_one(&mut conn)
    .await
    .unwrap();
    let version = if has_meta {
        read_schema_version(&mut conn, CLASS).await.unwrap()
    } else {
        None
    };
    let columns: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('toy')")
        .fetch_all(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    (version, columns)
}

fn columns(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| name.to_string()).collect()
}

/// A file at toy version 1: bootstrapped by a binary that knew only the
/// first rung.
async fn file_at_v1(path: &Path) {
    let pool = store_pool(path).await;
    bootstrap_versioned(&pool, CLASS, 1, LADDER).await.unwrap();
    pool.close().await;
}

/// Records every verdict a step reads.
#[derive(Default)]
struct Verdicts(Vec<Compatibility>);

impl SchemaSeam for Verdicts {
    async fn version_read(&mut self, verdict: Compatibility) {
        self.0.push(verdict);
    }
}

/// A rival opener of `path` with its own pool, returning what it read
/// and how it ended.
async fn rival_bootstrap(path: PathBuf) -> (Vec<Compatibility>, Result<Compatibility, String>) {
    let pool = store_pool(&path).await;
    let mut seen = Verdicts::default();
    let outcome = bootstrap_observed(&pool, CLASS, 3, LADDER, &mut seen)
        .await
        .map_err(|err| err.to_string());
    pool.close().await;
    (seen.0, outcome)
}

// ----- Serialised, in one process -----

#[tokio::test]
async fn two_openers_of_a_fresh_file_both_succeed_and_leave_one_schema() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("toy.db");
    let mut seam = RivalAtVersionRead::new(&path, rival_bootstrap(path.clone()));

    let pool = store_pool(&path).await;
    let first = bootstrap_observed(&pool, CLASS, 3, LADDER, &mut seam).await;
    let (rival_read, rival) = seam.rival_outcome().await;

    assert_eq!(
        first.expect("the first opener must succeed"),
        Compatibility::FreshInstall
    );
    assert_eq!(
        rival.expect("the second opener must succeed"),
        Compatibility::Current
    );
    assert_eq!(
        rival_read,
        vec![Compatibility::Current],
        "the second opener must read the version the first wrote, not the empty file"
    );
    assert_eq!(
        seam.lock_held,
        Some(true),
        "the first opener read the version without holding the write lock"
    );
    assert_eq!(on_disk(&path).await, (Some(3), columns(&["id", "b", "c"])));
}

#[tokio::test]
async fn two_openers_upgrading_one_file_both_succeed() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("toy.db");
    file_at_v1(&path).await;
    let mut seam = RivalAtVersionRead::new(&path, rival_bootstrap(path.clone()));

    let pool = store_pool(&path).await;
    let first = bootstrap_observed(&pool, CLASS, 3, LADDER, &mut seam).await;
    let (rival_read, rival) = seam.rival_outcome().await;

    assert_eq!(
        first.expect("the first opener must succeed"),
        Compatibility::NeedsUpgrade { from: 1 }
    );
    assert_eq!(
        rival.expect("the second opener must succeed"),
        Compatibility::Current
    );
    assert_eq!(rival_read, vec![Compatibility::Current]);
    assert_eq!(seam.lock_held, Some(true));
    assert_eq!(on_disk(&path).await, (Some(3), columns(&["id", "b", "c"])));
}

// ----- Atomic, in one process -----

#[tokio::test]
async fn a_fresh_ladder_interrupted_after_a_rung_leaves_the_file_empty() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("toy.db");
    let pool = store_pool(&path).await;

    let err = bootstrap_versioned(&pool, CLASS, 3, LADDER_FAILING_AT_3)
        .await
        .expect_err("the injected statement must fail the bootstrap");
    assert!(
        err.to_string().contains("no_such_column"),
        "failed for the wrong reason: {err}"
    );
    assert_eq!(
        on_disk(&path).await,
        (None, Vec::new()),
        "rungs 1 and 2 must not survive the failure of rung 3"
    );

    // And the next open — the same binary, the failure gone — succeeds.
    let verdict = bootstrap_versioned(&pool, CLASS, 3, LADDER).await.unwrap();
    assert_eq!(verdict, Compatibility::FreshInstall);
    assert_eq!(on_disk(&path).await, (Some(3), columns(&["id", "b", "c"])));
}

#[tokio::test]
async fn an_upgrade_interrupted_after_a_rung_stays_at_its_old_version() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("toy.db");
    file_at_v1(&path).await;
    let pool = store_pool(&path).await;

    bootstrap_versioned(&pool, CLASS, 3, LADDER_FAILING_AT_3)
        .await
        .expect_err("the injected statement must fail the upgrade");
    assert_eq!(
        on_disk(&path).await,
        (Some(1), columns(&["id"])),
        "rung 2, and the first half of rung 3, must roll back with the failure"
    );

    let verdict = bootstrap_versioned(&pool, CLASS, 3, LADDER).await.unwrap();
    assert_eq!(verdict, Compatibility::NeedsUpgrade { from: 1 });
    assert_eq!(on_disk(&path).await, (Some(3), columns(&["id", "b", "c"])));
}

#[tokio::test]
async fn a_binary_too_old_refuses_without_changing_the_file() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("toy.db");
    let pool = store_pool(&path).await;
    bootstrap_versioned(&pool, CLASS, 3, LADDER).await.unwrap();

    let snapshot = async |pool: &Pool<Sqlite>| -> Vec<String> {
        sqlx::query_scalar(
            "SELECT type || ' ' || name || ' ' || COALESCE(sql, '') FROM sqlite_master \
             UNION ALL SELECT class || ' ' || version || ' ' || updated_at FROM schema_meta \
             ORDER BY 1",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    };
    let before = snapshot(&pool).await;

    let err = bootstrap_versioned(&pool, CLASS, 2, &LADDER[..2])
        .await
        .expect_err("a binary at version 2 must refuse a file at version 3");
    assert!(
        matches!(
            err,
            SchemaError::BinaryTooOld {
                db_version: 3,
                binary_version: 2
            }
        ),
        "got {err:?}"
    );
    assert_eq!(snapshot(&pool).await, before);
    assert!(
        !write_lock_held(&path).await,
        "a refusal must give the schema lock back"
    );
}

/// Never lets the step past a rung: the step can only end by being
/// dropped, as the on-demand rebuild's task is at shutdown.
struct HangAfterRung(u32);

impl SchemaSeam for HangAfterRung {
    async fn rung_applied(&mut self, version: u32) {
        if version == self.0 {
            std::future::pending::<()>().await;
        }
    }
}

#[tokio::test]
async fn a_cancelled_upgrade_leaves_the_file_at_its_old_version() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("toy.db");
    file_at_v1(&path).await;
    let pool = store_pool(&path).await;

    let cancelled = tokio::time::timeout(
        Duration::from_millis(500),
        bootstrap_observed(&pool, CLASS, 3, LADDER, &mut HangAfterRung(2)),
    )
    .await;
    assert!(
        cancelled.is_err(),
        "the step should still have been held after rung 2: {cancelled:?}"
    );

    // Dropping the step hands its connection back to SQLite on sqlx's
    // worker thread, so the lock goes a moment later rather than at
    // once. Bounded by count and by time: 100 polls, 50 ms apart.
    let mut released = false;
    for _ in 0..100 {
        if !write_lock_held(&path).await {
            released = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(released, "the cancelled step never gave the lock back");
    assert_eq!(
        on_disk(&path).await,
        (Some(1), columns(&["id"])),
        "rung 2 must roll back with the cancelled step"
    );

    let verdict = bootstrap_versioned(&pool, CLASS, 3, LADDER)
        .await
        .expect("the next open after a cancelled upgrade must succeed");
    assert_eq!(verdict, Compatibility::NeedsUpgrade { from: 1 });
    assert_eq!(on_disk(&path).await, (Some(3), columns(&["id", "b", "c"])));
}

// ----- The step before the lock: a file another process is creating -----

/// How long the creating process keeps the file locked. The open under
/// test must still be waiting when it lets go.
const CREATOR_HOLDS: Duration = Duration::from_millis(500);

/// What an opener meets when another process created the store's file
/// an instant earlier and has not yet switched it into WAL mode: a file
/// in SQLite's default journal mode, with that process's write lock on
/// it. The opener's own switch into WAL mode needs that lock, and SQLite
/// will not wait for it. Runs `open` against such a file, lets the
/// creator go after [`CREATOR_HOLDS`], and returns what was opened.
async fn open_while_another_process_creates<T: Send + 'static>(
    path: &Path,
    open: impl Future<Output = Result<T, String>> + Send + 'static,
) -> T {
    let mut creator = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .connect()
        .await
        .expect("create the file as the other process");
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut creator)
        .await
        .expect("take the creator's lock");

    let opening = tokio::spawn(open);
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
        .expect("the open panicked")
        .unwrap_or_else(|err| panic!("the open must wait for the creator, then succeed: {err}"));
    assert!(
        !finished_while_held,
        "the open finished while the creator still held the file"
    );
    opened
}

/// The version `class` records in the file at `path`.
async fn recorded(path: &Path, class: &str) -> Option<u32> {
    let mut conn = SqliteConnectOptions::new()
        .filename(path)
        .connect()
        .await
        .expect("open for inspection");
    let version = read_schema_version(&mut conn, class).await.unwrap();
    conn.close().await.unwrap();
    version
}

#[tokio::test]
async fn a_worker_store_another_process_is_creating_opens_once_it_lets_go() {
    use crate::worker::store::{SCHEMA_CLASS, WORKER_SCHEMA_VERSION, WorkerStore};

    let dir = tempdir().unwrap();
    let path = dir.path().join("worker.db");
    let opening = path.clone();
    let store = open_while_another_process_creates(&path, async move {
        WorkerStore::open(&opening)
            .await
            .map_err(|err| err.to_string())
    })
    .await;
    drop(store);
    assert_eq!(
        recorded(&path, SCHEMA_CLASS).await,
        Some(WORKER_SCHEMA_VERSION)
    );
}

#[tokio::test]
async fn a_control_plane_store_another_process_is_creating_opens_once_it_lets_go() {
    use crate::control_plane::store::{
        CONTROL_PLANE_SCHEMA_VERSION, ControlPlaneStore, SCHEMA_CLASS,
    };

    let dir = tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    let opening = path.clone();
    let store = open_while_another_process_creates(&path, async move {
        ControlPlaneStore::open(&opening)
            .await
            .map_err(|err| err.to_string())
    })
    .await;
    drop(store);
    assert_eq!(
        recorded(&path, SCHEMA_CLASS).await,
        Some(CONTROL_PLANE_SCHEMA_VERSION)
    );
}

#[tokio::test]
async fn a_projection_another_process_is_creating_opens_once_it_lets_go() {
    use crate::control_plane::projection::{PROJECTION_SCHEMA_VERSION, ProjectionStore};

    let dir = tempdir().unwrap();
    let path = dir.path().join("projection.db");
    let opening = path.clone();
    let store = open_while_another_process_creates(&path, async move {
        ProjectionStore::open(&opening)
            .await
            .map_err(|err| err.to_string())
    })
    .await;
    assert_eq!(
        store.schema_version().await.unwrap(),
        PROJECTION_SCHEMA_VERSION
    );
}

/// The child is this test binary run again, filtered to `opener_process`
/// below — the same re-exec `fqd_smoke` uses for its `/proc` probe. What
/// makes these cross-process is that the parent's lock and the child's
/// are two processes' locks on one file, which is the lock SQLite takes
/// for `BEGIN IMMEDIATE`; an in-process mutex would not show up here.
#[cfg(unix)]
mod across_processes {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::time::Duration;

    use super::*;
    use crate::db::schema::test_seam::RIVAL_HEAD_START;

    const OPENER_PROCESS: &str = "db::schema::tests::across_processes::opener_process";
    const CHILD_DB: &str = "FQ_SCHEMA_TEST_DB";
    const CHILD_ROLE: &str = "FQ_SCHEMA_TEST_ROLE";
    /// Open the toy file and report the verdict.
    const ROLE_OPEN: &str = "open";
    /// Upgrade the toy file and stop, transaction open, after rung 2 — to be
    /// killed there.
    const ROLE_STALL: &str = "stall-after-rung-2";
    /// Every line the child means the parent to read starts with this.
    const SAYS: &str = "schema-test-child: ";
    /// How long a stalled child waits to be killed before giving up on its
    /// parent. Bounds the child if the parent dies first.
    const STALL: Duration = Duration::from_secs(60);

    fn opener_child(db: &Path, role: &str) -> fq_test_support::TestChild {
        fq_test_support::TestChild::builder(std::env::current_exe().expect("this test binary"))
            .args(["--exact", OPENER_PROCESS, "--nocapture"])
            .env(CHILD_DB, db)
            .env(CHILD_ROLE, role)
            .stdout(Stdio::piped())
            .spawn()
    }

    /// Stops the ladder after a rung, with the transaction still open.
    struct StallAfterRung(u32);

    impl SchemaSeam for StallAfterRung {
        async fn rung_applied(&mut self, version: u32) {
            if version == self.0 {
                println!("{SAYS}rung-applied {version}");
                tokio::time::sleep(STALL).await;
            }
        }
    }

    /// The child process's body. A no-op in an ordinary run: it acts only
    /// when a cross-process test below has started it.
    #[tokio::test]
    async fn opener_process() {
        let Ok(db) = std::env::var(CHILD_DB) else {
            return;
        };
        let role = std::env::var(CHILD_ROLE).expect("a child is always given a role");
        let pool = store_pool(Path::new(&db)).await;
        match role.as_str() {
            ROLE_OPEN => {
                let outcome = bootstrap_versioned(&pool, CLASS, 3, LADDER).await;
                match outcome {
                    Ok(verdict) => println!("{SAYS}verdict {verdict:?}"),
                    Err(err) => {
                        println!("{SAYS}error {err}");
                        panic!("the child's open failed: {err}");
                    }
                }
            }
            ROLE_STALL => {
                let outcome =
                    bootstrap_observed(&pool, CLASS, 3, LADDER, &mut StallAfterRung(2)).await;
                panic!("the child was not killed mid-ladder (outcome: {outcome:?})");
            }
            other => panic!("unknown child role {other:?}"),
        }
    }

    /// Starts the child opener when the parent's step has read the version,
    /// and gives it the same bounded head start the in-process rival gets.
    struct ChildAtVersionRead {
        db: PathBuf,
        child: Option<fq_test_support::TestChild>,
        /// Taken at once: a child that exits inside its head start is
        /// reaped by the poll, and its pipe goes with it.
        stdout: Option<std::process::ChildStdout>,
        lock_held: Option<bool>,
    }

    impl SchemaSeam for ChildAtVersionRead {
        async fn version_read(&mut self, _verdict: Compatibility) {
            self.lock_held = Some(write_lock_held(&self.db).await);
            let mut child = opener_child(&self.db, ROLE_OPEN);
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
    async fn a_second_process_waits_for_the_first_ones_bootstrap() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("toy.db");
        let mut seam = ChildAtVersionRead {
            db: path.clone(),
            child: None,
            stdout: None,
            lock_held: None,
        };

        let pool = store_pool(&path).await;
        let first = bootstrap_observed(&pool, CLASS, 3, LADDER, &mut seam).await;

        let mut child = seam.child.take().expect("the child was started");
        let mut stdout = seam.stdout.take().expect("the child's stdout is piped");
        let status = child
            .wait_timeout(Duration::from_secs(60))
            .expect("the child opener did not finish within a minute of the parent committing");
        let mut said = String::new();
        std::io::Read::read_to_string(&mut stdout, &mut said).unwrap();

        assert_eq!(
            first.expect("the first process's open must succeed"),
            Compatibility::FreshInstall
        );
        assert!(
            status.success(),
            "the second process's open must succeed: {status:?}\n{said}"
        );
        assert!(
            said.contains(&format!("{SAYS}verdict Current")),
            "the second process must read the version the first wrote:\n{said}"
        );
        assert_eq!(seam.lock_held, Some(true));
        assert_eq!(on_disk(&path).await, (Some(3), columns(&["id", "b", "c"])));
    }

    #[tokio::test]
    async fn a_process_killed_mid_upgrade_leaves_the_file_at_its_old_version() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("toy.db");
        file_at_v1(&path).await;

        let mut child = opener_child(&path, ROLE_STALL);
        let stdout = child.take_stdout().expect("the child's stdout is piped");
        let (lines, said) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if lines.send(line).is_err() {
                    return;
                }
            }
        });
        // Bounded by count and by time: the child prints a handful of
        // harness lines before its own.
        let mut stalled = false;
        for _ in 0..100 {
            match said.recv_timeout(Duration::from_secs(60)) {
                Ok(line) if line.contains(&format!("{SAYS}rung-applied 2")) => {
                    stalled = true;
                    break;
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        assert!(stalled, "the child never reported rung 2 applied");

        child
            .signal(libc::SIGKILL)
            .expect("kill the child mid-ladder");
        child
            .wait_timeout(Duration::from_secs(10))
            .expect("the killed child was not reaped");

        assert_eq!(
            on_disk(&path).await,
            (Some(1), columns(&["id"])),
            "the killed upgrade's rung 2 must not have reached the file"
        );
        let pool = store_pool(&path).await;
        let verdict = bootstrap_versioned(&pool, CLASS, 3, LADDER)
            .await
            .expect("the next open after the kill must succeed");
        assert_eq!(verdict, Compatibility::NeedsUpgrade { from: 1 });
        assert_eq!(on_disk(&path).await, (Some(3), columns(&["id", "b", "c"])));
    }
}
