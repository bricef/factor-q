//! A delivery a stopped daemon took and never acknowledged comes back to
//! the next start's consumer within [`ACK_WAIT`] (issue 1070).
//!
//! The consumer is strict-order: one delivery outstanding at a time. A
//! delivery the stopped daemon held unacknowledged therefore holds every
//! later event behind it until the broker gives up on it, and with it the
//! coordination half of the read horizon that gated reads wait on.

use super::*;
use crate::agent::AgentId;
use crate::bus::STREAM_NAME;
use crate::events::HostNoticePayload;
use crate::test_support::nats::test_nats;
use futures::StreamExt;

/// An event the fold does nothing with but mark.
fn notice(agent: &AgentId) -> Event {
    Event::new(
        agent.clone(),
        Uuid::now_v7(),
        EventPayload::HostNotice(HostNoticePayload {
            kind: "test_barrier".to_string(),
            body: String::new(),
        }),
    )
}

/// Run a mark-bearing consumer until its mark reaches `seq`, then stop it,
/// and say how long that took. `None` if it did not get there in `bound`.
async fn fold_to(
    bus: &EventBus,
    store: &Arc<ControlPlaneStore>,
    seq: u64,
    bound: Duration,
) -> Option<Duration> {
    let (mark, coordination) = crate::watermark::channel();
    let consumer = CoordinationConsumer::new(bus.clone(), store.clone()).with_watermark(mark);
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(async move { consumer.run(stopped).await });
    let started = std::time::Instant::now();
    let reached = coordination.wait_for(seq, bound).await.is_ok();
    let took = started.elapsed();
    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the consumer stops")
        .expect("its task joins")
        .expect("it exits cleanly");
    reached.then_some(took)
}

/// **A delivery left unacknowledged is folded within the ack window.** A
/// first consumer creates the durable and folds to the end of the stream.
/// The stopped daemon's last act is then to take the next event and not
/// acknowledge it. The next consumer folds that event within [`ACK_WAIT`]
/// and a margin, where at the bus default it would wait thirty seconds.
#[tokio::test]
async fn a_delivery_left_unacknowledged_comes_back_within_the_ack_window() {
    let server = test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        ControlPlaneStore::open(&dir.path().join("control-plane.db"))
            .await
            .unwrap(),
    );
    let agent = AgentId::new("unacknowledged").unwrap();
    let first = bus.publish(&notice(&agent)).await.expect("publish");
    fold_to(&bus, &store, first, Duration::from_secs(10))
        .await
        .expect("the first consumer folds to the end of the stream");

    let held = bus.publish(&notice(&agent)).await.expect("publish");
    let stream = bus
        .jetstream()
        .get_stream(STREAM_NAME)
        .await
        .expect("the event stream");
    let durable: async_nats::jetstream::consumer::PullConsumer = stream
        .get_consumer(CONSUMER_NAME)
        .await
        .expect("the durable the first consumer created");
    let mut batch = durable
        .fetch()
        .max_messages(1)
        .messages()
        .await
        .expect("fetch");
    let taken = batch
        .next()
        .await
        .expect("one delivery")
        .expect("delivered");
    assert_eq!(taken.info().unwrap().stream_sequence, held);
    drop((taken, batch));
    assert_eq!(
        stream
            .consumer_info(CONSUMER_NAME)
            .await
            .expect("consumer info")
            .num_ack_pending,
        1,
        "the stopped daemon left its delivery unacknowledged"
    );

    let took = fold_to(&bus, &store, held, ACK_WAIT + Duration::from_secs(3))
        .await
        .unwrap_or_else(|| {
            panic!(
                "the next consumer did not fold the unacknowledged delivery within {:?}",
                ACK_WAIT + Duration::from_secs(3)
            )
        });
    assert!(took <= ACK_WAIT + Duration::from_secs(3), "{took:?}");
}
