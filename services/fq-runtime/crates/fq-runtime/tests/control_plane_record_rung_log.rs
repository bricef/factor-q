//! What the control-plane store's open logs when the termination record's
//! rung corrects a row (issue 1070): one line at `error` for each row whose
//! archive state did not go with its status, naming the invocation, the
//! status and the archive state before and after, and nothing at the
//! next start. That line is the only trace of a rewrite the rung makes.
//!
//! **A binary of its own,** as `dispatcher_log_lines.rs` is: a captured
//! subscriber has to be the process-global one, since tracing caches a
//! callsite's interest for the whole process.
//!
//! The store it opens is built by hand at the version before the record's
//! rung, with the ownership table and the trigger claim table as that
//! version left them. One rung follows the record's on the ladder, the
//! consumed claim state's (issue 1055), which rebuilds the trigger claim
//! table, so that version is two before the binary's. A rung added after
//! them makes this test fail loudly (the open would not rebuild the table),
//! and the version written here must then move.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::{Arc, Mutex, OnceLock};

use fq_runtime::control_plane::{CONTROL_PLANE_SCHEMA_VERSION, ControlPlaneStore};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer as _;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Context, SubscriberExt};

/// The start of the line the open logs for each corrected row.
const CORRECTED: &str = "an ownership row's archive state did not go with its status";

/// One event logged at `info` or above: its level, message and fields.
#[derive(Debug, Clone)]
struct Line {
    level: tracing::Level,
    message: String,
    fields: BTreeMap<&'static str, String>,
}

impl Visit for Line {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        self.record(field, format!("{value:?}"));
    }
}

impl Line {
    fn record(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = value;
        } else {
            self.fields.insert(field.name(), value);
        }
    }

    fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }
}

/// Every line logged at `info` or above in this process.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<Line>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Log {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut line = Line {
            level: *event.metadata().level(),
            message: String::new(),
            fields: BTreeMap::new(),
        };
        event.record(&mut line);
        self.0.lock().unwrap().push(line);
    }
}

impl Log {
    /// The process's one subscriber, installed on the first call, before
    /// anything here can log.
    fn installed() -> &'static Log {
        static LOG: OnceLock<Log> = OnceLock::new();
        LOG.get_or_init(|| {
            let log = Log::default();
            tracing::subscriber::set_global_default(
                tracing_subscriber::registry().with(log.clone().with_filter(LevelFilter::INFO)),
            )
            .expect("install the process's one subscriber");
            log
        })
    }

    /// The lines about a corrected row.
    fn corrected(&self) -> Vec<Line> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.message.starts_with(CORRECTED))
            .cloned()
            .collect()
    }
}

/// A control plane at the version before the record's rung, its ownership
/// and trigger claim tables as that version left them, holding `rows`.
async fn a_store_before_the_record(path: &std::path::Path, rows: &[(&str, &str, &str)]) {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal),
        )
        .await
        .unwrap();
    for statement in [
        "CREATE TABLE schema_meta (class TEXT PRIMARY KEY, version INTEGER NOT NULL, \
         updated_at INTEGER NOT NULL)",
        "CREATE TABLE coordination_invocation_owner (invocation_id TEXT PRIMARY KEY, \
         worker_id TEXT NOT NULL, assigned_at INTEGER NOT NULL, status TEXT NOT NULL)",
        "CREATE INDEX idx_owner_worker_status \
         ON coordination_invocation_owner(worker_id, status)",
        "CREATE TABLE trigger_claim (stream TEXT NOT NULL, stream_epoch INTEGER NOT NULL, \
         stream_seq INTEGER NOT NULL, claimant TEXT NOT NULL, \
         state TEXT NOT NULL CHECK (state IN ('claimed', 'durably_started')), \
         invocation_id TEXT, claimed_at INTEGER NOT NULL, \
         PRIMARY KEY (stream, stream_epoch, stream_seq))",
        "ALTER TABLE coordination_invocation_owner \
         ADD COLUMN archive_state TEXT NOT NULL DEFAULT 'not_due' \
         CHECK (archive_state IN ('not_due', 'pending', 'archived'))",
    ] {
        sqlx::query(statement).execute(&pool).await.unwrap();
    }
    sqlx::query(
        "INSERT INTO schema_meta (class, version, updated_at) VALUES ('control_plane', ?, 0)",
    )
    .bind(CONTROL_PLANE_SCHEMA_VERSION - 2)
    .execute(&pool)
    .await
    .unwrap();
    for (id, status, archive) in rows {
        sqlx::query(
            "INSERT INTO coordination_invocation_owner \
             (invocation_id, worker_id, assigned_at, status, archive_state) \
             VALUES (?, 'w', 1, ?, ?)",
        )
        .bind(id)
        .bind(status)
        .bind(archive)
        .execute(&pool)
        .await
        .unwrap();
    }
    pool.close().await;
}

/// **The open logs each corrected row once, at `error`, and a later start
/// logs nothing.** Two rows whose archive state did not go with their
/// status, and one that did: the open that runs the rung logs one line for
/// each of the two, with the invocation, the status and the archive state
/// before and after, and none for the third; the next open, past the
/// rung, logs no more.
#[tokio::test]
async fn the_open_logs_each_row_the_record_rung_corrects_once() {
    let log = Log::installed();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    a_store_before_the_record(
        &path,
        &[
            ("corrected-failed", "failed", "not_due"),
            ("corrected-in-flight", "in_flight", "archived"),
            ("left-alone", "completed", "archived"),
        ],
    )
    .await;

    let store = ControlPlaneStore::open(&path).await.expect("the rung runs");
    let owner = store
        .get_invocation_owner("corrected-failed")
        .await
        .unwrap()
        .expect("the row is there");
    assert_eq!(
        owner.archive_state.as_str(),
        "archived",
        "the record's rung ran and corrected it"
    );
    let mut lines = log.corrected();
    lines.sort_by(|a, b| a.field("invocation_id").cmp(&b.field("invocation_id")));
    let read: Vec<_> = lines
        .iter()
        .map(|line| {
            (
                line.level,
                line.field("invocation_id"),
                line.field("status"),
                line.field("archive_state_before"),
                line.field("archive_state_after"),
            )
        })
        .collect();
    assert_eq!(
        read,
        [
            (
                tracing::Level::ERROR,
                Some("corrected-failed"),
                Some("failed"),
                Some("not_due"),
                Some("archived"),
            ),
            (
                tracing::Level::ERROR,
                Some("corrected-in-flight"),
                Some("in_flight"),
                Some("archived"),
                Some("not_due"),
            ),
        ],
        "one line at error for each corrected row, and none for the other: {lines:#?}"
    );
    drop(store);

    ControlPlaneStore::open(&path).await.expect("opens again");
    assert_eq!(
        log.corrected().len(),
        2,
        "the next start corrects nothing and logs nothing"
    );
}
