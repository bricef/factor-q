//! The operator drop path, end to end against a real broker.
//!
//! Moved from `fq-cli` with the split: these drive
//! `control_plane::operator::drop_invocation` and open a NATS
//! connection, so they test the daemon, not the client that asks it.
//!
//! The behaviour they pin — which agent the event is attributed to,
//! what an agent-less owner row does, and that an unknown id publishes
//! nothing — was the CLI wrapper's contract until `invocation.drop`
//! flipped to the edge (plan Phase 4, verb 18). It is the daemon's now.

use fq_runtime::events::{Event, EventPayload};
use fq_runtime::{AgentId, ControlPlaneStore, EventBus, ProjectionStore};
use futures::StreamExt;
use uuid::Uuid;

#[tokio::test]
async fn drop_invocation_emits_operator_recovered_for_agent() {
    // NATS-gated end-to-end of the publish path: seed a
    // ProjectionStore with one event so the agent lookup
    // works, then call drop_invocation and capture the event
    // on the agent-scoped operator_recovered subject.
    let server = fq_test_support::NatsServer::start();
    let url = server.url().to_string();

    use fq_runtime::events::{EventPayload as EP, TriggerSource, TriggeredPayload};
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    let db_paths = fq_runtime::RuntimeDbPaths::under(dir.path());
    let proj_store = ProjectionStore::open(&db_paths.projection).await.unwrap();

    let agent_id = AgentId::new(format!("op-drop-cli-{}", Uuid::now_v7().simple())).unwrap();
    let invocation_id = Uuid::now_v7();

    // Seed one event so agent_id_for_invocation has something
    // to find. Pick triggered — the most representative
    // first event for an invocation.
    let seed = Event::new(
        agent_id.clone(),
        invocation_id,
        EP::Triggered(TriggeredPayload {
            trigger_id: None,
            trigger_source: TriggerSource::Manual,
            trigger_subject: None,
            trigger_payload: serde_json::Value::Null,
            config_snapshot: fq_runtime::Agent::builder()
                .id(agent_id.as_str())
                .model("claude-haiku")
                .system_prompt("test")
                .build()
                .unwrap()
                .to_snapshot(),
        }),
    );
    proj_store.insert_event(&seed, None).await.unwrap();

    let control_store = ControlPlaneStore::open(&db_paths.control_plane)
        .await
        .unwrap();
    let bus = EventBus::connect(&url).await.expect("connect NATS");
    let mut sub = bus
        .subscribe(format!(
            "fq.agent.{}.invocation.operator_recovered",
            agent_id.as_str()
        ))
        .await
        .expect("subscribe");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let result = fq_runtime::control_plane::operator::drop_invocation(
        &bus,
        &proj_store,
        &control_store,
        &invocation_id.to_string(),
        Some("test reason"),
        None,
    )
    .await
    .expect("drop_invocation");
    assert_eq!(result.agent_id, agent_id.as_str());
    assert_eq!(result.reason.as_deref(), Some("test reason"));

    let captured = tokio::time::timeout(std::time::Duration::from_secs(2), sub.next())
        .await
        .expect("event timeout")
        .expect("stream closed")
        .expect("deserialise");
    assert_eq!(captured.envelope.invocation_id, invocation_id);
    match &captured.payload {
        EventPayload::InvocationOperatorRecovered(p) => {
            assert_eq!(p.action, "drop");
            assert_eq!(p.final_phase, "failed");
            assert_eq!(p.reason.as_deref(), Some("test reason"));
        }
        other => panic!("expected InvocationOperatorRecovered, got {other:?}"),
    }
}

#[tokio::test]
async fn drop_invocation_removes_agentless_owner() {
    let server = fq_test_support::NatsServer::start();
    let url = server.url().to_string();
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    let db_paths = fq_runtime::RuntimeDbPaths::under(dir.path());
    let proj_store = ProjectionStore::open(&db_paths.projection).await.unwrap();
    let control_store = ControlPlaneStore::open(&db_paths.control_plane)
        .await
        .unwrap();
    let fake_inv = Uuid::now_v7().to_string();
    control_store
        .register_worker("orphan-worker", "test", 1)
        .await
        .unwrap();
    control_store
        .assign_invocation(&fake_inv, "orphan-worker", 1)
        .await
        .unwrap();
    let bus = EventBus::connect(&url).await.expect("connect NATS");

    let result = fq_runtime::control_plane::operator::drop_invocation(
        &bus,
        &proj_store,
        &control_store,
        &fake_inv,
        None,
        None,
    )
    .await
    .expect("agent-less owner should drop");
    assert_eq!(result.agent_id, "operator");
    assert!(
        control_store
            .get_invocation_owner(&fake_inv)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn drop_invocation_errors_when_nothing_known() {
    // No projection event *and* no coordination owner row: a truly
    // unknown id must still error rather than emit a phantom
    // operator-recovered event for something that never existed.
    let server = fq_test_support::NatsServer::start();
    let url = server.url().to_string();
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    let db_paths = fq_runtime::RuntimeDbPaths::under(dir.path());
    let proj_store = ProjectionStore::open(&db_paths.projection).await.unwrap();
    let control_store = ControlPlaneStore::open(&db_paths.control_plane)
        .await
        .unwrap();
    let bus = EventBus::connect(&url).await.expect("connect NATS");

    let fake_inv = Uuid::now_v7().to_string();
    let err = fq_runtime::control_plane::operator::drop_invocation(
        &bus,
        &proj_store,
        &control_store,
        &fake_inv,
        None,
        None,
    )
    .await
    .expect_err("unknown invocation should error");
    assert!(format!("{err}").contains("not found"), "got: {err}");
}
