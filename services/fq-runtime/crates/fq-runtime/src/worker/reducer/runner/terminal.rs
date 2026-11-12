//! The terminal event a step ends the run with: built before the
//! terminal write, published after it. That is `completed` (#823), and a
//! `failed` the reducer itself decides (#946).
//!
//! Split from `runner.rs` to keep that file and `run_loop_inner` inside
//! their size budgets — a child module, so the runner's private config and
//! publish chain stay private while this code can still reach them.
//!
//! The run loop marks the row terminal before it publishes anything, as
//! each of its per-step state writes does (SQL first, then NATS). A step
//! whose action ends the run — `NextAction::Complete`, or
//! `NextAction::Failed` (the iteration ceiling, a truncation streak, an
//! internal harness error) — is that write, and the event it ends in is
//! the only one carrying the outcome: task status, summary and totals for
//! `completed`; kind, message, phase and partial totals for `failed`. So
//! the event is built first and stored in that same terminal upsert
//! (`invocation_state.terminal_outcome`): there is no instant at which the
//! row is terminal and the outcome exists only in memory. A publish lost
//! after the write is the archive sweeper's to repeat, from the row, as
//! the same event (see `store/terminal_outcome.rs` for why the whole event
//! is kept).
//!
//! Every caller of `emit_failed` (an LLM error, a budget stop, a run of
//! tool timeouts, a reducer step that errors, the host step budget) is
//! the other order: it fails mid-step, before any terminal write, and
//! publishes `failed` before it marks the row, so a lost one leaves the
//! row in flight for recovery, and nothing is stored.

use std::time::Instant;

use tracing::{info, warn};
use uuid::Uuid;

use super::{ReducerRunner, harness_error_to_failure_kind, map_store_err};
use crate::agent::AgentId;
use crate::events::{
    CompletedPayload, Event, EventPayload, FailedPayload, FailureKind, FailurePhase,
    InvocationTotals,
};
use crate::llm::ChatResponse;
use crate::worker::reducer::types::{NextAction, Reducer};
use crate::worker::store::terminal_outcome;
use crate::worker::{ExecutorError, InvocationOutcome};

/// A terminal step's event, built before the terminal upsert so it lands
/// in the same write as `terminal_at`.
pub(super) struct Terminal {
    /// The terminal event exactly as it will be published, its
    /// `parent_event_id` already chained, so a republish is the same event.
    event: Event,
    /// `event` serialised for `invocation_state.terminal_outcome`.
    encoded: String,
    ending: Ending,
}

/// What the run returns once its terminal event is out.
enum Ending {
    Completed {
        summary: Option<String>,
        duration_ms: u64,
        cost: f64,
    },
    Failed {
        kind: FailureKind,
        message: String,
    },
}

impl Terminal {
    /// What the terminal upsert stores in `terminal_outcome`.
    pub(super) fn outcome(&self) -> String {
        self.encoded.clone()
    }
}

/// A `failed` event. The one constructor for both writers of one: the
/// run loop's stored terminal here, and `emit_failed`'s mid-step publish.
pub(super) fn failed_event(
    agent_id: &AgentId,
    invocation_id: Uuid,
    error_kind: FailureKind,
    error_message: String,
    phase: FailurePhase,
    partial_totals: InvocationTotals,
) -> Event {
    Event::new(
        agent_id.clone(),
        invocation_id,
        EventPayload::Failed(FailedPayload {
            error_kind,
            error_message,
            phase,
            partial_totals,
            detail: None,
        }),
    )
}

/// Stamp the invocation's duration on `totals`, which the terminal event
/// carries, and return it.
fn stamp_duration(totals: &mut InvocationTotals, start: Instant) -> u64 {
    totals.total_duration_ms = start.elapsed().as_millis() as u64;
    totals.total_duration_ms
}

impl<R: Reducer + Send + Sync> ReducerRunner<R> {
    /// The terminal event a `Complete` or `Failed` step ends in; `None`
    /// for an action that does not end the run. Stamps the invocation's
    /// duration on `totals` as it does so, since the event carries it.
    ///
    /// `cursor` is chained onto the event here rather than at publish:
    /// nothing is published between this and [`Self::finish`], so
    /// `publish_chained` sets the same parent, and the stored event *is*
    /// the published one.
    pub(super) fn terminal(
        &self,
        action: &NextAction,
        agent_id: &AgentId,
        invocation_id: Uuid,
        totals: &mut InvocationTotals,
        start: Instant,
        cursor: Option<Uuid>,
    ) -> Result<Option<Terminal>, ExecutorError> {
        let (mut event, ending) = match action {
            NextAction::Complete { text, task_status } => {
                let duration_ms = stamp_duration(totals, start);
                let summary = (!text.is_empty()).then(|| text.clone());
                let event = Event::new(
                    agent_id.clone(),
                    invocation_id,
                    EventPayload::Completed(CompletedPayload {
                        task_status: *task_status,
                        result_summary: summary.clone(),
                        total_llm_calls: totals.total_llm_calls,
                        total_tool_calls: totals.total_tool_calls,
                        total_cost: totals.total_cost,
                        total_duration_ms: duration_ms,
                    }),
                );
                let ending = Ending::Completed {
                    summary,
                    duration_ms,
                    cost: totals.total_cost,
                };
                (event, ending)
            }
            NextAction::Failed(err) => {
                stamp_duration(totals, start);
                let kind = harness_error_to_failure_kind(err);
                let event = failed_event(
                    agent_id,
                    invocation_id,
                    kind,
                    err.message.clone(),
                    FailurePhase::Reducer,
                    *totals,
                );
                let ending = Ending::Failed {
                    kind,
                    message: err.message.clone(),
                };
                (event, ending)
            }
            NextAction::CallModel(_)
            | NextAction::CallTool(_)
            | NextAction::CallToolsParallel(_) => return Ok(None),
        };
        event.envelope.parent_event_id = cursor;
        let encoded = terminal_outcome::encode(&event).map_err(map_store_err)?;
        Ok(Some(Terminal {
            event,
            encoded,
            ending,
        }))
    }

    /// Publish a terminal event whose row is already terminal: the event,
    /// then the `terminal_published_at` stamp that tells the sweeper it
    /// went out, then `invocation.archived`. A failure at any step leaves
    /// the rest to the sweeper, which republishes the event first while
    /// the stamp is missing — the same event, so a republish of one that
    /// did land carries its `event_id` (at-least-once delivery; see
    /// `store/terminal_outcome.rs` for what deduplicates it, and when).
    ///
    /// A completion returns `Completed`; a reducer-decided failure returns
    /// `InvocationFailed`, with the kind its `failed` event carries.
    pub(super) async fn finish(
        &self,
        cursor: &mut Option<Uuid>,
        agent_id: &AgentId,
        invocation_id: Uuid,
        terminal: Terminal,
    ) -> Result<InvocationOutcome, ExecutorError> {
        let final_phase = match &terminal.ending {
            Ending::Completed { .. } => "completed",
            Ending::Failed { kind, .. } => {
                warn!(
                    agent_id = %agent_id,
                    invocation_id = %invocation_id,
                    error_kind = ?kind,
                    "reducer invocation failed"
                );
                "failed"
            }
        };
        self.publish_chained(cursor, terminal.event).await?;
        self.config
            .store
            .mark_terminal_published(&invocation_id.to_string(), self.config.clock.unix_now_ms())
            .await
            .map_err(map_store_err)?;
        self.publish_archived_and_mark_pending(cursor, agent_id, invocation_id, final_phase)
            .await?;
        match terminal.ending {
            Ending::Completed {
                summary,
                duration_ms,
                cost,
            } => {
                info!(
                    agent_id = %agent_id,
                    invocation_id = %invocation_id,
                    duration_ms,
                    cost,
                    "reducer invocation completed"
                );
                Ok(InvocationOutcome::Completed {
                    invocation_id,
                    response: ChatResponse::completed(summary),
                    cost,
                    duration_ms,
                })
            }
            Ending::Failed { kind, message } => {
                Err(ExecutorError::InvocationFailed { kind, message })
            }
        }
    }
}
