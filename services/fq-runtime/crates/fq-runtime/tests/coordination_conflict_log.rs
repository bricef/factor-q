//! The level the coordination consumer logs a conflicting ending at
//! (issue 1070): an ending that contradicts the one a run's ownership row
//! records is refused and logged, at error when the run published two
//! endings, and at warn only where the row cannot say, a `failed` row
//! archived with no ender on record, as a drop from before the termination
//! record left it.
//!
//! **A binary of its own,** as `coordination_refused_write_log.rs` is: a
//! captured subscriber has to be the process-global one, since tracing
//! caches a callsite's interest for the whole process.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use fq_runtime::control_plane::{ControlPlaneStore, CoordinationConsumer};
use fq_runtime::events::{
    CompletedPayload, Event, EventPayload, FailedPayload, FailureKind, FailurePhase,
    InvocationArchivedPayload, InvocationOperatorRecoveredPayload, InvocationTotals,
};
use fq_runtime::worker::WorkerId;
use fq_runtime::{AgentId, EventBus};
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer as _;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Context, SubscriberExt};
use uuid::Uuid;

/// The start of the warn line for a conflict on a row that records no
/// ender.
const NO_ENDER: &str = "an ending reached a run recorded failed and archived with no ender";
/// The start of the error line for two endings of one run.
const TWO_ENDINGS: &str = "two endings published for one run";
/// The start of every conflict line, whatever its level, before and after
/// this change.
const ANY_CONFLICT: [&str; 2] = ["an ending reached a run recorded failed", TWO_ENDINGS];

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

    /// The conflict lines logged for `invocation_id`.
    fn conflicts(&self, invocation_id: Uuid) -> Vec<Line> {
        let id = invocation_id.to_string();
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|line| {
                line.field("invocation_id") == Some(id.as_str())
                    && ANY_CONFLICT
                        .iter()
                        .any(|start| line.message.starts_with(start))
            })
            .cloned()
            .collect()
    }
}

fn completed() -> EventPayload {
    EventPayload::Completed(CompletedPayload {
        task_status: Default::default(),
        result_summary: Some("done".to_string()),
        total_llm_calls: 1,
        total_tool_calls: 0,
        total_cost: 0.0,
        total_duration_ms: 10,
    })
}

fn failed() -> EventPayload {
    EventPayload::Failed(FailedPayload {
        error_kind: FailureKind::LlmError,
        error_message: "the provider refused the request".to_string(),
        phase: FailurePhase::LlmRequest,
        partial_totals: InvocationTotals::default(),
        detail: None,
    })
}

fn archived_failed() -> EventPayload {
    EventPayload::InvocationArchived(InvocationArchivedPayload {
        worker_id: WorkerId::new("conflict-worker".to_string()).unwrap(),
        final_phase: "failed".to_string(),
        final_state_blob: b"{\"state\":1}".to_vec(),
        started_at_ms: 1_000,
        terminal_at_ms: 2_000,
    })
}

fn dropped() -> EventPayload {
    EventPayload::InvocationOperatorRecovered(InvocationOperatorRecoveredPayload {
        action: "drop".to_string(),
        final_phase: "failed".to_string(),
        reason: None,
        requested_by: Some("on-call".to_string()),
        archive: None,
    })
}

/// **A conflicting ending is an error where the row says who ended the
/// run, and a warn only where it cannot.** Five runs, each then reached by
/// an ending its row contradicts:
///
/// - one a drop from before the termination record left `failed`,
///   archived, with no ender, then a `completed`: warn, since the drop may
///   have come first;
/// - one that failed by itself and was archived, then a `completed`:
///   error, the run published two endings (a warn before the record, which
///   could not tell this row from a drop's);
/// - one a drop ended, archived `failed`, then a `completed`: error, since
///   the drop's ending is final and a run whose drop's mark won publishes
///   no ending of its own;
/// - two more rows from before the record, with no ender, that no drop
///   left, since a drop archived what it ended: one `failed` with its
///   archive pending, then a `completed`, and one `completed` and
///   archived, then a `failed`. Each is an error. The warn is for a row
///   `failed` and archived together, not for a `failed` row nor for an
///   archived one alone.
#[tokio::test]
async fn a_conflicting_ending_is_an_error_unless_the_row_names_no_ender() {
    let log = Log::installed();
    let server = fq_test_support::NatsServer::start();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    let store = Arc::new(ControlPlaneStore::open(&path).await.unwrap());
    let raw = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();

    let agent = AgentId::new(format!("conflict-{}", Uuid::now_v7().simple())).unwrap();
    let consumer_name = format!("conflict-log-{}", Uuid::now_v7().simple());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let consumer = CoordinationConsumer::new(bus.clone(), store.clone())
        .with_test_consumer_name(consumer_name.clone())
        .with_test_filter_subject(format!("fq.agent.{agent}.>"));
    let running = tokio::spawn(consumer.run(stopped));
    let stream = bus
        .jetstream()
        .get_stream(fq_runtime::bus::STREAM_NAME)
        .await
        .expect("the event stream");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while stream
        .get_consumer::<async_nats::jetstream::consumer::pull::Config>(&consumer_name)
        .await
        .is_err()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the consumer never attached"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // A drop from before the termination record: `failed`, archived, and
    // no ender, written as that build left it.
    let old_drop = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO coordination_invocation_owner \
             (invocation_id, worker_id, assigned_at, status, archive_state) \
         VALUES (?, 'old-worker', 1000, 'failed', 'archived')",
    )
    .bind(old_drop.to_string())
    .execute(&raw)
    .await
    .unwrap();
    // Two more from before the record, with no ender, that no drop left.
    let failed_pending = Uuid::now_v7();
    let completed_archived = Uuid::now_v7();
    for (run, status, archive) in [
        (failed_pending, "failed", "pending"),
        (completed_archived, "completed", "archived"),
    ] {
        sqlx::query(
            "INSERT INTO coordination_invocation_owner \
                 (invocation_id, worker_id, assigned_at, status, archive_state) \
             VALUES (?, 'old-worker', 1000, ?, ?)",
        )
        .bind(run.to_string())
        .bind(status)
        .bind(archive)
        .execute(&raw)
        .await
        .unwrap();
    }
    let own_failure = Uuid::now_v7();
    let drop_final = Uuid::now_v7();
    let publish = |invocation_id: Uuid, payload: EventPayload| {
        let bus = bus.clone();
        let agent = agent.clone();
        async move {
            bus.publish(&Event::new(agent, invocation_id, payload))
                .await
                .expect("publish");
        }
    };
    publish(old_drop, completed()).await;
    publish(own_failure, failed()).await;
    publish(own_failure, archived_failed()).await;
    publish(own_failure, completed()).await;
    publish(drop_final, dropped()).await;
    publish(drop_final, archived_failed()).await;
    publish(drop_final, completed()).await;
    publish(failed_pending, completed()).await;
    publish(completed_archived, failed()).await;

    let runs = [
        old_drop,
        own_failure,
        drop_final,
        failed_pending,
        completed_archived,
    ];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while runs.iter().any(|run| log.conflicts(*run).is_empty()) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "a conflict was never logged: {:?}",
            runs.map(|run| log.conflicts(run).len())
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = stop.send(());
    let _ = tokio::time::timeout(Duration::from_secs(10), running).await;

    let logged = |run: Uuid| {
        log.conflicts(run)
            .iter()
            .map(|line| {
                let start = [NO_ENDER, TWO_ENDINGS]
                    .into_iter()
                    .find(|start| line.message.starts_with(start))
                    .unwrap_or(line.message.as_str())
                    .to_string();
                (
                    line.level,
                    start,
                    line.field("reported").map(str::to_string),
                )
            })
            .collect::<Vec<_>>()
    };
    let completed = Some("completed".to_string());
    assert_eq!(
        logged(old_drop),
        [(
            tracing::Level::WARN,
            NO_ENDER.to_string(),
            completed.clone()
        )],
        "a row a drop from before the record left, with no ender on record"
    );
    assert_eq!(
        logged(own_failure),
        [(
            tracing::Level::ERROR,
            TWO_ENDINGS.to_string(),
            completed.clone()
        )],
        "a run's own failure, archived, then its completed: two endings"
    );
    assert_eq!(
        logged(drop_final),
        [(
            tracing::Level::ERROR,
            TWO_ENDINGS.to_string(),
            completed.clone()
        )],
        "a drop's ending made final by its archive, then a completed"
    );
    assert_eq!(
        logged(failed_pending),
        [(tracing::Level::ERROR, TWO_ENDINGS.to_string(), completed)],
        "a failed row with no ender whose archive is pending, then a completed"
    );
    assert_eq!(
        logged(completed_archived),
        [(
            tracing::Level::ERROR,
            TWO_ENDINGS.to_string(),
            Some("failed".to_string())
        )],
        "a completed, archived row with no ender, then a failed"
    );
}
