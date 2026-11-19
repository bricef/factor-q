//! A run's writes to its own WAL row, and what it does when someone else
//! has finished that row (issue 1070).
//!
//! **The rule.** A run whose worker log row was finished by someone else,
//! or is gone, publishes no ending of its own and never writes that row
//! again. Whoever marks the row finished first decides the run's outcome.
//! When the run's own finishing write lands first, the run ended by
//! itself, and a drop after it is only a request on record. When an
//! operator's drop, folded into this worker's store, marks it first, the
//! drop stopped the run, and what the step in flight then returns is not
//! the run's outcome.
//!
//! **Where the runner notices.** At these points, each of which returns
//! [`ExecutorError::FinishedElsewhere`], which no caller retries:
//!
//! - A write of the row after the run's first one ([`Self::write_own_row`])
//!   that does not land, because the row is terminal or gone. That covers a
//!   checkpoint, which would otherwise have re-created a row the archive
//!   hand-off deleted (a row recovery would then resume), and the finishing
//!   write, which would otherwise have done nothing and let the run publish
//!   `completed` anyway.
//! - The mark that ends a run on a mid-step `failed` (`end_mid_step`,
//!   `terminal.rs`), written with the `failed` stored in it and before it
//!   is published: a mark that does not land, or a row already terminal
//!   or gone when the run comes to it, means someone else finished the
//!   run, and no `failed` goes out.
//! - `publish_failed` (`publish_failure.rs`), for a `failed` of a run with
//!   no row to mark: a row missing after this drive wrote it was handed
//!   off, and a terminal one is not the run's own.
//!
//! With an operator's live drop the runner seldom gets that far: the halt
//! stops it at the next step boundary, before the reducer decides and
//! before any write (`boundary.rs`), or, armed during the first step, at
//! the first write, which is then not made. These points hold the rule where no
//! halt is armed for the run (a deferred run whose resume starts while
//! the drop is being folded, a drop another daemon handled), and for a
//! step that fails after a live drop's mark, which reaches the runner with
//! no boundary between.
//!
//! **What still goes out.** Everything the step in flight already did: a
//! model call's `llm.response` (or `llm.failure`), with its usage and
//! cost, and a tool's `tool.result`. No spend figure is lost. What does not
//! go out is an ending: no `completed`, no `failed`, and no
//! `invocation.archived` from the run, whose archive is the sweeper's to
//! hand off from the row the drop finished.
//!
//! **One winner.** Every ending of a run with a row is written into the
//! row before it is published, by a write that lands only on a row still in
//! flight, and a drop's fold marks the same row the same way. So the two
//! never both win: when the drop's mark lands first, the run's write
//! misses and nothing of the run's goes out; when the run's lands first,
//! the drop's misses, and the run's ending is the outcome.
//!
//! **Telling "never started" from "started, then handed off".** A run with
//! no row may be one that never made its first write, whose `failed` is
//! `Setup` (issue 1024), or one whose row a drop finished and its hand-off
//! deleted. The drive knows which from its own write: the live registry
//! notes it when the first write lands, and when a resume begins from a
//! row (`wrote_row`), and forgets it when the drive ends. That is the
//! drive's knowledge of its own write, the fact its `DurableStart` holds,
//! not a second record of the run's start.

use tokio::sync::mpsc;
use tracing::{info, warn};
use uuid::Uuid;

use super::{ReducerRunner, map_store_err};
use crate::agent::AgentId;
use crate::worker::reducer::liveness::ArmedHalt;
use crate::worker::reducer::types::Reducer;
use crate::worker::store::InvocationStateRow;
use crate::worker::{BoundaryStop, DurableStart, ExecutorError, InvocationOutcome};

impl<R: Reducer + Send + Sync> ReducerRunner<R> {
    /// Write the step's row.
    ///
    /// The run's first write, before `durable_start` has fired, creates
    /// the row and then fires it: the run is recoverable from the WAL, so
    /// the trigger dispatcher may ack (issue #41). It is not made under an
    /// operator's halt armed before it. The halt is checked at the top of
    /// every step, but the reducer runs between that check and this write,
    /// and a halt armed then used to be seen only at the next boundary,
    /// after this write and the step's model call: the drop's mark,
    /// folded while there was no row, never reached the row, which stayed
    /// in flight until the next start (issue 1070). So the halt is taken
    /// here as at a boundary, and the run returns what a halt at its first
    /// boundary returns, `Unstarted` (`Some`). The write is begun under the
    /// lock a halt is armed under, so a halt armed after that sees it under
    /// way; [`Self::write_first_row`] says what the run does then. Only the
    /// first write takes a halt: one armed during a later step is the next
    /// boundary's, and the step in flight finishes.
    ///
    /// Every later write lands only on the row still in flight; one that
    /// does not means someone else finished the row, and the run stops
    /// with [`ExecutorError::FinishedElsewhere`] rather than write over it
    /// or bring it back.
    pub(super) async fn write_own_row(
        &self,
        row: &InvocationStateRow,
        durable_start: &mut DurableStart,
        invocation_id: Uuid,
    ) -> Result<Option<InvocationOutcome>, ExecutorError> {
        if !durable_start.has_started() {
            if !self.live.begin_first_write(invocation_id) {
                info!(
                    agent_id = %row.agent_id,
                    %invocation_id,
                    step_index = row.step_index,
                    "operator halt — stopping invocation before its first WAL write"
                );
                return Ok(Some(
                    durable_start.stopped_at_boundary(invocation_id, BoundaryStop::Halt),
                ));
            }
            return self
                .write_first_row(row, durable_start, invocation_id)
                .await;
        }
        let landed = self
            .config
            .store
            .update_invocation_state(row)
            .await
            .map_err(map_store_err)?;
        if landed {
            return Ok(None);
        }
        Err(self.finished_elsewhere(invocation_id, "its row write did not land"))
    }

    /// The run's first write, begun with no halt armed. A halt armed while
    /// it is under way (the arm reports
    /// [`FirstWriteUnderWay`](crate::worker::RowAtHalt::FirstWriteUnderWay))
    /// is taken as soon as the write ends:
    ///
    /// - **The write lands.** The row is there and the run recoverable, so
    ///   the durable start fires and the run stops as at a boundary after
    ///   its first write, `Suspended`, before any model call. First it
    ///   waits for the word of the drop that armed the halt, and, told the
    ///   drop is published, marks its row as the drop's fold does
    ///   ([`Self::mark_once_the_drop_is_published`]).
    /// - **The write fails.** There is no row, and with a halt armed the
    ///   run stops as halted before its first write, `Unstarted`, which the
    ///   dispatcher takes as the drop's outcome and consumes the trigger
    ///   for. Returning the store error would have the dispatcher retry the
    ///   trigger, and the work the operator dropped would run again under a
    ///   new id. A halt armed only once the run has looked for one finds no
    ///   row and none to come (the arm reports
    ///   [`NeverWritten`](crate::worker::RowAtHalt::NeverWritten)), and the
    ///   error goes back as for any run that fails before its first write.
    async fn write_first_row(
        &self,
        row: &InvocationStateRow,
        durable_start: &mut DurableStart,
        invocation_id: Uuid,
    ) -> Result<Option<InvocationOutcome>, ExecutorError> {
        let written = self.config.store.upsert_invocation_state(row).await;
        let words = self.live.end_first_write(invocation_id, written.is_ok());
        if let Err(err) = written {
            if self.live.take_halt(invocation_id) {
                info!(
                    agent_id = %row.agent_id,
                    %invocation_id,
                    error = %err,
                    "operator halt — the invocation's first WAL write failed while a halt was \
                     armed; stopping it as halted before its first write"
                );
                return Ok(Some(
                    durable_start.stopped_at_boundary(invocation_id, BoundaryStop::Halt),
                ));
            }
            return Err(map_store_err(err));
        }
        durable_start.fire();
        if !self.live.take_halt(invocation_id) {
            return Ok(None);
        }
        info!(
            agent_id = %row.agent_id,
            %invocation_id,
            step_index = row.step_index,
            "operator halt — stopping invocation after its first WAL write, before any model call"
        );
        if let Some(words) = words {
            self.mark_once_the_drop_is_published(invocation_id, words)
                .await;
        }
        Ok(Some(
            durable_start.stopped_at_boundary(invocation_id, BoundaryStop::Halt),
        ))
    }

    /// The mark of a drop whose halt found the run's first write under way,
    /// made by the run once the row has landed: wait for the drop's word
    /// ([`crate::worker::DropWord`]), and, told it is published, mark the
    /// row finished in the drop's final phase as the coordination
    /// consumer's fold of the drop does, since that fold may have come
    /// before the row existed. Only a row still in flight is marked, so
    /// whichever of the two comes first is the mark. A drop that is not
    /// published says nothing, and the row is left in flight. The wait ends
    /// when the request that armed the halt does, and the mark is one write,
    /// bounded by the store's own timeouts; a mark that fails is logged, and
    /// the next start's recovery marks the row from the drop's fold.
    async fn mark_once_the_drop_is_published(
        &self,
        invocation_id: Uuid,
        mut words: mpsc::Receiver<String>,
    ) {
        let Some(final_phase) = words.recv().await else {
            return;
        };
        if let Err(err) = self
            .config
            .store
            .mark_invocation_operator_terminal(
                &invocation_id.to_string(),
                &final_phase,
                self.config.clock.unix_now_ms(),
            )
            .await
        {
            warn!(
                %invocation_id,
                error = %err,
                "could not mark the row of a run dropped during its first write; the next \
                 start's recovery marks it from the drop's fold"
            );
        }
    }

    /// The drive's last look for a halt, as it ends (issue 1070). A run
    /// that ends in an error with no row of its own would have its trigger
    /// retried by the dispatcher, under a new id. Its drive is live until
    /// it ends, through the grant servers' shutdown and the workspace's
    /// reclaim after the run itself, so a halt can be armed after the
    /// run's own look following a failed first write. Taken here, it
    /// stops the run as halted before its first write, `Unstarted`, which
    /// consumes the trigger. Left open is the instant between this look
    /// and the drive's end, nothing awaited in it: a halt armed then finds
    /// no row and none to come, and the trigger is retried under a new id.
    pub(super) fn halted_as_it_ends(
        &self,
        invocation_id: Uuid,
        outcome: Result<InvocationOutcome, ExecutorError>,
    ) -> Result<InvocationOutcome, ExecutorError> {
        let Err(err) = &outcome else {
            return outcome;
        };
        if self.live.wrote_row(invocation_id) || !self.live.take_halt(invocation_id) {
            return outcome;
        }
        info!(
            %invocation_id,
            error = %err,
            "operator halt — the invocation is ending in an error before its first WAL write, \
             with a halt armed; stopping it as halted before its first write"
        );
        Ok(InvocationOutcome::Unstarted {
            invocation_id,
            stopped_by: BoundaryStop::Halt,
        })
    }

    /// Arm an operator's halt on `invocation_id`, as
    /// [`Self::request_halt`] does, and say where the run's own row stood:
    /// `None` when this runner is not driving it (issue 1070).
    pub fn arm_halt(&self, invocation_id: Uuid) -> Option<ArmedHalt> {
        self.live.arm_halt(invocation_id)
    }

    /// Whether `invocation_id`'s first write is under way: for tests that
    /// must arm a halt in that instant.
    #[cfg(any(test, feature = "test-support"))]
    pub fn first_write_under_way(&self, invocation_id: Uuid) -> bool {
        self.live.first_write_under_way(invocation_id)
    }

    /// Say that someone else finished `invocation_id`'s row, and what the
    /// run noticed it by, and build the error the run stops with.
    pub(super) fn finished_elsewhere(&self, invocation_id: Uuid, noticed: &str) -> ExecutorError {
        info!(
            invocation_id = %invocation_id,
            noticed,
            "the run's row was finished by someone else (an operator's drop); \
             stopping without an ending of its own"
        );
        ExecutorError::FinishedElsewhere { invocation_id }
    }

    /// Register a resumed drive: it begins from a row of its own, so a row
    /// missing later was handed off, not never written. The row is noted
    /// as the drive is registered, under one lock, so a halt armed on the
    /// resumed run never finds it live with no row.
    pub(super) fn mark_resumed(
        &self,
        invocation_id: Uuid,
        agent_id: AgentId,
    ) -> super::super::liveness::ActiveInvocation<'_> {
        self.live
            .enter_from_row(invocation_id, agent_id, &self.rounds, &self.timeouts)
    }
}
