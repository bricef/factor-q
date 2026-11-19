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

use tracing::info;
use uuid::Uuid;

use super::{ReducerRunner, map_store_err};
use crate::agent::AgentId;
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
    /// boundary returns, `Unstarted` (`Some`). A halt armed once the write
    /// is under way is the next boundary's, as before.
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
            if self.live.take_halt(invocation_id) {
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
            self.config
                .store
                .upsert_invocation_state(row)
                .await
                .map_err(map_store_err)?;
            self.live.note_row_written(invocation_id);
            durable_start.fire();
            return Ok(None);
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
    /// missing later was handed off, not never written.
    pub(super) fn mark_resumed(
        &self,
        invocation_id: Uuid,
        agent_id: AgentId,
    ) -> super::super::liveness::ActiveInvocation<'_> {
        let active = self.mark_active(invocation_id, agent_id);
        self.live.note_row_written(invocation_id);
        active
    }
}
