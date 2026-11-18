//! A run that published its `failed` and died before marking its WAL row
//! terminal, and the next start (issue 1024).
//!
//! A run that fails part-way through a step publishes `failed` first and
//! marks its WAL row terminal after, so a lost `failed` leaves the row in
//! flight for recovery. When the `failed` did go out and the daemon died
//! before the mark, the coordination consumer folds the `failed` into the
//! run's ownership row. The next start's recovery reads that row before
//! it resumes anything (`reconcile_terminal_owners`): the run has ended,
//! its WAL row is marked terminal and handed to the archive sweeper, and
//! it is not resumed into a second ending.

use super::*;
use fq_runtime::control_plane::OwnerStatus;
use fq_runtime::events::{
    FailedPayload, FailureKind, FailurePhase, HostNoticePayload, InvocationTotals,
};

#[tokio::test]
async fn a_run_whose_failed_was_folded_is_not_resumed_when_its_wal_mark_was_lost() {
    let server = fq_test_support::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let scratch = tempfile::tempdir().unwrap();
    let (cp_store, worker_store) = stores_under(scratch.path()).await;
    let agent = AgentId::new(CAPPED_AGENT).unwrap();
    let invocation = Uuid::now_v7();

    // The run as the dead daemon left it: its WAL row in flight, its
    // `failed` on the stream.
    let mut row = first_wal_row(&invocation.to_string());
    row.agent_id = CAPPED_AGENT.to_string();
    worker_store.upsert_invocation_state(&row).await.unwrap();
    bus.publish(&Event::new(
        agent.clone(),
        invocation,
        EventPayload::Failed(FailedPayload {
            error_kind: FailureKind::LlmError,
            error_message: "the provider refused the request".to_string(),
            phase: FailurePhase::LlmRequest,
            partial_totals: InvocationTotals::default(),
            detail: None,
        }),
    ))
    .await
    .expect("publish the failed");

    // The dead daemon's coordination consumer folded it before the end.
    let (mark, coordination) = fq_runtime::watermark::channel();
    let consumer =
        fq_runtime::CoordinationConsumer::new(bus.clone(), cp_store.clone()).with_watermark(mark);
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(async move {
        consumer
            .run(stopped)
            .await
            .expect("the coordination consumer exits cleanly");
    });
    let barrier = bus
        .publish(&Event::new(
            agent,
            Uuid::now_v7(),
            EventPayload::HostNotice(HostNoticePayload {
                kind: "test_barrier".to_string(),
                body: String::new(),
            }),
        ))
        .await
        .expect("publish the barrier");
    coordination
        .wait_for(barrier, Duration::from_secs(10))
        .await
        .expect("the coordination consumer applies everything up to the barrier");
    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the consumer stops")
        .expect("its task joins");
    assert_eq!(
        cp_store
            .get_invocation_owner(&invocation.to_string())
            .await
            .unwrap()
            .map(|owner| owner.status),
        Some(OwnerStatus::Failed),
        "the ownership row follows the `failed`"
    );

    // The next start.
    let runtime_id = Uuid::now_v7();
    let worker_id = WorkerId::new(runtime_id.to_string()).unwrap();
    reconcile_terminal_owners(
        &worker_store,
        &cp_store,
        chrono::Utc::now().timestamp_millis(),
    )
    .await
    .expect("reconcile terminal owners");
    let recovered = classify_in_flight(&worker_store, &cp_store, &bus, runtime_id, &worker_id)
        .await
        .expect("startup recovery");
    assert!(
        recovered.recoverable.is_empty(),
        "the run ended; it is not resumed into a second ending"
    );
    let wal = worker_store
        .get_invocation_state(&invocation.to_string())
        .await
        .unwrap()
        .expect("the WAL row waits for its archive");
    assert_eq!(wal.phase, "failed");
    assert!(
        wal.terminal_at.is_some(),
        "the WAL row is marked terminal, so the archive sweeper hands it off"
    );
}
