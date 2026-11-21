//! A run's ownership row, written at its durable start (issue 721).
//!
//! At the run's first WAL write, once the trigger's claim is marked, the
//! dispatcher writes the run's row in the control plane's ownership table:
//! `in_flight`, under this daemon's worker id, from the run's own start,
//! the `started_at` its first WAL row records and the durable-start signal
//! carries, which is also the time startup recovery's restore gives a run.
//! The run waits while it is written, as it waits for the mark. So
//! `fq invocation list --status in_flight` and `fq invocation show` see a
//! running run from that moment, and the run's ending moves the row with
//! the worker it names, where before the ending created it with the agent's
//! id in the worker column.
//!
//! **Two writes, the mark first.** They are not one transaction. Neither
//! the claim nor any reader of a start needs the row: the worker's WAL row
//! exists before either write, and every reader that asks whether a run
//! started reads the WAL first (`RunRecords`). So a row that does not land
//! leaves the run started on every record that decides anything, and only
//! the list without it until its ending writes the row or recovery
//! restores it; a mark that does not land is settled as it always was, and
//! the row is written after it all the same.
//!
//! The mark goes first because the delivery waits on it and not on the
//! row. The mark runs inside `keeping_alive`, which sends in-progress acks
//! while it waits, under the retry budget that bounds it, and the ack
//! follows it. The row's read and insert run after that ack, so a store
//! that blocks them (each statement can wait out the busy timeout) costs
//! the run a wait and nothing else. Written first, the same two statements
//! would wait out a locked store before the mark: outside `keeping_alive`,
//! before the ack, and inside the delivery's ack window, so the broker
//! could deliver the trigger again while the run was going.
//!
//! **What the write meets.** The row is applied through the ownership
//! table's own transition ([`OwnershipEvent::DurablyStarted`]): no row gives
//! `in_flight`; a row already `in_flight` for the run is a retry's, and
//! left; an `ambiguous` or an ended row refuses it. The one sequence that
//! reaches a refusal is an operator's drop folded between the run's first
//! WAL write and this write: the row then already reads `failed`, ended by
//! the drop, and keeps the agent's id the drop's fold wrote in its worker
//! column. That is logged at `warn`. A write the store fails, or a row this
//! build cannot read, is logged at `error`, as a mark the store refuses
//! is, and the run goes on. The insert itself is not one the store's checks
//! can refuse: `in_flight` with no ender and no request is a state every
//! check admits.

use tracing::{debug, error, warn};

use crate::control_plane::ControlPlaneStore;
use crate::control_plane::ownership::{Applied, OwnerFacts, OwnershipError, OwnershipEvent, apply};

use super::super::TriggerDispatcher;
use super::Claimed;

impl TriggerDispatcher {
    /// Write the ownership row of `claim`'s run at its durable start, under
    /// `worker_id`, from `started_at_ms`, and log what the write met, naming
    /// the delivery as the mark's lines do.
    pub(super) async fn write_started_row(
        &self,
        msg: &async_nats::jetstream::Message,
        store: &ControlPlaneStore,
        worker_id: &str,
        claim: &Claimed,
        trigger_id: uuid::Uuid,
        started_at_ms: i64,
    ) {
        let Claimed {
            stream_seq,
            invocation_id,
        } = *claim;
        let subject = msg.subject.as_str();
        let written = apply(
            store,
            &invocation_id.to_string(),
            OwnershipEvent::DurablyStarted,
            OwnerFacts {
                worker_id,
                at_ms: started_at_ms,
                ended_at_ms: started_at_ms,
                request: None,
            },
        )
        .await;
        match written {
            Ok(Applied::Moved { .. } | Applied::Duplicate { .. }) => {
                debug!(%invocation_id, %trigger_id, stream_seq, subject, worker_id,
                    "wrote the run's ownership row at its durable start");
            }
            Ok(Applied::Refused { recorded }) => {
                warn!(%invocation_id, %trigger_id, stream_seq, subject, recorded = %recorded.name(),
                    "the run's ownership row already records it ended or awaiting triage, most \
                     likely an operator's drop folded before its first WAL write was recorded \
                     here: its durable start is not written over it, and the row keeps the \
                     worker it names");
            }
            // The durable start has no cell that ignores or contradicts a
            // row; reached only by a table that changed under this caller.
            Ok(other @ (Applied::Ignored | Applied::Conflict { .. })) => {
                warn!(%invocation_id, %trigger_id, stream_seq, subject, applied = ?other,
                    "the run's ownership row answered its durable start unexpectedly; left as \
                     it was");
            }
            Err(OwnershipError::Unreadable { detail, .. }) => {
                error!(%invocation_id, %trigger_id, stream_seq, subject, detail,
                    "could not write the run's ownership row at its durable start: the row \
                     holds a state this build cannot read. Left as it is; the run goes on");
            }
            Err(err) => {
                error!(error = %err, %invocation_id, %trigger_id, stream_seq, subject,
                    "could not write the run's ownership row at its durable start. The run \
                     goes on, and the worker's WAL records its start; `fq invocation list` leaves \
                     the run out until its ending writes the row, or startup recovery restores \
                     it");
            }
        }
    }
}
