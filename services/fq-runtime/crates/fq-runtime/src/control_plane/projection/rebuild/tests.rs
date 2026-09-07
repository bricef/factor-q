//! The rebuild against a real broker: the replay that follows a
//! version bump backfills history, and the supervisor rebuilds under a
//! running consumer without losing what comes after.
//!
//! Same-version replays only. What a rebuild does with an event whose
//! envelope version this build does not support is the consumer's
//! parse policy, not the rebuild's, and is proved where that policy
//! lives.

use std::sync::Arc;
use std::time::Duration;

use tempfile::tempdir;
use uuid::Uuid;

use super::*;
use crate::agent::AgentId;
use crate::control_plane::projection::store::{EventFilter, PROJECTION_SCHEMA_VERSION};
use crate::events::{
    AssistantPart, ConfigSnapshot, CostMetadata, Event, EventPayload, LlmCallOrigin,
    LlmResponsePayload, SandboxSnapshot, StopReason, TokenUsage, TriggerSource, TriggeredPayload,
};

fn aid(s: &str) -> AgentId {
    AgentId::new(s).expect("test agent id must be valid")
}

fn unique_agent() -> String {
    format!("rebuild-{}", Uuid::now_v7().simple())
}

fn triggered(agent: &str, inv: Uuid) -> Event {
    Event::new(
        aid(agent),
        inv,
        EventPayload::Triggered(TriggeredPayload {
            trigger_id: None,
            trigger_source: TriggerSource::Manual,
            trigger_subject: None,
            trigger_payload: serde_json::json!({}),
            config_snapshot: ConfigSnapshot {
                name: agent.to_string(),
                model: "claude-haiku-4-5".to_string(),
                system_prompt: "test".to_string(),
                tools: vec![],
                sandbox: SandboxSnapshot::default(),
                budget: None,
                ..Default::default()
            },
        }),
    )
}

/// A priced call whose provider reported a thought-versus-spoken
/// split — the column #626 left NULL for history, and the live example
/// of what a rebuild backfills.
fn llm_response(agent: &str, inv: Uuid, reasoning_tokens: Option<u32>) -> Event {
    Event::new(
        aid(agent),
        inv,
        EventPayload::LlmResponse(LlmResponsePayload {
            round: 0,
            origin: LlmCallOrigin::AgentTurn,
            call_id: Uuid::now_v7(),
            parts: vec![AssistantPart::Text {
                text: "ok".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 20,
                cache_write_tokens: 10,
                reasoning_tokens,
            },
        }),
    )
    .with_cost(CostMetadata {
        call_id: Uuid::now_v7(),
        model: "kimi-k3".to_string(),
        input_tokens: 100,
        output_tokens: 50,
        cache_read_tokens: 20,
        cache_write_tokens: 10,
        input_cost: 0.0001,
        output_cost: 0.00025,
        total_cost: 0.02,
        cumulative_invocation_cost: 0.02,
        cumulative_agent_cost: 0.02,
        origin: LlmCallOrigin::AgentTurn,
        reasoning_tokens,
    })
}

async fn agent_rows(store: &ProjectionStore, agent: &str) -> usize {
    store
        .query_events(
            &EventFilter {
                agent: Some(agent),
                ..Default::default()
            },
            100,
        )
        .await
        .unwrap()
        .len()
}

async fn reasoning_of(store: &ProjectionStore, inv: Uuid) -> Option<Option<i64>> {
    store
        .cost_of_invocation(&inv.to_string())
        .await
        .unwrap()
        .map(|cost| cost.total_reasoning_tokens)
}

/// Run the real projection consumer — durable `fq-projector`, the
/// reset-on-pending path included — until `done` holds, then stop it.
async fn project_until<F, Fut>(bus: &EventBus, store: &Arc<ProjectionStore>, done: F)
where
    F: Fn(Arc<ProjectionStore>) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let consumer = ProjectionConsumer::new(bus.clone(), store.clone());
    let handle = tokio::spawn(consumer.run(shutdown_rx));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !done(store.clone()).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the projection consumer did not reach the expected state in time"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("consumer stops on shutdown")
        .expect("consumer task")
        .expect("consumer exits clean");
}

/// Decision 5, end to end. Events are projected into a file at version
/// N through the real consumer; a column is NULL for that history (as
/// `reasoning_tokens` was for every row written before #626); the
/// build is bumped to N+1 by stamping the file N; the reopen rebuilds;
/// the consumer resets its durable and replays; and the row carries
/// the event's value where it had NULL.
#[tokio::test]
async fn a_version_bump_rebuilds_and_the_replay_backfills_history() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let dir = tempdir().unwrap();
    let path = dir.path().join("projection.db");
    let agent = unique_agent();
    let inv = Uuid::now_v7();
    let response = llm_response(&agent, inv, Some(45));
    let response_id = response.envelope.event_id.to_string();
    bus.publish(&triggered(&agent, inv)).await.unwrap();
    bus.publish(&response).await.unwrap();

    // Version N, populated by the real consumer.
    let store = Arc::new(ProjectionStore::open(&path).await.unwrap());
    let a = agent.clone();
    project_until(&bus, &store, |store| {
        let a = a.clone();
        async move { agent_rows(&store, &a).await >= 2 }
    })
    .await;
    assert_eq!(reasoning_of(&store, inv).await, Some(Some(45)));
    // A fresh file on a fresh broker found no durable: nothing to
    // record.
    assert!(store.rebuild_record().await.unwrap().is_none());
    assert!(store.consumer_reset_pending().await.unwrap().is_none());

    // The version-N shape: the column was NULL for history. Written
    // behind the store's back, as an older build would have.
    drop(store);
    let older = sqlx::SqlitePool::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    sqlx::query("UPDATE events SET reasoning_tokens = NULL WHERE event_id = ?")
        .bind(&response_id)
        .execute(&older)
        .await
        .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "PRAGMA user_version = {}",
        PROJECTION_SCHEMA_VERSION - 1
    )))
    .execute(&older)
    .await
    .unwrap();
    older.close().await;

    // N+1 opens the file: rebuilt, and waiting on the consumer.
    let store = Arc::new(ProjectionStore::open(&path).await.unwrap());
    let record = store.rebuild_record().await.unwrap().expect("rebuilt");
    assert_eq!(record.from_version, Some(PROJECTION_SCHEMA_VERSION - 1));
    assert!(store.consumer_reset_pending().await.unwrap().is_some());
    assert_eq!(
        agent_rows(&store, &agent).await,
        1,
        "only the cost-bearing row survives the drop"
    );
    assert_eq!(
        reasoning_of(&store, inv).await,
        Some(None),
        "carried across with the shape it had: still NULL before the replay"
    );

    // The consumer resets the durable and replays from the beginning:
    // the plain event is back and the cost row carries the event's
    // value.
    let a = agent.clone();
    project_until(&bus, &store, |store| {
        let a = a.clone();
        async move {
            agent_rows(&store, &a).await >= 2 && reasoning_of(&store, inv).await == Some(Some(45))
        }
    })
    .await;

    let status = rebuild_status(&store, &bus)
        .await
        .unwrap()
        .expect("recorded");
    assert!(
        !status.consumer_reset_pending,
        "the consumer reset the durable"
    );
    assert_eq!(status.from_version, Some(PROJECTION_SCHEMA_VERSION - 1));
    assert_eq!(status.schema_version, PROJECTION_SCHEMA_VERSION);
    let target = status
        .target_seq
        .expect("the reset records the replay target");
    assert!(
        target >= 2,
        "the target is the stream's last sequence, got {target}"
    );
    // Acks land after the row does; the replay is complete once the
    // durable's floor has reached the target.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let status = rebuild_status(&store, &bus).await.unwrap().unwrap();
        if !status.in_progress {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the replay never reported complete: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The operator path: a rebuild requested through the handle while the
/// supervisor runs the consumer. The consumer is stopped, the file
/// rebuilt, the durable reset, and the consumer started again — so the
/// replay brings the history back and a later event still lands.
#[tokio::test]
async fn the_supervisor_rebuilds_on_request_and_keeps_projecting() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let dir = tempdir().unwrap();
    let store = Arc::new(
        ProjectionStore::open(&dir.path().join("projection.db"))
            .await
            .unwrap(),
    );
    let agent = unique_agent();
    let first = Uuid::now_v7();
    bus.publish(&llm_response(&agent, first, Some(3)))
        .await
        .unwrap();

    let (watermark_tx, watermark) = crate::watermark::channel();
    let supervisor =
        ProjectionSupervisor::new(bus.clone(), store.clone()).with_watermark(watermark_tx);
    let handle = supervisor.rebuild_handle();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(supervisor.run(shutdown_rx));

    let wait_for_rows = |n: usize| {
        let store = store.clone();
        let agent = agent.clone();
        async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
            while agent_rows(&store, &agent).await < n {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "expected {n} rows for {agent}"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    };
    wait_for_rows(1).await;
    let mark_before = watermark.current();

    let rebuild = handle
        .rebuild(Some("backfill check".to_string()))
        .await
        .expect("the supervisor rebuilds");
    assert!(rebuild.reason.contains("backfill check"), "{rebuild:?}");
    assert!(!rebuild.consumer_reset_pending, "answered after the reset");
    assert!(rebuild.target_seq.is_some());
    assert_eq!(
        rebuild.from_version, None,
        "an operator rebuild has no version to come from"
    );
    assert_eq!(
        store.rebuild_record().await.unwrap().unwrap().reason,
        rebuild.reason
    );

    // The consumer is running again: the replay re-derives the first
    // row and a new event lands after it. The mark never regressed.
    let second = Uuid::now_v7();
    bus.publish(&triggered(&agent, second)).await.unwrap();
    wait_for_rows(2).await;
    assert!(
        watermark.current() >= mark_before,
        "a replay never regresses the mark"
    );

    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("supervisor stops on shutdown")
        .expect("supervisor task")
        .expect("supervisor exits clean");
}

/// A handle with no supervisor behind it says so rather than hanging.
#[tokio::test]
async fn a_detached_handle_refuses() {
    let err = ProjectionRebuildHandle::detached()
        .rebuild(None)
        .await
        .unwrap_err();
    assert!(matches!(err, RebuildError::NoSupervisor), "{err:?}");
}
