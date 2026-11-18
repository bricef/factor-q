//! `invocation.ending_held`: the ending of a run whose own ending event
//! the worker holds (issue 1071).
//!
//! A run's ending reaches the control plane as its own `completed` or
//! `failed`. When the run's finishing write landed and that publish was
//! lost, the archive sweeper republishes the event the run stored in its
//! WAL row. Sometimes it cannot: this build refuses to republish it (an
//! envelope version it does not read), or the broker refuses it for its
//! size. The sweeper then holds the row, and this event says instead that
//! the run ended and how. It copies nothing from the stored event, so it
//! has a fixed, small shape this build can always publish.

use serde::{Deserialize, Serialize};

use crate::worker::WorkerId;

/// Payload for [`EventPayload::InvocationEndingHeld`](crate::events::EventPayload::InvocationEndingHeld).
/// The run's agent and invocation ride the envelope. Folded into the
/// ownership row as the ending it stands for, with the archive pending.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvocationEndingHeldPayload {
    /// How the run ended, from the worker's row: the phase its own
    /// finishing write set together with the ending it stored.
    pub outcome: HeldOutcome,
    /// When the run finished: the worker row's `terminal_at` (unix ms),
    /// not this event's own time, which can be much later.
    pub ended_at_ms: i64,
    /// When the run started: the worker row's `started_at` (unix ms).
    pub started_at_ms: i64,
    /// The worker holding the row, which a row the fold creates names as
    /// its owner.
    pub worker_id: WorkerId,
    /// Why the stored ending cannot go out.
    pub reason: EndingHeldReason,
    /// The run's totals as the worker's own records give them, when it
    /// still has them: recomputed from its per-call rows, not copied
    /// from the stored ending. Each model call's usage and cost went out
    /// on its own `llm.response` or `llm.failure` when it was made; this
    /// is the run-level copy, which no cost report sums.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totals: Option<RecordedTotals>,
}

/// The ending an [`InvocationEndingHeldPayload`] stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeldOutcome {
    Completed,
    Failed,
}

/// Why the worker holds a run's stored ending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndingHeldReason {
    /// This build refuses to republish the stored event: an envelope
    /// version it does not read, or not this run's ending.
    StoredEventRefused,
    /// The broker refuses the stored event for its size, as when
    /// `max_payload` was lowered after the run stored it.
    PayloadTooLarge,
    /// A reason a newer build added. Nothing writes it.
    #[serde(other)]
    Unknown,
}

/// A run's totals as the worker's per-call rows give them: the model
/// calls that produced an outcome, the tool calls that completed, and
/// what every completed model call cost: the rule a resumed run's
/// totals are rebuilt by. A run's duration is not among
/// them: the duration a run's ending carries is its last attempt's, which
/// the rows do not hold.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RecordedTotals {
    pub total_llm_calls: u32,
    pub total_tool_calls: u32,
    pub total_cost: f64,
}
