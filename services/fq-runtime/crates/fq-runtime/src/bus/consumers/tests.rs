use std::time::Duration;

use super::*;
use crate::bus::ConsumerRedeliveryPolicy;

/// Read the server's view of a durable — the assertion has to be about
/// what JetStream actually holds, not about the config this process
/// meant to send.
async fn server_config(
    bus: &EventBus,
    stream: &str,
    consumer: &str,
) -> async_nats::jetstream::consumer::Config {
    let stream = bus.jetstream().get_stream(stream).await.expect("stream");
    let mut consumer = stream
        .get_consumer::<consumer::pull::Config>(consumer)
        .await
        .expect("consumer");
    consumer.info().await.expect("consumer info").config.clone()
}

/// Finding B4's first half, asserted at the server: every durable this
/// bus creates carries an explicit `ack_wait`, the ones with no
/// dead-letter path redeliver without limit, and the trigger consumer —
/// the one that dead-letters — keeps its finite bound.
#[tokio::test]
async fn every_durable_carries_an_explicit_ack_wait_and_a_considered_delivery_bound() {
    let server = crate::test_support::nats::test_nats();
    let policy = ConsumerRedeliveryPolicy {
        ack_wait: Duration::from_secs(45),
        ..ConsumerRedeliveryPolicy::default()
    };
    let bus = EventBus::connect(server.url())
        .await
        .expect("connect NATS")
        .with_redelivery_policy(policy);

    // Every factory, including the shapes production uses.
    bus.durable_consumer("t-plain", None).await.expect("plain");
    bus.durable_consumer_strict("t-strict", None)
        .await
        .expect("strict");
    bus.durable_consumer_with_filter("t-filtered", "fq.agent.*.invocation.*", None)
        .await
        .expect("filtered");
    bus.durable_consumer_with_filters(
        "t-multi",
        &["fq.agent.*.completed", "fq.agent.*.failed"],
        None,
    )
    .await
    .expect("multi");
    bus.advisory_consumer("t-advisory").await.expect("advisory");

    for name in ["t-plain", "t-strict", "t-filtered", "t-multi"] {
        let config = server_config(&bus, STREAM_NAME, name).await;
        assert_eq!(
            config.ack_wait,
            Duration::from_secs(45),
            "{name} must carry the policy's explicit ack_wait"
        );
        assert_eq!(
            config.max_deliver, UNLIMITED_MAX_DELIVER,
            "{name} has no dead-letter path, so a transient fault must never \
             drop the event to a delivery bound"
        );
    }

    let advisory = server_config(&bus, ADVISORY_STREAM_NAME, "t-advisory").await;
    assert_eq!(advisory.ack_wait, Duration::from_secs(45));
    assert_eq!(
        advisory.max_deliver, UNLIMITED_MAX_DELIVER,
        "an advisory dropped to a delivery bound is an exhausted trigger with no record"
    );

    bus.trigger_consumer_with_filter("t-trigger", "fq.trigger.>", 1)
        .await
        .expect("trigger");
    let trigger = server_config(&bus, TRIGGER_STREAM_NAME, "t-trigger").await;
    assert_eq!(
        trigger.max_deliver, TRIGGER_MAX_DELIVER,
        "the trigger consumer dead-letters, so its bound stays finite"
    );
    assert_eq!(trigger.backoff, TRIGGER_RETRY_BACKOFF.to_vec());
    // The server's rule, asserted so nobody reads the explicit
    // `ack_wait` above it and believes it: a consumer with a `backoff`
    // schedule has its ack window *replaced* by the schedule's first
    // entry. This is the trigger consumer's real first-delivery
    // deadline, and moving it belongs to exactly-once dispatch (#327).
    assert_eq!(
        trigger.ack_wait, TRIGGER_RETRY_BACKOFF[0],
        "JetStream overrides ack_wait with backoff[0] where a schedule is set"
    );
}

/// `get_or_create` keeps an existing durable's configuration, which is
/// right for its position and wrong for its policy. A daemon that
/// starts with new `[bus]` settings must repair what it finds rather
/// than run on a consumer nobody configured — **and must not cost that
/// consumer its place in the stream.** An upgrade that replayed the
/// event log through the projection would be a far worse bug than the
/// stale policy it fixed, so the acked floor is asserted here rather
/// than inferred from reading async-nats.
#[tokio::test]
async fn an_existing_durable_is_repaired_without_losing_its_place() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");

    let worker_id =
        crate::worker::WorkerId::new(format!("repair-{}", uuid::Uuid::now_v7().simple())).unwrap();
    for _ in 0..3 {
        bus.publish(&crate::events::Event::system(
            uuid::Uuid::now_v7(),
            crate::events::EventPayload::WorkerHeartbeat(crate::events::WorkerHeartbeatPayload {
                worker_id: worker_id.clone(),
            }),
        ))
        .await
        .expect("publish");
    }
    let filter = format!("fq.worker.{}.heartbeat", worker_id.as_str());

    // A durable as an older binary would have left it: a short ack
    // window and a finite delivery bound on a stream with no
    // dead-letter path.
    let legacy = bus
        .jetstream()
        .get_stream(STREAM_NAME)
        .await
        .expect("stream")
        .get_or_create_consumer(
            "t-legacy",
            consumer::pull::Config {
                durable_name: Some("t-legacy".to_string()),
                ack_policy: consumer::AckPolicy::Explicit,
                filter_subject: filter.clone(),
                ack_wait: Duration::from_secs(5),
                max_deliver: 5,
                ..Default::default()
            },
        )
        .await
        .expect("legacy consumer");

    let before = server_config(&bus, STREAM_NAME, "t-legacy").await;
    assert_eq!(before.max_deliver, 5, "the legacy shape is what we planted");

    // Consume and ack the first two, so the durable has a real
    // position rather than an empty one.
    let mut batch = legacy
        .fetch()
        .max_messages(2)
        .messages()
        .await
        .expect("fetch");
    let mut acked = 0;
    while let Some(msg) = futures::StreamExt::next(&mut batch).await {
        msg.expect("message").ack().await.expect("ack");
        acked += 1;
    }
    assert_eq!(acked, 2, "two of the three are behind us");

    // The repair.
    let repaired = bus
        .durable_consumer_with_filter("t-legacy", &filter, None)
        .await
        .expect("reopen");
    let after = server_config(&bus, STREAM_NAME, "t-legacy").await;
    assert_eq!(after.ack_wait, ConsumerRedeliveryPolicy::default().ack_wait);
    assert_eq!(
        after.max_deliver, UNLIMITED_MAX_DELIVER,
        "the delivery bound that could drop an event is repaired away"
    );

    // The place survived: the next delivery is the third message, not
    // the first. A repair that replayed would hand back sequence 1.
    let mut batch = repaired
        .fetch()
        .max_messages(1)
        .messages()
        .await
        .expect("fetch after repair");
    let next = futures::StreamExt::next(&mut batch)
        .await
        .expect("a third message")
        .expect("message");
    assert_eq!(
        next.info().expect("metadata").stream_sequence,
        3,
        "the acked floor survived the in-place update — no replay"
    );
}

/// The strict consumer's scope repair, kept from the code this module
/// collected: a mark-bearing durable vouches for every sequence, so one
/// created filtered must widen to the whole stream while keeping its
/// acked floor.
#[tokio::test]
async fn a_strict_durable_is_widened_and_made_contiguous() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");

    bus.durable_consumer_with_filter("t-was-filtered", "fq.agent.*.invocation.*", None)
        .await
        .expect("filtered");
    bus.durable_consumer_strict("t-was-filtered", None)
        .await
        .expect("strict reopen");

    let config = server_config(&bus, STREAM_NAME, "t-was-filtered").await;
    assert_eq!(config.max_ack_pending, 1, "resolved-contiguous delivery");
    assert!(
        config.filter_subject.is_empty() && config.filter_subjects.is_empty(),
        "a mark-bearing consumer must see every sequence, got \
         {:?} / {:?}",
        config.filter_subject,
        config.filter_subjects
    );
}

/// The summariser is the one durable whose handler is not a SQLite
/// write: it calls a model inline, under the worker's response budget.
/// Its ack window has to cover that, or a slow summary is redelivered
/// *behind the one still generating it* and the sequential loop
/// regenerates it — the same work, paid for twice (#611 review).
///
/// Asserted at the broker, against the deadline the daemon configures.
#[tokio::test]
async fn the_summary_durable_outlasts_the_llm_response_budget() {
    use crate::control_plane::summary_consumer::CONSUMER_NAME as SUMMARY_CONSUMER;

    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");

    let llm_deadline = Duration::from_secs(600);
    let consumer = crate::SummaryConsumer::new(
        bus.clone(),
        std::sync::Arc::new(crate::llm::fixture::FixtureClient::new()),
        std::sync::Arc::new(crate::pricing::PricingTable::empty()),
        "test-model".to_string(),
        120,
    )
    .with_llm_deadline(llm_deadline);

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move { consumer.run(shutdown_rx).await });

    // Poll for the durable: the loop creates it on start.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let config = loop {
        if let Ok(stream) = bus.jetstream().get_stream(STREAM_NAME).await
            && let Ok(mut c) = stream
                .get_consumer::<consumer::pull::Config>(SUMMARY_CONSUMER)
                .await
            && let Ok(info) = c.info().await
        {
            break info.config.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the summary consumer never created its durable"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    assert!(
        config.ack_wait >= llm_deadline,
        "the ack window ({:?}) must outlast the response budget ({llm_deadline:?}) the \
         handler runs under",
        config.ack_wait
    );
    assert_ne!(
        config.ack_wait,
        ConsumerRedeliveryPolicy::default().ack_wait,
        "and it is emphatically not the bus default, which is sized for a SQLite write"
    );

    let _ = shutdown_tx.send(());
    let _ = handle.await;
}
