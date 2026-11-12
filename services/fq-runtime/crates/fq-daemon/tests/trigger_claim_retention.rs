//! The real `fqd` sweeps trigger claims as its own dispatcher's claimant
//! (<https://github.com/bricef/factor-q/issues/969>).
//!
//! The claim window deletes a `claimed` row only when another claimant
//! holds it: a claim a process that has exited left behind. Which claimant
//! is this daemon's is something only the binary knows. `run_hosted` tells
//! the sweeper the worker id it gives its dispatcher, and a sweeper that is
//! not told deletes no `claimed` row at all. So this runs the daemon itself
//! and watches what its sweep leaves:
//!
//! - an old `claimed` row an exited process left goes, which fails if the
//!   daemon never tells the sweeper its claimant;
//! - an old `claimed` row under the daemon's own worker id stays, which
//!   fails if the id the sweeper is told is not the daemon's.
//!
//! Isolation follows `crash_before_claim_mark.rs`: a private broker, a
//! scratch config and stores under the scratch cache directory. No agent is
//! loaded, so no trigger is ever dispatched: every row is the test's own,
//! written through the store's claim and mark methods with a backdated
//! clock, on an incarnation of the stream the daemon never consumes.

#![cfg(unix)]

use std::process::Stdio;
use std::time::{Duration, Instant};

use fq_runtime::bus::TRIGGER_STREAM_NAME;
use fq_runtime::control_plane::{TriggerKey, WorkerStatus};
use fq_runtime::{ControlPlaneStore, RuntimeDbPaths};
use fq_test_support::TestChild;

/// The claimant of every row but one: a daemon that ran before this one.
const EXITED: &str = "daemon-that-exited";
/// An incarnation of the trigger stream from before the private broker
/// created its own.
const OLD_EPOCH: i64 = 1_700_000_000_000_000_000;
const DAY_MS: i64 = 24 * 60 * 60 * 1_000;

/// Polls run every 100 ms for at most sixty seconds.
const POLL: Duration = Duration::from_millis(100);
const DEADLINE: Duration = Duration::from_secs(60);

fn fqd_binary() -> &'static str {
    env!("CARGO_BIN_EXE_fqd")
}

/// The scratch layout `fqd` runs on, with the claim window at its floor,
/// the other two windows off, and a one-second sweep.
fn scratch() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("fq-trigger-claim-retention-")
        .tempdir()
        .expect("create scratch directory");
    for sub in ["cache", "state", "agents"] {
        std::fs::create_dir_all(dir.path().join(sub)).unwrap();
    }
    std::fs::write(
        dir.path().join("fqd.toml"),
        "[edge]\nbind = \"127.0.0.1:0\"\n\n\
         [state]\nretention_days = -1\nstale_worker_retention_days = -1\n\
         trigger_claim_retention_days = 2\nsweep_interval_seconds = 1\n",
    )
    .unwrap();
    dir
}

/// One claim row, written as the dispatcher writes one.
struct Row {
    seq: u64,
    claimant: String,
    age_days: i64,
    started: bool,
}

impl Row {
    fn claimed(seq: u64, claimant: &str, age_days: i64) -> Self {
        Self {
            seq,
            claimant: claimant.to_string(),
            age_days,
            started: false,
        }
    }

    fn started(seq: u64, age_days: i64) -> Self {
        Self {
            started: true,
            ..Self::claimed(seq, EXITED, age_days)
        }
    }

    /// The invocation id the row names, unique per row: how the test finds
    /// the row again without writing a claim to look.
    fn invocation(&self) -> String {
        format!("inv-{}", self.seq)
    }

    async fn write(&self, store: &ControlPlaneStore) {
        let key = TriggerKey {
            stream: TRIGGER_STREAM_NAME,
            stream_epoch: OLD_EPOCH,
            stream_seq: self.seq,
        };
        let claimed_at = chrono::Utc::now().timestamp_millis() - self.age_days * DAY_MS;
        store
            .claim_trigger(key, &self.claimant, &self.invocation(), claimed_at)
            .await
            .expect("write the claim");
        if self.started {
            store
                .mark_trigger_started(key, &self.invocation())
                .await
                .expect("mark the claim started");
        }
    }

    async fn kept(&self, store: &ControlPlaneStore) -> bool {
        store
            .trigger_claim_for_invocation(&self.invocation())
            .await
            .expect("read the claim")
            .is_some()
    }
}

/// The running daemon, and what to say when it is not doing what the test
/// waits for.
struct Daemon {
    child: TestChild,
    log: std::path::PathBuf,
}

impl Daemon {
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Poll until `done` holds, failing if the daemon exits or the deadline
    /// passes first.
    async fn wait_until<F, Fut>(&mut self, what: &str, mut done: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if done().await {
                return;
            }
            if let Some(status) = self.child.try_wait().expect("poll the daemon") {
                panic!(
                    "fqd exited ({status:?}) while waiting for {what}\n--- daemon log ---\n{}",
                    self.log()
                );
            }
            assert!(
                Instant::now() < deadline,
                "timed out after {DEADLINE:?} waiting for {what}\n--- daemon log ---\n{}",
                self.log()
            );
            tokio::time::sleep(POLL).await;
        }
    }
}

/// **The daemon's sweep deletes the old claims an exited process left, and
/// keeps an old claim under its own worker id.**
///
/// A `claimed` row and a started row, each three days old and left by
/// another claimant, go; the same pair one day old, inside the two-day
/// window, stays. Then a three-day-old `claimed` row under the daemon's own
/// worker id is written, followed by two sentinels left by the other
/// claimant: once both sentinels have gone, a sweep has run after the
/// daemon's row was written, and that row must still be there.
#[tokio::test(flavor = "multi_thread")]
async fn the_daemon_sweeps_exited_claims_and_keeps_its_own() {
    let server = fq_test_support::NatsServer::start();
    let scratch = scratch();
    let paths = RuntimeDbPaths::under(&scratch.path().join("cache"));
    let store = ControlPlaneStore::open(&paths.control_plane)
        .await
        .expect("open the control-plane store");

    let abandoned_old = Row::claimed(1, EXITED, 3);
    let started_old = Row::started(2, 3);
    let abandoned_recent = Row::claimed(3, EXITED, 1);
    let started_recent = Row::started(4, 1);
    for row in [
        &abandoned_old,
        &started_old,
        &abandoned_recent,
        &started_recent,
    ] {
        row.write(&store).await;
    }

    let log = scratch.path().join("fqd.log");
    let log_file = std::fs::File::create(&log).expect("create daemon log");
    let child = TestChild::builder(fqd_binary())
        .env("FQ_DAEMON_CONFIG", scratch.path().join("fqd.toml"))
        .env("FQ_NATS_URL", server.url())
        .env("FQ_CACHE_DIR", scratch.path().join("cache"))
        .env("FQ_STATE_DIR", scratch.path().join("state"))
        .env("FQ_AGENTS_DIR", scratch.path().join("agents"))
        .env("FQ_LOG_FORMAT", "json")
        .stdout(Stdio::from(log_file.try_clone().unwrap()))
        .stderr(Stdio::from(log_file))
        .spawn();
    let mut daemon = Daemon { child, log };
    let store = &store;

    // ---- What an exited process left. ----
    let (a, b) = (&abandoned_old, &started_old);
    daemon
        .wait_until(
            "the sweep to take the three-day-old claims",
            move || async move { !a.kept(store).await && !b.kept(store).await },
        )
        .await;
    assert!(
        abandoned_recent.kept(store).await,
        "a claimed row inside the window is kept\n--- daemon log ---\n{}",
        daemon.log()
    );
    assert!(
        started_recent.kept(store).await,
        "a started row inside the window is kept\n--- daemon log ---\n{}",
        daemon.log()
    );

    // ---- What this daemon holds. ----
    let workers = store.list_workers().await.expect("list workers");
    let alive: Vec<_> = workers
        .iter()
        .filter(|w| w.status == WorkerStatus::Alive)
        .collect();
    assert_eq!(
        alive.len(),
        1,
        "the daemon registers one live worker: {workers:?}"
    );
    let own = Row::claimed(5, &alive[0].worker_id, 3);
    own.write(store).await;
    for sentinel in [Row::claimed(6, EXITED, 3), Row::claimed(7, EXITED, 3)] {
        sentinel.write(store).await;
        let s = &sentinel;
        daemon
            .wait_until(
                "a sweep after the daemon's own claim was written",
                move || async move { !s.kept(store).await },
            )
            .await;
    }
    assert!(
        own.kept(store).await,
        "a claim under the daemon's own worker id is never swept: the sweeper was told \
         another claimant\n--- daemon log ---\n{}",
        daemon.log()
    );

    let _ = daemon.child.signal(libc::SIGTERM);
    let _ = daemon.child.wait_timeout(Duration::from_secs(15));
}
