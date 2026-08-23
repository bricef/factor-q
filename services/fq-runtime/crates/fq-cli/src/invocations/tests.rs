use super::*;
use fq_runtime::events::{Event, EventPayload};
use fq_runtime::{ControlPlaneStore, ProjectionStore};
use futures::StreamExt;
use uuid::Uuid;

#[test]
fn format_invocation_list_row_human_renders_short_id_and_truncated_fields() {
    let item = fq_ops::views::InvocationSummaryView {
        invocation_id: "019e3b328fd47de1aae0bb91bb24528d".to_string(),
        agent_id: Some("a".repeat(40)),
        worker_id: "worker-42".to_string(),
        status: "ambiguous".to_string(),
        assigned_at_ms: 1_700_000_000_000,
        started_at_ms: 1_700_000_000_000,
        archived: false,
        summary: None,
    };
    let line = format_invocation_list_row_human(&item);
    assert!(line.starts_with("019e3b32"), "expected 8-char id prefix");
    assert!(line.contains("ambiguous"));
    assert!(line.contains("worker-42"));
    assert!(line.contains("no"));
    // Agent string was truncated to 22 chars.
    assert!(line.contains(&"a".repeat(22)));
    assert!(!line.contains(&"a".repeat(23)));
}

/// #216: the summary line rides last, truncated char-safe; absent
/// renders an em-dash.
#[test]
fn format_invocation_list_row_human_renders_summary_last() {
    let mut item = fq_ops::views::InvocationSummaryView {
        invocation_id: "019e3b328fd47de1aae0bb91bb24528d".to_string(),
        agent_id: Some("m0-issue-fix".to_string()),
        worker_id: "w".to_string(),
        status: "in_flight".to_string(),
        assigned_at_ms: 0,
        started_at_ms: 0,
        archived: false,
        summary: Some("Fixing #7: editing widget.rs".to_string()),
    };
    let line = format_invocation_list_row_human(&item);
    assert!(
        line.ends_with("Fixing #7: editing widget.rs"),
        "got: {line}"
    );

    item.summary = Some("x".repeat(200));
    let line = format_invocation_list_row_human(&item);
    assert!(line.ends_with('…'), "truncated: {line}");
    assert!(line.chars().count() < 150, "bounded: {line}");

    item.summary = None;
    let line = format_invocation_list_row_human(&item);
    assert!(line.ends_with('—'), "fallback dash: {line}");
}

#[test]
fn format_invocation_list_row_human_marks_archived() {
    let item = fq_ops::views::InvocationSummaryView {
        invocation_id: "inv".to_string(),
        agent_id: Some("a".to_string()),
        worker_id: String::new(),
        status: "completed".to_string(),
        assigned_at_ms: 0,
        started_at_ms: 0,
        archived: true,
        summary: None,
    };
    let line = format_invocation_list_row_human(&item);
    // The archived flag sits before the (now trailing) summary
    // column (#216).
    assert!(
        line.contains(" yes "),
        "archived flag should be 'yes', got: {line:?}"
    );
}

/// The write behind `invocation.drop`, exercised where the CLI's
/// wrapper used to be. The wrapper went with the verb's flip to
/// the edge (plan Phase 4, verb 18); the behaviour it encoded —
/// which agent the event is attributed to, what an agent-less
/// owner row does, and that an unknown id publishes nothing — is
/// the daemon's contract now, so these drive
/// `operator::drop_invocation` directly.
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

/// The `--json` list shape is an operator contract: the swap from the
/// CLI-local struct to `views::InvocationSummaryView` (#105 layer 1)
/// must not move these fields.
#[test]
fn invocation_summary_view_serialises_to_stable_json_shape() {
    let item = fq_ops::views::InvocationSummaryView {
        invocation_id: "inv-1".to_string(),
        agent_id: Some("agent-1".to_string()),
        worker_id: "worker-1".to_string(),
        status: "in_flight".to_string(),
        assigned_at_ms: 42,
        started_at_ms: 41,
        archived: false,
        summary: None,
    };
    let v = serde_json::to_value(&item).unwrap();
    assert_eq!(v["invocation_id"], "inv-1");
    assert_eq!(v["agent_id"], "agent-1");
    assert_eq!(v["worker_id"], "worker-1");
    assert_eq!(v["status"], "in_flight");
    assert_eq!(v["assigned_at_ms"], 42);
    assert_eq!(v["started_at_ms"], 41);
    assert_eq!(v["archived"], false);
}
