//! Putting an invocation down for a rate limit (#278).
//!
//! A `RateLimited` error that reaches the runner is one the retry layer
//! gave up on — the provider asked for longer than
//! `max_retry_after_ms`, or the attempts ran out on a short ask. By the
//! maintainer's decision on #278 that is a **deferral**: the invocation
//! suspends at its step boundary with its WAL row in flight, says so
//! on the bus, and is resumed after a delay. No `failed` event, no
//! terminal, no retry consumed anywhere.
//!
//! It also holds the other reading of an errored model-call row on
//! resume: a failure whose ending was lost, which is reproduced, with the
//! call's own `llm.failure` and the run's totals (issue 1108).
//!
//! A child of `runner.rs` for the same reason `failure.rs` is: the
//! runner's config and store stay private, and the runner file may only
//! shrink.

use std::time::Duration;

use tracing::warn;
use uuid::Uuid;

use super::publish_failure::Refused;
use super::{InvocationCtx, ReducerRunner, map_store_err};
use crate::agent::AgentId;
use crate::events::{
    DeferralReason, Event, EventPayload, FailureKind, FailurePhase, InvocationDeferredPayload,
};
use crate::worker::ExecutorError;
use crate::worker::reducer::rounds::totals_of_wal_rows;
use crate::worker::reducer::types::Reducer;
use crate::worker::store::{LlmDispatchRow, ToolDispatchRow};

/// The `invocation_state.phase` a deferred row carries, for the stuck
/// sweep and the operator surface. Not a classifier: the phase is one
/// column the next step boundary overwrites, so a resume reads the
/// call's own row (`llm_dispatch.deferred_at`) to tell a deferral's 429
/// from a failure whose terminal was lost.
pub(super) const DEFERRED_PHASE: &str = "deferred";

impl<R: Reducer + Send + Sync> ReducerRunner<R> {
    /// Put the invocation down: stamp the call's WAL row, mark the state
    /// row deferred, then say so. WAL before bus, as everywhere in the
    /// runner — the rows are what a restart reads, the event is what an
    /// operator reads. The call's row carries the stamp because it is
    /// the fact a resume classifies by: the state row's phase moves on
    /// at the next step boundary, the row's `deferred_at` does not.
    ///
    /// `resume_after` is the throttle's answer for `model`: at least
    /// what the provider asked for, at least the model's escalating
    /// default pause, and at least the pause still in force.
    pub(super) async fn defer_invocation(
        &self,
        ctx: &mut InvocationCtx<'_>,
        call_id: Uuid,
        model: &str,
        provider_asked: Option<Duration>,
        resume_after: Duration,
    ) -> Result<(), ExecutorError> {
        warn!(
            agent_id = %ctx.agent_id,
            invocation_id = %ctx.invocation_id,
            call_id = %call_id,
            model,
            provider_asked_ms = provider_asked.map(|d| d.as_millis() as u64),
            resume_after_ms = resume_after.as_millis() as u64,
            "model rate-limited past the retry policy; deferring the invocation"
        );
        self.config
            .store
            .mark_llm_deferred(
                &ctx.invocation_id.to_string(),
                &call_id.to_string(),
                self.config.clock.unix_now_ms(),
            )
            .await
            .map_err(map_store_err)?;
        self.mark_deferred(ctx.invocation_id).await?;
        self.publish_chained(
            ctx.cursor,
            Event::new(
                ctx.agent_id.clone(),
                ctx.invocation_id,
                EventPayload::InvocationDeferred(InvocationDeferredPayload {
                    reason: DeferralReason::RateLimited,
                    model: model.to_string(),
                    retry_after_ms: resume_after.as_millis() as u64,
                }),
            ),
        )
        .await
    }

    /// The other reading of an errored LLM row on resume: a provider
    /// failure whose `failed` terminal was lost to the crash — the
    /// response column holds the error string, not a `ChatResponse`.
    /// The invocation's fate was already determined; reproduce it
    /// instead of trying to replay the row (finding 6, caught by the
    /// slice-7 deep soak: resume previously died on a deserialise error
    /// here). Always returns `Err`.
    ///
    /// The crash may have lost the call's own `llm.failure` too, the only
    /// event carrying its usage and cost, so the row's stored copy goes
    /// out first, the very event, and the run's `failed` after it, carrying
    /// the run's totals as its rows record them (`llms` and `tools`, the
    /// run's WAL rows), not zeros (issue 1108). The `llm.failure` goes out
    /// before the run's row is marked finished, the `failed` is stored in
    /// that mark, so a stop between the two leaves the run for the next
    /// start to reproduce again. A copy of `llm.failure` that had reached
    /// the stream already is the same event, and counts once
    /// (`store/llm_failure.rs` says how).
    pub(super) async fn reproduce_lost_failure(
        &self,
        agent_id: &AgentId,
        invocation_id: Uuid,
        row: &LlmDispatchRow,
        llms: &[LlmDispatchRow],
        tools: &[ToolDispatchRow],
    ) -> Result<crate::worker::InvocationOutcome, ExecutorError> {
        let message = row
            .response
            .clone()
            .unwrap_or_else(|| "provider error (no detail recorded)".to_string());
        let totals = totals_of_wal_rows(llms, tools);
        let mut cursor: Option<Uuid> = None;
        if let Some((failure, encoded_len)) = self.stored_llm_failure(invocation_id, row).await? {
            // An `llm.failure` the broker would refuse for its size ends the
            // run on the refusal's `failed`, as a live run's does (#828),
            // rather than leave it in flight for every start to refuse.
            if let Some(refused) = Refused::measure(&*self.config.sink, &failure, encoded_len) {
                let failed = refused.failed_event(agent_id, invocation_id, totals);
                self.fail_mid_step(&mut cursor, agent_id, failed).await?;
                return Err(refused.failure());
            }
            self.publish_chained(&mut cursor, failure).await?;
        }
        self.emit_failed(
            agent_id,
            invocation_id,
            FailureKind::LlmError,
            format!("{message} (reproduced on resume)"),
            FailurePhase::LlmRequest,
            totals,
            &mut cursor,
        )
        .await?;
        Err(ExecutorError::Llm(crate::llm::LlmError::RequestFailed(
            message,
        )))
    }

    /// The `llm.failure` the failed call's row keeps, with its length on
    /// the wire, when it is one this build can publish as that call's: read
    /// back as every consumer of the log reads an event, and naming this run
    /// and this call. Anything else is logged and not published, and the
    /// run's `failed` still carries the call's cost in its totals. `None`
    /// too for a row closed before the row kept one (worker schema v13).
    async fn stored_llm_failure(
        &self,
        invocation_id: Uuid,
        row: &LlmDispatchRow,
    ) -> Result<Option<(Event, usize)>, ExecutorError> {
        let Some(stored) = self
            .config
            .store
            .llm_failure_event(&row.invocation_id, &row.request_id)
            .await
            .map_err(map_store_err)?
        else {
            return Ok(None);
        };
        let event = Event::from_wire(stored.as_bytes()).ok().filter(|event| {
            event.envelope.invocation_id == invocation_id
                && matches!(
                    &event.payload,
                    EventPayload::LlmFailure(failure) if failure.call_id.to_string() == row.request_id
                )
        });
        if event.is_none() {
            warn!(
                invocation_id = %invocation_id,
                call_id = %row.request_id,
                "the failed call's row keeps an event this build cannot publish as its \
                 llm.failure; the call's cost reaches only the run's failed"
            );
        }
        Ok(event.map(|event| (event, stored.len())))
    }

    /// Set `phase = "deferred"` on the invocation's state row, keeping
    /// every other column, while the row is still the run's. A model call
    /// comes after the step's write, so a row that is finished, gone, or
    /// finished before this write lands was finished by someone else (an
    /// operator's drop, issue 1070): the run stops with
    /// [`ExecutorError::FinishedElsewhere`] instead of announcing a
    /// deferral that nothing will resume, and holding its agent's slot for
    /// the whole delay.
    async fn mark_deferred(&self, invocation_id: Uuid) -> Result<(), ExecutorError> {
        let key = invocation_id.to_string();
        let row = self
            .config
            .store
            .get_invocation_state(&key)
            .await
            .map_err(map_store_err)?;
        let Some(mut row) = row.filter(|row| row.terminal_at.is_none()) else {
            return Err(
                self.finished_elsewhere(invocation_id, "its row was finished at a deferral")
            );
        };
        row.phase = DEFERRED_PHASE.to_string();
        // A deferral is progress: the row moved, and the stuck sweep
        // measures silence from here.
        row.updated_at = self.config.clock.unix_now_ms();
        let landed = self
            .config
            .store
            .update_invocation_state(&row)
            .await
            .map_err(map_store_err)?;
        if landed {
            return Ok(());
        }
        Err(self.finished_elsewhere(invocation_id, "its row write did not land"))
    }
}
