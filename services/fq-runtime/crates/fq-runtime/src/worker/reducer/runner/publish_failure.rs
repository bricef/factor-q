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
//!   there is no row, so the `failed` is the whole record, and it names
//!   the trigger (`trigger_id`, `trigger_subject`), since nothing else on
//!   the stream would. The error is not transient, so the dispatcher
//!   consumes the trigger.
//! - **Mid-run**, live or after a resume's replay (every event from the
//!   first `llm.request` on, which follows the first WAL write): the
//!   `failed` is published, then the row is marked terminal and the
//!   archive handed off, the order every mid-step failure takes, so the
//!   workspace is reclaimed and recovery has nothing to resume.
//! - **After the run has ended**: the row is terminal, so the refused
//!   event came after the outcome (`invocation.archived` behind
//!   `completed` or `failed`). That is not a failure of the invocation,
//!   and the error is passed on exactly as before. The row is the test,
//!   not which event went out.
//! - **The terminal event itself** never gets here: the run loop measures
//!   it before the row goes terminal (`terminal.rs`) and ends the run on
//!   this `failed` instead.
//!
//! **What becomes of the archive.** A run ended by an oversized
//! `llm.request` goes terminal with that request's messages in its state,
//! and `invocation.archived` writes the state as base64, four bytes for
//! every three. The archive is then over the limit too, unless what the
//! request carries besides its messages, its tool definitions mostly, is
//! more than a quarter of the limit. The archive sweeper holds such a row
//! (`worker/archive_retry.rs`): nothing more is published for it, it is
//! kept whole, logged once per daemon run and offered again at the next
//! start, the pass goes on to the rows behind it, and `fq doctor` and
//! `fq status` list the hold with its reason and remedy. A run ended at a
//! `tool.result` or at its terminal event does not hold the refused event
//! in its state, so its archive is refused only if the conversation
//! before it was already large; one ended at `triggered` has no row to
//! archive.
//!
//! **What recovery reports.** A run that ends here on a resume returns an
//! error from the resume; startup recovery reads the worker row, finds it
//! terminal, logs that at warn and publishes nothing ambiguous (#1005).
//! The control plane's ownership row reads `in_flight` until the archive
//! is acknowledged, which for a held row is until an operator acts
//! (#1024); the held row in `fq doctor` is how an operator finds the run.
//!
//! **Two resume paths do not come here.** `resume` returns early for a
//! budget found exceeded on resume and for a provider failure whose
//! terminal was lost to a crash, both through `emit_failed`, before the
//! loop runs. Routing them here would grow `resume` past its size budget.
//! The first `failed` is a short fixed message; the second carries the
//! provider's error text, and if that alone were over the broker's limit
//! the refusal would leave the row in flight for every start to resume
//! into again, as before #828.
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
//! run's result is handled, once in `resume` and once for the fresh
//! path's loop; [`ReducerRunner::end_unstarted_on_refusal`] at the fresh
//! path's two points before its first WAL write, its own `triggered` and
//! the unavailable-MCP refusal's, where the trigger is in hand.

use std::time::Instant;

use tracing::warn;
use uuid::Uuid;

use serde_json::json;

use super::{ReducerRunner, map_store_err, terminal};
use crate::agent::{Agent, AgentId};
use crate::bus::{BusError, EventSink};
use crate::dead_letter::{DEAD_LETTER_SUBJECT_KEY, DEAD_LETTER_TRIGGER_ID_KEY};
use crate::events::{Event, EventKind, FailureDetail, FailureKind, FailurePhase, InvocationTotals};
use crate::trigger::Trigger;
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

    /// Whether `sink` would refuse `event`, whose JSON is `body_len`
    /// bytes, for its size: the sink's own answer, so this cannot count
    /// differently from its publish.
    pub(super) fn measure(sink: &dyn EventSink, event: &Event, body_len: usize) -> Option<Self> {
        Self::from_error(&sink.check_size(event, body_len).err()?)
    }

    /// The `error_message` of the `failed`, for people reading it; the
    /// same facts are in its `detail`.
    pub(super) fn message(&self) -> String {
        format!(
            "event of {} bytes exceeds the broker max_payload of {} bytes",
            self.size, self.limit
        )
    }

    /// What the run returns once its `failed` is out.
    fn failure(&self) -> ExecutorError {
        ExecutorError::InvocationFailed {
            kind: FailureKind::PayloadTooLarge,
            message: self.message(),
        }
    }

    /// The `failed` this refusal ends the run with.
    pub(super) fn failed_event(
        &self,
        agent_id: &AgentId,
        invocation_id: Uuid,
        partial_totals: InvocationTotals,
    ) -> Event {
        warn!(
            agent_id = %agent_id,
            invocation_id = %invocation_id,
            event_type = ?self.event_type,
            size = self.size,
            limit = self.limit,
            "an event is too large for the broker; ending the invocation"
        );
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
    /// An error before the `failed` is out and the row marked terminal is
    /// returned as it is: the run has not ended, and recovery finds the
    /// row in flight. From that point the run has ended, and it returns
    /// its own failure whatever happens to the `invocation.archived` that
    /// follows. That one is often refused too, since it carries the
    /// conversation the refused event was built from; the error is logged
    /// and the row is left terminal and unarchived (see the module doc).
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
        totals.total_duration_ms = start.elapsed().as_millis() as u64;
        let failed = refused.failed_event(agent_id, invocation_id, totals);
        if row.is_none() {
            // Nothing durable, so nothing to mark or archive. No path
            // reaches this today: the fresh path's `triggered` ends in
            // `end_unstarted_on_refusal`, which also names the trigger.
            self.publish_chained(cursor, failed).await?;
            return Err(refused.failure());
        }
        self.end_mid_step(cursor, invocation_id, failed).await?;
        if let Err(err) = self
            .publish_archived_and_mark_pending(cursor, agent_id, invocation_id, "failed")
            .await
        {
            warn!(
                invocation_id = %invocation_id,
                error = %err,
                "the run ended, but its invocation.archived was not published; \
                 the terminal row is left for the archive sweeper"
            );
        }
        Err(refused.failure())
    }

    /// The same, for a refusal before the first WAL write: the fresh
    /// path's `triggered`, on its own or ahead of a refusal for an
    /// unavailable MCP server. There is no row, so the `failed` is the
    /// whole record, and with no `triggered` on the stream nothing else
    /// would say which trigger this run answered. The `failed` names it
    /// with the dead letter's keys, `trigger_id` and `trigger_subject`;
    /// both readers of those keys (`dead_letter::from_event` and
    /// `trigger::from_event`) take only a `trigger_exhausted` failure, so
    /// this one is never mistaken for a dead letter. The payload is not
    /// copied: it may be what made `triggered` too large.
    pub(super) async fn end_unstarted_on_refusal(
        &self,
        outcome: Result<InvocationOutcome, ExecutorError>,
        agent: &Agent,
        trigger: &Trigger,
        invocation_id: Uuid,
        start: Instant,
    ) -> Result<InvocationOutcome, ExecutorError> {
        let Some(refused) = Refused::of(&outcome) else {
            return outcome;
        };
        let totals = InvocationTotals {
            total_duration_ms: start.elapsed().as_millis() as u64,
            ..InvocationTotals::default()
        };
        let mut failed = refused
            .failed_event(agent.id(), invocation_id, totals)
            .annotate(DEAD_LETTER_TRIGGER_ID_KEY, json!(trigger.id.to_string()));
        if let Some(subject) = &trigger.subject {
            failed = failed.annotate(DEAD_LETTER_SUBJECT_KEY, json!(subject));
        }
        self.publish_chained(&mut None, failed).await?;
        Err(refused.failure())
    }
}
