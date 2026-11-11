//! The race the schema lock closes (#970), made deterministic.
//!
//! [`RivalAtVersionRead`] pauses a schema step at the moment it has read
//! the recorded version and before it has acted on it — the window two
//! unserialised openers used to share — starts a second opener there,
//! and gives that rival a bounded head start before letting the first go
//! on. Without the lock the rival reads the same old version and runs
//! the whole ladder inside its head start, so the first then applies a
//! rung that is already there. With the lock the rival cannot even begin
//! until the first commits, and the head start runs out with the rival
//! still waiting. Every store's race test is built on it, so they all
//! stage the same interleaving.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection};
use tokio::task::JoinHandle;

use super::{Compatibility, SchemaSeam};

/// How long the rival is given while the first opener is paused. Without
/// the schema lock a rival ladder finishes in milliseconds, far inside
/// this; with it the rival cannot finish at all until the first opener
/// commits, so every race test spends exactly this long here. Well under
/// [`super::SCHEMA_LOCK_WAIT`], which is how long the rival will wait.
pub(crate) const RIVAL_HEAD_START: Duration = Duration::from_secs(2);

/// How long a test waits for the rival to finish once the first opener
/// has committed.
const RIVAL_FINISH: Duration = Duration::from_secs(60);

type Rival<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Starts `rival` when the watched step reads the version; see the
/// module docs.
pub(crate) struct RivalAtVersionRead<T> {
    db: PathBuf,
    pending: Option<Rival<T>>,
    running: Option<JoinHandle<T>>,
    /// The rival's outcome, when it finished inside its head start.
    finished: Option<T>,
    /// What the paused step read.
    pub(crate) verdict: Option<Compatibility>,
    /// Whether some connection held the database's write lock while the
    /// step was paused: the schema lock, if there is one.
    pub(crate) lock_held: Option<bool>,
}

impl<T: Send + 'static> RivalAtVersionRead<T> {
    pub(crate) fn new(db: &Path, rival: impl Future<Output = T> + Send + 'static) -> Self {
        RivalAtVersionRead {
            db: db.to_path_buf(),
            pending: Some(Box::pin(rival)),
            running: None,
            finished: None,
            verdict: None,
            lock_held: None,
        }
    }

    /// What the rival came to, waiting a bounded time for it to finish.
    pub(crate) async fn rival_outcome(&mut self) -> T {
        if let Some(outcome) = self.finished.take() {
            return outcome;
        }
        let rival = self
            .running
            .take()
            .expect("the watched step never read a version, so the rival never started");
        tokio::time::timeout(RIVAL_FINISH, rival)
            .await
            .expect("the rival opener did not finish within a minute of the first committing")
            .expect("the rival opener panicked")
    }
}

impl<T: Send + 'static> SchemaSeam for RivalAtVersionRead<T> {
    async fn version_read(&mut self, verdict: Compatibility) {
        self.verdict = Some(verdict);
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

/// Whether some connection holds the write lock on the database at
/// `path` right now. The probe has no busy timeout, so it asks once: it
/// tries to take the lock, and lets go at once if it got it.
pub(crate) async fn write_lock_held(path: &Path) -> bool {
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
        Err(err) if super::is_busy(&err) => true,
        Err(err) => panic!("the lock probe failed for another reason: {err}"),
    };
    probe.close().await.expect("close the lock probe");
    held
}
