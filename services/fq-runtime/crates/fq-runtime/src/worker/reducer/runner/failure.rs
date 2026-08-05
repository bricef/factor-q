//! Closing an LLM call that produced no response (#447).
//!
//! Split from `runner.rs` to keep that file inside its size budget —
//! a child module, so the runner's private config and round ledger
//! stay private while this code can still reach them.

use tracing::warn;
use uuid::Uuid;

use super::{ReducerRunner, map_store_err};
use crate::events::{self as events, Event, EventPayload, InvocationTotals};
use crate::worker::ExecutorError;
use crate::worker::reducer::emit::{self, FailedCall};
use crate::worker::reducer::types::Reducer;

impl<R: Reducer + Send + Sync> ReducerRunner<R> {
    /// Close a call that produced no response: WAL `dispatched` →
    /// `completed(is_error)`, then the two events that make the failure
    /// visible — `llm.dispatched`, so the middle state the WAL already
    /// records is on the bus too, and the terminal `llm.failure`.
    ///
    /// Before this existed both arms closed the WAL and returned
    /// without touching the bus, so the trail held an `llm.request`
    /// with no outcome and a consumer folding the pair could not tell
    /// "the call failed" from "the event was lost" (#447).
    ///
    /// The Round is the current one, not a fresh one — only building a
    /// response event calls `rounds.next`, so a failure consumes none
    /// today. That asymmetry is real but is its own change.
    ///
    /// Invocation status is untouched. A failed *call* is not a failed
    /// *invocation* — an agent turn publishes `failed` separately, and
    /// a sampling failure is deliberately non-fatal.
    pub(super) async fn fail_llm_call(
        &self,
        call: FailedCall<'_>,
        totals: &mut InvocationTotals,
        cursor: &mut Option<Uuid>,
    ) -> Result<(), ExecutorError> {
        // Priced only when the provider's usage survived *and* the
        // model has pricing. Anything else leaves cost absent rather
        // than zero — `None` means "we cannot see what it billed".
        let priced = call
            .usage
            .zip(self.config.pricing.lookup(call.model))
            .map(|(usage, pricing)| pricing.calculate(&usage));
        let total_cost = priced.map(|(_, _, total)| total).unwrap_or(0.0);
        if call.usage.is_some() && priced.is_none() {
            warn!(
                model = %call.model,
                "no pricing known for model; recovered failure usage cannot be priced"
            );
        }
        // Recovered spend counts against the invocation and its budget
        // like any other: money the provider took is money spent.
        totals.total_cost += total_cost;

        let inv_str = call.invocation_id.to_string();
        let req_str = call.call_id.to_string();
        self.config
            .store
            .write_llm_dispatched(&inv_str, &req_str, self.config.clock.unix_now_ms())
            .await
            .map_err(map_store_err)?;
        self.config
            .store
            .write_llm_completed(
                &inv_str,
                &req_str,
                &call.error_message,
                true,
                total_cost,
                self.config.clock.unix_now_ms(),
            )
            .await
            .map_err(map_store_err)?;

        self.publish_chained(
            cursor,
            Event::new(
                call.agent_id.clone(),
                call.invocation_id,
                EventPayload::LlmDispatched(events::LlmDispatchedPayload {
                    call_id: call.call_id,
                    model: call.model.to_string(),
                }),
            ),
        )
        .await?;
        self.publish_chained(
            cursor,
            emit::llm_failure_event(
                self.rounds.current(call.invocation_id),
                &call,
                priced,
                totals.total_cost,
            ),
        )
        .await
    }
}
