//! What the coordination consumer does with a write the control-plane
//! store refuses for good (issue 1070): it acknowledges the event, so the
//! event is not redelivered and holds nothing behind it, and logs one line
//! at `error` naming the invocation and the event, since an event it meant
//! to apply is left unapplied.
//!
//! **A binary of its own,** as `control_plane_record_rung_log.rs` is: a
//! captured subscriber has to be the process-global one, since tracing
//! caches a callsite's interest for the whole process.
//!
//! A trigger on the ownership table stands in for a rule the store holds
//! the write to: it refuses every new row, as a check refuses a pair.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use fq_runtime::control_plane::{ControlPlaneStore, CoordinationConsumer};
use fq_runtime::events::{DropArchive, Event, EventPayload, InvocationOperatorRecoveredPayload};
use fq_runtime::{AgentId, EventBus};
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer as _;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Context, SubscriberExt};
use uuid::Uuid;

/// The start of the line the consumer logs for a refused write.
const REFUSED: &str = "the control-plane store refused the event's write for good";

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

    fn refused(&self) -> Vec<Line> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.message.starts_with(REFUSED))
            .cloned()
            .collect()
    }
}

/// **A refused write is acknowledged and logged once, at `error`.** The
/// consumer folds an operator's drop whose ownership row the store will
/// not take. It logs one line at error with the invocation, the agent and
/// the event's type, and acknowledges the event: nothing is left pending
/// or redelivered on its durable consumer.
#[tokio::test]
async fn a_refused_write_is_acknowledged_and_logged_once_at_error() {
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
    sqlx::query(
        "CREATE TRIGGER refuse_the_row BEFORE INSERT ON coordination_invocation_owner \
         BEGIN SELECT RAISE(ABORT, 'the test refuses the row'); END",
    )
    .execute(&raw)
    .await
    .unwrap();

    let agent = format!("refused-{}", Uuid::now_v7().simple());
    let consumer_name = format!("refused-write-{}", Uuid::now_v7().simple());
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

    let invocation_id = Uuid::now_v7();
    bus.publish(&Event::new(
        AgentId::new(agent.clone()).unwrap(),
        invocation_id,
        EventPayload::InvocationOperatorRecovered(InvocationOperatorRecoveredPayload {
            action: "drop".to_string(),
            final_phase: "failed".to_string(),
            reason: None,
            requested_by: None,
            archive: Some(DropArchive::WillNotCome),
        }),
    ))
    .await
    .expect("publish the drop");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while log.refused().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no line at error says the store refused the drop's write: the event was not \
             taken as refused for good"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Long enough for a NAK to come back round, had there been one.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let mut durable = stream
        .get_consumer::<async_nats::jetstream::consumer::pull::Config>(&consumer_name)
        .await
        .expect("the consumer");
    let info = durable.info().await.expect("the consumer's state").clone();
    let lines = log.refused();
    let _ = stop.send(());
    let _ = tokio::time::timeout(Duration::from_secs(10), running).await;

    assert_eq!(
        lines
            .iter()
            .map(|line| (
                line.level,
                line.field("invocation_id"),
                line.field("agent_id"),
                line.field("event_type"),
            ))
            .collect::<Vec<_>>(),
        [(
            tracing::Level::ERROR,
            Some(invocation_id.to_string().as_str()),
            Some(agent.as_str()),
            Some("invocation_operator_recovered"),
        )],
        "one line at error, once: {lines:#?}"
    );
    assert_eq!(
        (info.num_ack_pending, info.num_redelivered),
        (0, 0),
        "the event is acknowledged, not left pending or redelivered"
    );
}
