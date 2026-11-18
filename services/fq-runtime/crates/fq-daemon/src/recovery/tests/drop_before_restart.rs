//! A drop the coordination consumer had not folded when the daemon died,
//! and the next start (issue 1024).
//!
//! An operator drops a run while it is inside a tool call (`fq invocation
//! drop --live`): the drop is published at once, and the run halts at its
//! next step boundary. If the daemon dies before that boundary, and before
//! its coordination consumer has folded the drop, nothing records the drop
//! but the event on the stream: the worker's WAL row is still in flight,
//! with the tool handed off and no result, and a triggered run has no
//! ownership row. The next start waits for its coordination consumer to
//! fold the drop before recovery decides (issue 1070, `backlog_at_boot`
//! drives that start). When that wait runs out first, recovery finds the
//! run ambiguous and publishes `invocation.ambiguous`, which lands on the
//! stream after the drop; the consumer then folds the drop, and the report
//! after it. This test drives that order: recovery over an unfolded drop,
//! then the fold.

use super::*;
use fq_runtime::bus::STREAM_NAME;
use fq_runtime::control_plane::OwnerStatus;
use fq_runtime::events::{HostNoticePayload, InvocationOperatorRecoveredPayload, subjects};

#[tokio::test]
async fn a_drop_published_before_the_daemon_died_is_not_undone_by_the_next_start() {
    let server = fq_test_support::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let scratch = tempfile::tempdir().unwrap();
    let (cp_store, worker_store) = stores_under(scratch.path()).await;
    let agent = AgentId::new(CAPPED_AGENT).unwrap();
    let invocation = Uuid::now_v7();

    // The run as the dead daemon left it: in flight, inside a tool call.
    let mut row = first_wal_row(&invocation.to_string());
    row.agent_id = CAPPED_AGENT.to_string();
    worker_store.upsert_invocation_state(&row).await.unwrap();
    seed_a_crashed_tool_call(&worker_store, &invocation.to_string()).await;

    // The operator's drop, published before the daemon died, never folded.
    let drop_seq = bus
        .publish(&Event::new(
            agent.clone(),
            invocation,
            EventPayload::InvocationOperatorRecovered(InvocationOperatorRecoveredPayload {
                action: "drop".to_string(),
                final_phase: "failed".to_string(),
                reason: Some("abandoned mid tool call".to_string()),
            }),
        ))
        .await
        .expect("publish the drop");

    // The next start, its wait for the backlog run out: recovery over the
    // unfolded drop, in the order `assemble` runs it.
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
        "a run killed mid tool call is ambiguous: held for an operator, not resumed"
    );
    let report_seq = bus
        .jetstream()
        .get_stream(STREAM_NAME)
        .await
        .expect("the event stream")
        .get_last_raw_message_by_subject(&subjects::agent_invocation_ambiguous(CAPPED_AGENT))
        .await
        .expect("recovery reported the run ambiguous")
        .sequence;
    assert!(
        report_seq > drop_seq,
        "the report is on the stream after the drop ({report_seq} after {drop_seq})"
    );

    // Then the coordination consumer starts, and folds the stream.
    let (mark, coordination) = fq_runtime::watermark::channel();
    let consumer = fq_runtime::CoordinationConsumer::new(bus.clone(), cp_store.clone())
        .with_worker_store(worker_store.clone())
        .with_self_worker_id(worker_id.as_str().to_string())
        .with_watermark(mark);
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

    let owner = cp_store
        .get_invocation_owner(&invocation.to_string())
        .await
        .unwrap()
        .expect("the drop wrote the run's ownership row");
    assert_eq!(
        owner.status,
        OwnerStatus::Failed,
        "the run stays dropped: the report after the drop does not send it back to triage"
    );

    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the consumer stops")
        .expect("its task joins");
}
