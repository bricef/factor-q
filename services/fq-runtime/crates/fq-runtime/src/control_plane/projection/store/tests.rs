//! Unit tests for [`super`]. Extracted from the parent module so the
//! file that ships is the file you read (#390); `super::*` keeps the
//! same access it had inline.

use super::*;
use crate::agent::AgentId;
use crate::events::{
    CompletedPayload, ConfigSnapshot, CostMetadata, Event, EventPayload, FailedPayload,
    FailureKind, FailurePhase, InvocationTotals, LlmRequestPayload, LlmResponsePayload, Message,
    MessageRole, RequestParams, SandboxSnapshot, StopReason, TokenUsage, TriggerSource,
    TriggeredPayload,
};
use serde_json::json;
use tempfile::tempdir;
use uuid::Uuid;

/// Tiny helper for fixtures: `AgentId::new(s).unwrap()` would be
/// noise at every call site. Panics on invalid input — only used
/// in test code where the inputs are hardcoded by us.
fn aid(s: &str) -> AgentId {
    AgentId::new(s).expect("test agent id must be valid")
}

fn summary_event(inv: Uuid, kind: crate::events::SummaryKind, line: &str) -> Event {
    Event::new(
        AgentId::summary(),
        inv,
        EventPayload::InvocationSummary(crate::events::InvocationSummaryPayload {
            kind,
            summary: line.to_string(),
        }),
    )
    .with_cost(CostMetadata {
        call_id: Uuid::now_v7(),
        model: "cheap-model".to_string(),
        input_tokens: 400,
        output_tokens: 20,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        input_cost: 0.0004,
        output_cost: 0.0001,
        total_cost: 0.0005,
        cumulative_invocation_cost: 0.0005,
        cumulative_agent_cost: 0.0005,
        origin: Default::default(),
    })
}

#[tokio::test]
async fn sweep_events_deletes_old_rows_and_keeps_fresh_rows() {
    let dir = tempdir().unwrap();
    let store = ProjectionStore::open(&dir.path().join("projection.db"))
        .await
        .unwrap();
    let old = sample_triggered("old", Uuid::now_v7());
    let fresh = sample_triggered("fresh", Uuid::now_v7());
    store.insert_event(&old, None).await.unwrap();
    store.insert_event(&fresh, None).await.unwrap();
    sqlx::query("UPDATE events SET timestamp = ? WHERE event_id = ?")
        .bind("2020-01-01T00:00:00+00:00")
        .bind(old.envelope.event_id.to_string())
        .execute(&store.pool)
        .await
        .unwrap();

    let cutoff = chrono::DateTime::parse_from_rfc3339("2021-01-01T00:00:00Z")
        .unwrap()
        .timestamp_millis();
    assert_eq!(store.sweep_events(cutoff).await.unwrap(), 1);
    assert_eq!(store.count().await.unwrap(), 1);
    let remaining: String = sqlx::query_scalar("SELECT event_id FROM events")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_eq!(remaining, fresh.envelope.event_id.to_string());
}

#[tokio::test]
async fn sweep_events_batches_until_backlog_clear() {
    let dir = tempdir().unwrap();
    let store = ProjectionStore::open(&dir.path().join("projection.db"))
        .await
        .unwrap();
    for name in ["old-a", "old-b", "old-c"] {
        let event = sample_triggered(name, Uuid::now_v7());
        store.insert_event(&event, None).await.unwrap();
        sqlx::query("UPDATE events SET timestamp = ? WHERE event_id = ?")
            .bind("2020-01-01T00:00:00+00:00")
            .bind(event.envelope.event_id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
    }
    let fresh = sample_triggered("fresh", Uuid::now_v7());
    store.insert_event(&fresh, None).await.unwrap();

    let cutoff = chrono::DateTime::parse_from_rfc3339("2021-01-01T00:00:00Z")
        .unwrap()
        .timestamp_millis();
    // Batch size 1 forces one delete round per backlog row (plus
    // the terminating short round): the loop must clear the whole
    // backlog, count it accurately, and leave fresh rows alone.
    assert_eq!(store.sweep_events_batched(cutoff, 1).await.unwrap(), 3);
    assert_eq!(store.count().await.unwrap(), 1);
}

/// Cost information outlives retention: a cost-bearing row older
/// than the cutoff survives the sweep and still feeds the spend
/// figures, while a non-cost row of the same age is deleted.
#[tokio::test]
async fn sweep_events_preserves_cost_bearing_rows() {
    let dir = tempdir().unwrap();
    let store = ProjectionStore::open(&dir.path().join("projection.db"))
        .await
        .unwrap();
    let costed = sample_llm_response_with_cost("biller", Uuid::now_v7(), 0.25);
    let uncosted = sample_triggered("biller", Uuid::now_v7());
    store.insert_event(&costed, None).await.unwrap();
    store.insert_event(&uncosted, None).await.unwrap();
    for event in [&costed, &uncosted] {
        sqlx::query("UPDATE events SET timestamp = ? WHERE event_id = ?")
            .bind("2020-01-01T00:00:00+00:00")
            .bind(event.envelope.event_id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
    }

    let cutoff = chrono::DateTime::parse_from_rfc3339("2021-01-01T00:00:00Z")
        .unwrap()
        .timestamp_millis();
    // Only the uncosted row is swept.
    assert_eq!(store.sweep_events(cutoff).await.unwrap(), 1);
    assert_eq!(store.count().await.unwrap(), 1);

    // The all-time spend figure is intact after the sweep.
    let summary = store.cost_summary(Some("biller"), None).await.unwrap();
    assert_eq!(summary.len(), 1);
    assert!((summary[0].total_cost - 0.25).abs() < f64::EPSILON);
}

#[tokio::test]
async fn agent_id_for_invocation_ignores_operator_only_tombstone() {
    let dir = tempdir().unwrap();
    let store = ProjectionStore::open(&dir.path().join("projection.db"))
        .await
        .unwrap();
    let inv = Uuid::now_v7();
    let event = Event::new(
        AgentId::operator(),
        inv,
        EventPayload::InvocationOperatorRecovered(
            crate::events::InvocationOperatorRecoveredPayload {
                action: "drop".to_string(),
                final_phase: "failed".to_string(),
                reason: None,
            },
        ),
    );

    store.insert_event(&event, None).await.unwrap();
    assert_eq!(
        store
            .agent_id_for_invocation(&inv.to_string())
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn agent_id_for_invocation_uses_first_real_agent_not_summary() {
    let dir = tempdir().unwrap();
    let store = ProjectionStore::open(&dir.path().join("projection.db"))
        .await
        .unwrap();
    let inv = Uuid::now_v7();
    let summary = summary_event(inv, crate::events::SummaryKind::Start, "starting");
    let mut triggered = sample_triggered("builder", inv);
    triggered.envelope.timestamp = summary.envelope.timestamp + chrono::Duration::seconds(1);

    // Insert the real event first while giving the sentinel row an earlier
    // timestamp, pinning both sentinel exclusion and timestamp ordering.
    store.insert_event(&triggered, None).await.unwrap();
    store.insert_event(&summary, None).await.unwrap();
    assert_eq!(
        store
            .agent_id_for_invocation(&inv.to_string())
            .await
            .unwrap(),
        Some("builder".to_string())
    );
}

/// #216: a summary event lands twice — as a costed events row
/// under the reserved `summary` agent (the operator-overhead
/// accounting), and as the per-invocation current line (last
/// write wins).
#[tokio::test]
async fn summary_events_are_costed_and_upsert_the_current_line() {
    let dir = tempdir().unwrap();
    let store = ProjectionStore::open(&dir.path().join("projection.db"))
        .await
        .unwrap();
    let inv = Uuid::now_v7();

    store
        .insert_event(
            &summary_event(
                inv,
                crate::events::SummaryKind::Start,
                "Fixing #7: starting",
            ),
            None,
        )
        .await
        .unwrap();
    store
        .insert_event(
            &summary_event(
                inv,
                crate::events::SummaryKind::Progress,
                "Fixing #7: editing widget.rs",
            ),
            None,
        )
        .await
        .unwrap();

    // The current line: last write wins.
    let summaries = store.summaries_for(&[inv.to_string()]).await.unwrap();
    assert_eq!(
        summaries.get(&inv.to_string()).map(String::as_str),
        Some("Fixing #7: editing widget.rs")
    );
    assert!(
        store
            .summaries_for(&["no-such".to_string()])
            .await
            .unwrap()
            .is_empty()
    );

    // The cost accounting: events rows under agent `summary` carry
    // model/tokens/cost from the envelope, so `fq costs` reports
    // the summariser as its own row.
    let rows = store
        .query_events(
            &EventFilter {
                agent: Some("summary"),
                event_type: None,
                since: None,
            },
            10,
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.event_type == "invocation_summary"));
    assert!(rows.iter().all(|r| r.total_cost == Some(0.0005)));
    assert!(
        rows.iter()
            .all(|r| r.model.as_deref() == Some("cheap-model"))
    );

    // Reported, not just recorded (#216's operator-costed
    // guarantee): the summariser appears in the cost aggregations
    // `fq costs` renders — per-agent and per-model.
    let agents = store.cost_summary(None, None).await.unwrap();
    let summary_row = agents
        .iter()
        .find(|c| c.agent_id == "summary")
        .expect("summary agent row in cost_summary");
    assert!((summary_row.total_cost - 0.001).abs() < 1e-9);
    let models = store.cost_by_model(None, None).await.unwrap();
    assert!(
        models.iter().any(|m| m.model == "cheap-model"),
        "summariser model in the per-model split"
    );
}

fn sample_triggered(agent: &str, inv: Uuid) -> Event {
    Event::new(
        aid(agent),
        inv,
        EventPayload::Triggered(TriggeredPayload {
            trigger_source: TriggerSource::Manual,
            trigger_subject: None,
            trigger_payload: json!({}),
            config_snapshot: ConfigSnapshot {
                name: agent.to_string(),
                model: "claude-haiku-4-5".to_string(),
                system_prompt: "You are a test.".to_string(),
                tools: vec![],
                sandbox: SandboxSnapshot::default(),
                budget: None,
                ..Default::default()
            },
        }),
    )
}

/// What a `since` bound actually does here, and why
/// [`crate::views::since`] renders one rather than passing an
/// operator's spelling through: `timestamp` is TEXT and `timestamp >=
/// ?` is a **lexical** comparison against RFC3339 as `insert_event`
/// wrote it. So a bare date lowered to that day's first moment selects
/// the whole day — sub-second events at midnight included — and nothing
/// from the day before. `cost_summary` compares the same column the
/// same way, which is why both `--since` verbs can share one grammar.
#[tokio::test]
async fn a_date_since_selects_that_whole_day_and_nothing_before_it() {
    let dir = tempdir().unwrap();
    let store = ProjectionStore::open(&dir.path().join("projection.db"))
        .await
        .unwrap();
    // Spelled the way `insert_event` spells it, so the comparison
    // under test is the one production runs.
    let stored = |rfc3339: &str| {
        chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&chrono::Utc)
            .to_rfc3339()
    };
    for (agent, timestamp) in [
        ("eve", stored("2026-04-24T23:59:59.999Z")),
        ("midnight", stored("2026-04-25T00:00:00Z")),
        ("morning", stored("2026-04-25T09:15:00.500Z")),
        ("tomorrow", stored("2026-04-26T00:00:00Z")),
    ] {
        let event = sample_triggered(agent, Uuid::now_v7());
        store.insert_event(&event, None).await.unwrap();
        sqlx::query("UPDATE events SET timestamp = ? WHERE event_id = ?")
            .bind(timestamp)
            .bind(event.envelope.event_id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
    }

    let since = crate::views::since::lower_bound("2026-04-25").expect("a date is a since");
    let selected = store
        .query_events(
            &EventFilter {
                agent: None,
                event_type: None,
                since: Some(&since),
            },
            10,
        )
        .await
        .unwrap();
    let mut agents: Vec<&str> = selected.iter().map(|r| r.agent_id.as_str()).collect();
    agents.sort_unstable();
    assert_eq!(
        agents,
        ["midnight", "morning", "tomorrow"],
        "`--since 2026-04-25` must select the 25th onwards, midnight included"
    );
}

/// LLM response with cost attached via envelope. After step 3
/// of the envelope-refactor plan, cost rides on the
/// `llm.response` envelope rather than as its own event.
fn sample_llm_response_with_cost(agent: &str, inv: Uuid, cost: f64) -> Event {
    Event::new(
        aid(agent),
        inv,
        EventPayload::LlmResponse(LlmResponsePayload {
            round: 0,
            origin: crate::events::LlmCallOrigin::AgentTurn,
            call_id: Uuid::now_v7(),
            content: Some("ok".to_string()),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 20,
                cache_write_tokens: 10,
            },
        }),
    )
    .with_cost(CostMetadata {
        call_id: Uuid::now_v7(),
        model: "claude-haiku-4-5".to_string(),
        input_tokens: 100,
        output_tokens: 50,
        cache_read_tokens: 20,
        cache_write_tokens: 10,
        input_cost: 0.0001,
        output_cost: 0.00025,
        total_cost: cost,
        cumulative_invocation_cost: cost,
        cumulative_agent_cost: cost,
        origin: crate::events::LlmCallOrigin::AgentTurn,
    })
}

fn sample_completed(agent: &str, inv: Uuid) -> Event {
    Event::new(
        aid(agent),
        inv,
        EventPayload::Completed(CompletedPayload {
            task_status: crate::events::TaskStatus::default(),
            result_summary: Some("done".to_string()),
            total_llm_calls: 1,
            total_tool_calls: 0,
            total_cost: 0.0011,
            total_duration_ms: 123,
        }),
    )
}

fn sample_failed(agent: &str, inv: Uuid) -> Event {
    Event::new(
        aid(agent),
        inv,
        EventPayload::Failed(FailedPayload {
            error_kind: FailureKind::BudgetExceeded,
            error_message: "blew the budget".to_string(),
            phase: FailurePhase::LlmResponse,
            partial_totals: InvocationTotals {
                total_llm_calls: 1,
                total_tool_calls: 0,
                total_cost: 0.5,
                total_duration_ms: 99,
                sampling_cost: 0.0,
                elicitation_cost: 0.0,
            },
        }),
    )
}

fn sample_llm_request(agent: &str, inv: Uuid) -> Event {
    Event::new(
        aid(agent),
        inv,
        EventPayload::LlmRequest(LlmRequestPayload {
            origin: crate::events::LlmCallOrigin::AgentTurn,
            call_id: Uuid::now_v7(),
            model: "claude-haiku-4-5".to_string(),
            messages: vec![Message {
                role: MessageRole::System,
                content: Some("hi".to_string()),
                tool_calls: vec![],
                tool_call_id: None,
            }],
            tools_available: vec![],
            request_params: RequestParams {
                effort: None,
                temperature: None,
                max_tokens: Some(1024),
            },
        }),
    )
}

fn sample_llm_response(agent: &str, inv: Uuid) -> Event {
    Event::new(
        aid(agent),
        inv,
        EventPayload::LlmResponse(LlmResponsePayload {
            round: 0,
            origin: crate::events::LlmCallOrigin::AgentTurn,
            call_id: Uuid::now_v7(),
            content: Some("hi".to_string()),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 5,
                output_tokens: 3,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        }),
    )
}

/// A failed call (#447). `usage` decides whether it bills: `Some` is
/// the empty-completion case where the provider's counts survived,
/// `None` a transport error where they did not — and `None` must leave
/// the envelope cost absent rather than zeroed.
fn sample_llm_failure(agent: &str, inv: Uuid, cost: Option<f64>) -> Event {
    let usage = cost.map(|_| TokenUsage {
        input_tokens: 100,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    });
    let event = Event::new(
        aid(agent),
        inv,
        EventPayload::LlmFailure(crate::events::LlmFailurePayload {
            round: 1,
            call_id: Uuid::now_v7(),
            model: "claude-haiku-4-5".to_string(),
            error_kind: crate::events::LlmErrorKind::EmptyResponse,
            error_message: "model returned an empty response".to_string(),
            duration_ms: 900,
            usage,
            origin: crate::events::LlmCallOrigin::AgentTurn,
        }),
    );
    let (Some(total_cost), Some(usage)) = (cost, usage) else {
        return event;
    };
    event.with_cost(CostMetadata {
        call_id: Uuid::now_v7(),
        model: "claude-haiku-4-5".to_string(),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        input_cost: total_cost,
        output_cost: 0.0,
        total_cost,
        cumulative_invocation_cost: total_cost,
        cumulative_agent_cost: total_cost,
        origin: crate::events::LlmCallOrigin::AgentTurn,
    })
}

/// Recovered failure spend is *reported*, not merely projected. Before
/// #447 the cost queries listed `llm_response` and
/// `invocation_summary` only, so a billing failure would have landed
/// in the table and never appeared in a total.
#[tokio::test]
async fn cost_queries_include_recovered_failure_spend() {
    let (store, _dir) = open_store().await;
    let inv = Uuid::now_v7();
    store
        .insert_event(&sample_llm_response_with_cost("biller", inv, 0.25), None)
        .await
        .unwrap();
    store
        .insert_event(&sample_llm_failure("biller", inv, Some(0.01)), None)
        .await
        .unwrap();

    let summary = store.cost_summary(Some("biller"), None).await.unwrap();
    assert_eq!(summary.len(), 1);
    assert!((summary[0].total_cost - 0.26).abs() < 1e-9, "{summary:?}");
    assert_eq!(summary[0].total_input_tokens, 200);

    let per_invocation = store.cost_of_invocation(&inv.to_string()).await.unwrap();
    assert!(
        (per_invocation.expect("a cost row").total_cost - 0.26).abs() < 1e-9,
        "the per-invocation figure must agree with the per-agent one"
    );
    let by_model = store.cost_by_model(None, None).await.unwrap();
    assert!((by_model[0].total_cost - 0.26).abs() < 1e-9, "{by_model:?}");
}

/// The other half of "`None` is not zero": a failure whose usage was
/// never recoverable carries no cost, so it is swept with the rest of
/// the trail. A zeroed cost would pin it against the sweep's
/// `total_cost IS NOT NULL` exemption forever, as a cost record for
/// spend nobody can account for.
#[tokio::test]
async fn failure_without_usage_is_swept_like_any_other_row() {
    let (store, _dir) = open_store().await;
    let unbilled = sample_llm_failure("biller", Uuid::now_v7(), None);
    let billed = sample_llm_failure("biller", Uuid::now_v7(), Some(0.01));
    store.insert_event(&unbilled, None).await.unwrap();
    store.insert_event(&billed, None).await.unwrap();
    for event in [&unbilled, &billed] {
        sqlx::query("UPDATE events SET timestamp = ? WHERE event_id = ?")
            .bind("2020-01-01T00:00:00+00:00")
            .bind(event.envelope.event_id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
    }

    let cutoff = chrono::DateTime::parse_from_rfc3339("2021-01-01T00:00:00Z")
        .unwrap()
        .timestamp_millis();
    assert_eq!(store.sweep_events(cutoff).await.unwrap(), 1);
    let rows = store
        .query_events(&EventFilter::default(), 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].total_cost, Some(0.01));
    assert_eq!(rows[0].error_kind.as_deref(), Some("empty_response"));
}

async fn open_store() -> (ProjectionStore, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("projection.db");
    let store = ProjectionStore::open(&path).await.unwrap();
    (store, dir)
}

#[tokio::test]
async fn opens_and_creates_schema() {
    let (store, _dir) = open_store().await;
    assert_eq!(store.count().await.unwrap(), 0);
}

#[tokio::test]
async fn migrates_existing_projection_with_cache_columns() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("projection.db");
    std::fs::File::create(&path).unwrap();
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE events (event_id TEXT PRIMARY KEY, timestamp TEXT NOT NULL, \
         agent_id TEXT NOT NULL, invocation_id TEXT NOT NULL, event_type TEXT NOT NULL, \
         model TEXT, input_tokens INTEGER, output_tokens INTEGER, total_cost REAL, \
         error_kind TEXT, duration_ms INTEGER)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let store = ProjectionStore::open(&path).await.unwrap();
    store
        .insert_event(
            &sample_llm_response_with_cost("alpha", Uuid::now_v7(), 0.01),
            None,
        )
        .await
        .unwrap();
    let summary = store.cost_summary(None, None).await.unwrap();
    assert_eq!(summary[0].total_cache_read_tokens, 20);
    assert_eq!(summary[0].total_cache_write_tokens, 10);
}

#[tokio::test]
async fn inserts_and_counts_events() {
    let (store, _dir) = open_store().await;
    let inv = Uuid::now_v7();
    store
        .insert_event(&sample_triggered("alpha", inv), None)
        .await
        .unwrap();
    store
        .insert_event(&sample_llm_response_with_cost("alpha", inv, 0.0011), None)
        .await
        .unwrap();
    store
        .insert_event(&sample_completed("alpha", inv), None)
        .await
        .unwrap();

    assert_eq!(store.count().await.unwrap(), 3);
}

/// Heartbeats are an operational signal, not data: `insert_event`
/// drops them, and the migration sweep evicts rows older builds
/// accumulated.
#[tokio::test]
async fn heartbeats_are_not_projected_and_legacy_rows_are_swept() {
    use crate::events::WorkerHeartbeatPayload;
    use crate::worker::WorkerId;

    let (store, dir) = open_store().await;
    let heartbeat = Event::system(
        Uuid::now_v7(),
        EventPayload::WorkerHeartbeat(WorkerHeartbeatPayload {
            worker_id: WorkerId::new("w1".to_string()).unwrap(),
        }),
    );
    store.insert_event(&heartbeat, None).await.unwrap();
    // A real event still lands; the heartbeat never did.
    store
        .insert_event(&sample_triggered("alpha", Uuid::now_v7()), None)
        .await
        .unwrap();
    assert_eq!(store.count().await.unwrap(), 1);
    let rows = store
        .query_events(&EventFilter::default(), 10)
        .await
        .unwrap();
    assert!(rows.iter().all(|r| r.event_type != "worker_heartbeat"));

    // A row written by an older build (heartbeats were projected
    // until 2026-07-15) is deleted by the reopen migration.
    sqlx::query(
        "INSERT INTO events (event_id, timestamp, agent_id, invocation_id, event_type) \
         VALUES ('legacy-hb', '2026-07-14T00:00:00+00:00', 'system', 'inv', 'worker_heartbeat')",
    )
    .execute(&store.pool)
    .await
    .unwrap();
    assert_eq!(store.count().await.unwrap(), 2);
    drop(store);
    let reopened = ProjectionStore::open(&dir.path().join("projection.db"))
        .await
        .unwrap();
    assert_eq!(reopened.count().await.unwrap(), 1, "legacy heartbeat swept");
}

/// The projection is an index over the log, so a row records where in
/// the log it points. `event.get` resolves an *identity* through that
/// column (plan Phase 4, cohort 4.2): an index that dropped the
/// position would list events nobody could then read whole.
///
/// The three answers are the contract, and none of them collapses
/// into another. An unknown id is not an unlocated row, and an
/// unlocated row is not a missing event: a row seeded straight into
/// the index — which is also how rows written before the column
/// existed read — knows the event and not where its payload sits, and
/// says exactly that rather than a zero claiming to be a position.
#[tokio::test]
async fn an_identity_resolves_to_the_log_position_it_indexes() {
    let (store, _dir) = open_store().await;
    let positioned = sample_triggered("alpha", Uuid::now_v7());
    let seeded = sample_triggered("beta", Uuid::now_v7());
    store.insert_event(&positioned, Some(4_242)).await.unwrap();
    store.insert_event(&seeded, None).await.unwrap();

    let located = |event: &Event| {
        let store = store.clone();
        let id = event.envelope.event_id.to_string();
        async move { store.event_location(&id).await.unwrap() }
    };
    assert_eq!(located(&positioned).await, EventLocation::At(4_242));
    assert_eq!(located(&seeded).await, EventLocation::Unlocated);
    assert_eq!(
        store
            .event_location(&Uuid::now_v7().to_string())
            .await
            .unwrap(),
        EventLocation::Unindexed,
        "an id the index has never seen is not an unlocated row"
    );
}

#[tokio::test]
async fn insert_is_idempotent_by_event_id() {
    let (store, _dir) = open_store().await;
    let inv = Uuid::now_v7();
    let event = sample_triggered("alpha", inv);
    store.insert_event(&event, None).await.unwrap();
    store.insert_event(&event, None).await.unwrap(); // re-delivery
    store.insert_event(&event, None).await.unwrap();
    assert_eq!(store.count().await.unwrap(), 1);
}

#[tokio::test]
async fn queries_filter_by_agent() {
    let (store, _dir) = open_store().await;
    let inv = Uuid::now_v7();
    store
        .insert_event(&sample_triggered("alpha", inv), None)
        .await
        .unwrap();
    store
        .insert_event(&sample_triggered("beta", Uuid::now_v7()), None)
        .await
        .unwrap();

    let filter = EventFilter {
        agent: Some("alpha"),
        ..Default::default()
    };
    let rows = store.query_events(&filter, 100).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].agent_id, "alpha");
}

#[tokio::test]
async fn queries_filter_by_event_type() {
    let (store, _dir) = open_store().await;
    let inv = Uuid::now_v7();
    store
        .insert_event(&sample_triggered("alpha", inv), None)
        .await
        .unwrap();
    store
        .insert_event(&sample_llm_response_with_cost("alpha", inv, 0.01), None)
        .await
        .unwrap();
    store
        .insert_event(&sample_completed("alpha", inv), None)
        .await
        .unwrap();

    // After step 3 of the envelope-refactor plan, cost rides on
    // `llm.response` envelopes; filter by the response event
    // type and check the cost denormalised onto the row.
    let filter = EventFilter {
        event_type: Some("llm_response"),
        ..Default::default()
    };
    let rows = store.query_events(&filter, 100).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].event_type, "llm_response");
    assert_eq!(rows[0].total_cost, Some(0.01));
}

#[tokio::test]
async fn queries_respect_limit() {
    let (store, _dir) = open_store().await;
    for _ in 0..5 {
        store
            .insert_event(&sample_triggered("alpha", Uuid::now_v7()), None)
            .await
            .unwrap();
    }
    let filter = EventFilter::default();
    let rows = store.query_events(&filter, 3).await.unwrap();
    assert_eq!(rows.len(), 3);
}

#[tokio::test]
async fn cost_summary_aggregates_by_agent() {
    let (store, _dir) = open_store().await;
    store
        .insert_event(
            &sample_llm_response_with_cost("alpha", Uuid::now_v7(), 0.10),
            None,
        )
        .await
        .unwrap();
    store
        .insert_event(
            &sample_llm_response_with_cost("alpha", Uuid::now_v7(), 0.05),
            None,
        )
        .await
        .unwrap();
    store
        .insert_event(
            &sample_llm_response_with_cost("beta", Uuid::now_v7(), 0.20),
            None,
        )
        .await
        .unwrap();

    let summary = store.cost_summary(None, None).await.unwrap();
    assert_eq!(summary.len(), 2);

    let beta = summary.iter().find(|s| s.agent_id == "beta").unwrap();
    assert!((beta.total_cost - 0.20).abs() < 1e-9);
    assert_eq!(beta.event_count, 1);

    let alpha = summary.iter().find(|s| s.agent_id == "alpha").unwrap();
    assert!((alpha.total_cost - 0.15).abs() < 1e-9);
    assert_eq!(alpha.event_count, 2);
    assert_eq!(alpha.total_input_tokens, 200);
    assert_eq!(alpha.total_output_tokens, 100);
    assert_eq!(alpha.total_cache_read_tokens, 40);
    assert_eq!(alpha.total_cache_write_tokens, 20);
    // Two events on two distinct invocations.
    assert_eq!(alpha.invocation_count, 2);
}

/// The drill-down queries group the same cost-bearing rows by
/// invocation and by model — no new columns, only new GROUP BYs.
#[tokio::test]
async fn cost_detail_groups_by_invocation_and_model() {
    let (store, _dir) = open_store().await;
    let inv1 = Uuid::now_v7();
    let inv2 = Uuid::now_v7();
    for (inv, cost) in [(inv1, 0.10), (inv1, 0.05), (inv2, 0.20)] {
        store
            .insert_event(&sample_llm_response_with_cost("alpha", inv, cost), None)
            .await
            .unwrap();
    }
    // Another agent's spend must not leak into alpha's drill-down.
    store
        .insert_event(
            &sample_llm_response_with_cost("beta", Uuid::now_v7(), 9.0),
            None,
        )
        .await
        .unwrap();

    let invs = store.cost_by_invocation("alpha", None, 10).await.unwrap();
    assert_eq!(invs.len(), 2);
    // Newest first by each invocation's first cost event.
    assert!(
        invs[0].first_timestamp >= invs[1].first_timestamp,
        "rows must be newest-first: {invs:?}"
    );
    let one = invs
        .iter()
        .find(|r| r.invocation_id == inv1.to_string())
        .unwrap();
    assert_eq!(one.event_count, 2);
    assert!((one.total_cost - 0.15).abs() < 1e-9);
    assert_eq!(one.total_input_tokens, 200);
    assert_eq!(one.total_cache_read_tokens, 40);
    let two = invs
        .iter()
        .find(|r| r.invocation_id == inv2.to_string())
        .unwrap();
    assert_eq!(two.event_count, 1);
    assert!((two.total_cost - 0.20).abs() < 1e-9);

    // The cap holds.
    assert_eq!(
        store
            .cost_by_invocation("alpha", None, 1)
            .await
            .unwrap()
            .len(),
        1
    );

    // The single-invocation aggregate matches the grouped rows.
    let one = store
        .cost_of_invocation(&inv1.to_string())
        .await
        .unwrap()
        .expect("inv1 has cost events");
    assert_eq!(one.event_count, 2);
    assert!((one.total_cost - 0.15).abs() < 1e-9);
    assert!(
        store
            .cost_of_invocation("no-such-id")
            .await
            .unwrap()
            .is_none()
    );

    // All fixture events carry the same model → one row, summed.
    let models = store.cost_by_model(Some("alpha"), None).await.unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].model, "claude-haiku-4-5");
    assert_eq!(models[0].event_count, 3);
    assert!((models[0].total_cost - 0.35).abs() < 1e-9);

    // Unfiltered, the same GROUP BY spans every agent — the
    // top-level costs page's by-model split.
    let all_models = store.cost_by_model(None, None).await.unwrap();
    assert_eq!(all_models.len(), 1);
    assert_eq!(all_models[0].event_count, 4);
    assert!((all_models[0].total_cost - 9.35).abs() < 1e-9);
}

/// Bucketing invariants that hold whatever the wall clock says:
/// every cost event lands in exactly one bucket, bucket sums equal
/// the grand total, hourly refines daily, and keys carry the
/// fixed-width UTC prefix shape.
#[tokio::test]
async fn cost_buckets_partition_the_spend() {
    let (store, _dir) = open_store().await;
    for cost in [0.10, 0.05, 0.20] {
        store
            .insert_event(
                &sample_llm_response_with_cost("alpha", Uuid::now_v7(), cost),
                None,
            )
            .await
            .unwrap();
    }
    let daily = store.cost_by_time_bucket(false, None).await.unwrap();
    let hourly = store.cost_by_time_bucket(true, None).await.unwrap();
    let day_sum: f64 = daily.iter().map(|b| b.total_cost).sum();
    let hour_sum: f64 = hourly.iter().map(|b| b.total_cost).sum();
    assert!((day_sum - 0.35).abs() < 1e-9, "{daily:?}");
    assert!((hour_sum - 0.35).abs() < 1e-9, "{hourly:?}");
    assert!(!daily.is_empty() && daily.len() <= hourly.len());
    for b in &daily {
        assert_eq!(b.bucket.len(), 10, "day key shape: {}", b.bucket);
    }
    for b in &hourly {
        assert_eq!(b.bucket.len(), 13, "hour key shape: {}", b.bucket);
        assert_eq!(b.bucket.as_bytes()[10], b'T');
    }
    // A `since` beyond every event excludes all buckets.
    let none = store
        .cost_by_time_bucket(false, Some("9999-01-01"))
        .await
        .unwrap();
    assert!(none.is_empty());
}

#[tokio::test]
async fn cost_summary_filters_by_agent() {
    let (store, _dir) = open_store().await;
    store
        .insert_event(
            &sample_llm_response_with_cost("alpha", Uuid::now_v7(), 0.10),
            None,
        )
        .await
        .unwrap();
    store
        .insert_event(
            &sample_llm_response_with_cost("beta", Uuid::now_v7(), 0.20),
            None,
        )
        .await
        .unwrap();

    let summary = store.cost_summary(Some("alpha"), None).await.unwrap();
    assert_eq!(summary.len(), 1);
    assert_eq!(summary[0].agent_id, "alpha");
}

fn sample_failed_kind(agent: &str, inv: Uuid, kind: FailureKind) -> Event {
    Event::new(
        aid(agent),
        inv,
        EventPayload::Failed(FailedPayload {
            error_kind: kind,
            error_message: "boom".to_string(),
            phase: FailurePhase::LlmResponse,
            partial_totals: InvocationTotals::default(),
        }),
    )
}

#[tokio::test]
async fn projected_failure_kinds_match_wire_serialization() {
    let (store, _dir) = open_store().await;
    let kinds = [
        FailureKind::BudgetExceeded,
        FailureKind::LlmError,
        FailureKind::MaxIterations,
        FailureKind::ToolError,
        FailureKind::SandboxViolation,
        FailureKind::RuntimeError,
        FailureKind::TriggerExhausted,
    ];
    for kind in kinds {
        let event = sample_failed_kind("a", Uuid::now_v7(), kind);
        let wire = serde_json::to_value(kind)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned();
        store.insert_event(&event, None).await.unwrap();
        let projected = store
            .query_events(&EventFilter::default(), 100)
            .await
            .unwrap();
        assert!(
            projected
                .iter()
                .any(|row| row.error_kind.as_deref() == Some(&wire))
        );
    }
}

#[tokio::test]
async fn failure_summary_groups_by_kind() {
    let (store, _dir) = open_store().await;
    store
        .insert_event(
            &sample_failed_kind("a", Uuid::now_v7(), FailureKind::BudgetExceeded),
            None,
        )
        .await
        .unwrap();
    store
        .insert_event(
            &sample_failed_kind("a", Uuid::now_v7(), FailureKind::BudgetExceeded),
            None,
        )
        .await
        .unwrap();
    store
        .insert_event(
            &sample_failed_kind("b", Uuid::now_v7(), FailureKind::ToolError),
            None,
        )
        .await
        .unwrap();
    // A non-failed event must not be counted.
    store
        .insert_event(&sample_completed("a", Uuid::now_v7()), None)
        .await
        .unwrap();

    let summary = store.failure_summary().await.unwrap();
    let total: i64 = summary.iter().map(|s| s.count).sum();
    assert_eq!(total, 3);
    let budget = summary
        .iter()
        .find(|s| s.error_kind == "budget_exceeded")
        .unwrap();
    assert_eq!(budget.count, 2);
    let tool = summary
        .iter()
        .find(|s| s.error_kind == "tool_error")
        .unwrap();
    assert_eq!(tool.count, 1);
}

#[tokio::test]
async fn failure_summary_empty_when_no_failures() {
    let (store, _dir) = open_store().await;
    store
        .insert_event(&sample_completed("a", Uuid::now_v7()), None)
        .await
        .unwrap();
    assert!(store.failure_summary().await.unwrap().is_empty());
}

#[tokio::test]
async fn extract_fields_covers_all_event_types() {
    let (store, _dir) = open_store().await;
    let inv = Uuid::now_v7();
    store
        .insert_event(&sample_triggered("alpha", inv), None)
        .await
        .unwrap();
    store
        .insert_event(&sample_llm_request("alpha", inv), None)
        .await
        .unwrap();
    store
        .insert_event(&sample_llm_response("alpha", inv), None)
        .await
        .unwrap();
    store
        .insert_event(&sample_llm_response_with_cost("alpha", inv, 0.01), None)
        .await
        .unwrap();
    store
        .insert_event(&sample_completed("alpha", inv), None)
        .await
        .unwrap();
    store
        .insert_event(&sample_failed("alpha", Uuid::now_v7()), None)
        .await
        .unwrap();
    // No panic, all inserts succeed.
    assert_eq!(store.count().await.unwrap(), 6);
}

#[tokio::test]
async fn failed_event_error_message_is_projected_and_returned() {
    let (store, _dir) = open_store().await;
    let invocation_id = Uuid::now_v7();
    store
        .insert_event(&sample_failed("alpha", invocation_id), None)
        .await
        .unwrap();

    let rows = store
        .query_events(&EventFilter::default(), 10)
        .await
        .unwrap();
    assert_eq!(rows[0].error_kind.as_deref(), Some("budget_exceeded"));
    assert_eq!(rows[0].error_message.as_deref(), Some("blew the budget"));
}

#[tokio::test]
async fn read_only_open_fails_if_missing() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("nonexistent.db");
    let err = ProjectionStore::open_read_only(&path).await.unwrap_err();
    assert!(matches!(err, StoreError::NotInitialised(_)));
}

#[tokio::test]
async fn read_only_open_succeeds_after_write_open() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("projection.db");
    {
        let writer = ProjectionStore::open(&path).await.unwrap();
        writer
            .insert_event(&sample_triggered("alpha", Uuid::now_v7()), None)
            .await
            .unwrap();
    }
    let reader = ProjectionStore::open_read_only(&path).await.unwrap();
    assert_eq!(reader.count().await.unwrap(), 1);
}
