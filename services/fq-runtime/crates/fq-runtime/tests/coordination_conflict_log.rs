//! The level the coordination consumer logs a conflicting ending at
//! (issue 1070): an ending that contradicts the one a run's ownership row
//! records is refused and logged, at warn on a row an operator's drop
//! leaves, `failed` with its archive written or none to come, where the
//! drop most likely came first, and at error anywhere else, where the run
//! published two endings.
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
    CompletedPayload, DropArchive, Event, EventPayload, FailedPayload, FailureKind, FailurePhase,
    InvocationArchivedPayload, InvocationOperatorRecoveredPayload, InvocationTotals,
};
use fq_runtime::worker::WorkerId;
use fq_runtime::{AgentId, EventBus};
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer as _;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Context, SubscriberExt};
use uuid::Uuid;

/// The start of the warn line for a conflict on a row a drop left.
const A_DROPS_ROW: &str =
    "an ending reached a run recorded failed with its archive written or none";
/// The start of the error line for two endings of one run.
const TWO_ENDINGS: &str = "two endings published for one run";
/// The start of every conflict line, whatever its level.
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

fn archived_completed() -> EventPayload {
    EventPayload::InvocationArchived(InvocationArchivedPayload {
        worker_id: WorkerId::new("conflict-worker".to_string()).unwrap(),
        final_phase: "completed".to_string(),
        final_state_blob: b"{\"state\":1}".to_vec(),
        started_at_ms: 1_000,
        terminal_at_ms: 2_000,
    })
}

fn dropped_with_none_to_come() -> EventPayload {
    EventPayload::InvocationOperatorRecovered(InvocationOperatorRecoveredPayload {
        action: "drop".to_string(),
        final_phase: "failed".to_string(),
        reason: None,
        requested_by: Some("on-call".to_string()),
        archive: Some(DropArchive::WillNotCome),
    })
}

/// **A conflicting ending is a warn on a row a drop left, and an error
/// anywhere else.** Two runs:
///
/// - one a drop saying no archive will come left `failed` with none to
///   come, then reached by an archive saying `completed`: warn, since the
///   drop most likely came first;
/// - one that completed, then reached by a `failed`: error, the run
///   published two endings.
#[tokio::test]
async fn a_conflicting_ending_is_a_warn_on_a_row_a_drop_left_and_an_error_elsewhere() {
    let log = Log::installed();
    let server = fq_test_support::NatsServer::start();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        ControlPlaneStore::open(&dir.path().join("control-plane.db"))
            .await
            .unwrap(),
    );

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

    let none_to_come = Uuid::now_v7();
    let two_endings = Uuid::now_v7();
    let publish = |invocation_id: Uuid, payload: EventPayload| {
        let bus = bus.clone();
        let agent = agent.clone();
        async move {
            bus.publish(&Event::new(agent, invocation_id, payload))
                .await
                .expect("publish");
        }
    };
    publish(none_to_come, dropped_with_none_to_come()).await;
    publish(none_to_come, archived_completed()).await;
    publish(two_endings, completed()).await;
    publish(two_endings, failed()).await;

    let runs = [none_to_come, two_endings];
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
                let start = [A_DROPS_ROW, TWO_ENDINGS]
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
    assert_eq!(
        logged(none_to_come),
        [(
            tracing::Level::WARN,
            A_DROPS_ROW.to_string(),
            Some("completed".to_string())
        )],
        "a row a drop left with no archive to come, reached by an archive saying completed"
    );
    assert_eq!(
        logged(two_endings),
        [(
            tracing::Level::ERROR,
            TWO_ENDINGS.to_string(),
            Some("failed".to_string())
        )],
        "a completed run, then its failed: two endings"
    );
}
