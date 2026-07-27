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
    pub(crate) fn seed_from_wal(
        &self,
        invocation_id: Uuid,
        llms: &[crate::worker::store::LlmDispatchRow],
    ) -> (u32, f64) {
        use crate::worker::store::DispatchStatus;
        let mut calls = 0u32;
        let mut cost = 0.0f64;
        for r in llms {
            if r.status == DispatchStatus::Completed && r.is_error != Some(true) {
                calls += 1;
                cost += r.cost_usd.unwrap_or(0.0);
            }
        }
        self.seed(invocation_id, u64::from(calls));
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
