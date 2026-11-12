//! What stops an invocation at a step boundary, and what the stop
//! returns (#974).
//!
//! Moved out of `run_loop_inner` when the first-boundary case was added.
//! Not forced by the ratchet: before the move `runner.rs` and
//! `run_loop_inner` were 23 and 21 lines inside their baselines. Both are
//! far over the caps (1,716 lines over the file cap, 93 over the function
//! cap) and `runner.rs` is being split under #78, so new logic for them
//! goes in a module of its own rather than being appended. A child module,
//! so the runner's drain flag and live registry stay private while this
//! code can still reach them.

use tracing::info;
use uuid::Uuid;

use super::ReducerRunner;
use crate::agent::AgentId;
use crate::worker::reducer::types::Reducer;
use crate::worker::{BoundaryStop, DurableStart, InvocationOutcome};

impl<R: Reducer + Send + Sync> ReducerRunner<R> {
    /// Whether the invocation stops at this step boundary, and what it
    /// returns if it does. Asked at the top of every step, the first one
    /// included, before the reducer runs.
    ///
    /// Two things stop it, and neither emits a terminal event. An
    /// operator halt (`invocation.drop --live`) is invocation-scoped: the
    /// coordination consumer records the drop from the operator-drop
    /// event (marking the WAL row terminal, when there is one), so the
    /// halt only stops the in-memory driver.
    /// A drain (ADR-0027) stops every invocation on this worker. The halt
    /// is asked first, and consumed when it is.
    ///
    /// What the stop returns depends on whether anything durable exists
    /// yet, which `durable_start` knows. At a boundary after the first WAL
    /// write (any boundary of a resumed invocation, or of a fresh one past
    /// its first step) the previous step's checkpoint is on disk: the WAL
    /// state is a clean between-steps point, the same as a crash at this
    /// boundary, and the row stays in flight for recovery to resume. That
    /// is `Suspended`. At a fresh invocation's first boundary nothing
    /// durable exists. Its `triggered` event is on the stream, but
    /// recovery resumes from `invocation_state` rows and none has been
    /// written, so a `Suspended` here would promise a resume that cannot
    /// happen while the dispatcher acked the trigger. That is `Unstarted`,
    /// which leaves the work to whoever handed it over.
    pub(super) fn stop_at_boundary(
        &self,
        invocation_id: Uuid,
        agent_id: &AgentId,
        step_index: u32,
        durable_start: &DurableStart,
    ) -> Option<InvocationOutcome> {
        let stopped_by = if self.live.take_halt(invocation_id) {
            BoundaryStop::Halt
        } else if self.drain.is_draining() {
            BoundaryStop::Drain
        } else {
            return None;
        };
        let outcome = durable_start.stopped_at_boundary(invocation_id, stopped_by);
        // Whether the first WAL write landed: a halted invocation's row is
        // the drop's to mark terminal, a drained one's is recovery's.
        let durably_started = matches!(outcome, InvocationOutcome::Suspended { .. });
        match stopped_by {
            BoundaryStop::Halt => info!(
                %agent_id, %invocation_id, step_index, durably_started,
                "operator halt — stopping invocation at step boundary"
            ),
            BoundaryStop::Drain if durably_started => info!(
                %agent_id, %invocation_id, step_index,
                "draining — suspending invocation at step boundary"
            ),
            BoundaryStop::Drain => info!(
                %agent_id, %invocation_id, step_index,
                "draining — stopping invocation before its first WAL write, with nothing to resume"
            ),
        }
        Some(outcome)
    }
}
