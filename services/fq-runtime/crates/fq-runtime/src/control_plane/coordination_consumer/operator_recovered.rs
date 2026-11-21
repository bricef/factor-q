//! The fold of an operator's drop, `invocation.operator_recovered`: moved
//! from `coordination_consumer.rs`, which the size cap holds at 800
//! production lines, ahead of the drop's fold growing (issue 1070).

use chrono::Utc;
use tracing::{debug, warn};

use super::{CoordinationConsumer, CoordinationConsumerError, folded_now, leaves_the_row_as_it_is};
use crate::control_plane::ownership::{Applied, ArchiveExpected, Ending, OwnershipEvent};
use crate::control_plane::store::{ArchiveState, ControlPlaneStoreError, InvocationArchiveRow};
use crate::events::{Event, InvocationOperatorRecoveredPayload};

impl CoordinationConsumer {
    /// Step 9: an operator's termination request (a drop), applied as a
    /// terminal transition.
    ///
    /// 1. Write the archive row (idempotent on `invocation_id`).
    ///    State blob is empty in v1 — the CP doesn't have the
    ///    worker's state for an ambiguous invocation.
    /// 2. Move the ownership row to `final_phase` with its archive
    ///    written, from every state: an operator's word wins over a
    ///    worker's outcome, finished or not, so an operator can
    ///    correct a wrong one. The row keeps the worker id it names.
    /// 3. **No ack** — unlike `invocation.archived`, no worker
    ///    is waiting to clean up. If the worker is still alive
    ///    and emits `invocation.archived` afterwards, the
    ///    archive's arrival keeps the operator's ending (see
    ///    `handle_invocation_archived`).
    ///
    /// **A drop that says no archive will come** (`archive: none`, issue
    /// 1070) reverses steps 1 and 2: the row is moved first, and a run
    /// that had not ended reads `failed` with its archive `none`, with no
    /// archive record written, since none will come and an empty one would
    /// say the run's state was archived. A row the move leaves anywhere
    /// else, a finished run's, gets the record as before, after the move.
    /// A delivery cut off between the two is redelivered and finds the
    /// row moved, a duplicate, and writes the record then. A drop from
    /// before the field, or one saying `pending` or a value this build
    /// does not know, is folded as before.
    ///
    /// An ownership row this build cannot read, or one whose move the
    /// store refuses by its own rules, does not stop the drop: the archive
    /// record is written (with the drop's time as the run's start if the
    /// row cannot be read), the row is left as it is, and the worker's WAL
    /// row is still marked terminal, since the operator meant to stop the
    /// run. Then the event is refused as permanent.
    pub(crate) async fn handle_invocation_operator_recovered(
        &self,
        event: &Event,
        payload: &InvocationOperatorRecoveredPayload,
    ) -> Result<(), CoordinationConsumerError> {
        let invocation_id = event.envelope.invocation_id.to_string();
        let now_ms = Utc::now().timestamp_millis();
        debug!(
            invocation_id = %invocation_id,
            action = %payload.action,
            final_phase = %payload.final_phase,
            "applying operator-recovered terminal transition"
        );

        // The run's start, for the archive record, when a row records one
        // this build can read.
        let started_at = match self.store.get_invocation_owner(&invocation_id).await {
            Ok(owner) => owner.map_or(now_ms, |owner| owner.assigned_at),
            Err(ControlPlaneStoreError::Malformed(_)) => now_ms,
            Err(err) => return Err(err.into()),
        };

        // v1 always emits "failed"; an unknown phase is a failure too,
        // so an audit trail is preserved.
        let expected = ArchiveExpected::of_drop(payload.archive);
        let what = OwnershipEvent::TerminationRequested(
            Ending::of_operator_phase(&payload.final_phase),
            expected,
        );
        let moved = if expected == ArchiveExpected::No {
            let moved = self
                .move_row(
                    event,
                    what,
                    folded_now(event.envelope.agent_id.as_str()),
                    None,
                )
                .await;
            // The record, after the move, wherever the move did not leave
            // the run with no archive to come, and where the row was left
            // as it is (it cannot be read, or its move was refused), as
            // before.
            let write = match &moved {
                Ok(applied) => !leaves_no_archive_to_come(applied),
                Err(err) => leaves_the_row_as_it_is(err),
            };
            if write {
                self.write_the_drops_record(event, payload, started_at, now_ms)
                    .await?;
            }
            moved
        } else {
            self.write_the_drops_record(event, payload, started_at, now_ms)
                .await?;
            self.move_row(
                event,
                what,
                folded_now(event.envelope.agent_id.as_str()),
                None,
            )
            .await
        };
        if let Err(err) = &moved
            && !leaves_the_row_as_it_is(err)
        {
            return moved.map(|_| ());
        }

        if let Some(worker_store) = &self.worker_store {
            worker_store
                .mark_invocation_operator_terminal(&invocation_id, &payload.final_phase, now_ms)
                .await?;
        }
        if let Err(err) = &moved {
            warn!(
                invocation_id = %invocation_id,
                error = %err,
                "the drop took effect: its archive record is written and the worker's row marked \
                 finished; the run's ownership row is left as it is, since it cannot be read or \
                 the store refused its move"
            );
        }

        moved.map(|_| ())
    }

    /// The drop's archive record: the drop's phase, an empty state (the
    /// control plane does not hold the worker's), the run's start as its
    /// row records it, and the drop's time. Idempotent on the id.
    async fn write_the_drops_record(
        &self,
        event: &Event,
        payload: &InvocationOperatorRecoveredPayload,
        started_at: i64,
        now_ms: i64,
    ) -> Result<(), CoordinationConsumerError> {
        self.store
            .insert_archive(&InvocationArchiveRow {
                invocation_id: event.envelope.invocation_id.to_string(),
                agent_id: event.envelope.agent_id.as_str().to_string(),
                final_phase: payload.final_phase.clone(),
                final_state_blob: Vec::new(),
                started_at,
                terminal_at: now_ms,
                archived_at: now_ms,
            })
            .await?;
        Ok(())
    }
}

/// Whether a drop's move left the row with no archive to come: moved
/// there, or already there. Nothing else takes a drop's record away.
fn leaves_no_archive_to_come(applied: &Applied) -> bool {
    match applied {
        Applied::Moved { to, .. } => to.archive == ArchiveState::WillNotCome,
        Applied::Duplicate { at } => at.archive == ArchiveState::WillNotCome,
        Applied::Ignored | Applied::Refused { .. } | Applied::Conflict { .. } => false,
    }
}
