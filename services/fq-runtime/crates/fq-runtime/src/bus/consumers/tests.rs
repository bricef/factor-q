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
    let mut stream = bus.jetstream().get_stream(stream).await.expect("stream");
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
    bus.durable_consumer("t-plain").await.expect("plain");
    bus.durable_consumer_strict("t-strict").await.expect("strict");
    bus.durable_consumer_with_filter("t-filtered", "fq.agent.*.invocation.*")
        .await
        .expect("filtered");
    bus.durable_consumer_with_filters("t-multi", &["fq.agent.*.completed", "fq.agent.*.failed"])
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
    assert_eq!(trigger.ack_wait, Duration::from_secs(45));
    assert_eq!(
        trigger.max_deliver, TRIGGER_MAX_DELIVER,
        "the trigger consumer dead-letters, so its bound stays finite"
    );
    assert_eq!(trigger.backoff, TRIGGER_RETRY_BACKOFF.to_vec());
}

/// `get_or_create` keeps an existing durable's configuration, which is
/// right for its position and wrong for its policy. A daemon that
/// starts with new `[bus]` settings must repair what it finds rather
/// than run on a consumer nobody configured.
#[tokio::test]
async fn an_existing_durable_is_repaired_to_this_daemons_policy() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");

    // A durable as an older binary would have left it: a short ack
    // window and a finite delivery bound on a stream with no
    // dead-letter path.
    bus.jetstream()
        .get_stream(STREAM_NAME)
        .await
        .expect("stream")
        .get_or_create_consumer(
            "t-legacy",
            consumer::pull::Config {
                durable_name: Some("t-legacy".to_string()),
                ack_policy: consumer::AckPolicy::Explicit,
                ack_wait: Duration::from_secs(5),
                max_deliver: 5,
                ..Default::default()
            },
        )
        .await
        .expect("legacy consumer");

    let before = server_config(&bus, STREAM_NAME, "t-legacy").await;
    assert_eq!(before.max_deliver, 5, "the legacy shape is what we planted");

    bus.durable_consumer("t-legacy").await.expect("reopen");
    let after = server_config(&bus, STREAM_NAME, "t-legacy").await;
    assert_eq!(after.ack_wait, ConsumerRedeliveryPolicy::default().ack_wait);
    assert_eq!(
        after.max_deliver, UNLIMITED_MAX_DELIVER,
        "the delivery bound that could drop an event is repaired away"
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

    bus.durable_consumer_with_filter("t-was-filtered", "fq.agent.*.invocation.*")
        .await
        .expect("filtered");
    bus.durable_consumer_strict("t-was-filtered")
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
