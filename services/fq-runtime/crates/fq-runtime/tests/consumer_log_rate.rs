//! The redelivery log's rate limit, counted as lines (#549, review
//! finding B4).
//!
//! Its own test binary because the assertion is about *output*: the
//! only way to see what a spawned consumer task logged is a captured
//! subscriber, and a captured subscriber has to be the process-global
//! one — which can be installed exactly once. A single test per binary
//! is the price of measuring the thing rather than the limiter behind
//! it, and the limiter has its own unit tests in `bus::retry`.
//!
//! What is being defended: before the escalating NAK, a handler that
//! kept failing produced an error line per broker round-trip, which on
//! a control-plane consumer is thousands a second. The fix has two
//! halves and this covers the second — the delay stops the loop, the
//! rate limit stops the log from burying every other line in the
//! daemon's journal while it happens.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fq_runtime::bus::{ConsumerRedeliveryPolicy, EventBus};
use fq_runtime::control_plane::durable_consumer::{
    DeliverFrom, DurableConsumerConfig, HandlerError, run_durable_consumer,
};
use fq_runtime::events::{Event, EventPayload, WorkerHeartbeatPayload};
use fq_runtime::worker::WorkerId;
use tokio::sync::oneshot;
use tracing_subscriber::fmt::MakeWriter;
use uuid::Uuid;

#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for SharedBuf {
    type Writer = SharedBuf;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// A handler that fails forever, with the escalation turned off: every
/// redelivery is at the cap, so nothing is an escalation step and the
/// interval alone decides. Dozens of redeliveries inside one interval
/// must produce one line.
#[tokio::test(flavor = "multi_thread")]
async fn a_wedged_consumer_logs_once_per_interval_not_once_per_redelivery() {
    let buf = SharedBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        // Plain text: the assertions below read field values out of the
        // line, and ANSI escapes sit between a field's name and its `=`.
        .with_ansi(false)
        .with_max_level(tracing::Level::ERROR)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("install the capturing subscriber");

    let server = fq_test_support::NatsServer::start();
    let policy = ConsumerRedeliveryPolicy {
        // Flat, fast redelivery: the escalation is not what is under
        // test here, and a flat delay is the worst case for the log.
        nak_initial: Duration::from_millis(20),
        nak_max: Duration::from_millis(20),
        // Longer than the test runs, so every line after the first is
        // the interval's doing.
        log_interval: Duration::from_secs(3_600),
        ..ConsumerRedeliveryPolicy::default()
    };
    let bus = EventBus::connect(server.url())
        .await
        .expect("connect NATS")
        .with_redelivery_policy(policy);

    let worker_id = WorkerId::new(format!("lograte-{}", Uuid::now_v7().simple())).unwrap();
    bus.publish(&Event::system(
        Uuid::now_v7(),
        EventPayload::WorkerHeartbeat(WorkerHeartbeatPayload {
            worker_id: worker_id.clone(),
        }),
    ))
    .await
    .expect("publish");

    let attempts = Arc::new(AtomicUsize::new(0));
    let config = DurableConsumerConfig {
        durable_name: format!("fq-lograte-{}", Uuid::now_v7().simple()),
        filter_subjects: vec![format!("fq.worker.{}.heartbeat", worker_id.as_str())],
        deliver_from: DeliverFrom::Beginning,
        strict_order: false,
    };
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let bus_for_loop = bus.clone();
    let attempts_for_loop = attempts.clone();
    let handle = tokio::spawn(async move {
        run_durable_consumer(&bus_for_loop, config, shutdown_rx, move |_delivery| {
            let attempts = attempts_for_loop.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                Err(HandlerError::transient(std::io::Error::other(
                    "database or disk is full (SQLITE_FULL)",
                )))
            }
        })
        .await
    });

    // Twenty redeliveries, all inside the one-hour log interval.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while attempts.load(Ordering::SeqCst) < 20 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the failing message stopped coming back after {} deliveries",
            attempts.load(Ordering::SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let _ = shutdown_tx.send(());
    let _ = handle.await;

    let captured = String::from_utf8(buf.0.lock().unwrap().clone()).expect("utf-8 log");
    let nak_lines = captured
        .lines()
        .filter(|line| line.contains("handler failed; NAK for redelivery"))
        .count();
    assert_eq!(
        nak_lines,
        1,
        "at least 20 redeliveries inside one log interval must produce one line, got \
         {nak_lines}:\n{captured}"
    );
    // The line has to be worth the one slot it gets: it names the
    // consumer and says how long the next attempt is away, which is what
    // turns "something is failing" into "this consumer, backing off".
    let line = captured
        .lines()
        .find(|line| line.contains("handler failed; NAK for redelivery"))
        .expect("the one line");
    assert!(line.contains("consumer=\"fq-lograte-"), "got: {line}");
    assert!(line.contains("retry_in_ms=20"), "got: {line}");
    assert!(line.contains("delivered=1"), "got: {line}");
}
