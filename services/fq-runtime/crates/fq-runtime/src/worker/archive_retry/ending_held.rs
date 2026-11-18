//! Telling the control plane that a held row's run ended (issue 1071).
//!
//! The ownership row learns that a run ended from the run's own
//! `completed` or `failed` on the stream. When the sweeper holds a
//! finished row because that stored ending cannot go out, it publishes
//! `invocation.ending_held` in its place: the run's outcome and times,
//! with nothing copied from the stored event, so it has a fixed, small
//! shape this build can always publish. Two holds keep the ending off the
//! stream: a stored event this build refuses to republish, and a stored
//! ending the broker refuses for its size. A hold of `invocation.archived`
//! does not, since the run's ending went out before it, and a row whose
//! ids cannot go on an event cannot publish this either.
//!
//! It goes out once per daemon run for each such row, the first pass that
//! holds it, and counts as sent only once the broker has it, so a bus
//! error tries it again on the next pass. Every copy, in every daemon
//! run, carries the same `event_id` ([`event_id`]), and the fold of a
//! second copy finds the row already saying it.

use tracing::{error, info, warn};
use uuid::Uuid;

use fq_ops::health::ArchiveHoldReason;

use super::{ArchiveRetryError, ArchiveRetrySweeper, event_ids};
use crate::bus::BusError;
use crate::control_plane::ownership::Ending;
use crate::events::{
    EndingHeldReason, Event, EventPayload, HeldOutcome, InvocationEndingHeldPayload, RecordedTotals,
};
use crate::worker::reducer::rounds::totals_of_llm_rows;
use crate::worker::store::{DispatchStatus, InvocationStateRow, WorkerStore, WorkerStoreError};

/// Why `hold` keeps the run's own ending off the stream, when it does.
pub(super) fn reason_of(hold: &ArchiveHoldReason) -> Option<EndingHeldReason> {
    match hold {
        ArchiveHoldReason::StoredEventRefused { .. } => Some(EndingHeldReason::StoredEventRefused),
        ArchiveHoldReason::PayloadTooLarge { event_type, .. } if is_an_ending(event_type) => {
            Some(EndingHeldReason::PayloadTooLarge)
        }
        ArchiveHoldReason::PayloadTooLarge { .. } | ArchiveHoldReason::MalformedRow { .. } => None,
    }
}

/// Whether a size hold names the run's stored ending, `completed` or
/// `failed` (the hold carries the refused event's `event_type`), rather
/// than `invocation.archived`.
fn is_an_ending(event_type: &str) -> bool {
    matches!(event_type, "completed" | "failed")
}

/// The `event_id` every copy of a run's held ending carries, the same in
/// every daemon run: a v7 whose time is when the run ended, `ended_at_ms`,
/// and whose random bits are the run's invocation id's with a fixed tag
/// mixed in. A copy the broker still remembers is dropped as a duplicate
/// (`Nats-Msg-Id`), and the projection keeps one row for it.
///
/// Its time is the run's end, not the publish, because a reader may sort a
/// run's events by `event_id` (the schema's first invariant names it the
/// fallback order), and the event stands for the ending: it sorts where
/// the run's own ending would have, after everything the run did. Nothing
/// in the runtime orders or dates events by the id: the projection orders
/// by the envelope's `timestamp`, the copy's publish time, and only
/// operator signals break a tie on it.
pub(super) fn event_id(invocation_id: Uuid, ended_at_ms: i64) -> Uuid {
    const TAG: [u8; 10] = *b"held-end.!";
    let millis = u64::try_from(ended_at_ms).unwrap_or(0);
    let mut random = [0u8; 10];
    for (byte, (own, tag)) in random
        .iter_mut()
        .zip(invocation_id.as_bytes()[6..].iter().zip(TAG))
    {
        *byte = own ^ tag;
    }
    uuid::Builder::from_unix_timestamp_millis(millis, &random).into_uuid()
}

/// The outcome a finished row records: the ending its phase names, read
/// by the rule the run's `invocation.archived` is read by.
pub(super) fn outcome_of(row: &InvocationStateRow) -> HeldOutcome {
    HeldOutcome::from(Ending::of_archived_phase(&row.phase))
}

/// The run's totals as its per-call rows in the WAL give them, by the rule
/// a resume rebuilds them by: the model calls that produced an outcome,
/// the cost of every completed model call, and the tool calls that
/// completed. The rows stay until the archive is acknowledged, so a held
/// row still has them.
pub(super) async fn recorded_totals(
    store: &WorkerStore,
    invocation_id: &str,
) -> Result<RecordedTotals, WorkerStoreError> {
    let llms = store
        .list_llm_dispatches_for_invocation(invocation_id)
        .await?;
    let tools = store
        .list_tool_dispatches_for_invocation(invocation_id)
        .await?;
    let (totals, _rounds) = totals_of_llm_rows(&llms);
    let completed_tools = tools
        .iter()
        .filter(|row| row.status == DispatchStatus::Completed)
        .count();
    Ok(RecordedTotals {
        total_llm_calls: totals.total_llm_calls,
        total_tool_calls: u32::try_from(completed_tools).unwrap_or(u32::MAX),
        total_cost: totals.total_cost,
    })
}

/// The totals the run's own ending carries, when the row stores one this
/// build can read: a stored ending refused for size could reach the stream
/// later, under a raised limit.
fn stored_totals(row: &InvocationStateRow) -> Option<RecordedTotals> {
    match row.unpublished_terminal_event().ok()??.payload {
        EventPayload::Completed(completed) => Some(RecordedTotals {
            total_llm_calls: completed.total_llm_calls,
            total_tool_calls: completed.total_tool_calls,
            total_cost: completed.total_cost,
        }),
        EventPayload::Failed(failed) => Some(RecordedTotals {
            total_llm_calls: failed.partial_totals.total_llm_calls,
            total_tool_calls: failed.partial_totals.total_tool_calls,
            total_cost: failed.partial_totals.total_cost,
        }),
        _ => None,
    }
}

impl ArchiveRetrySweeper {
    /// The totals a held ending carries: the recorded ones, unless the run's
    /// own stored ending, readable here, carries others. Then none: that
    /// ending could still go out under a raised limit, and the stream would
    /// hold two figures for one run. Exact equality, because a figure reads
    /// back as it was written. A stored ending this build cannot read
    /// cannot be compared; it never reaches the stream, and the recorded
    /// totals go out uncompared.
    async fn held_ending_totals(
        &self,
        row: &InvocationStateRow,
    ) -> Result<Option<RecordedTotals>, ArchiveRetryError> {
        let recorded = recorded_totals(&self.store, &row.invocation_id)
            .await
            .map_err(|err| ArchiveRetryError::Store(err.to_string()))?;
        match stored_totals(row) {
            Some(stored) if stored != recorded => {
                warn!(
                    invocation_id = %row.invocation_id,
                    recorded = ?recorded,
                    stored = ?stored,
                    "the held row's recorded totals differ from its stored ending's; its held \
                     ending carries none"
                );
                Ok(None)
            }
            _ => Ok(Some(recorded)),
        }
    }

    /// Publish a held row's ending when the hold keeps the run's own off
    /// the stream and this daemon run has not yet sent it. A bus failure
    /// ends the pass, as every other does, and the next pass sends it. A
    /// refusal for size, under a `max_payload` too small for this event,
    /// is logged at error and not tried again this run.
    pub(super) async fn publish_held_ending(
        &self,
        row: &InvocationStateRow,
    ) -> Result<(), ArchiveRetryError> {
        let Some(reason) = self.holds.ending_held_owed(&row.invocation_id) else {
            return Ok(());
        };
        let (Ok((agent_id, invocation_id)), Some(ended_at_ms)) = (event_ids(row), row.terminal_at)
        else {
            return Ok(());
        };
        let outcome = outcome_of(row);
        let totals = self.held_ending_totals(row).await?;
        let mut event = Event::new(
            agent_id,
            invocation_id,
            EventPayload::InvocationEndingHeld(InvocationEndingHeldPayload {
                outcome,
                ended_at_ms,
                started_at_ms: row.started_at,
                worker_id: self.worker_id.clone(),
                reason,
                totals,
            }),
        );
        event.envelope.event_id = event_id(invocation_id, ended_at_ms);
        match self.sink.publish(&event).await {
            Ok(_) => info!(
                invocation_id = %row.invocation_id,
                outcome = ?outcome,
                reason = ?reason,
                "published the held row's ending, so the control plane reads the run as ended"
            ),
            Err(BusError::PayloadTooLarge { size, limit, .. }) => error!(
                invocation_id = %row.invocation_id,
                size_bytes = size,
                limit_bytes = limit,
                "the held row's ending is larger than the broker's max_payload; the control \
                 plane will not read the run as ended until the limit is raised and fqd restarted"
            ),
            Err(err) => return Err(ArchiveRetryError::Publish(err.to_string())),
        }
        self.holds.ending_held_sent(&row.invocation_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod totals_tests;
