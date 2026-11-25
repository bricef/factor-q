//! A successful model call's `llm.response`, owed to the stream until its
//! publish returns (issue 1168).
//!
//! The event is the only one carrying what the call cost. The call's row
//! is closed with its cost before the event goes out, SQL first, as every
//! WAL write, so a bus fault or a stop at the publish used to leave the run
//! in flight with the cost on no event, and the resume replayed the row and
//! went on without it: no cost report counted the call. Now the close
//! keeps the event on the row, and the row lets it go once the publish
//! returns (`store/llm_response.rs` has why, and why that counts once);
//! a resume publishes whatever a row still keeps, the very event, before
//! the run goes on.
//!
//! **Only the last call can owe one.** Every publish here is awaited
//! before the run does anything else, and an error from it ends the run's
//! drive, so no later call starts while one is owed. A drain or a halt
//! stops the run at a step boundary, after the row let the event go. What
//! a resume finds is the run's last call, stopped at or just after its
//! publish, and every call before it is left alone.
//!
//! A child of `runner.rs`, as `deferral.rs` is, so the runner's config,
//! store and publish chain stay private.

use tracing::warn;
use uuid::Uuid;

use super::publish_failure::Refused;
use super::{ReducerRunner, map_store_err};
use crate::agent::AgentId;
use crate::events::{Event, EventPayload};
use crate::worker::ExecutorError;
use crate::worker::reducer::rounds::totals_of_wal_rows;
use crate::worker::reducer::types::Reducer;
use crate::worker::store::{LlmDispatchRow, ToolDispatchRow};

impl<R: Reducer + Send + Sync> ReducerRunner<R> {
    /// Close a successful call's row on its `response` (the response JSON)
    /// and `cost_usd`, keeping `event`, its `llm.response`, in the same
    /// write; publish the event; then let the row's copy go. A fault at the
    /// publish, or a stop before the last write, leaves the copy for the
    /// resume to publish.
    pub(super) async fn close_and_publish_response(
        &self,
        cursor: &mut Option<Uuid>,
        invocation_id: &str,
        request_id: &str,
        response: &str,
        cost_usd: f64,
        mut event: Event,
    ) -> Result<(), ExecutorError> {
        // Chained here as `publish_chained` will chain it, so the event the
        // row keeps is the one published.
        event.envelope.parent_event_id = *cursor;
        let completed_at = self.config.clock.unix_now_ms();
        self.config
            .store
            .write_llm_responded(
                invocation_id,
                request_id,
                response,
                cost_usd,
                completed_at,
                &event,
            )
            .await
            .map_err(map_store_err)?;
        self.publish_chained(cursor, event).await?;
        self.config
            .store
            .mark_llm_response_published(invocation_id, request_id)
            .await
            .map_err(map_store_err)
    }

    /// Publish every `llm.response` the run's rows still keep, the very
    /// events, in the order the rows closed, letting each go once it is
    /// out. Returns the event chain's cursor: the last one published, which
    /// the resumed run's own events chain on from, as a reproduced
    /// failure's `failed` chains on from its `llm.failure`; `None` when
    /// nothing was owed, and the resumed chain starts fresh.
    ///
    /// Each is measured first. One the broker would refuse for its size
    /// ends the run on the refusal's `failed`, with the totals its rows
    /// record (`llms`, `tools`), as the live run would have ended (#828),
    /// rather than leave it in flight for every start to refuse again.
    pub(super) async fn publish_owed(
        &self,
        agent_id: &AgentId,
        invocation_id: Uuid,
        llms: &[LlmDispatchRow],
        tools: &[ToolDispatchRow],
    ) -> Result<Option<Uuid>, ExecutorError> {
        let inv_str = invocation_id.to_string();
        let owed = self
            .config
            .store
            .unpublished_llm_responses(&inv_str)
            .await
            .map_err(map_store_err)?;
        let mut cursor = None;
        for (request_id, stored) in owed {
            let Some(event) = owed_response(invocation_id, &request_id, &stored) else {
                warn!(
                    invocation_id = %invocation_id,
                    call_id = %request_id,
                    "the call's row keeps an event this build cannot publish as its \
                     llm.response; the call's cost reaches only the run's totals"
                );
                continue;
            };
            if let Some(refused) = Refused::measure(&*self.config.sink, &event, stored.len()) {
                let totals = totals_of_wal_rows(llms, tools);
                let failed = refused.failed_event(agent_id, invocation_id, totals);
                self.fail_mid_step(&mut cursor, agent_id, failed).await?;
                return Err(refused.failure());
            }
            // Its own parent, as it went out the first time, whatever the
            // cursor holds.
            let mut published = None;
            self.publish_chained(&mut published, event).await?;
            cursor = published;
            self.config
                .store
                .mark_llm_response_published(&inv_str, &request_id)
                .await
                .map_err(map_store_err)?;
        }
        Ok(cursor)
    }
}

/// `stored`, read back as every consumer of the log reads an event, when
/// it is one this build can publish as the call's `llm.response`: naming
/// this run and this call. Anything else is `None`.
fn owed_response(invocation_id: Uuid, request_id: &str, stored: &str) -> Option<Event> {
    Event::from_wire(stored.as_bytes()).ok().filter(|event| {
        event.envelope.invocation_id == invocation_id
            && matches!(
                &event.payload,
                EventPayload::LlmResponse(response) if response.call_id.to_string() == request_id
            )
    })
}
