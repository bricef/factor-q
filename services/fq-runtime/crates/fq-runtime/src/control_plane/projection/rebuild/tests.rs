//! The rebuild against a real broker: the replay that follows a
//! version bump backfills history, the supervisor rebuilds under a
//! running consumer without losing what comes after, and — #648 — a
//! stream still holding history in a version this build does not read
//! is replayed from the floor, with the rows below it kept as they
//! were, rather than halting the projector.

use std::sync::Arc;
use std::time::Duration;

use tempfile::tempdir;
use uuid::Uuid;

use super::*;
use crate::agent::AgentId;
use crate::control_plane::projection::store::{
    EventFilter, EventLocation, EventRow, PROJECTION_SCHEMA_VERSION,
};
use crate::events::{
    AssistantPart, ConfigSnapshot, CostMetadata, Event, EventPayload, LlmCallOrigin,
    LlmResponsePayload, SandboxSnapshot, StopReason, TokenUsage, TriggerSource, TriggeredPayload,
};
use crate::test_support::corpus::{corpus_bytes, publish_corpus, publish_raw};

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
        reported_cost: None,
    })
}

async fn agent_rows(store: &ProjectionStore, agent: &str) -> usize {
    rows_of(store, agent).await.len()
}

async fn rows_of(store: &ProjectionStore, agent: &str) -> Vec<EventRow> {
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
}

async fn reasoning_of(store: &ProjectionStore, inv: Uuid) -> Option<Option<i64>> {
    store
        .cost_of_invocation(&inv.to_string())
        .await
        .unwrap()
        .map(|cost| cost.total_reasoning_tokens)
}

/// The projector's halt, if it has halted (#409).
fn halted(bus: &EventBus) -> Option<fq_ops::health::UnsupportedEvent> {
    bus.consumer_ledger().record(CONSUMER_NAME).halted_on
}

/// Run the real projection consumer — durable `fq-projector`, the
/// reset-on-pending path included — until `done` holds, then stop it.
/// A halt while waiting fails at once: a halt is a verdict, not a
/// delay, and the wait would otherwise run out with nothing said.
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
        if let Some(on) = halted(bus) {
            panic!("the projector halted on {on:?} instead of replaying from the floor");
        }
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

/// Wait for the rebuild record to report the replay complete.
async fn wait_until_complete(store: &ProjectionStore, bus: &EventBus) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let status = rebuild_status(store, bus).await.unwrap().unwrap();
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

/// Write the version-N shape behind the store's back, as an older
/// build would have: the column NULL for history, the file stamped N.
async fn age_the_file(path: &std::path::Path, null_reasoning_for: &str) {
    let older = sqlx::SqlitePool::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    sqlx::query("UPDATE events SET reasoning_tokens = NULL WHERE event_id = ?")
        .bind(null_reasoning_for)
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
}

/// Decision 5 of #645, end to end. Events are projected into a file at
/// version N through the real consumer; a column is NULL for that
/// history (as `reasoning_tokens` was for every row written before
/// #626); the build is bumped to N+1 by stamping the file N; the
/// reopen rebuilds with every row carried; the consumer finds the
/// floor (the first sequence — every message is readable), resets its
/// durable and replays; and the row carries the event's value where it
/// had NULL.
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
    let first_seq = bus.publish(&triggered(&agent, inv)).await.unwrap();
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

    drop(store);
    age_the_file(&path, &response_id).await;

    // N+1 opens the file: rebuilt with every row carried, and waiting
    // on the consumer.
    let store = Arc::new(ProjectionStore::open(&path).await.unwrap());
    let record = store.rebuild_record().await.unwrap().expect("rebuilt");
    assert_eq!(record.from_version, Some(PROJECTION_SCHEMA_VERSION - 1));
    assert!(store.consumer_reset_pending().await.unwrap().is_some());
    assert_eq!(
        agent_rows(&store, &agent).await,
        2,
        "every row survives the rebuild at open"
    );
    assert_eq!(
        reasoning_of(&store, inv).await,
        Some(None),
        "carried across with the shape it had: still NULL before the replay"
    );

    // The consumer resets the durable and replays from the floor —
    // here the first sequence: the plain event is re-derived and the
    // cost row carries the event's value.
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
    let record = store.rebuild_record().await.unwrap().unwrap();
    assert_eq!(
        record.floor_seq,
        Some(first_seq),
        "a stream this build reads whole floors at its first sequence"
    );
    assert_eq!(
        record.carried_below_floor, 0,
        "nothing lies below the first sequence"
    );
    // Acks land after the row does; the replay is complete once the
    // durable's floor has reached the target.
    wait_until_complete(&store, &bus).await;
}

/// The issue (#648), end to end on the pinned broker. The stream holds
/// v2 history — the #409 corpus, published raw — followed by current
/// events; a file at version N indexes both ranges: rows for the v2
/// range that an older build projected and nothing can re-derive, and
/// rows for the current range in the version-N shape; the build is
/// bumped to N+1; the reopen rebuilds. The replay must start at the
/// floor: the projector does not halt, the rows below the floor are
/// present and unchanged, the rows at or above it are re-derived
/// (`reasoning_tokens` NULL to the event's value), and the record
/// names the floor and what it carried.
#[tokio::test]
async fn a_rebuild_replays_from_the_floor_and_carries_the_rows_below_it() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let dir = tempdir().unwrap();
    let path = dir.path().join("projection.db");

    // v2 history first, then current events.
    let older = publish_corpus(&bus, "v2").await;
    let agent = unique_agent();
    let inv = Uuid::now_v7();
    let response = llm_response(&agent, inv, Some(45));
    let response_id = response.envelope.event_id.to_string();
    let plain_seq = bus.publish(&triggered(&agent, inv)).await.unwrap();
    let response_seq = bus.publish(&response).await.unwrap();
    assert_eq!(plain_seq, older.last().unwrap() + 1);

    // Version N, populated by the real consumer. A fresh file on a
    // stream that starts unreadable already replays from the floor —
    // a start at the beginning would halt right here.
    let store = Arc::new(ProjectionStore::open(&path).await.unwrap());
    let a = agent.clone();
    project_until(&bus, &store, |store| {
        let a = a.clone();
        async move { agent_rows(&store, &a).await >= 2 }
    })
    .await;
    assert_eq!(reasoning_of(&store, inv).await, Some(Some(45)));

    // The rows an older build projected from the v2 range, which
    // nothing can re-derive: one plain, one priced, at their positions.
    let older_inv = Uuid::now_v7();
    let older_plain = triggered(&agent, older_inv);
    let older_priced = llm_response(&agent, older_inv, Some(7));
    store
        .insert_event(&older_plain, Some(older[0]))
        .await
        .unwrap();
    store
        .insert_event(&older_priced, Some(older[1]))
        .await
        .unwrap();
    let below_floor_before: Vec<EventRow> = rows_of(&store, &agent)
        .await
        .into_iter()
        .filter(|row| row.invocation_id == older_inv.to_string())
        .collect();
    assert_eq!(below_floor_before.len(), 2);
    drop(store);
    age_the_file(&path, &response_id).await;

    // N+1 opens the file: rebuilt, every row carried, waiting on the
    // consumer.
    let store = Arc::new(ProjectionStore::open(&path).await.unwrap());
    assert!(store.consumer_reset_pending().await.unwrap().is_some());
    assert_eq!(agent_rows(&store, &agent).await, 4);
    assert_eq!(reasoning_of(&store, inv).await, Some(None));
    assert_eq!(
        store.rebuild_record().await.unwrap().unwrap().floor_seq,
        None,
        "the floor is found by the consumer, not at open"
    );

    // The consumer finds the floor, drops what the replay re-derives,
    // resets the durable at the floor, and replays — without halting.
    let a = agent.clone();
    project_until(&bus, &store, |store| {
        let a = a.clone();
        async move {
            reasoning_of(&store, inv).await == Some(Some(45)) && agent_rows(&store, &a).await >= 4
        }
    })
    .await;
    assert_eq!(
        halted(&bus),
        None,
        "the projector must not halt on the v2 history below the floor"
    );

    // Below the floor: present and unchanged, positions included.
    let below_floor_after: Vec<EventRow> = rows_of(&store, &agent)
        .await
        .into_iter()
        .filter(|row| row.invocation_id == older_inv.to_string())
        .collect();
    assert_eq!(
        below_floor_after, below_floor_before,
        "rows below the floor are carried as they were"
    );
    assert_eq!(
        store
            .event_location(&older_plain.envelope.event_id.to_string())
            .await
            .unwrap(),
        EventLocation::At(older[0])
    );
    assert_eq!(reasoning_of(&store, older_inv).await, Some(Some(7)));

    // At or above the floor: re-derived from the stream.
    assert_eq!(
        store.event_location(&response_id).await.unwrap(),
        EventLocation::At(response_seq)
    );

    // The record names the floor and what it carried, and the rebuild
    // completes.
    let record = store.rebuild_record().await.unwrap().unwrap();
    assert_eq!(record.floor_seq, Some(plain_seq), "the first v3 message");
    assert_eq!(record.carried_below_floor, 2, "the two older rows");
    assert_eq!(record.target_seq, Some(response_seq));
    wait_until_complete(&store, &bus).await;
}

/// #645's crash-safety, kept across the floor step: a start that died
/// between the floor step and the reset leaves the reset note in
/// place, so the next start redoes the floor step from the note and
/// then the reset, and the file ends where an uninterrupted start
/// would have left it.
#[tokio::test]
async fn a_crash_between_the_floor_step_and_the_reset_is_redone_from_the_note() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let dir = tempdir().unwrap();
    let path = dir.path().join("projection.db");

    let older = publish_corpus(&bus, "v2").await;
    let agent = unique_agent();
    let inv = Uuid::now_v7();
    let response = llm_response(&agent, inv, Some(45));
    let response_id = response.envelope.event_id.to_string();
    let plain_seq = bus.publish(&triggered(&agent, inv)).await.unwrap();
    bus.publish(&response).await.unwrap();

    let store = Arc::new(ProjectionStore::open(&path).await.unwrap());
    let a = agent.clone();
    project_until(&bus, &store, |store| {
        let a = a.clone();
        async move { agent_rows(&store, &a).await >= 2 }
    })
    .await;
    let older_inv = Uuid::now_v7();
    store
        .insert_event(&llm_response(&agent, older_inv, Some(7)), Some(older[0]))
        .await
        .unwrap();
    drop(store);
    age_the_file(&path, &response_id).await;
    let store = Arc::new(ProjectionStore::open(&path).await.unwrap());

    // The first half alone, as a start that died right after it: the
    // plain row at the floor is dropped, the floor is on its own key,
    // the note and the durable from the version-N run are untouched.
    let (floor, carried) = apply_replay_floor(&bus, &store).await.unwrap();
    assert_eq!(floor.floor, plain_seq);
    assert_eq!(carried, 1);
    assert_eq!(agent_rows(&store, &agent).await, 2);
    assert!(
        store.consumer_reset_pending().await.unwrap().is_some(),
        "the note outlives the floor step"
    );
    assert_eq!(
        store.pending_replay_floor().await.unwrap(),
        Some(crate::control_plane::projection::store::PendingFloor {
            floor_seq: plain_seq,
            carried_below_floor: 1
        })
    );
    assert!(
        store
            .rebuild_record()
            .await
            .unwrap()
            .unwrap()
            .floor_seq
            .is_none(),
        "the record is completed by the reset, not the floor step"
    );

    // The next start: the note says reset, so the floor step is redone
    // (nothing more to drop, the same floor recorded), the durable is
    // deleted and recreated at the floor, and the replay completes.
    let a = agent.clone();
    project_until(&bus, &store, |store| {
        let a = a.clone();
        async move {
            reasoning_of(&store, inv).await == Some(Some(45)) && agent_rows(&store, &a).await >= 3
        }
    })
    .await;
    assert_eq!(halted(&bus), None);
    assert!(store.consumer_reset_pending().await.unwrap().is_none());
    assert!(store.pending_replay_floor().await.unwrap().is_none());
    let record = store.rebuild_record().await.unwrap().unwrap();
    assert_eq!(record.floor_seq, Some(plain_seq));
    assert_eq!(record.carried_below_floor, 1);
    assert_eq!(reasoning_of(&store, older_inv).await, Some(Some(7)));
    wait_until_complete(&store, &bus).await;
}

/// A stream this build reads none of: the rebuild has nothing to
/// replay and says so — the floor is past the target, and the record
/// reports complete as soon as the reset is — every row is kept, and
/// the projector, waiting at the floor rather than halted, projects
/// the next current event when it comes.
#[tokio::test]
async fn a_stream_with_nothing_readable_replays_nothing_and_projects_what_comes_next() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let dir = tempdir().unwrap();
    let path = dir.path().join("projection.db");
    let older = publish_corpus(&bus, "v2").await;
    let last_older = *older.last().unwrap();

    // A file at version N holding one row an older build projected.
    let agent = unique_agent();
    let older_inv = Uuid::now_v7();
    let store = ProjectionStore::open(&path).await.unwrap();
    store.consumer_reset_done(last_older, false).await.unwrap();
    store
        .insert_event(&llm_response(&agent, older_inv, Some(7)), Some(older[0]))
        .await
        .unwrap();
    drop(store);
    age_the_file(&path, "no such row").await;

    let store = Arc::new(ProjectionStore::open(&path).await.unwrap());
    project_until(&bus, &store, |store| async move {
        store.consumer_reset_pending().await.unwrap().is_none()
    })
    .await;
    assert_eq!(halted(&bus), None);
    let record = store.rebuild_record().await.unwrap().unwrap();
    assert_eq!(record.floor_seq, Some(last_older + 1), "past the last");
    assert_eq!(record.target_seq, Some(last_older));
    assert_eq!(record.carried_below_floor, 1);
    let status = rebuild_status(&store, &bus).await.unwrap().unwrap();
    assert!(
        !status.in_progress,
        "nothing to replay is complete at once: {status:?}"
    );
    assert_eq!(agent_rows(&store, &agent).await, 1);
    assert_eq!(reasoning_of(&store, older_inv).await, Some(Some(7)));

    // What comes next is projected from the floor.
    let inv = Uuid::now_v7();
    bus.publish(&triggered(&agent, inv)).await.unwrap();
    let a = agent.clone();
    project_until(&bus, &store, |store| {
        let a = a.clone();
        async move { agent_rows(&store, &a).await >= 2 }
    })
    .await;
}

/// #409's rule is untouched by the floor: a version this build does
/// not read *after* the floor still halts the replay, unacked, and
/// says so. Eight current events, a v2 one, a current one: the floor
/// is the first sequence — the search halves down from the middle and
/// every position it probes is readable, so the v2 message past them
/// is never seen — and the replay stops at it.
#[tokio::test]
async fn an_unreadable_version_after_the_floor_still_halts_the_replay() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let dir = tempdir().unwrap();
    let store = Arc::new(
        ProjectionStore::open(&dir.path().join("projection.db"))
            .await
            .unwrap(),
    );
    let agent = unique_agent();
    let inv = Uuid::now_v7();
    let first = triggered(&agent, inv);
    let first_seq = bus.publish(&first).await.unwrap();
    for _ in 0..7 {
        bus.publish(&triggered(&agent, Uuid::now_v7()))
            .await
            .unwrap();
    }
    let (_, v2) = corpus_bytes("v2").swap_remove(0);
    let older_seq = publish_raw(&bus, v2).await;
    assert_eq!(older_seq, first_seq + 8);
    bus.publish(&llm_response(&agent, inv, Some(1)))
        .await
        .unwrap();

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let consumer = ProjectionConsumer::new(bus.clone(), store.clone());
    let handle = tokio::spawn(consumer.run(shutdown_rx));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let on = loop {
        if let Some(on) = halted(&bus) {
            break on;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "a mixed stream must still halt the projector"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(on.schema_version, 2);
    assert_eq!(on.stream_seq, Some(older_seq));
    assert_eq!(
        agent_rows(&store, &agent).await,
        8,
        "everything from the floor to the halt is projected, nothing past it is"
    );
    assert_eq!(
        store
            .event_location(&first.envelope.event_id.to_string())
            .await
            .unwrap(),
        EventLocation::At(first_seq),
        "the row projected is the one at the floor"
    );
    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("the halted loop stops on shutdown")
        .expect("join")
        .expect("shutdown is a clean stop");
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
    let record = store.rebuild_record().await.unwrap().unwrap();
    assert_eq!(record.reason, rebuild.reason);
    assert_eq!(
        record.floor_seq,
        Some(1),
        "the operator path finds the floor too"
    );
    assert_eq!(
        (rebuild.floor_seq, rebuild.carried_below_floor),
        (Some(1), 0),
        "and the answer carries it: {rebuild:?}"
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
