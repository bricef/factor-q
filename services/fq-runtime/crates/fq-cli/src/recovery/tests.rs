use super::*;
use futures::StreamExt;
use std::time::Duration;

/// The #64 idempotency AC: a persistently-failing recovery
/// (re-classified or re-failed on every daemon restart) emits
/// `invocation.ambiguous` exactly once, not once per restart.
#[tokio::test]
async fn publish_ambiguous_once_fires_exactly_once_per_invocation() {
    let server = fq_test_support::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let dir = tempfile::tempdir().unwrap();
    let wstore = fq_runtime::WorkerStore::open(&dir.path().join("worker.db"))
        .await
        .expect("open worker store");

    let inv_id = Uuid::now_v7();
    let agent = format!("amb-once-{}", Uuid::now_v7().simple());
    wstore
        .upsert_invocation_state(&fq_runtime::worker::InvocationStateRow {
            invocation_id: inv_id.to_string(),
            agent_id: agent.clone(),
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
        })
        .await
        .unwrap();

    let mut sub = bus
        .subscribe(format!("fq.agent.{agent}.invocation.ambiguous"))
        .await
        .expect("subscribe");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let payload = || fq_runtime::events::InvocationAmbiguousPayload {
        stuck_entity: "recovery".to_string(),
        stuck_call_id: inv_id.to_string(),
        note: "resume failed (test)".to_string(),
    };
    let agent_id = AgentId::new(agent.clone()).unwrap();

    // First failure publishes…
    publish_ambiguous_once(
        &wstore,
        &bus,
        agent_id.clone(),
        inv_id,
        &inv_id.to_string(),
        payload(),
    )
    .await;
    let event = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("invocation.ambiguous within 5s")
        .expect("stream open")
        .expect("event deserialises");
    assert!(matches!(
        event.payload,
        EventPayload::InvocationAmbiguous(_)
    ));

    // …the second (same invocation, "next restart") is stamped out.
    publish_ambiguous_once(
        &wstore,
        &bus,
        agent_id,
        inv_id,
        &inv_id.to_string(),
        payload(),
    )
    .await;
    let quiet = tokio::time::timeout(Duration::from_millis(500), sub.next()).await;
    assert!(
        quiet.is_err(),
        "second failure must not re-publish invocation.ambiguous"
    );
}
