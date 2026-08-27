//! Reconstructing a capability stream from the WAL — ordering the
//! completed rows, regrouping them into the turns the live loop
//! emitted, and cutting a batch the crash landed inside.
//!
//! Extracted from `runner.rs` (#78). These are pure functions over
//! recorded rows: no `self`, no bus, no store, no clock. That is what
//! makes resume testable without a broker, and it is why this block
//! moves first — it is the one seam in `runner.rs` that needs nothing
//! else to move with it.
//!
//! The caller is `ReducerRunner::resume`, which reads the WAL, sorts
//! ([`sort_into_replay_order`]), cuts a partial trailing batch
//! ([`truncate_incomplete_final_batch`]) and coalesces
//! ([`coalesce_tool_results`]) before feeding the result to the
//! harness.

use crate::worker::reducer::types::{CapabilityResult, ToolCallResult};

/// The two candidate orderings for one completed WAL row: the v9 shared
/// completion `seq` (None on pre-v9 legacy rows) and the row's
/// `completed_at` timestamp.
pub(super) fn replay_sort_key(seq: Option<i64>, completed_at: Option<i64>) -> (Option<i64>, i64) {
    (seq, completed_at.unwrap_or(0))
}

/// Sort completed WAL capabilities into replay order (#172).
///
/// The decision is made once for the whole list, never pairwise — a
/// comparator that mixes two keys per-pair is not a total order (a
/// seq-vs-seq comparison can contradict the timestamp comparisons made
/// against a legacy row, which cycles, and `sort_by` may panic on an
/// inconsistent comparator):
///
/// - Every row sequenced (post-v9 WAL): the shared completion sequence
///   is the total order; timestamps are decoration.
/// - Any legacy `NULL`-seq row present (WAL spanning the v8→v9
///   migration): fall back to `completed_at` chronology for the whole
///   list, preserving migration-era order. `seq` still breaks
///   same-millisecond ties among the rows that have it (legacy rows
///   sort after sequenced rows within a tied millisecond, and pure
///   legacy ties keep insertion order via the stable sort — the pre-v9
///   behaviour, tools before LLMs).
pub(super) fn sort_into_replay_order(completed: &mut [((Option<i64>, i64), CapabilityResult)]) {
    let fully_sequenced = completed.iter().all(|((seq, _), _)| seq.is_some());
    if fully_sequenced {
        completed.sort_by_key(|((seq, _), _)| seq.expect("checked fully_sequenced"));
    } else {
        completed.sort_by_key(|((seq, at), _)| (*at, seq.unwrap_or(i64::MAX)));
    }
}

/// Regroup a chronologically-ordered capability stream so each model
/// turn's tool results collapse into the single capability the live
/// loop emitted: a lone [`CapabilityResult::ToolResult`] for a
/// one-call turn, a [`CapabilityResult::ParallelToolResults`] for a
/// multi-call turn (mirroring the harness's `CallTool` /
/// `CallToolsParallel` split). Recovery persists one `tool_dispatch`
/// row per call, but the harness answers a parallel turn with a single
/// capability; feeding the rows individually desyncs replay at the
/// second result ("expected ModelResult after CallModel"). A maximal
/// run of consecutive tool results belongs to one turn — the next
/// model call only starts once the turn's results are integrated — so
/// each run becomes one capability. Non-tool capabilities pass through
/// in place.
pub(super) fn coalesce_tool_results(
    ordered: Vec<((Option<i64>, i64), CapabilityResult)>,
) -> Vec<CapabilityResult> {
    let mut out: Vec<CapabilityResult> = Vec::with_capacity(ordered.len());
    let mut batch: Vec<ToolCallResult> = Vec::new();
    for (_, capability) in ordered {
        match capability {
            CapabilityResult::ToolResult(result) => batch.push(result),
            other => {
                flush_tool_batch(&mut batch, &mut out);
                out.push(other);
            }
        }
    }
    flush_tool_batch(&mut batch, &mut out);
    out
}

/// Emit an accumulated run of tool results as the one capability the
/// live loop produced — a bare `ToolResult` for a single call,
/// `ParallelToolResults` for several — then clear the batch. An empty
/// batch emits nothing.
fn flush_tool_batch(batch: &mut Vec<ToolCallResult>, out: &mut Vec<CapabilityResult>) {
    match batch.len() {
        0 => {}
        1 => out.push(CapabilityResult::ToolResult(
            batch.pop().expect("len checked == 1"),
        )),
        _ => out.push(CapabilityResult::ParallelToolResults(std::mem::take(batch))),
    }
}

/// If the last model turn in `completed` dispatched more tool calls
/// than have completed rows, the crash fell inside that batch. Drop the
/// recorded partial results (the trailing tool capabilities) so replay
/// stops at the model turn and `run_loop_inner` re-runs the batch to
/// completion — `run_tool` reuses the already-completed calls and runs
/// only the missing ones. Returns the number of results dropped (0 when
/// the final batch is whole, or there is no pending batch). Only the
/// final batch can be partial: earlier batches are whole, or the
/// invocation could not have progressed past them.
pub(super) fn truncate_incomplete_final_batch(
    completed: &mut Vec<((Option<i64>, i64), CapabilityResult)>,
) -> usize {
    let Some(last_model) = completed
        .iter()
        .rposition(|(_, c)| matches!(c, CapabilityResult::ModelResult(_)))
    else {
        return 0;
    };
    let requested = match &completed[last_model].1 {
        CapabilityResult::ModelResult(response) => response.tool_calls().count(),
        _ => unreachable!("rposition matched a ModelResult"),
    };
    // Everything after the last model turn is that turn's tool results —
    // nothing else runs before the next (never-reached) model call.
    let recorded = completed.len() - last_model - 1;
    if requested > 0 && recorded < requested {
        completed.truncate(last_model + 1);
        recorded
    } else {
        0
    }
}

#[cfg(test)]
mod tests;
