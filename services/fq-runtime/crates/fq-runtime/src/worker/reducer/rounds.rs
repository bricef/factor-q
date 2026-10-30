//! Round bookkeeping for the reducer (Phase 3d): the model-turn
//! count stamped onto turn-bearing events, per driven invocation.
//! One Round is an assistant action plus the tool results it
//! initiated — the count `max_iterations` gates. Split from
//! `runner.rs` to keep that file inside its size budget.

use std::collections::HashMap;
use std::sync::Mutex;

use uuid::Uuid;

use crate::events::InvocationTotals;

/// The per-invocation Round counters. Keyed by invocation because
/// one runner services concurrent invocations; entries are seeded
/// from the WAL on resume and dropped by the runner's active-guard.
#[derive(Default)]
pub(crate) struct RoundLedger {
    rounds: Mutex<HashMap<Uuid, u64>>,
}

impl RoundLedger {
    /// The Round a turn-bearing event belongs to right now (0 before
    /// the first response).
    pub(crate) fn current(&self, invocation_id: Uuid) -> u64 {
        *self
            .rounds
            .lock()
            .expect("rounds lock poisoned")
            .get(&invocation_id)
            .unwrap_or(&0)
    }

    /// Advance to the next Round (a new model turn) and return it.
    pub(crate) fn next(&self, invocation_id: Uuid) -> u64 {
        let mut rounds = self.rounds.lock().expect("rounds lock poisoned");
        let round = rounds.entry(invocation_id).or_insert(0);
        *round += 1;
        *round
    }

    /// Seed on resume from the WAL's completed-call count.
    pub(crate) fn seed(&self, invocation_id: Uuid, completed_llm_calls: u64) {
        self.rounds
            .lock()
            .expect("rounds lock poisoned")
            .insert(invocation_id, completed_llm_calls);
    }

    /// Seed from the WAL's completed-call rows on resume, returning
    /// the agent-call count plus total and server-origin cost buckets —
    /// one pass serves the round ledger and invocation totals.
    ///
    /// The counters have different filters on purpose. `calls`
    /// feeds `total_llm_calls`, which counts turns that produced an
    /// outcome, so errored rows are excluded. `cost` sums *every*
    /// completed row: since #447 an errored row can carry real spend
    /// (an empty completion still bills for the prefill), and dropping
    /// it here would forget that money on every resume — the budget
    /// accumulator is reconstituted from exactly this column. The Round
    /// seed likewise counts every completed agent-turn row, because a
    /// failed agent call consumes a Round. Server-origin calls consume
    /// cost but are nested inside tools, so they consume no reducer Round.
    pub(crate) fn seed_from_wal(
        &self,
        invocation_id: Uuid,
        llms: &[crate::worker::store::LlmDispatchRow],
    ) -> InvocationTotals {
        use crate::events::LlmCallOrigin;
        use crate::worker::store::DispatchStatus;
        let mut calls = 0u32;
        let mut rounds = 0u64;
        let mut total_cost = 0.0f64;
        let mut sampling_cost = 0.0f64;
        let mut elicitation_cost = 0.0f64;
        for row in llms {
            if row.status != DispatchStatus::Completed {
                continue;
            }
            let cost = row.cost_usd.unwrap_or(0.0);
            total_cost += cost;
            match &row.origin {
                LlmCallOrigin::AgentTurn => {
                    rounds += 1;
                    if row.is_error != Some(true) {
                        calls += 1;
                    }
                }
                LlmCallOrigin::Sampling { .. } => sampling_cost += cost,
                LlmCallOrigin::Elicitation { .. } => elicitation_cost += cost,
            }
        }
        self.seed(invocation_id, rounds);
        InvocationTotals {
            total_llm_calls: calls,
            total_cost,
            sampling_cost,
            elicitation_cost,
            ..InvocationTotals::default()
        }
    }

    /// Drop the counter when the invocation leaves the runner.
    pub(crate) fn forget(&self, invocation_id: Uuid) {
        self.rounds
            .lock()
            .expect("rounds lock poisoned")
            .remove(&invocation_id);
    }
}

#[cfg(test)]
mod tests {
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
}
