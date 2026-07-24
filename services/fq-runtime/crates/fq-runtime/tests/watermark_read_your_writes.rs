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
