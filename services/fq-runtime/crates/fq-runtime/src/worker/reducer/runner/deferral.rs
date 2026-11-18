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
//! A child of `runner.rs` for the same reason `failure.rs` is: the
//! runner's config and store stay private, and the runner file may only
//! shrink.

use std::time::Duration;

use tracing::warn;
use uuid::Uuid;

use super::{InvocationCtx, ReducerRunner, map_store_err};
use crate::agent::AgentId;
use crate::events::{
    DeferralReason, Event, EventPayload, FailureKind, FailurePhase, InvocationDeferredPayload,
    InvocationTotals,
};
use crate::worker::ExecutorError;
use crate::worker::reducer::types::Reducer;
use crate::worker::store::LlmDispatchRow;

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
    pub(super) async fn reproduce_lost_failure(
        &self,
        agent_id: &AgentId,
        invocation_id: Uuid,
        row: &LlmDispatchRow,
    ) -> Result<crate::worker::InvocationOutcome, ExecutorError> {
        let message = row
            .response
            .clone()
            .unwrap_or_else(|| "provider error (no detail recorded)".to_string());
        let mut cursor: Option<Uuid> = None;
        self.emit_failed(
            agent_id,
            invocation_id,
            FailureKind::LlmError,
            format!("{message} (reproduced on resume)"),
            FailurePhase::LlmRequest,
            InvocationTotals::default(),
            &mut cursor,
        )
        .await?;
        Err(ExecutorError::Llm(crate::llm::LlmError::RequestFailed(
            message,
        )))
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
