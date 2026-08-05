//! Round bookkeeping for the reducer (Phase 3d): the model-turn
//! count stamped onto turn-bearing events, per driven invocation.
//! One Round is an assistant action plus the tool results it
//! initiated — the count `max_iterations` gates. Split from
//! `runner.rs` to keep that file inside its size budget.

use std::collections::HashMap;
use std::sync::Mutex;

use uuid::Uuid;

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
    /// the (completed calls, summed cost) pair the totals need — one
    /// pass serves both.
    ///
    /// The three counts have different filters on purpose. `calls`
    /// feeds `total_llm_calls`, which counts turns that produced an
    /// outcome, so errored rows are excluded. `cost` sums *every*
    /// completed row: since #447 an errored row can carry real spend
    /// (an empty completion still bills for the prefill), and dropping
    /// it here would forget that money on every resume — the budget
    /// accumulator is reconstituted from exactly this column. The Round
    /// seed likewise counts every completed row, because a failed call
    /// consumes a Round: a resume that seeded from successes alone
    /// would re-issue Round numbers the pre-crash run already spent.
    pub(crate) fn seed_from_wal(
        &self,
        invocation_id: Uuid,
        llms: &[crate::worker::store::LlmDispatchRow],
    ) -> (u32, f64) {
        use crate::worker::store::DispatchStatus;
        let mut calls = 0u32;
        let mut rounds = 0u64;
        let mut cost = 0.0f64;
        for r in llms {
            if r.status != DispatchStatus::Completed {
                continue;
            }
            rounds += 1;
            cost += r.cost_usd.unwrap_or(0.0);
            if r.is_error != Some(true) {
                calls += 1;
            }
        }
        self.seed(invocation_id, rounds);
        (calls, cost)
    }

    /// Drop the counter when the invocation leaves the runner.
    pub(crate) fn forget(&self, invocation_id: Uuid) {
        self.rounds
            .lock()
            .expect("rounds lock poisoned")
            .remove(&invocation_id);
    }
}
