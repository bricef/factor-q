//! The fold of an operator's drop, `invocation.operator_recovered`: moved
//! from `coordination_consumer.rs`, which the size cap holds at 800
//! production lines, ahead of the drop's fold growing (issue 1070).

use chrono::Utc;
use tracing::{debug, warn};

use super::{CoordinationConsumer, CoordinationConsumerError, folded_now, is_unreadable};
use crate::control_plane::ownership::{Ending, OwnershipEvent};
use crate::control_plane::store::{ControlPlaneStoreError, InvocationArchiveRow};
use crate::events::Event;

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
    /// An ownership row this build cannot read does not stop the drop:
    /// the archive record takes the drop's time as the run's start, the
    /// row is left as it is, and the worker's WAL row is still marked
    /// terminal, since the operator meant to stop the run. Then the event
    /// is refused as permanent, as every event for such a row is.
    pub(crate) async fn handle_invocation_operator_recovered(
        &self,
        event: &Event,
        payload: &crate::events::InvocationOperatorRecoveredPayload,
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

        self.store
            .insert_archive(&InvocationArchiveRow {
                invocation_id: invocation_id.clone(),
                agent_id: event.envelope.agent_id.as_str().to_string(),
                final_phase: payload.final_phase.clone(),
                final_state_blob: Vec::new(),
                started_at,
                terminal_at: now_ms,
                archived_at: now_ms,
            })
            .await?;

        // v1 always emits "failed"; an unknown phase is a failure too,
        // so an audit trail is preserved.
        let moved = self
            .move_row(
                event,
                OwnershipEvent::TerminationRequested(Ending::of_operator_phase(
                    &payload.final_phase,
                )),
                folded_now(event.envelope.agent_id.as_str()),
                None,
            )
            .await;
        if let Err(err) = &moved
            && !is_unreadable(err)
        {
            return moved.map(|_| ());
        }

        if let Some(worker_store) = &self.worker_store {
            worker_store
                .mark_invocation_operator_terminal(&invocation_id, &payload.final_phase, now_ms)
                .await?;
        }
        if moved.is_err() {
            warn!(
                invocation_id = %invocation_id,
                "the drop took effect: its archive record is written and the worker's row marked \
                 finished; the run's ownership row cannot be read and is left as it is"
            );
        }

        moved.map(|_| ())
    }
}
