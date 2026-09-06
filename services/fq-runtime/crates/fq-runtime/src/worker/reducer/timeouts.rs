//! How many tool calls have timed out in a row, per driven invocation.
//!
//! A single timed-out call is reported to the model as a tool error, so
//! an agent can route around a slow tool. That is the right answer once
//! and the wrong answer forever: against a dead MCP server every call
//! times out, and an agent that keeps trying spends its whole budget
//! one deadline at a time. This counts the run so
//! `[tools] max_consecutive_timeouts` can end the invocation instead
//! (<https://github.com/bricef/factor-q/issues/547>).
//!
//! Keyed by invocation because one runner services concurrent
//! invocations, and dropped by the runner's active-guard — the same
//! shape, and the same lifetime, as [`RoundLedger`](super::rounds).

use std::collections::HashMap;
use std::sync::Mutex;

use uuid::Uuid;

/// Per-invocation consecutive-timeout counters.
#[derive(Default)]
pub(crate) struct TimeoutLedger {
    streaks: Mutex<HashMap<Uuid, u32>>,
}

impl TimeoutLedger {
    /// Count one timed-out call and return the length of the run it
    /// belongs to (1 for the first).
    pub(crate) fn record_timeout(&self, invocation_id: Uuid) -> u32 {
        let mut streaks = self.streaks.lock().expect("timeout ledger poisoned");
        let streak = streaks.entry(invocation_id).or_insert(0);
        *streak += 1;
        *streak
    }

    /// A call came back. Success or a tool-reported error both count:
    /// either way the tool answered, which is the thing a run of
    /// timeouts says is not happening.
    pub(crate) fn record_answer(&self, invocation_id: Uuid) {
        self.streaks
            .lock()
            .expect("timeout ledger poisoned")
            .remove(&invocation_id);
    }

    /// Drop the counter when the invocation leaves the runner.
    pub(crate) fn forget(&self, invocation_id: Uuid) {
        self.streaks
            .lock()
            .expect("timeout ledger poisoned")
            .remove(&invocation_id);
    }
}
