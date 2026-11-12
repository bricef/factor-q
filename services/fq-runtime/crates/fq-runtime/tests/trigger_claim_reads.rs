//! How often the trigger-to-invocation link reads the stores, counted as
//! statements.
//!
//! The claim table has no index on `invocation_id` and nothing sweeps
//! it, so it grows by one row per trigger and a lookup by invocation
//! reads it through. What is defended here is that no operator read
//! multiplies that by the size of what it shows: the invocation list
//! makes no claim read at all, one invocation's detail makes one, and the
//! stranded-claim check makes one, plus two primary-key reads for each
//! claimed row with no record of its run, and nothing for the started
//! rows that make up the rest of the table. A timing would say the same
//! thing only on a big enough table and only sometimes; a statement count
//! says it on any table.
//!
//! Its own test binary for the reason `consumer_log_rate.rs` gives: the
//! statements are seen through sqlx's own `sqlx::query` events, which
//! the SQLite driver emits from its worker threads, so the capturing
//! subscriber has to be the process-global one, and that can be
//! installed once.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use fq_runtime::bus::{DEFAULT_TRIGGER_MAX_AGE, TRIGGER_REDELIVERY_WINDOW};
use fq_runtime::control_plane::store::{InvocationArchiveRow, OwnerStatus, TriggerKey};
use fq_runtime::views::{ClaimJudgement, DEFAULT_LONG_DISPATCH_THRESHOLD_MS};
use fq_runtime::worker::InvocationStateRow;
use fq_runtime::{ControlPlaneStore, ProjectionStore, RuntimeDbPaths, Views, WorkerStore};
use tracing_subscriber::Layer as _;
use tracing_subscriber::layer::{Context, SubscriberExt};

/// Every statement sqlx reported, as the text of its fields.
#[derive(Clone, Default)]
struct Statements(Arc<Mutex<Vec<String>>>);

impl Statements {
    fn clear(&self) {
        self.0.lock().unwrap().clear();
    }

    /// Every statement since the last `clear`.
    fn all(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    /// Statements that named the claim table since the last `clear`.
    fn on_claim_table(&self) -> usize {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.contains("trigger_claim"))
            .count()
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Statements {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if event.metadata().target() != "sqlx::query" {
            return;
        }
        let mut text = String::new();
        event.record(&mut Fields(&mut text));
        self.0.lock().unwrap().push(text);
    }
}

/// The summary is a statement's first four words and `db.statement` the
/// whole of it when it is longer, so the table a statement reads is in
/// one field or the other.
struct Fields<'a>(&'a mut String);

impl tracing::field::Visit for Fields<'_> {
    fn record_str(&mut self, _field: &tracing::field::Field, value: &str) {
        self.0.push_str(value);
        self.0.push(' ');
    }

    fn record_debug(&mut self, _field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, "{value:?} ");
    }
}

const NOW_MS: i64 = 2_000_000_000_000;
const THIS_DAEMON: &str = "daemon-now";
const EXITED: &str = "daemon-before";

fn key(seq: u64) -> TriggerKey<'static> {
    TriggerKey {
        stream: "fq-triggers",
        stream_epoch: 1,
        stream_seq: seq,
    }
}

fn wal_row(invocation_id: &str) -> InvocationStateRow {
    InvocationStateRow {
        invocation_id: invocation_id.into(),
        agent_id: "agent-a".into(),
        schema_version: 1,
        phase: "reducing".into(),
        state_blob: vec![],
        step_index: 1,
        started_at: NOW_MS - 3_600_000,
        updated_at: NOW_MS - 3_600_000,
        terminal_at: None,
        workspace_ref: None,
        archive_status: None,
        archive_published_at: None,
        trigger_source: Some("subject".into()),
        trigger_subject: None,
        trigger_payload: None,
        terminal_outcome: None,
        terminal_published_at: None,
    }
}

/// `finished` archived runs, each started by a trigger whose claim is
/// marked, and `unsettled` claims an exited process left with nothing on
/// record — every one of them a row the stranded check reports.
async fn seed(cp: &ControlPlaneStore, first_seq: u64, finished: u64, unsettled: u64) {
    for n in 0..finished {
        let id = format!("finished-{}", first_seq + n);
        cp.claim_trigger(key(first_seq + n), EXITED, &id, NOW_MS - 7_200_000)
            .await
            .unwrap();
        cp.mark_trigger_started(key(first_seq + n), &id)
            .await
            .unwrap();
        cp.insert_archive(&InvocationArchiveRow {
            invocation_id: id.clone(),
            agent_id: "agent-a".into(),
            final_phase: "completed".into(),
            final_state_blob: vec![],
            started_at: NOW_MS - 7_200_000,
            terminal_at: NOW_MS - 7_100_000,
            archived_at: NOW_MS - 7_000_000,
        })
        .await
        .unwrap();
        cp.upsert_invocation_ownership(&id, "worker", NOW_MS - 7_000_000, OwnerStatus::Completed)
            .await
            .unwrap();
    }
    for n in 0..unsettled {
        let seq = first_seq + finished + n;
        cp.claim_trigger(key(seq), EXITED, &format!("lost-{seq}"), NOW_MS - 7_200_000)
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn no_operator_read_scans_the_claim_table_once_per_row() {
    let statements = Statements::default();
    let only_statements = tracing_subscriber::filter::Targets::new()
        .with_target("sqlx::query", tracing::Level::DEBUG);
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(statements.clone().with_filter(only_statements)),
    )
    .expect("install the capturing subscriber");

    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimeDbPaths::under(dir.path());
    let cp = ControlPlaneStore::open(&paths.control_plane).await.unwrap();
    let worker = WorkerStore::open(&paths.worker).await.unwrap();
    let _projection = ProjectionStore::open(&paths.projection).await.unwrap();
    // One live run with its claim, so a detail read has something to find.
    cp.claim_trigger(key(1), THIS_DAEMON, "live", NOW_MS - 3_600_000)
        .await
        .unwrap();
    cp.mark_trigger_started(key(1), "live").await.unwrap();
    worker
        .upsert_invocation_state(&wal_row("live"))
        .await
        .unwrap();
    seed(&cp, 2, 3, 1).await;
    let views = Views::open(&paths).await.unwrap();
    let judgement = ClaimJudgement {
        claimant: THIS_DAEMON,
        dispatching_since_ms: NOW_MS - 86_400_000,
        redelivery_window_ms: TRIGGER_REDELIVERY_WINDOW.as_millis() as i64,
        stream_epoch: 1,
        stream_max_age_ms: DEFAULT_TRIGGER_MAX_AGE.as_millis() as i64,
        horizon_ms: Some(30 * 86_400_000),
        now_ms: NOW_MS,
    };
    // Every read once before counting, so the pools have opened the
    // connections they will use and no set-up statement is counted.
    views.invocation_index(None, true, 500).await.unwrap();
    views
        .invocation("live", NOW_MS, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
        .await
        .unwrap();
    views.stranded_trigger_claims(&judgement).await.unwrap();

    // A small table first, then the same reads over forty times the rows.
    let mut unsettled_total = 1;
    for (finished, unsettled) in [(0, 0), (120, 24)] {
        seed(&cp, 1_000 + finished, finished, unsettled).await;
        unsettled_total += unsettled;

        statements.clear();
        let listed = views.invocation_index(None, true, 500).await.unwrap();
        assert!(listed.len() >= 3, "the list has rows to show");
        assert_eq!(
            statements.on_claim_table(),
            0,
            "the invocation list carries no trigger, so it reads no claim"
        );

        statements.clear();
        let detail = views
            .invocation("live", NOW_MS, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
            .await
            .unwrap()
            .expect("the live run");
        assert_eq!(detail.trigger.expect("started by a trigger").stream_seq, 1);
        assert_eq!(statements.on_claim_table(), 1, "one invocation, one read");

        statements.clear();
        let stranded = views.stranded_trigger_claims(&judgement).await.unwrap();
        assert_eq!(stranded.len() as u64, unsettled_total);
        assert_eq!(
            statements.on_claim_table(),
            1,
            "{} stranded claims judged from one read of the table",
            stranded.len()
        );
        // Each claimed row with no coordination row costs a WAL read and,
        // when that misses, a second coordination read: two per stranded
        // claim here, and nothing for the started rows however many.
        assert_eq!(
            statements.all() as u64,
            1 + 2 * unsettled_total,
            "every statement the check ran, over {} started claims",
            finished + 4
        );
    }
}
