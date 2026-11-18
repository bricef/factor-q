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
//! is kept). An event the sink would refuse for its size could never be
//! repeated, so it is measured before it is stored, and the run ends on
//! a `payload_too_large` `failed` in its place (#828).
//!
//! Every caller of `emit_failed` (an LLM error, a budget stop, a run of
//! tool timeouts, a reducer step that errors, the host step budget) is
//! the other order: it fails mid-step, before any terminal write, and
//! publishes `failed` before it marks the row, so a lost one leaves the
//! row in flight for recovery, and nothing is stored.

use std::time::Instant;

use tracing::{info, warn};
use uuid::Uuid;

use super::publish_failure::Refused;
use super::{ReducerRunner, harness_error_to_failure_kind, map_store_err};
use crate::agent::AgentId;
use crate::events::{
    CompletedPayload, Event, EventPayload, FailedPayload, FailureDetail, FailureKind, FailurePhase,
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

    /// The `phase` the terminal upsert records: the outcome's, which is
    /// not always the action's, since a `completed` too large for the
    /// broker ends the run `failed` (#828). The sweeper archives a row
    /// under its phase, so the two must agree.
    pub(super) fn phase(&self) -> &'static str {
        match self.ending {
            Ending::Completed { .. } => "completed",
            Ending::Failed { .. } => "failed",
        }
    }
}

/// A `failed` event, shared by every writer that ends a run: the run
/// loop's stored terminal here, `emit_failed`'s mid-step publish, the
/// refusal for an unavailable MCP server (`mcp.rs`), and the `failed` an
/// event too large for the broker ends a run with (`publish_failure.rs`),
/// the only one that passes a `detail` (#952).
pub(super) fn failed_event(
    agent_id: &AgentId,
    invocation_id: Uuid,
    error_kind: FailureKind,
    error_message: String,
    phase: FailurePhase,
    partial_totals: InvocationTotals,
    detail: Option<FailureDetail>,
) -> FailedEvent {
    FailedEvent {
        agent_id: agent_id.clone(),
        invocation_id,
        payload: FailedPayload {
            error_kind,
            error_message,
            phase,
            partial_totals,
            detail,
        },
        annotations: Vec::new(),
    }
}

/// A `failed` before it becomes an [`Event`]: what
/// [`ReducerRunner::publish_failed`] takes, so nothing but a `failed` can
/// be handed to it, and the run it ends is the one its envelope will
/// name. The stored terminal makes its event at once ([`Self::into_event`]);
/// `publish_failed` first sets the phase from the run's WAL row.
pub(super) struct FailedEvent {
    agent_id: AgentId,
    /// The run the `failed` ends, the envelope's `invocation_id`.
    pub(super) invocation_id: Uuid,
    pub(super) payload: FailedPayload,
    /// Envelope annotations, as [`Event::annotate`] takes them.
    annotations: Vec<(String, serde_json::Value)>,
}

impl FailedEvent {
    /// The same `failed`, with an annotation on its envelope.
    pub(super) fn annotate(mut self, key: &str, value: serde_json::Value) -> Self {
        self.annotations.push((key.to_string(), value));
        self
    }

    /// The event to publish or store.
    pub(super) fn into_event(self) -> Event {
        let event = Event::new(
            self.agent_id,
            self.invocation_id,
            EventPayload::Failed(self.payload),
        );
        self.annotations
            .into_iter()
            .fold(event, |event, (key, value)| event.annotate(key, value))
    }
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
        let (mut event, mut ending) = match action {
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
                    None,
                )
                .into_event();
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
        let mut encoded = terminal_outcome::encode(&event).map_err(map_store_err)?;
        // A terminal event the sink would refuse must not be the one the
        // row stores: no publish or sweep could ever deliver it, and the
        // row would be terminal with its outcome nowhere. The run ends on
        // the refusal's `failed` instead, decided while nothing is yet
        // terminal (#828; `publish_failure.rs`).
        if let Some(refused) = Refused::measure(&*self.config.sink, &event, encoded.len()) {
            event = refused
                .failed_event(agent_id, invocation_id, *totals)
                .into_event();
            event.envelope.parent_event_id = cursor;
            encoded = terminal_outcome::encode(&event).map_err(map_store_err)?;
            ending = Ending::Failed {
                kind: FailureKind::PayloadTooLarge,
                message: refused.message(),
            };
        }
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

    /// End a run that fails mid-step: [`Self::end_mid_step`], then hand
    /// off the archive.
    pub(super) async fn fail_mid_step(
        &self,
        cursor: &mut Option<Uuid>,
        agent_id: &AgentId,
        failed: FailedEvent,
    ) -> Result<(), ExecutorError> {
        let invocation_id = failed.invocation_id;
        self.end_mid_step(cursor, failed).await?;
        self.publish_archived_and_mark_pending(cursor, agent_id, invocation_id, "failed")
            .await
    }

    /// Publish a mid-step `failed`, then mark the row terminal: the point
    /// at which the run has ended.
    ///
    /// Every caller arrives before any terminal write (a `failed` the
    /// reducer decides is stored with the loop's terminal upsert instead,
    /// #946), so this is the point at which `invocation_state` is marked
    /// terminal, after the publish: a lost `failed` leaves the row in
    /// flight for recovery. The `failed` goes out through
    /// `publish_failed`, so a run that never wrote its row (a reducer whose
    /// first step errors) fails in `Setup`, whatever phase the caller built
    /// it with, and a run whose row an operator's drop already finished, or
    /// finished and handed off, publishes nothing and stops with
    /// `ExecutorError::FinishedElsewhere` before the row is touched (issue
    /// 1070; `own_row.rs`). The mark is still a no-op on a row that turned
    /// terminal between that check and here.
    pub(super) async fn end_mid_step(
        &self,
        cursor: &mut Option<Uuid>,
        failed: FailedEvent,
    ) -> Result<(), ExecutorError> {
        let invocation_id = failed.invocation_id;
        self.publish_failed(cursor, failed).await?;
        let terminal_at_ms = self.config.clock.unix_now_ms();
        self.ensure_terminal("failed", invocation_id, terminal_at_ms)
            .await
    }
}
