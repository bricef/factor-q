//! The fold of an operator's drop, `invocation.operator_recovered`: moved
//! from `coordination_consumer.rs`, which the size cap holds at 800
//! production lines, ahead of the drop's fold growing (issue 1070).

use chrono::Utc;
use tracing::{debug, warn};

use super::{
    CoordinationConsumer, CoordinationConsumerError, folded_now, leaves_the_row_as_it_is,
    request_of,
};
use crate::control_plane::ownership::{ArchiveExpected, OwnerFacts, OwnershipEvent};
use crate::events::{Event, InvocationOperatorRecoveredPayload};

impl CoordinationConsumer {
    /// Step 9: an operator's termination request (a drop), folded as the
    /// transition table says (issue 1070).
    ///
    /// 1. Move the ownership row. A run that has not ended (no row,
    ///    `in_flight` or `ambiguous`) ends `failed`, by the drop,
    ///    provisionally, its archive pending, or `none` when the drop
    ///    says no archive will come. A run that has ended keeps its
    ///    outcome, whoever ended it. Either way the request (the drop's
    ///    own time, who asked and why) is recorded beside the outcome,
    ///    the first one only. The row keeps the worker id it names.
    /// 2. Record the drop beside the reason of a claim the run consumed
    ///    (issue 1055): beside a claim that does not record it, the row
    ///    reads to the watch and requeue as a start. Before step 1 and
    ///    after, the second time also when the row is left as it is; a
    ///    claim consumed later records it itself, reading the row. Left
    ///    open (issue 1113): one consumed between the first record and the
    ///    row stands beside it without the drop until the second.
    /// 3. Mark the worker's WAL row finished, when this process hosts
    ///    it, so the run stops and the archive sweeper hands the row off
    ///    with the run's real state.
    /// 4. **No ack**, and **no archive record**: unlike
    ///    `invocation.archived`, no worker is waiting to clean up, and the
    ///    archive record comes only from the worker's archive, so an
    ///    empty one cannot shadow the run's real state.
    ///
    /// An ownership row this build cannot read, or one whose move the
    /// store refuses by its own rules, does not stop the drop: the row is
    /// left as it is, and the worker's WAL row is still marked terminal,
    /// since the operator meant to stop the run. Then the event is refused
    /// as permanent.
    pub(crate) async fn handle_invocation_operator_recovered(
        &self,
        event: &Event,
        payload: &InvocationOperatorRecoveredPayload,
    ) -> Result<(), CoordinationConsumerError> {
        let invocation_id = event.envelope.invocation_id.to_string();
        debug!(
            invocation_id = %invocation_id,
            action = %payload.action,
            final_phase = %payload.final_phase,
            "applying an operator's termination request"
        );

        // Step 2, first: before the row exists to be read as a start. The
        // drop is dated at its event's own time, as the row's request is.
        let dropped_at = event.envelope.timestamp.timestamp_millis();
        self.store
            .record_trigger_claim_drop(&invocation_id, dropped_at)
            .await?;

        // v1 always emits "failed", and a drop's ending is a failure
        // whatever its phase says. The drop brings its request.
        let request = request_of(event);
        let moved = self
            .move_row(
                event,
                OwnershipEvent::TerminationRequested(ArchiveExpected::of_drop(payload.archive)),
                OwnerFacts {
                    request: request.as_ref(),
                    ..folded_now(event, event.envelope.agent_id.as_str())
                },
                None,
            )
            .await;
        if let Err(err) = &moved
            && !leaves_the_row_as_it_is(err)
        {
            return moved.map(|_| ());
        }
        // Step 2, again: for a claim consumed while the row was written,
        // or while the store refused its move.
        self.store
            .record_trigger_claim_drop(&invocation_id, dropped_at)
            .await?;

        if let Some(worker_store) = &self.worker_store {
            worker_store
                .mark_invocation_operator_terminal(
                    &invocation_id,
                    &payload.final_phase,
                    Utc::now().timestamp_millis(),
                )
                .await?;
        }
        if let Err(err) = &moved {
            warn!(
                invocation_id = %invocation_id,
                error = %err,
                "the drop took effect: the worker's row is marked finished; the run's ownership \
                 row is left as it is, since it cannot be read or the store refused its move"
            );
        }

        moved.map(|_| ())
    }
}
