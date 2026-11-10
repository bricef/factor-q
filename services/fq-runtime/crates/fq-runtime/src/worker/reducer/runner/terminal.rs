//! The `completed` terminal: built before the terminal write, published
//! after it (#823).
//!
//! Split from `runner.rs` to keep that file and `run_loop_inner` inside
//! their size budgets — a child module, so the runner's private config and
//! publish chain stay private while this code can still reach them.
//!
//! The run loop marks the row terminal before it publishes anything, as
//! every write in the runner does (SQL first, then NATS). `completed` is
//! the only event carrying the outcome, so it is built first and stored in
//! that same terminal upsert (`invocation_state.terminal_outcome`): there
//! is no instant at which the row is terminal and the outcome exists only
//! in memory. A publish lost after the write is the archive sweeper's to
//! repeat, from the row, as the same event (see `store/terminal_outcome.rs`
//! for why the whole event is kept).

use std::time::Instant;

use tracing::info;
use uuid::Uuid;

use super::{ReducerRunner, map_store_err};
use crate::agent::AgentId;
use crate::events::{CompletedPayload, Event, EventPayload, InvocationTotals};
use crate::llm::ChatResponse;
use crate::worker::reducer::types::{NextAction, Reducer};
use crate::worker::store::terminal_outcome;
use crate::worker::{ExecutorError, InvocationOutcome};

/// A `Complete` step's outcome, built before the terminal upsert so its
/// `completed` event lands in the same write as `terminal_at`.
pub(super) struct Completion {
    /// The `completed` event exactly as it will be published, its
    /// `parent_event_id` already chained, so a republish is the same event.
    event: Event,
    /// `event` serialised for `invocation_state.terminal_outcome`.
    encoded: String,
    summary: Option<String>,
    duration_ms: u64,
}

impl Completion {
    /// What the terminal upsert stores in `terminal_outcome`.
    pub(super) fn outcome(&self) -> String {
        self.encoded.clone()
    }
}

impl<R: Reducer + Send + Sync> ReducerRunner<R> {
    /// The completion a `Complete` step ends in; `None` for any other
    /// action. Stamps the invocation's duration on `totals` as it does so,
    /// since the event it builds carries it.
    ///
    /// `cursor` is chained onto the event here rather than at publish:
    /// nothing is published between this and [`Self::finish_completion`],
    /// so `publish_chained` sets the same parent, and the stored event *is*
    /// the published one.
    pub(super) fn completion(
        &self,
        action: &NextAction,
        agent_id: &AgentId,
        invocation_id: Uuid,
        totals: &mut InvocationTotals,
        start: Instant,
        cursor: Option<Uuid>,
    ) -> Result<Option<Completion>, ExecutorError> {
        let NextAction::Complete { text, task_status } = action else {
            return Ok(None);
        };
        let duration_ms = start.elapsed().as_millis() as u64;
        totals.total_duration_ms = duration_ms;
        let summary = (!text.is_empty()).then(|| text.clone());
        let mut event = Event::new(
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
        event.envelope.parent_event_id = cursor;
        let encoded = terminal_outcome::encode(&event).map_err(map_store_err)?;
        Ok(Some(Completion {
            event,
            encoded,
            summary,
            duration_ms,
        }))
    }

    /// Publish a completion whose row is already terminal: `completed`,
    /// then the `terminal_published_at` stamp that tells the sweeper it went
    /// out, then `invocation.archived`. A failure at any step leaves the
    /// rest to the sweeper, which republishes `completed` first while the
    /// stamp is missing — the same event, so a republish of one that did
    /// land is deduplicated on its `event_id`.
    pub(super) async fn finish_completion(
        &self,
        cursor: &mut Option<Uuid>,
        agent_id: &AgentId,
        invocation_id: Uuid,
        completion: Completion,
        total_cost: f64,
    ) -> Result<InvocationOutcome, ExecutorError> {
        self.publish_chained(cursor, completion.event).await?;
        self.config
            .store
            .mark_terminal_published(&invocation_id.to_string(), self.config.clock.unix_now_ms())
            .await
            .map_err(map_store_err)?;
        self.publish_archived_and_mark_pending(cursor, agent_id, invocation_id, "completed")
            .await?;
        info!(
            agent_id = %agent_id,
            invocation_id = %invocation_id,
            duration_ms = completion.duration_ms,
            cost = total_cost,
            "reducer invocation completed"
        );
        Ok(InvocationOutcome::Completed {
            invocation_id,
            response: ChatResponse::completed(completion.summary),
            cost: total_cost,
            duration_ms: completion.duration_ms,
        })
    }
}
