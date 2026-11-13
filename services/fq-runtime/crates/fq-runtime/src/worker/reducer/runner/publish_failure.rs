//! An event the broker refuses for its size ends the run
//! ([#828](https://github.com/bricef/factor-q/issues/828)).
//!
//! The bus refuses any event over the broker's `max_payload` before it
//! reaches the wire, with `BusError::PayloadTooLarge`. Inside a run that
//! surfaces as an error out of whichever publish was refused, with no
//! terminal event behind it. It used to be classed as transient like
//! every other bus error, but the trigger had been acked at durable
//! start, so nothing redelivered it: the row stayed in flight, every
//! restart resumed it, the replay rebuilt the same event and the guard
//! refused it again. The refusal is permanent, so it ends the run with
//! one `failed` of its own kind, `payload_too_large`, carrying which
//! event it was, its size and the limit. Nothing is truncated: the record
//! would then say something other than what the model saw.
//!
//! Where the refusal lands decides what is left to do:
//!
//! - **Before the first WAL write** (the `triggered` event, on the
//!   ordinary path or ahead of a refusal for an unavailable MCP server):
//!   there is no row, so the `failed` is the whole record. The error is
//!   not transient, so the dispatcher consumes the trigger.
//! - **Mid-run**, live or after a resume's replay (every event from the
//!   first `llm.request` on, which follows the first WAL write): the
//!   `failed` is published, then the row is marked terminal and the
//!   archive handed off, the order every mid-step failure takes, so the
//!   workspace is reclaimed and recovery has nothing to resume.
//! - **After the run has ended**: the row is terminal, so the refused
//!   event came after the outcome (`invocation.archived` behind
//!   `completed` or `failed`). That is not a failure of the invocation,
//!   and the error is passed on exactly as before; the archive is the
//!   sweeper's (see #981). The row is the test, not which event went out.
//! - **The terminal event itself** never gets here: the run loop measures
//!   it before the row goes terminal (`terminal.rs`) and ends the run on
//!   this `failed` instead.
//!
//! The `failed` always fits. It shares nothing with the event refused:
//! no history, no output, no state. It is a fixed set of short fields —
//! the kind, a one-line message of two numbers, the phase, six totals,
//! and a detail of an event type and two numbers — under the envelope
//! every event has, about 800 bytes with a short agent id, against a
//! limit of a megabyte on a stock broker. A broker that refused it would
//! refuse nearly everything a run publishes, `triggered` first.
//!
//! Split from `runner.rs`, as `failure.rs` and `terminal.rs` were, to
//! keep that file and the frozen `run`/`resume` paths inside their size
//! budgets. [`ReducerRunner::end_on_refused_publish`] is called where a
//! run's result is handled: once in `resume`, and in the fresh path once
//! for the loop (whose result includes a refused `triggered`) and once
//! for the unavailable-MCP refusal that returns before it.

use std::time::Instant;

use tracing::warn;
use uuid::Uuid;

use super::{ReducerRunner, map_store_err, terminal};
use crate::agent::{Agent, AgentId};
use crate::bus::BusError;
use crate::events::{Event, EventKind, FailureDetail, FailureKind, FailurePhase, InvocationTotals};
use crate::worker::reducer::types::Reducer;
use crate::worker::{ExecutorError, InvocationOutcome};

/// An event the broker would not take: which type it was, how many bytes
/// it serialised to, and the limit it exceeded.
#[derive(Debug, Clone, Copy)]
pub(super) struct Refused {
    event_type: EventKind,
    size: usize,
    limit: usize,
}

impl Refused {
    fn from_error(err: &BusError) -> Option<Self> {
        match *err {
            BusError::PayloadTooLarge {
                event_type,
                size,
                limit,
            } => Some(Self {
                event_type,
                size,
                limit,
            }),
            _ => None,
        }
    }

    /// The size refusal `outcome` ended in, if that is what ended it.
    fn of(outcome: &Result<InvocationOutcome, ExecutorError>) -> Option<Self> {
        match outcome {
            Err(ExecutorError::Bus(err)) => Self::from_error(err),
            _ => None,
        }
    }

    /// `event`, serialised to `size` bytes, measured against a sink's
    /// `limit`: refused when over it, by the bus's own rule.
    pub(super) fn measure(event: &Event, size: usize, limit: Option<usize>) -> Option<Self> {
        let kind = EventKind::from(&event.payload);
        let refusal = crate::bus::check_payload_size(kind, size, limit?).err()?;
        Self::from_error(&refusal)
    }

    /// The `error_message` of the `failed`, for people reading it; the
    /// same facts are in its `detail`.
    pub(super) fn message(&self) -> String {
        format!(
            "event of {} bytes exceeds the broker max_payload of {} bytes",
            self.size, self.limit
        )
    }

    /// The `failed` this refusal ends the run with.
    pub(super) fn failed_event(
        &self,
        agent_id: &AgentId,
        invocation_id: Uuid,
        partial_totals: InvocationTotals,
    ) -> Event {
        terminal::failed_event(
            agent_id,
            invocation_id,
            FailureKind::PayloadTooLarge,
            self.message(),
            phase_of(self.event_type),
            partial_totals,
            Some(FailureDetail::PayloadTooLarge {
                event_type: self.event_type,
                size_bytes: self.size as u64,
                limit_bytes: self.limit as u64,
            }),
        )
    }
}

/// The phase a run is in when it publishes an event of this type, for
/// the `failed` that event's refusal ends it with. Every type is named so
/// that a new one has to be placed here; the types the runner never
/// publishes for an invocation fall to `Reducer`, the phase of the loop
/// itself.
fn phase_of(event_type: EventKind) -> FailurePhase {
    match event_type {
        EventKind::Triggered => FailurePhase::Setup,
        EventKind::LlmRequest => FailurePhase::LlmRequest,
        EventKind::LlmDispatched
        | EventKind::LlmResponse
        | EventKind::LlmFailure
        | EventKind::InvocationDeferred => FailurePhase::LlmResponse,
        EventKind::ToolCall | EventKind::ToolDispatched => FailurePhase::ToolCall,
        EventKind::ToolResult => FailurePhase::ToolResult,
        EventKind::Completed
        | EventKind::Failed
        | EventKind::HostNotice
        | EventKind::InvocationSummary
        | EventKind::InvocationAmbiguous
        | EventKind::InvocationStuck
        | EventKind::InvocationArchived
        | EventKind::InvocationArchiveAcked
        | EventKind::InvocationOperatorRecovered
        | EventKind::InvocationOperatorResumed
        | EventKind::SystemStartup
        | EventKind::SystemShutdown
        | EventKind::SystemTaskFailed
        | EventKind::SystemRecovery
        | EventKind::WorkerHeartbeat
        | EventKind::WorkerOrphaned
        | EventKind::McpServerLog
        | EventKind::MaintenanceRun
        | EventKind::OperatorSignal
        | EventKind::Unknown => FailurePhase::Reducer,
    }
}

impl<R: Reducer + Send + Sync> ReducerRunner<R> {
    /// What a run's result becomes once a size refusal is accounted for.
    /// Any result but `BusError::PayloadTooLarge` passes through, and so
    /// does that one once the row is terminal (the module doc says why).
    /// Otherwise the run ends here with its `failed`, and returns
    /// `InvocationFailed` with the new kind, which no caller retries.
    ///
    /// `agent` is the one the run was driven for, whose id every event of
    /// the run carries. `totals` are the run's as the refusal left them;
    /// the duration is stamped here, as every other failure path stamps
    /// it. A store that cannot say whether the row is terminal returns its
    /// own error, which is transient: before the first WAL write the
    /// dispatcher redelivers, and after it the row stays in flight for
    /// recovery.
    ///
    /// The close's own errors are returned as every failure path returns
    /// them. One of them is common here: the `invocation.archived` behind
    /// the `failed` carries the conversation the refused event was built
    /// from, so it is often too large as well. The `failed` is out and the
    /// row terminal by then, the run returns that refusal, and the archive
    /// is the sweeper's (see #981).
    pub(super) async fn end_on_refused_publish(
        &self,
        outcome: Result<InvocationOutcome, ExecutorError>,
        agent: &Agent,
        invocation_id: Uuid,
        mut totals: InvocationTotals,
        start: Instant,
        cursor: &mut Option<Uuid>,
    ) -> Result<InvocationOutcome, ExecutorError> {
        let Some(refused) = Refused::of(&outcome) else {
            return outcome;
        };
        let agent_id = agent.id();
        let row = self
            .config
            .store
            .get_invocation_state(&invocation_id.to_string())
            .await
            .map_err(map_store_err)?;
        if row.as_ref().is_some_and(|row| row.terminal_at.is_some()) {
            return outcome;
        }
        warn!(
            agent_id = %agent_id,
            invocation_id = %invocation_id,
            event_type = ?refused.event_type,
            size = refused.size,
            limit = refused.limit,
            "an event is too large for the broker; ending the invocation"
        );
        totals.total_duration_ms = start.elapsed().as_millis() as u64;
        let failed = refused.failed_event(agent_id, invocation_id, totals);
        match row {
            // Nothing durable: no row to mark, nothing to archive.
            None => self.publish_chained(cursor, failed).await?,
            Some(_) => {
                self.fail_mid_step(cursor, agent_id, invocation_id, failed)
                    .await?
            }
        }
        Err(ExecutorError::InvocationFailed {
            kind: FailureKind::PayloadTooLarge,
            message: refused.message(),
        })
    }
}
