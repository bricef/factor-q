//! Round bookkeeping for the reducer (Phase 3d): the model-turn
//! count stamped onto turn-bearing events, per driven invocation.
//! One Round is an assistant action plus the tool results it
//! initiated — the count `max_iterations` gates. Split from
//! `runner.rs` to keep that file inside its size budget.

use std::collections::HashMap;
use std::sync::Mutex;

use uuid::Uuid;

use crate::events::{InvocationTotals, LlmCallOrigin};

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

    /// The Round an LLM call's outcome (`llm.response` or `llm.failure`)
    /// is stamped with. An agent turn is a new model turn, so it opens
    /// the next Round. A server-initiated call — sampling or elicitation
    /// — runs inside a tool call of the current Round, so it belongs to
    /// that Round and does not advance it (#824): advancing would number
    /// the tool's own result, and every later turn, one Round high, and
    /// a resume (which seeds from agent-turn rows, [`Self::seed_from_wal`])
    /// would then re-issue Round numbers the live run had spent. The
    /// match is exhaustive so a new origin has to decide.
    pub(crate) fn for_call(&self, invocation_id: Uuid, origin: &LlmCallOrigin) -> u64 {
        match origin {
            LlmCallOrigin::AgentTurn => self.next(invocation_id),
            LlmCallOrigin::Sampling { .. } | LlmCallOrigin::Elicitation { .. } => {
                self.current(invocation_id)
            }
        }
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
mod tests;
