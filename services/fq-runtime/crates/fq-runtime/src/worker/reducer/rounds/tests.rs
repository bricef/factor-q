//! Unit tests for the Round ledger (`rounds.rs`).

use super::*;
use crate::events::LlmCallOrigin;
use crate::worker::store::{DispatchStatus, LlmDispatchRow, ToolDispatchRow};

fn completed(origin: LlmCallOrigin, cost: f64, is_error: bool) -> LlmDispatchRow {
    LlmDispatchRow {
        invocation_id: "inv".to_string(),
        request_id: format!("request-{cost}"),
        model: "model".to_string(),
        status: DispatchStatus::Completed,
        request_payload: "{}".to_string(),
        response: None,
        cost_usd: Some(cost),
        origin,
        is_error: Some(is_error),
        intent_at: 1,
        dispatched_at: Some(2),
        completed_at: Some(3),
        seq: Some(1),
        deferred_at: None,
    }
}

#[test]
fn wal_seed_counts_only_agent_turns_but_restores_all_cost_buckets() {
    let ledger = RoundLedger::default();
    let invocation_id = Uuid::now_v7();
    let rows = vec![
        completed(LlmCallOrigin::AgentTurn, 1.0, false),
        completed(
            LlmCallOrigin::Sampling {
                server: "sampling".to_string(),
            },
            2.0,
            false,
        ),
        completed(
            LlmCallOrigin::Elicitation {
                server: "elicitation".to_string(),
            },
            3.0,
            true,
        ),
    ];

    let totals = ledger.seed_from_wal(invocation_id, &rows);
    assert_eq!(totals.total_llm_calls, 1);
    assert_eq!(totals.total_cost, 6.0);
    assert_eq!(totals.sampling_cost, 2.0);
    assert_eq!(totals.elicitation_cost, 3.0);
    assert_eq!(ledger.next(invocation_id), 2);
}

#[test]
fn only_agent_turns_open_a_round_live() {
    let ledger = RoundLedger::default();
    let invocation_id = Uuid::now_v7();
    let sampling = LlmCallOrigin::Sampling {
        server: "srv".to_string(),
    };
    let elicitation = LlmCallOrigin::Elicitation {
        server: "srv".to_string(),
    };

    assert_eq!(ledger.for_call(invocation_id, &LlmCallOrigin::AgentTurn), 1);
    // A sampling or elicitation call inside round 1's tool call belongs
    // to round 1, and leaves the next agent turn to open round 2.
    assert_eq!(ledger.for_call(invocation_id, &sampling), 1);
    assert_eq!(ledger.for_call(invocation_id, &elicitation), 1);
    assert_eq!(ledger.current(invocation_id), 1);
    assert_eq!(ledger.for_call(invocation_id, &LlmCallOrigin::AgentTurn), 2);
}

/// The live ledger and the resume seed agree: a resume after
/// `[agent, sampling, elicitation, agent]` continues where the live run
/// would, instead of re-issuing a spent Round.
#[test]
fn the_resume_seed_matches_the_live_ledger() {
    let origins = [
        LlmCallOrigin::AgentTurn,
        LlmCallOrigin::Sampling {
            server: "srv".to_string(),
        },
        LlmCallOrigin::Elicitation {
            server: "srv".to_string(),
        },
        LlmCallOrigin::AgentTurn,
    ];
    let live = RoundLedger::default();
    let live_id = Uuid::now_v7();
    for origin in &origins {
        live.for_call(live_id, origin);
    }

    let resumed = RoundLedger::default();
    let resumed_id = Uuid::now_v7();
    let rows: Vec<_> = origins
        .iter()
        .map(|origin| completed(origin.clone(), 0.0, false))
        .collect();
    resumed.seed_from_wal(resumed_id, &rows);

    assert_eq!(resumed.current(resumed_id), live.current(live_id));
    assert_eq!(resumed.next(resumed_id), live.next(live_id));
}

/// An errored agent turn, an empty completion say, cost money and took a
/// Round but produced no outcome, so it is not a call; a call still open
/// is none of the three. The rule a resume seeds a run's totals from.
#[test]
fn an_errored_agent_turn_costs_and_takes_a_round_but_is_not_a_call() {
    let mut open = completed(LlmCallOrigin::AgentTurn, 0.0, false);
    open.status = DispatchStatus::Dispatched;
    open.cost_usd = None;
    open.is_error = None;
    let rows = vec![
        completed(LlmCallOrigin::AgentTurn, 1.0, false),
        completed(LlmCallOrigin::AgentTurn, 0.5, true),
        open,
    ];
    let (totals, rounds) = totals_of_llm_rows(&rows);
    assert_eq!(totals.total_llm_calls, 1);
    assert_eq!(totals.total_cost, 1.5);
    assert_eq!(rounds, 2);
}

fn tool(status: DispatchStatus, is_error: bool) -> ToolDispatchRow {
    let done = status == DispatchStatus::Completed;
    ToolDispatchRow {
        invocation_id: "inv".to_string(),
        tool_call_id: format!("tool-{status:?}-{is_error}"),
        tool_name: "tool".to_string(),
        status,
        parameters: "{}".to_string(),
        result: done.then(|| "the tool ran".to_string()),
        is_error: done.then_some(is_error),
        intent_at: 1,
        dispatched_at: (status != DispatchStatus::Intent).then_some(2),
        completed_at: done.then_some(3),
        seq: done.then_some(1),
    }
}

/// A run's totals from all its rows: its model calls as
/// `totals_of_llm_rows` counts them, and every tool call that completed,
/// an errored one included. A tool row still `intent` or `dispatched` has
/// no outcome and is not counted. No row records a duration.
#[test]
fn a_runs_wal_totals_count_only_the_tool_calls_that_completed() {
    let llms = vec![
        completed(LlmCallOrigin::AgentTurn, 1.0, false),
        completed(LlmCallOrigin::AgentTurn, 0.5, true),
        completed(
            LlmCallOrigin::Sampling {
                server: "srv".to_string(),
            },
            0.25,
            false,
        ),
    ];
    let tools = vec![
        tool(DispatchStatus::Completed, false),
        tool(DispatchStatus::Completed, true),
        tool(DispatchStatus::Dispatched, false),
        tool(DispatchStatus::Intent, false),
    ];
    let totals = totals_of_wal_rows(&llms, &tools);
    let (of_llms, _rounds) = totals_of_llm_rows(&llms);
    assert_eq!(totals.total_tool_calls, 2, "the two completed tool calls");
    assert_eq!(
        (
            totals.total_llm_calls,
            totals.total_cost,
            totals.sampling_cost,
            totals.elicitation_cost
        ),
        (
            of_llms.total_llm_calls,
            of_llms.total_cost,
            of_llms.sampling_cost,
            of_llms.elicitation_cost
        ),
        "the model calls' totals, as totals_of_llm_rows gives them"
    );
    assert_eq!((totals.total_llm_calls, totals.total_cost), (1, 1.75));
    assert_eq!(totals.total_duration_ms, 0);
}
