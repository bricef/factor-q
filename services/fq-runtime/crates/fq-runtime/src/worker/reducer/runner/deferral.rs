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
use crate::events::{DeferralReason, Event, EventPayload, InvocationDeferredPayload};
use crate::worker::ExecutorError;
use crate::worker::reducer::types::Reducer;

/// The `invocation_state.phase` a deferred row carries. Recovery reads
/// it: an errored LLM row under this phase is the recorded 429 the
/// deferral was decided on, not a failure whose terminal was lost.
pub(super) const DEFERRED_PHASE: &str = "deferred";

impl<R: Reducer + Send + Sync> ReducerRunner<R> {
    /// Put the invocation down: mark the WAL row deferred, then say so.
    /// WAL before bus, as everywhere in the runner — the row is what a
    /// restart reads, the event is what an operator reads.
    ///
    /// `resume_after` is the throttle's answer for `model`: at least
    /// what the provider asked for, at least the model's escalating
    /// default pause, and at least the pause still in force.
    pub(super) async fn defer_invocation(
        &self,
        ctx: &mut InvocationCtx<'_>,
        model: &str,
        provider_asked: Option<Duration>,
        resume_after: Duration,
    ) -> Result<(), ExecutorError> {
        warn!(
            agent_id = %ctx.agent_id,
            invocation_id = %ctx.invocation_id,
            model,
            provider_asked_ms = provider_asked.map(|d| d.as_millis() as u64),
            resume_after_ms = resume_after.as_millis() as u64,
            "model rate-limited past the retry policy; deferring the invocation"
        );
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

    /// Set `phase = "deferred"` on the invocation's state row, keeping
    /// every other column. The row exists — a model call happens after
    /// the step's upsert — and is not terminal; if either is untrue the
    /// deferral is still announced, but a resume will find whatever the
    /// row actually says.
    async fn mark_deferred(&self, invocation_id: Uuid) -> Result<(), ExecutorError> {
        let key = invocation_id.to_string();
        let row = self
            .config
            .store
            .get_invocation_state(&key)
            .await
            .map_err(map_store_err)?;
        let Some(mut row) = row else {
            warn!(
                invocation_id = %invocation_id,
                "no state row to mark deferred; the deferral is announced anyway"
            );
            return Ok(());
        };
        if row.terminal_at.is_some() {
            return Ok(());
        }
        row.phase = DEFERRED_PHASE.to_string();
        // A deferral is progress: the row moved, and the stuck sweep
        // measures silence from here.
        row.updated_at = self.config.clock.unix_now_ms();
        self.config
            .store
            .upsert_invocation_state(&row)
            .await
            .map_err(map_store_err)
    }
}
