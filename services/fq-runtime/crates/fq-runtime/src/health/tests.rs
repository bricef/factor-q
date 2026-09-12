//! Health-probe tests, including the fault injection finding B4 asks
//! for: a consumer whose handler fails forever.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::oneshot;
use uuid::Uuid;

use super::*;
use crate::bus::EventBus;
use crate::control_plane::durable_consumer::{
    DeliverFrom, DurableConsumerConfig, HandlerError, run_durable_consumer,
};
use crate::events::{Event, EventPayload, WorkerHeartbeatPayload};
use crate::worker::WorkerId;

/// The roster is what "every consumer" means, so it is asserted rather
/// than left to the call sites: three streams, and the summariser only
/// where one is configured.
#[test]
fn the_expected_roster_covers_every_durable_and_only_expects_a_configured_summariser() {
    let without = core_streams(false);
    let names: Vec<&str> = without
        .iter()
        .flat_map(|(_, c)| c.iter().copied())
        .collect();
    assert_eq!(
        names,
        vec![
            "fq-projector",
            "fq-coordination",
            "fq-heartbeat",
            "fq-dispatcher",
            "fq-advisory-watch",
        ],
        "every durable the daemon creates, and no summariser without one configured"
    );
    assert_eq!(
        without.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        vec!["fq-events", "fq-triggers", "fq-advisories"]
    );

    let with = core_streams(true);
    let names: Vec<&str> = with.iter().flat_map(|(_, c)| c.iter().copied()).collect();
    assert!(
        names.contains(&"fq-summary"),
        "a configured summariser is a required consumer: {names:?}"
    );
}

#[test]
fn stuck_needs_outstanding_work_a_real_redelivery_and_more_than_the_threshold() {
    let policy = ConsumerRedeliveryPolicy {
        stuck_after_redeliveries: 5,
        ..ConsumerRedeliveryPolicy::default()
    };
    assert!(
        !is_stuck(0, 1, 99, policy),
        "nothing pending is idle, not stuck"
    );
    assert!(
        !is_stuck(1, 1, 4, policy),
        "under the threshold is retrying"
    );
    assert!(is_stuck(1, 1, 5, policy), "at the threshold is stuck");

    // The out-of-order acker's guard: the arithmetic over the
    // contiguous floor can run high on a consumer that acks
    // concurrently, but the server says nothing has been delivered
    // twice, so nothing is being retried.
    assert!(
        !is_stuck(1, 0, 99, policy),
        "a consumer that has redelivered nothing cannot be stuck retrying"
    );

    // A threshold of zero would call the first delivery of anything a
    // fault; one redelivery is the floor.
    let zeroed = ConsumerRedeliveryPolicy {
        stuck_after_redeliveries: 0,
        ..policy
    };
    assert!(!is_stuck(1, 1, 0, zeroed));
    assert!(is_stuck(1, 1, 1, zeroed));
}

/// The dispatcher's shape, against a live broker: a consumer with room
/// for several in-flight messages acks later ones while an earlier one
/// is still on its honest **first** delivery. The contiguous acked
/// floor lags, so the arithmetic over it counts those acks — and the
/// verdict must not, because nothing has been redelivered.
///
/// Without the `num_redelivered` gate this reads `stuck` on a healthy
/// dispatcher as soon as `stuck_after_redeliveries` later triggers ack.
#[tokio::test]
async fn a_consumer_that_acks_out_of_order_is_not_stuck() {
    let server = crate::test_support::nats::test_nats();
    let policy = ConsumerRedeliveryPolicy {
        stuck_after_redeliveries: 2,
        ..ConsumerRedeliveryPolicy::default()
    };
    let bus = EventBus::connect(server.url())
        .await
        .expect("connect NATS")
        .with_redelivery_policy(policy);

    let worker_id = WorkerId::new(format!("ooo-{}", Uuid::now_v7().simple())).unwrap();
    for _ in 0..5 {
        bus.publish(&Event::system(
            Uuid::now_v7(),
            EventPayload::WorkerHeartbeat(WorkerHeartbeatPayload {
                worker_id: worker_id.clone(),
                last_step_at_ms: None,
            }),
        ))
        .await
        .expect("publish");
    }

    // Room for every message at once — the dispatcher's window, not
    // the projection's resolved-contiguous one.
    let durable = format!("fq-ooo-{}", Uuid::now_v7().simple());
    let consumer = bus
        .durable_consumer_with_filter(
            &durable,
            &format!("fq.worker.{}.heartbeat", worker_id.as_str()),
            None,
        )
        .await
        .expect("consumer");

    // Take all five, then ack every one but the first. The first is
    // still on delivery one: honestly in flight, never redelivered.
    let mut batch = consumer
        .fetch()
        .max_messages(5)
        .messages()
        .await
        .expect("fetch");
    let mut taken = Vec::new();
    while let Some(msg) = futures::StreamExt::next(&mut batch).await {
        taken.push(msg.expect("message"));
    }
    assert_eq!(taken.len(), 5, "all five must be delivered at once");
    for msg in taken.iter().skip(1) {
        msg.ack().await.expect("ack");
    }

    let health = probe_stream(
        &bus.jetstream(),
        crate::bus::STREAM_NAME,
        &[durable.as_str()],
        policy,
        bus.consumer_ledger(),
    )
    .await;
    let StreamHealth::Available { consumers, .. } = &health else {
        panic!("the event stream must be available: {health:?}");
    };
    let ConsumerHealth::Active {
        stuck,
        redeliveries,
        num_redelivered,
        ..
    } = &consumers[0]
    else {
        panic!("the durable exists, so it is Active: {:?}", consumers[0]);
    };
    assert_eq!(
        *num_redelivered, 0,
        "nothing was redelivered — every message is on its first delivery"
    );
    assert!(
        !stuck,
        "acking out of order is not a wedge; the arithmetic over the contiguous \
         floor read {redeliveries} but nothing is being retried"
    );
    assert!(
        !consumers[0].is_fault(),
        "a healthy concurrent consumer must not reach the red list: {:?}",
        consumers[0]
    );
}

/// The `SQLITE_FULL` class from the Phase 1 exit criterion, injected: a
/// handler that returns `Transient` forever, on a durable with
/// unlimited redelivery.
///
/// Three things must hold at once, and each was broken before #549:
/// the redelivery gaps grow rather than running at broker speed; the
/// error log is bounded rather than one line per round-trip; and
/// `control.status` reports the consumer as stuck **by name** rather
/// than reporting nothing wrong.
///
/// The policy is scaled down (40ms doubling to 320ms) so the escalation
/// is observable in a test rather than over five minutes — the shape is
/// what is under test, and the production numbers are asserted in
/// `config.rs`.
#[tokio::test]
async fn a_permanently_failing_handler_backs_off_logs_at_a_bounded_rate_and_reports_stuck() {
    let server = crate::test_support::nats::test_nats();
    let policy = ConsumerRedeliveryPolicy {
        nak_initial: Duration::from_millis(40),
        nak_max: Duration::from_millis(320),
        log_interval: Duration::from_secs(3_600),
        stuck_after_redeliveries: 4,
        ..ConsumerRedeliveryPolicy::default()
    };
    let bus = EventBus::connect(server.url())
        .await
        .expect("connect NATS")
        .with_redelivery_policy(policy);

    let worker_id = WorkerId::new(format!("stuck-{}", Uuid::now_v7().simple())).unwrap();
    bus.publish(&Event::system(
        Uuid::now_v7(),
        EventPayload::WorkerHeartbeat(WorkerHeartbeatPayload {
            worker_id: worker_id.clone(),
            last_step_at_ms: None,
        }),
    ))
    .await
    .expect("publish");

    let durable = format!("fq-stuck-{}", Uuid::now_v7().simple());
    let config = DurableConsumerConfig {
        durable_name: durable.clone(),
        filter_subjects: vec![format!("fq.worker.{}.heartbeat", worker_id.as_str())],
        deliver_from: DeliverFrom::Beginning,
        strict_order: false,
        ack_wait: None,
    };

    // Every delivery instant, so the growing gaps are measured rather
    // than inferred from the policy that produced them.
    let deliveries: Arc<std::sync::Mutex<Vec<std::time::Instant>>> = Arc::new(Vec::new().into());
    let attempts = Arc::new(AtomicUsize::new(0));
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let bus_for_loop = bus.clone();
    let deliveries_for_loop = deliveries.clone();
    let attempts_for_loop = attempts.clone();
    let handle = tokio::spawn(async move {
        run_durable_consumer(&bus_for_loop, config, shutdown_rx, move |_delivery| {
            let deliveries = deliveries_for_loop.clone();
            let attempts = attempts_for_loop.clone();
            async move {
                deliveries.lock().unwrap().push(std::time::Instant::now());
                attempts.fetch_add(1, Ordering::SeqCst);
                // The full disk that never clears.
                Err(HandlerError::transient(std::io::Error::other(
                    "database or disk is full (SQLITE_FULL)",
                )))
            }
        })
        .await
    });

    // Six deliveries: 40, 80, 160, 320, 320ms apart — about a second.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while attempts.load(Ordering::SeqCst) < 6 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the NAK'd message stopped coming back after {} deliveries",
            attempts.load(Ordering::SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The delay escalates, and then it stops escalating. Measured gaps
    // carry scheduling noise, so the assertions are on the shape: later
    // gaps are meaningfully larger than the first, and none runs away
    // past the cap.
    let gaps: Vec<Duration> = {
        let times = deliveries.lock().unwrap();
        times.windows(2).map(|w| w[1] - w[0]).collect()
    };
    assert!(gaps.len() >= 5, "expected five gaps, got {gaps:?}");
    assert!(
        gaps[3] > gaps[0],
        "the redelivery delay must grow: {gaps:?}"
    );
    assert!(
        gaps[0] >= Duration::from_millis(30),
        "the first retry must not be immediate — that is the hot loop: {gaps:?}"
    );
    for gap in &gaps {
        assert!(
            *gap < Duration::from_millis(2_000),
            "no gap may exceed the cap by an order of magnitude: {gaps:?}"
        );
    }

    // The health surface: stuck, and named. Poll, because the consumer
    // info the daemon reads lags the deliveries by a round trip.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let stuck = loop {
        let health = probe_stream(
            &bus.jetstream(),
            crate::bus::STREAM_NAME,
            &[durable.as_str()],
            policy,
            bus.consumer_ledger(),
        )
        .await;
        let StreamHealth::Available { consumers, .. } = &health else {
            panic!("the event stream must be available: {health:?}");
        };
        let consumer = consumers.first().expect("the probed consumer");
        if consumer.is_fault() {
            break consumer.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "a consumer redelivering forever never reported unhealthy: {consumer:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(
        stuck.name(),
        durable,
        "health must name the stuck consumer — `something is wrong` is not actionable"
    );
    let ConsumerHealth::Active {
        stuck: is_stuck,
        redeliveries,
        ..
    } = &stuck
    else {
        panic!("the durable exists, so it is Active: {stuck:?}");
    };
    assert!(is_stuck, "the verdict is stuck, not merely lagging");
    assert!(
        *redeliveries >= 4,
        "the redelivery count is what makes the verdict readable: {redeliveries}"
    );

    let _ = shutdown_tx.send(());
    let _ = handle.await;
}

/// The other half of the same probe: a consumer that keeps up is not
/// reported stuck, however many messages it has handled. Without this,
/// the test above would pass on a probe that called everything stuck.
#[tokio::test]
async fn a_healthy_consumer_is_never_reported_stuck() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");

    let worker_id = WorkerId::new(format!("healthy-{}", Uuid::now_v7().simple())).unwrap();
    for _ in 0..5 {
        bus.publish(&Event::system(
            Uuid::now_v7(),
            EventPayload::WorkerHeartbeat(WorkerHeartbeatPayload {
                worker_id: worker_id.clone(),
                last_step_at_ms: None,
            }),
        ))
        .await
        .expect("publish");
    }

    let durable = format!("fq-healthy-{}", Uuid::now_v7().simple());
    let config = DurableConsumerConfig {
        durable_name: durable.clone(),
        filter_subjects: vec![format!("fq.worker.{}.heartbeat", worker_id.as_str())],
        deliver_from: DeliverFrom::Beginning,
        strict_order: false,
        ack_wait: None,
    };
    let handled = Arc::new(AtomicUsize::new(0));
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let bus_for_loop = bus.clone();
    let handled_for_loop = handled.clone();
    let handle = tokio::spawn(async move {
        run_durable_consumer(&bus_for_loop, config, shutdown_rx, move |_delivery| {
            let handled = handled_for_loop.clone();
            async move {
                handled.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        })
        .await
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while handled.load(Ordering::SeqCst) < 5 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the healthy consumer never drained its five messages"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let health = probe_stream(
        &bus.jetstream(),
        crate::bus::STREAM_NAME,
        &[durable.as_str()],
        bus.redelivery_policy(),
        bus.consumer_ledger(),
    )
    .await;
    let StreamHealth::Available { consumers, .. } = &health else {
        panic!("the event stream must be available: {health:?}");
    };
    assert!(
        !consumers[0].is_fault(),
        "a consumer that handled everything is healthy: {:?}",
        consumers[0]
    );

    let _ = shutdown_tx.send(());
    let _ = handle.await;
}

/// The defect, at the probe: a durable filtered to a subject nothing
/// publishes to, on a stream that has taken several messages it does
/// not match, is caught up — and the broker says so itself, with
/// `num_pending` 0 behind the consumer's own filter.
///
/// This is the summariser's shape on the dogfood broker. The stream
/// head runs away from a filtered consumer as fast as anyone publishes
/// anything, so a verdict read off the distance to it calls this
/// consumer lagging and the number only ever grows.
#[tokio::test]
async fn a_filtered_durable_matching_nothing_on_the_stream_is_caught_up() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");

    // A filter that no publisher on this stream will ever match.
    let quiet = WorkerId::new(format!("quiet-{}", Uuid::now_v7().simple())).unwrap();
    let durable = format!("fq-quiet-{}", Uuid::now_v7().simple());
    bus.durable_consumer_with_filter(
        &durable,
        &format!("fq.worker.{}.heartbeat", quiet.as_str()),
        None,
    )
    .await
    .expect("consumer");

    // Traffic it is not subscribed to, published after it exists, so
    // the stream head moves without offering it anything.
    let noisy = WorkerId::new(format!("noisy-{}", Uuid::now_v7().simple())).unwrap();
    for _ in 0..5 {
        bus.publish(&Event::system(
            Uuid::now_v7(),
            EventPayload::WorkerHeartbeat(WorkerHeartbeatPayload {
                worker_id: noisy.clone(),
                last_step_at_ms: None,
            }),
        ))
        .await
        .expect("publish");
    }

    let health = probe_stream(
        &bus.jetstream(),
        crate::bus::STREAM_NAME,
        &[durable.as_str()],
        bus.redelivery_policy(),
        bus.consumer_ledger(),
    )
    .await;
    let StreamHealth::Available {
        consumers,
        last_seq,
        ..
    } = &health
    else {
        panic!("the event stream must be available: {health:?}");
    };
    let ConsumerHealth::Active {
        delivered,
        num_pending,
        ack_pending,
        ..
    } = &consumers[0]
    else {
        panic!("the durable exists, so it is Active: {:?}", consumers[0]);
    };
    assert!(
        *last_seq > *delivered,
        "the head must have moved past the consumer for this to be the case under test: \
         head {last_seq}, delivered {delivered}"
    );
    assert_eq!(
        (*num_pending, *ack_pending),
        (0, 0),
        "nothing matching is waiting and nothing is in flight: {:?}",
        consumers[0]
    );
    assert_eq!(
        consumers[0].progress(),
        Some(ConsumerProgress::CaughtUp),
        "a consumer the broker owes nothing is caught up wherever it sits: {:?}",
        consumers[0]
    );
    assert!(!consumers[0].is_fault(), "{:?}", consumers[0]);
}

/// A durable nobody has created reads as `Missing` by name, which is
/// what makes "`fq doctor` reports every consumer" true of a daemon
/// whose task failed to start.
#[tokio::test]
async fn an_expected_consumer_that_does_not_exist_is_reported_missing_by_name() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");

    let consumers = probe_core_consumers(
        &bus.jetstream(),
        true,
        ConsumerRedeliveryPolicy::default(),
        bus.consumer_ledger(),
    )
    .await;
    let names: Vec<&str> = consumers.iter().map(|c| c.name()).collect();
    assert_eq!(
        names,
        vec![
            "fq-projector",
            "fq-coordination",
            "fq-heartbeat",
            "fq-summary",
            "fq-dispatcher",
            "fq-advisory-watch",
        ],
        "every expected durable is reported, existing or not"
    );
    assert!(
        consumers
            .iter()
            .all(|c| matches!(c, ConsumerHealth::Missing { .. })),
        "no daemon has run against this broker, so all six are missing: {consumers:?}"
    );
}
