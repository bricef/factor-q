//! Unit tests for the Round ledger (`rounds.rs`).

use super::*;
use crate::events::LlmCallOrigin;
use crate::worker::store::{DispatchStatus, LlmDispatchRow};

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
