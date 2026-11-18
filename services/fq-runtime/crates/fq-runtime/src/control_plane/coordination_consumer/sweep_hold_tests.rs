//! The consumer's tick sweeps, held until startup recovery has decided
//! (issue 1070).
//!
//! The daemon starts this consumer before its recovery. At that point the
//! roster still holds the stopped daemon's worker as alive with its
//! heartbeat long lapsed, and the WAL holds its runs in flight with their
//! last step long past: exactly what the two sweeps report. Held, they
//! report neither until the daemon releases them.

use super::*;
use crate::control_plane::liveness::StuckSweep;
use crate::test_support::nats::test_nats;
use crate::worker::store::{InvocationStateRow, WorkerStore};
use futures::{Stream, StreamExt};

/// A run of the stopped daemon, in flight in the WAL since epoch plus one
/// second: past any stuck threshold.
fn in_flight_since_long_ago(invocation_id: &str) -> InvocationStateRow {
    InvocationStateRow {
        invocation_id: invocation_id.to_string(),
        agent_id: "held-sweeps".to_string(),
        schema_version: 1,
        phase: "awaiting_model".to_string(),
        state_blob: b"{}".to_vec(),
        step_index: 0,
        started_at: 1_000,
        updated_at: 1_000,
        terminal_at: None,
        workspace_ref: None,
        archive_status: None,
        archive_published_at: None,
        trigger_source: None,
        trigger_subject: None,
        trigger_payload: None,
        terminal_outcome: None,
        terminal_published_at: None,
    }
}

/// The next report either sweep publishes: `worker.orphaned` or
/// `invocation.stuck`, by event type.
async fn next_report(
    events: &mut (impl Stream<Item = Result<Event, BusError>> + Unpin),
) -> &'static str {
    loop {
        let event = events
            .next()
            .await
            .expect("the subscription stays open")
            .expect("the event deserialises");
        match event.payload {
            EventPayload::WorkerOrphaned(_) => return "worker.orphaned",
            EventPayload::InvocationStuck(_) => return "invocation.stuck",
            _ => continue,
        }
    }
}

/// **Held, the tick sweeps nothing; released, the next tick sweeps.** A
/// consumer ticking every 20 ms over the stopped daemon's worker and run
/// reports neither for half a second while held, and leaves the worker
/// row alive. Once released, the next tick publishes `worker.orphaned` for
/// the worker and `invocation.stuck` for the run.
#[tokio::test]
async fn held_sweeps_report_nothing_until_released() {
    let server = test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        ControlPlaneStore::open(&dir.path().join("control-plane.db"))
            .await
            .unwrap(),
    );
    let worker_store = Arc::new(
        WorkerStore::open(&dir.path().join("worker.db"))
            .await
            .unwrap(),
    );
    let stopped_daemon = format!("stopped-daemon-{}", Uuid::now_v7().simple());
    store
        .register_worker(&stopped_daemon, "host", 1_000)
        .await
        .unwrap();
    worker_store
        .upsert_invocation_state(&in_flight_since_long_ago(&Uuid::now_v7().to_string()))
        .await
        .unwrap();

    let mut reports = bus.subscribe("fq.>").await.expect("subscribe");
    // The subscription reaches the server before anything is published.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (hold, release) = hold_sweeps();
    let consumer = CoordinationConsumer::new(bus.clone(), store.clone())
        .with_worker_store(worker_store.clone())
        .with_self_worker_id("the-daemon-starting".to_string())
        .with_stale_threshold_ms(1)
        .with_sweep_interval_ms(20)
        .with_stuck_sweep(Arc::new(StuckSweep::new(
            bus.clone(),
            worker_store.clone(),
            store.clone(),
            1,
        )))
        .with_sweeps_held(hold);
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(async move { consumer.run(stopped).await });

    let held = tokio::time::timeout(Duration::from_millis(500), next_report(&mut reports)).await;
    assert!(
        held.is_err(),
        "a held sweep reported {held:?} before recovery had decided"
    );
    assert_eq!(
        store
            .get_worker(&stopped_daemon)
            .await
            .unwrap()
            .map(|worker| worker.status),
        Some(WorkerStatus::Alive),
        "a held stale-worker sweep marks nothing"
    );

    release.release();
    let mut released = Vec::new();
    for _ in 0..2 {
        released.push(
            tokio::time::timeout(Duration::from_secs(5), next_report(&mut reports))
                .await
                .expect("the first tick after the release sweeps"),
        );
    }
    released.sort_unstable();
    assert_eq!(released, ["invocation.stuck", "worker.orphaned"]);

    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the consumer stops")
        .expect("its task joins")
        .expect("it exits cleanly");
}
