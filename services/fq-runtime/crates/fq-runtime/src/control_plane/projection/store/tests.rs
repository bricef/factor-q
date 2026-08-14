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
    summary_event_costing(inv, kind, line, 0.0005)
}

fn summary_event_costing(
    inv: Uuid,
    kind: crate::events::SummaryKind,
    line: &str,
    total_cost: f64,
) -> Event {
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
        input_cost: total_cost * 0.8,
        output_cost: total_cost * 0.2,
        total_cost,
        cumulative_invocation_cost: total_cost,
        cumulative_agent_cost: total_cost,
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
            trigger_id: None,
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

/// The allocation rule (#466), stated as a single fixture: summariser
/// spend is money, so every *aggregate* reports it; it is the engine's
/// money, so no *per-invocation* view charges it to a run. Both halves
/// were wrong before this, in opposite directions — the time series
/// dropped it, and `cost_by_invocation` billed it to the invocation the
/// line described.
#[tokio::test]
async fn summariser_spend_reaches_every_aggregate_and_no_invocation() {
    let (store, _dir) = open_store().await;
    let inv = Uuid::now_v7();
    store
        .insert_event(&sample_llm_response_with_cost("biller", inv, 0.25), None)
        .await
        .unwrap();
    store
        .insert_event(
            &summary_event(inv, crate::events::SummaryKind::Progress, "summarising"),
            None,
        )
        .await
        .unwrap();

    // Aggregates: the fleet total, the per-model split, and the time
    // series all carry the summariser's $0.0005.
    let agents = store.cost_summary(None, None).await.unwrap();
    let fleet: f64 = agents.iter().map(|a| a.total_cost).sum();
    assert!((fleet - 0.2505).abs() < 1e-9, "{agents:?}");
    let by_model: f64 = store
        .cost_by_model(None, None)
        .await
        .unwrap()
        .iter()
        .map(|m| m.total_cost)
        .sum();
    assert!((by_model - 0.2505).abs() < 1e-9);
    let bucketed: f64 = store
        .cost_by_time_bucket(false, None)
        .await
        .unwrap()
        .iter()
        .map(|b| b.total_cost)
        .sum();
    assert!(
        (bucketed - 0.2505).abs() < 1e-9,
        "the time series must sum to the same money as the agent table"
    );

    // Per-invocation: neither view charges the summariser's call to
    // the invocation whose progress it described.
    let of_inv = store
        .cost_of_invocation(&inv.to_string())
        .await
        .unwrap()
        .expect("a cost row");
    assert!((of_inv.total_cost - 0.25).abs() < 1e-9);
    let biller = store.cost_by_invocation("biller", None, 100).await.unwrap();
    assert_eq!(biller.len(), 1);
    assert!((biller[0].total_cost - 0.25).abs() < 1e-9);
    assert!(
        store
            .cost_by_invocation(AgentId::SUMMARY_STR, None, 100)
            .await
            .unwrap()
            .is_empty(),
        "the summariser has spend but no invocations of its own to charge it to"
    );

    // And the remainder is named, not left to be inferred from the gap.
    let summary_row = agents
        .iter()
        .find(|a| a.agent_id == AgentId::SUMMARY_STR)
        .expect("summary agent row");
    assert!((summary_row.framework_cost - 0.0005).abs() < 1e-9);
    let biller_row = agents.iter().find(|a| a.agent_id == "biller").unwrap();
    assert_eq!(biller_row.framework_cost, 0.0);
}

/// The other half of the fix, pinned on its own because it is the
/// disagreement operators actually hit: the spend chart and the agent
/// table sit on the same page over the same window, and before #466
/// the chart quietly omitted summariser spend. `since` is applied to
/// both, so the agreement has to survive a bound that cuts through the
/// data.
#[tokio::test]
async fn bucketed_spend_agrees_with_the_agent_table_over_the_same_window() {
    let (store, _dir) = open_store().await;
    let inv = Uuid::now_v7();
    for (event, ts) in [
        (
            sample_llm_response_with_cost("biller", inv, 0.25),
            "2026-04-25T09:00:00+00:00",
        ),
        (
            summary_event(inv, crate::events::SummaryKind::Progress, "in window"),
            "2026-04-25T09:05:00+00:00",
        ),
        (
            sample_llm_response_with_cost("biller", inv, 4.0),
            "2026-04-24T09:00:00+00:00",
        ),
        (
            summary_event(inv, crate::events::SummaryKind::Start, "before window"),
            "2026-04-24T09:05:00+00:00",
        ),
    ] {
        store.insert_event(&event, None).await.unwrap();
        sqlx::query("UPDATE events SET timestamp = ? WHERE event_id = ?")
            .bind(ts)
            .bind(event.envelope.event_id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
    }

    let since = crate::views::since::lower_bound("2026-04-25").expect("a date is a since");
    let agents: f64 = store
        .cost_summary(None, Some(&since))
        .await
        .unwrap()
        .iter()
        .map(|a| a.total_cost)
        .sum();
    let buckets = store
        .cost_by_time_bucket(false, Some(&since))
        .await
        .unwrap();
    let bucketed: f64 = buckets.iter().map(|b| b.total_cost).sum();
    assert!((agents - 0.2505).abs() < 1e-9, "windowed agent total");
    assert!(
        (bucketed - agents).abs() < 1e-9,
        "chart {bucketed} vs table {agents} over the same window: {buckets:?}"
    );
}

use proptest::prelude::*;

/// A spend shape: which of four invocations each cost lands on, and
/// how much (in micro-dollars, so the generator deals in exact
/// integers and only the summing is floating point).
fn spend() -> impl Strategy<Value = Vec<(usize, u32)>> {
    proptest::collection::vec((0usize..4, 1u32..1_000_000), 0..12)
}

proptest! {
    /// The law the split has to satisfy, and the reason the shortfall
    /// is safe to ship: for **every** agent, whatever the spend shape,
    ///
    /// ```text
    /// total_cost = <sum of its per-invocation costs> + framework_cost
    /// ```
    ///
    /// A total that reconciles to its parts plus a named remainder is
    /// auditable; one that merely fails to reconcile is a support
    /// question. Stated as a property rather than a fixture because
    /// the interesting inputs are the ones nobody thinks to write
    /// down: summariser lines on invocations with no other spend,
    /// several agents sharing an invocation id, agents with no
    /// framework spend at all. It fails if any query changes its row
    /// filter without the remainder following — which is exactly how
    /// the two queries this fix corrects drifted apart.
    #[test]
    fn every_agent_total_is_its_invocations_plus_its_framework_cost(
        calls in spend(),
        summaries in spend(),
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (store, _dir) = open_store().await;
            let invs: Vec<Uuid> = (0..4).map(|_| Uuid::now_v7()).collect();
            let agents = ["alpha", "beta"];
            for (n, (which, micros)) in calls.iter().enumerate() {
                let event = sample_llm_response_with_cost(
                    agents[n % agents.len()],
                    invs[*which],
                    f64::from(*micros) / 1e6,
                );
                store.insert_event(&event, None).await.unwrap();
            }
            for (which, micros) in &summaries {
                let event = summary_event_costing(
                    invs[*which],
                    crate::events::SummaryKind::Progress,
                    "line",
                    f64::from(*micros) / 1e6,
                );
                store.insert_event(&event, None).await.unwrap();
            }

            for row in store.cost_summary(None, None).await.unwrap() {
                let allocated: f64 = store
                    .cost_by_invocation(&row.agent_id, None, i64::MAX)
                    .await
                    .unwrap()
                    .iter()
                    .map(|i| i.total_cost)
                    .sum();
                prop_assert!(
                    (row.total_cost - (allocated + row.framework_cost)).abs() < 1e-9,
                    "{}: total {} != allocated {} + framework {}",
                    row.agent_id,
                    row.total_cost,
                    allocated,
                    row.framework_cost,
                );
            }
            Ok(())
        })?;
    }

    /// The same money, sliced two ways: an aggregate is an aggregate,
    /// so bucketing spend by time must sum to bucketing it by agent.
    /// The pair disagreed for as long as one of them dropped
    /// summariser rows.
    #[test]
    fn bucketed_spend_sums_to_the_fleet_total(calls in spend(), summaries in spend()) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (store, _dir) = open_store().await;
            let invs: Vec<Uuid> = (0..4).map(|_| Uuid::now_v7()).collect();
            for (n, (which, micros)) in calls.iter().enumerate() {
                let agent = if n % 2 == 0 { "alpha" } else { "beta" };
                let event =
                    sample_llm_response_with_cost(agent, invs[*which], f64::from(*micros) / 1e6);
                store.insert_event(&event, None).await.unwrap();
            }
            for (which, micros) in &summaries {
                let event = summary_event_costing(
                    invs[*which],
                    crate::events::SummaryKind::Outcome,
                    "line",
                    f64::from(*micros) / 1e6,
                );
                store.insert_event(&event, None).await.unwrap();
            }

            let by_agent: f64 = store
                .cost_summary(None, None)
                .await
                .unwrap()
                .iter()
                .map(|a| a.total_cost)
                .sum();
            let by_bucket: f64 = store
                .cost_by_time_bucket(true, None)
                .await
                .unwrap()
                .iter()
                .map(|b| b.total_cost)
                .sum();
            prop_assert!(
                (by_agent - by_bucket).abs() < 1e-9,
                "by agent {by_agent} != by bucket {by_bucket}"
            );
            Ok(())
        })?;
    }
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
/// accumulated. Both derive from `events::transient` — the same list
/// that keeps them off `event.stream`, so the operator surface's two
/// reads over this substrate cannot drift apart again.
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

/// A database from an older build is refused by name, not by the
/// driver error the first mismatched query would otherwise raise.
///
/// The fixture rewinds a current database to an older shape by
/// dropping the columns that have been added since, which is exactly
/// the set a read-only handle cannot put back. Both are dropped, not
/// one, so the error has to report a set rather than the first thing
/// it noticed.
#[tokio::test]
async fn read_only_open_rejects_a_database_from_an_older_build() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("projection.db");
    {
        let writer = ProjectionStore::open(&path).await.unwrap();
        writer
            .insert_event(&sample_triggered("alpha", Uuid::now_v7()), None)
            .await
            .unwrap();
        for column in ["seq", "error_message"] {
            sqlx::query(&format!("ALTER TABLE events DROP COLUMN {column}"))
                .execute(&writer.pool)
                .await
                .unwrap();
        }
    }

    let err = ProjectionStore::open_read_only(&path).await.unwrap_err();
    let StoreError::SchemaOutdated { missing, .. } = &err else {
        panic!("expected SchemaOutdated, got: {err:?}");
    };
    assert!(
        missing.contains("seq") && missing.contains("error_message"),
        "the error should name every missing column, got: {missing}"
    );

    // The check is what produced that error, so opening read-write —
    // which migrates — has to clear it. Otherwise the message would be
    // telling operators to do something that does not work.
    {
        ProjectionStore::open(&path).await.unwrap();
    }
    ProjectionStore::open_read_only(&path).await.unwrap();
}

/// A file that exists but was never projected into is "not
/// initialised", not "outdated". `pragma_table_info` answers with no
/// rows in both cases, so the two are only distinguishable if the
/// check looks — and an operator sent to run a migration that would
/// not help is worse off than one told the projector never ran.
#[tokio::test]
async fn read_only_open_reports_an_empty_database_as_uninitialised() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("projection.db");
    {
        let writer = ProjectionStore::open(&path).await.unwrap();
        sqlx::query("DROP TABLE events")
            .execute(&writer.pool)
            .await
            .unwrap();
    }

    let err = ProjectionStore::open_read_only(&path).await.unwrap_err();
    assert!(
        matches!(err, StoreError::NotInitialised(_)),
        "expected NotInitialised, got: {err:?}"
    );
}

// ============================================================
// The `triggers` table — a trigger's permanent record.
// ============================================================

/// A `triggered` event that names its trigger, the way every event
/// written since the identity landed does.
fn named_triggered(agent: &str, trigger_id: Uuid, payload: serde_json::Value) -> Event {
    let mut event = sample_triggered(agent, Uuid::now_v7());
    if let EventPayload::Triggered(p) = &mut event.payload {
        p.trigger_id = Some(trigger_id);
        p.trigger_source = TriggerSource::Subject;
        p.trigger_subject = Some(crate::events::subjects::trigger(agent));
        p.trigger_payload = payload;
    }
    event
}

fn at(event: Event, timestamp: &str) -> Event {
    let mut event = event;
    event.envelope.timestamp = chrono::DateTime::parse_from_rfc3339(timestamp)
        .unwrap()
        .with_timezone(&chrono::Utc);
    event
}

/// **A trigger comes back whole, payload included, from one lookup.**
///
/// That is the property the Trigger atom is built on: the record holds
/// the payload, so `trigger.get` has no second hop into the log and
/// therefore no way to answer "I know of it but cannot show it to you".
#[tokio::test]
async fn a_recorded_trigger_reads_back_whole() {
    let (store, _dir) = open_store().await;
    let id = Uuid::now_v7();
    let payload = json!({"task": "look at #12", "refs": ["repo/path"]});
    store
        .insert_event(&named_triggered("alpha", id, payload.clone()), Some(77))
        .await
        .unwrap();

    let trigger = store
        .trigger(&id.to_string())
        .await
        .unwrap()
        .expect("a recorded trigger");
    assert_eq!(trigger.id, id);
    assert_eq!(trigger.payload, payload, "the body is kept verbatim");
    assert_eq!(trigger.source, TriggerSource::Subject);
    assert_eq!(trigger.subject.as_deref(), Some("fq.trigger.alpha"));
    assert_eq!(
        store.trigger(&Uuid::now_v7().to_string()).await.unwrap(),
        None,
        "an id no record names has no trigger — the caller names that state"
    );
}

/// An event that names no trigger writes no trigger row. The lens is
/// the whole rule: `insert_event` runs it, and anything it declines —
/// an `llm_response`, a pre-identity `triggered` — leaves the permanent
/// table alone.
#[tokio::test]
async fn an_event_that_names_no_trigger_records_none() {
    let (store, _dir) = open_store().await;
    // `sample_triggered` predates the identity: `trigger_id` is None.
    store
        .insert_event(&sample_triggered("alpha", Uuid::now_v7()), Some(1))
        .await
        .unwrap();
    store
        .insert_event(&sample_completed("alpha", Uuid::now_v7()), Some(2))
        .await
        .unwrap();
    assert!(
        store
            .query_triggers(None, None, 50)
            .await
            .unwrap()
            .is_empty(),
        "only an event that names a trigger records one"
    );
}

/// **The first record wins.** A trigger redelivered N times has one
/// `triggered` record per delivery, all naming it; the earliest is when
/// the runtime first took responsibility for it, which is what
/// `recorded_at` means — and it is also what makes a redelivery to the
/// projection consumer a no-op rather than a rewrite.
#[tokio::test]
async fn a_redelivered_trigger_is_still_one_trigger() {
    let (store, _dir) = open_store().await;
    let id = Uuid::now_v7();
    let first = at(
        named_triggered("alpha", id, json!({"n": 1})),
        "2026-01-01T00:00:00Z",
    );
    let second = at(
        named_triggered("alpha", id, json!({"n": 1})),
        "2026-01-02T00:00:00Z",
    );
    store.insert_event(&first, Some(10)).await.unwrap();
    store.insert_event(&second, Some(20)).await.unwrap();

    let listed = store.query_triggers(None, None, 50).await.unwrap();
    assert_eq!(listed.len(), 1, "one trigger, however many records of it");
    assert_eq!(listed[0].recorded_at, "2026-01-01T00:00:00+00:00");
    // …and the cursor is the first record's too, so a stream that has
    // already passed it does not hand it back a second time.
    let streamed = store.triggers_from(None, None, 11, 64).await.unwrap();
    assert!(
        streamed.is_empty(),
        "a redelivery must not re-notify the same trigger; got {streamed:?}"
    );
}

/// A dead-lettered trigger is recorded like any other. It may have no
/// `triggered` event at all — a trigger for an unknown agent never
/// starts an invocation — so this is the only record it ever gets.
#[tokio::test]
async fn a_dead_lettered_trigger_is_recorded() {
    let (store, _dir) = open_store().await;
    let id = Uuid::now_v7();
    let event = Event::new(
        aid("alpha"),
        Uuid::now_v7(),
        EventPayload::Failed(FailedPayload {
            error_kind: FailureKind::TriggerExhausted,
            error_message: "trigger exhausted after 5 deliveries (limit 5)".into(),
            phase: FailurePhase::Setup,
            partial_totals: InvocationTotals::default(),
        }),
    )
    .annotate(
        crate::dead_letter::DEAD_LETTER_TRIGGER_ID_KEY,
        json!(id.to_string()),
    )
    .annotate(
        crate::dead_letter::DEAD_LETTER_SUBJECT_KEY,
        json!("fq.trigger.alpha"),
    )
    .annotate(
        crate::dead_letter::DEAD_LETTER_PAYLOAD_KEY,
        json!({"task": "the one that died"}),
    );
    store.insert_event(&event, Some(5)).await.unwrap();

    let trigger = store
        .trigger(&id.to_string())
        .await
        .unwrap()
        .expect("a dead-lettered trigger is still gettable");
    assert_eq!(trigger.payload, json!({"task": "the one that died"}));
}

/// The trigger a requeue mints, as the store records it.
fn requeue_of(original: Uuid, agent: &str, payload: serde_json::Value) -> crate::trigger::Trigger {
    use crate::trigger::Trigger;
    Trigger::requeue_of(
        &Trigger::named(
            original,
            TriggerSource::Subject,
            Some(crate::events::subjects::trigger(agent)),
            payload,
        ),
        crate::events::subjects::trigger(agent),
    )
}

/// **A dead letter is requeued at most once, and the database is what
/// says so.** The reservation is the check: the second claim on the
/// same original loses, and what it lost to is nameable.
///
/// Asserted at this layer because the guarantee lives here. A check the
/// caller performs before writing would pass this file's obvious test
/// and still let two concurrent requeues both publish; a claim that IS
/// the write cannot.
#[tokio::test]
async fn a_dead_letter_can_be_claimed_for_requeue_only_once() {
    let (store, _dir) = open_store().await;
    let original = Uuid::now_v7();
    let first = requeue_of(original, "alpha", json!({"n": 1}));
    let second = requeue_of(original, "alpha", json!({"n": 1}));
    assert_ne!(first.id, second.id, "two attempts, two candidate triggers");

    assert!(
        store
            .reserve_requeue(&first, "alpha", "2026-01-01T00:00:00+00:00")
            .await
            .unwrap(),
        "the first claim wins"
    );
    assert!(
        !store
            .reserve_requeue(&second, "alpha", "2026-01-02T00:00:00+00:00")
            .await
            .unwrap(),
        "the second claim on the same original must lose"
    );
    assert_eq!(
        store.requeue_of(&original.to_string()).await.unwrap(),
        Some(first.id),
        "and the loser can name the winner — which is what its refusal carries"
    );
    // The losing candidate was never written, so nothing points at it.
    assert!(
        store
            .trigger(&second.id.to_string())
            .await
            .unwrap()
            .is_none()
    );

    // The winner is a whole, gettable trigger that remembers where it
    // came from — and the original is untouched by any of this.
    let requeued = store
        .trigger(&first.id.to_string())
        .await
        .unwrap()
        .expect("the reserved trigger is a record like any other");
    assert_eq!(requeued.requeued_from, Some(original));
    assert_eq!(requeued.payload, json!({"n": 1}));
    assert_eq!(
        store.requeue_of(&Uuid::now_v7().to_string()).await.unwrap(),
        None,
        "a trigger nobody requeued has no requeue"
    );
}

/// The uniqueness is on *requeues*, and constrains nothing else.
///
/// SQLite lets any number of rows hold NULL in a unique index, which is
/// the whole reason the column can live on `triggers` rather than in a
/// side table: ordinary triggers — every trigger — are unaffected, and
/// two different dead letters can each be requeued.
#[tokio::test]
async fn the_uniqueness_binds_requeues_and_nothing_else() {
    let (store, _dir) = open_store().await;
    for i in 0..3 {
        store
            .insert_event(
                &named_triggered("alpha", Uuid::now_v7(), json!({ "n": i })),
                Some(i + 1),
            )
            .await
            .unwrap();
    }
    assert_eq!(
        store.query_triggers(None, None, 50).await.unwrap().len(),
        3,
        "three triggers, none of them a requeue, all with NULL lineage"
    );

    for original in [Uuid::now_v7(), Uuid::now_v7()] {
        assert!(
            store
                .reserve_requeue(
                    &requeue_of(original, "alpha", json!({"n": 9})),
                    "alpha",
                    "2026-01-01T00:00:00+00:00"
                )
                .await
                .unwrap(),
            "different dead letters are different claims"
        );
    }
    assert_eq!(store.query_triggers(None, None, 50).await.unwrap().len(), 5);
}

/// A claim whose publish never landed is given back — and the release
/// cannot reach anything that is not that claim.
///
/// `triggers` is the one table retention never touches, so a general
/// "delete a trigger" would be a hole in that promise. This one is
/// scoped to a row that is a requeue *of the original named*, which is
/// the only row a failed publish could have created.
#[tokio::test]
async fn a_released_claim_can_be_made_again_and_reaches_nothing_else() {
    let (store, _dir) = open_store().await;
    let original = Uuid::now_v7();
    let claimed = requeue_of(original, "alpha", json!({"n": 1}));
    store
        .reserve_requeue(&claimed, "alpha", "2026-01-01T00:00:00+00:00")
        .await
        .unwrap();

    // A release naming the wrong original removes nothing: the scope is
    // the pair, so a mistaken id cannot delete a real record.
    store
        .release_requeue(claimed.id, Uuid::now_v7())
        .await
        .unwrap();
    assert!(
        store
            .trigger(&claimed.id.to_string())
            .await
            .unwrap()
            .is_some()
    );

    // An ordinary trigger is out of reach even when named exactly.
    let ordinary = Uuid::now_v7();
    store
        .insert_event(
            &named_triggered("alpha", ordinary, json!({"n": 2})),
            Some(1),
        )
        .await
        .unwrap();
    store.release_requeue(ordinary, original).await.unwrap();
    assert!(
        store
            .trigger(&ordinary.to_string())
            .await
            .unwrap()
            .is_some(),
        "a recorded trigger is not a reservation and must survive"
    );

    // The real release frees the key, so the operator can try again.
    store.release_requeue(claimed.id, original).await.unwrap();
    assert_eq!(store.requeue_of(&original.to_string()).await.unwrap(), None);
    let retry = requeue_of(original, "alpha", json!({"n": 1}));
    assert!(
        store
            .reserve_requeue(&retry, "alpha", "2026-01-03T00:00:00+00:00")
            .await
            .unwrap()
    );
}

/// **A requeued trigger becomes streamable once something names it on
/// the log.** Its row is written at publish time, before any record
/// exists, so it starts with no cursor; the record that arrives later
/// fills the position in and changes nothing else.
///
/// Without this the requeued trigger would list and get but never
/// stream — a permanent hole in the one overlay that says "something
/// new arrived", on exactly the triggers an operator is watching for.
#[tokio::test]
async fn a_later_record_gives_a_reserved_trigger_its_cursor() {
    let (store, _dir) = open_store().await;
    let original = Uuid::now_v7();
    let requeued = requeue_of(original, "alpha", json!({"n": 1}));
    store
        .reserve_requeue(&requeued, "alpha", "2026-01-01T00:00:00+00:00")
        .await
        .unwrap();
    assert!(
        store
            .triggers_from(None, None, 1, 64)
            .await
            .unwrap()
            .is_empty(),
        "a row with no log position has no cursor it could honestly be served at"
    );

    // The dispatcher runs it; the projection folds the `triggered`
    // event. First-record-wins keeps everything the requeue recorded —
    // the payload it re-ran, the lineage, when it was reserved — and
    // the one thing that was unknown becomes known.
    let recorded = at(
        named_triggered("alpha", requeued.id, json!({"different": true})),
        "2026-06-01T00:00:00Z",
    );
    store.insert_event(&recorded, Some(42)).await.unwrap();

    let streamed = store.triggers_from(None, None, 1, 64).await.unwrap();
    assert_eq!(streamed.len(), 1, "now it streams");
    assert_eq!(streamed[0].0, 42, "at the position the record gave it");
    assert_eq!(streamed[0].1.requeued_from, Some(original));
    assert_eq!(
        streamed[0].1.payload,
        json!({"n": 1}),
        "first record wins on everything that describes the trigger"
    );
    assert_eq!(
        store.query_triggers(None, None, 50).await.unwrap()[0].recorded_at,
        "2026-01-01T00:00:00+00:00",
        "including when the runtime took responsibility for it"
    );
    // A third record does not move the cursor either: the fill-in is
    // for an unknown position, not a last-write-wins.
    store
        .insert_event(
            &named_triggered("alpha", requeued.id, json!({"n": 1})),
            Some(99),
        )
        .await
        .unwrap();
    assert_eq!(
        store.triggers_from(None, None, 1, 64).await.unwrap()[0].0,
        42
    );
}

/// **A trigger is kept forever.** The sweep clears the event row that
/// recorded it — a `triggered` event bears no cost, so nothing exempts
/// it — and the trigger itself is untouched, because the sweep only
/// ever deletes from `events`. Same intent as the cost exemption,
/// reached structurally instead of by a second predicate.
#[tokio::test]
async fn the_retention_sweep_never_removes_a_trigger() {
    let (store, _dir) = open_store().await;
    let id = Uuid::now_v7();
    let event = at(
        named_triggered("alpha", id, json!({"task": "ancient"})),
        "2020-01-01T00:00:00Z",
    );
    store.insert_event(&event, Some(3)).await.unwrap();

    let cutoff = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .timestamp_millis();
    assert_eq!(
        store.sweep_events(cutoff).await.unwrap(),
        1,
        "the event row is swept, being older than the cutoff and cost-free"
    );
    assert_eq!(store.count().await.unwrap(), 0);
    assert!(
        store.trigger(&id.to_string()).await.unwrap().is_some(),
        "the trigger outlives the event that recorded it"
    );
}

/// Each declared filter axis narrows, and they compose. The listing is
/// newest-first and carries no payload — the index/state split the atom
/// declares.
#[tokio::test]
async fn a_listing_honours_each_declared_axis() {
    let (store, _dir) = open_store().await;
    let mine_old = Uuid::now_v7();
    let mine_new = Uuid::now_v7();
    let theirs = Uuid::now_v7();
    for (id, agent, ts) in [
        (mine_old, "alpha", "2026-01-01T00:00:00Z"),
        (mine_new, "alpha", "2026-03-01T00:00:00Z"),
        (theirs, "beta", "2026-03-02T00:00:00Z"),
    ] {
        store
            .insert_event(&at(named_triggered(agent, id, json!({})), ts), Some(1))
            .await
            .unwrap();
    }
    let ids = |rows: Vec<crate::trigger::TriggerView>| {
        rows.into_iter().map(|r| r.trigger_id).collect::<Vec<_>>()
    };

    assert_eq!(
        ids(store.query_triggers(None, None, 50).await.unwrap()),
        vec![
            theirs.to_string(),
            mine_new.to_string(),
            mine_old.to_string()
        ],
        "unnarrowed, newest recorded first"
    );
    assert_eq!(
        ids(store.query_triggers(Some("alpha"), None, 50).await.unwrap()),
        vec![mine_new.to_string(), mine_old.to_string()],
        "`agent` narrows to one agent's triggers"
    );
    assert_eq!(
        ids(store
            .query_triggers(None, Some("2026-02-01T00:00:00+00:00"), 50)
            .await
            .unwrap()),
        vec![theirs.to_string(), mine_new.to_string()],
        "`since` is a lower bound on when the trigger was recorded"
    );
    assert_eq!(
        ids(store
            .query_triggers(Some("alpha"), Some("2026-02-01T00:00:00+00:00"), 50)
            .await
            .unwrap()),
        vec![mine_new.to_string()],
        "the two axes compose"
    );
    assert_eq!(
        ids(store.query_triggers(None, None, 1).await.unwrap()),
        vec![theirs.to_string()],
        "`limit` is the caller's own bound, applied to the newest"
    );
}

/// The stream reads the same population under the same narrowing, in
/// sequence order — the seam a List/Stream pair composes on — and skips
/// a record that arrived with no log position, because there is no
/// cursor it could honestly be handed back at.
#[tokio::test]
async fn a_stream_page_is_the_same_population_in_cursor_order() {
    let (store, _dir) = open_store().await;
    let (first, second, unpositioned) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    store
        .insert_event(&named_triggered("alpha", first, json!({"n": 1})), Some(10))
        .await
        .unwrap();
    store
        .insert_event(&named_triggered("beta", second, json!({"n": 2})), Some(20))
        .await
        .unwrap();
    store
        .insert_event(&named_triggered("alpha", unpositioned, json!({})), None)
        .await
        .unwrap();

    let page = store.triggers_from(None, None, 0, 64).await.unwrap();
    assert_eq!(
        page.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
        vec![10, 20],
        "sequence order, and no row without a position"
    );
    assert_eq!(
        page[0].1.payload,
        json!({"n": 1}),
        "a stream item is the whole trigger, payload included"
    );
    assert_eq!(
        store
            .triggers_from(Some("beta"), None, 0, 64)
            .await
            .unwrap()
            .len(),
        1,
        "the same narrowing applies to the stream"
    );
    assert_eq!(
        store.triggers_from(None, None, 11, 64).await.unwrap().len(),
        1,
        "a cursor resumes past what it has already seen"
    );
    assert_eq!(
        store.max_trigger_seq().await.unwrap(),
        20,
        "the tail is this population's, not the log's"
    );
}

/// **A cursor beyond every sequence answers with nothing, not with
/// everything.**
///
/// `from_seq` arrives as a `u64` straight off the wire and the column
/// is a SQLite `INTEGER`, so an unguarded `as i64` turns anything above
/// `i64::MAX` negative — and `seq >= <negative>` matches every row.
/// The caller would get the *oldest* page back for a cursor from beyond
/// the end of the log, and a `next_from_seq` far behind the one it
/// asked with: silent re-delivery plus a cursor that went backwards,
/// from a value any client can send. The three probes are the boundary
/// itself and the two sentinels either side of it.
#[tokio::test]
async fn a_cursor_past_every_sequence_answers_empty_rather_than_wrapping() {
    let (store, _dir) = open_store().await;
    store
        .insert_event(
            &named_triggered("alpha", Uuid::now_v7(), json!({"n": 1})),
            Some(10),
        )
        .await
        .unwrap();
    assert_eq!(
        store.triggers_from(None, None, 10, 64).await.unwrap().len(),
        1,
        "the fixture is only interesting if an in-range cursor finds it"
    );
    for cursor in [i64::MAX as u64, i64::MAX as u64 + 1, u64::MAX] {
        assert!(
            store
                .triggers_from(None, None, cursor, 64)
                .await
                .unwrap()
                .is_empty(),
            "a cursor at {cursor} must answer with nothing, not replay the table"
        );
    }
}

/// A read-only handle cannot migrate, so it says which upgrade is
/// missing rather than letting SQLite report a table nobody asked for.
#[tokio::test]
async fn read_only_open_names_a_missing_triggers_table() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("projection.db");
    {
        let writer = ProjectionStore::open(&path).await.unwrap();
        sqlx::query("DROP TABLE triggers")
            .execute(&writer.pool)
            .await
            .unwrap();
    }
    let err = ProjectionStore::open_read_only(&path).await.unwrap_err();
    assert!(
        matches!(&err, StoreError::SchemaOutdated { missing, .. }
            if missing.contains("triggers")),
        "expected SchemaOutdated naming the table, got: {err:?}"
    );
}

/// **A database whose `triggers` table predates `requeued_from` is
/// widened, and only then indexed.**
///
/// `CREATE TABLE IF NOT EXISTS` cannot widen a table that exists, so
/// the column arrives by `ALTER`; and the UNIQUE index on it has to be
/// created *after* that, or it names a column the older file does not
/// have. Reopening is the test: the first open below leaves the
/// pre-column shape, the second must migrate it and still be able to
/// claim a requeue.
///
/// The read-only handle is checked in the same breath, for the reason
/// its sibling above exists: it cannot migrate, and every trigger read
/// selects this column, so it must name the upgrade rather than let a
/// driver report SQL the operator never wrote.
#[tokio::test]
async fn an_older_triggers_table_gains_the_requeue_column_before_its_index() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("projection.db");
    {
        let writer = ProjectionStore::open(&path).await.unwrap();
        // Reproduce the pre-requeue shape exactly: the column and the
        // index that depends on it, both gone.
        sqlx::query("DROP INDEX idx_triggers_requeued_from")
            .execute(&writer.pool)
            .await
            .unwrap();
        sqlx::query("ALTER TABLE triggers DROP COLUMN requeued_from")
            .execute(&writer.pool)
            .await
            .unwrap();

        let err = ProjectionStore::open_read_only(&path).await.unwrap_err();
        assert!(
            matches!(&err, StoreError::SchemaOutdated { missing, .. }
                if missing.contains("requeued_from")),
            "a handle that cannot migrate must name the column, got: {err:?}"
        );
    }

    let store = ProjectionStore::open(&path).await.unwrap();
    let original = Uuid::now_v7();
    let requeued = requeue_of(original, "alpha", json!({"n": 1}));
    assert!(
        store
            .reserve_requeue(&requeued, "alpha", "2026-01-01T00:00:00+00:00")
            .await
            .unwrap()
    );
    // The index came with the column, so the guarantee holds on a
    // migrated database exactly as on a fresh one.
    assert!(
        !store
            .reserve_requeue(
                &requeue_of(original, "alpha", json!({"n": 1})),
                "alpha",
                "2026-01-02T00:00:00+00:00"
            )
            .await
            .unwrap(),
        "the UNIQUE index must exist after the migration, not only after a fresh create"
    );
    ProjectionStore::open_read_only(&path)
        .await
        .expect("a migrated database reads");
}
