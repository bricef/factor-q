//! The 3a acceptance oracle: `publish` returns the event's stream
//! sequence, the projection consumer advances the watermark as it
//! applies, and a read gated `at_watermark(min_seq)` — with `min_seq`
//! from the publish ack — is guaranteed to see the published rows.
//! This is the read-your-writes composition Phase 3c's command
//! receipts ride on, proven at the runtime layer against a live
//! broker.

use std::sync::Arc;
use std::time::Duration;

use fq_runtime::agent::AgentId;
use fq_runtime::bus::EventBus;
use fq_runtime::control_plane::projection::consumer::ProjectionConsumer;
use fq_runtime::control_plane::projection::store::ProjectionStore;
use fq_runtime::db::RuntimeDbPaths;
use fq_runtime::events::{CompletedPayload, Event, EventPayload, TaskStatus};
use fq_runtime::views::{Views, ViewsError};
use fq_runtime::watermark;
use uuid::Uuid;

fn completed(agent: &str) -> Event {
    Event::new(
        AgentId::new(agent).expect("valid test agent id"),
        Uuid::now_v7(),
        EventPayload::Completed(CompletedPayload {
            task_status: TaskStatus::default(),
            result_summary: Some("ok".to_string()),
            total_llm_calls: 1,
            total_tool_calls: 0,
            total_cost: 0.001,
            total_duration_ms: 10,
        }),
    )
}

#[tokio::test]
async fn a_read_gated_at_the_publish_sequence_sees_the_write() {
    let server = fq_test_support::NatsServer::start();
    let bus = EventBus::connect(server.url()).await.expect("connect");

    let scratch = tempfile::tempdir().unwrap();
    let paths = RuntimeDbPaths::under(scratch.path());
    let store = Arc::new(
        ProjectionStore::open(&paths.projection)
            .await
            .expect("open projection store"),
    );
    // Views opens all three stores; initialise the other two files.
    drop(
        fq_runtime::control_plane::store::ControlPlaneStore::open(&paths.control_plane)
            .await
            .expect("init control-plane store"),
    );
    drop(
        fq_runtime::worker::store::WorkerStore::open(&paths.worker)
            .await
            .expect("init worker store"),
    );

    // The watermark pair: consumer advances, reads wait.
    let (watermark_tx, watermark) = watermark::channel();
    let consumer = ProjectionConsumer::new(bus.clone(), store.clone()).with_watermark(watermark_tx);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move { consumer.run(shutdown_rx).await });

    // Publish returns the event-log coordinate — monotonic per
    // publish, exactly what a command receipt will carry.
    let mut last_seq = 0;
    for i in 0..5 {
        let seq = bus.publish(&completed("wm-test")).await.expect("publish");
        assert!(seq > last_seq, "publish {i}: sequence must increase");
        last_seq = seq;
    }

    // The gated read: wait until the fold includes the last publish,
    // then the projection MUST hold all five rows — no sleep, no
    // polling, the watermark is the synchronisation.
    let views = Views::open(&paths)
        .await
        .expect("open views")
        .with_watermark(watermark.clone());
    views
        .at_watermark(Some(last_seq), Duration::from_secs(10))
        .await
        .expect("watermark reaches the published sequence");
    let count = views.event_count().await.expect("count");
    assert!(
        count >= 5,
        "the fold at watermark {last_seq} must include all 5 published events, saw {count}"
    );

    // A coordinate beyond anything published reports lag with both
    // positions, bounded — never a hang.
    let err = views
        .at_watermark(Some(last_seq + 1000), Duration::from_millis(200))
        .await
        .expect_err("unreachable watermark must time out");
    assert!(
        matches!(
            err,
            ViewsError::Watermark(watermark::WatermarkError::Lag { wanted, .. })
                if wanted == last_seq + 1000
        ),
        "got {err:?}"
    );

    // min_seq without an attached watermark refuses honestly (the
    // direct-CLI read path).
    let bare = Views::open(&paths).await.expect("open bare views");
    let err = bare
        .at_watermark(Some(1), Duration::from_millis(50))
        .await
        .expect_err("no watermark on this path");
    assert!(
        matches!(err, ViewsError::WatermarkUnavailable),
        "got {err:?}"
    );

    // And no min_seq is always free, on any path.
    bare.at_watermark(None, Duration::from_millis(1))
        .await
        .expect("ungated read");

    let _ = shutdown_tx.send(());
    handle.await.expect("join").expect("clean consumer exit");
}

/// The ultrareview's counterexample (bug_001), now a regression test:
/// a transient failure NAKs one mid-run sequence. Without
/// resolved-contiguous delivery a later sequence would leapfrog it,
/// advance the mark, and release readers into the gap. Under
/// `strict_order` the server redelivers the NAK'd sequence before
/// anything later, so when the mark reaches S, everything at or
/// below S has applied — proven here by injecting the failure and
/// asserting both the redelivery order and the contiguity of the
/// applied set at release time.
#[tokio::test]
async fn a_transient_failure_never_lets_the_watermark_expose_a_gap() {
    use std::collections::BTreeSet;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use fq_runtime::control_plane::durable_consumer::{
        DeliverFrom, DurableConsumerConfig, HandlerError, run_durable_consumer,
    };

    let server = fq_test_support::NatsServer::start();
    let bus = EventBus::connect(server.url()).await.expect("connect");

    // Five events; the middle one will fail its first delivery.
    let mut seqs = Vec::new();
    for _ in 0..5 {
        seqs.push(bus.publish(&completed("wm-gap")).await.expect("publish"));
    }
    let victim = seqs[2];
    let last = *seqs.last().unwrap();

    let (watermark_tx, watermark) = watermark::channel();
    let watermark_tx = Arc::new(watermark_tx);
    let applied: Arc<Mutex<BTreeSet<u64>>> = Arc::new(Mutex::new(BTreeSet::new()));
    let delivered: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let failed_once = Arc::new(AtomicBool::new(false));

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn({
        let (tx, applied, delivered, failed_once) = (
            watermark_tx.clone(),
            applied.clone(),
            delivered.clone(),
            failed_once.clone(),
        );
        let bus = bus.clone();
        async move {
            let config = DurableConsumerConfig {
                durable_name: "fq-projector-gap-test".to_string(),
                filter_subjects: Vec::new(),
                deliver_from: DeliverFrom::Beginning,
                strict_order: true,
            };
            run_durable_consumer(&bus, config, shutdown_rx, |delivery| {
                let seq = delivery.stream_seq.expect("jetstream metadata");
                let (tx, applied, delivered, failed_once) = (
                    tx.clone(),
                    applied.clone(),
                    delivered.clone(),
                    failed_once.clone(),
                );
                async move {
                    delivered.lock().unwrap().push(seq);
                    // Mirror the projection handler: fail transiently
                    // once at the victim, otherwise apply-then-advance.
                    if seq == victim && !failed_once.swap(true, Ordering::SeqCst) {
                        return Err(HandlerError::transient(std::io::Error::other(
                            "injected transient store failure",
                        )));
                    }
                    applied.lock().unwrap().insert(seq);
                    tx.advance(seq);
                    Ok(())
                }
            })
            .await
        }
    });

    // Released at the victim's sequence => the victim (and everything
    // below it) has applied. This is the exact read the counterexample
    // released into a gap.
    watermark
        .wait_for(victim, Duration::from_secs(10))
        .await
        .expect("watermark reaches the victim after redelivery");
    {
        let applied = applied.lock().unwrap();
        for seq in seqs.iter().filter(|s| **s <= victim) {
            assert!(
                applied.contains(seq),
                "released at {victim} but sequence {seq} is not applied: {applied:?}"
            );
        }
    }

    // And the mechanism: after the NAK, the very next delivery is the
    // victim's retry — nothing later leapfrogs it.
    watermark
        .wait_for(last, Duration::from_secs(10))
        .await
        .expect("the run completes");
    {
        let delivered = delivered.lock().unwrap();
        let first_victim = delivered.iter().position(|s| *s == victim).unwrap();
        assert_eq!(
            delivered.get(first_victim + 1),
            Some(&victim),
            "strict order must redeliver the NAK'd sequence next, saw {delivered:?}"
        );
        assert_eq!(applied.lock().unwrap().len(), 5, "all five applied");
    }

    let _ = shutdown_tx.send(());
    handle.await.expect("join").expect("clean exit");
}
