//! The dispatcher's half of a deferral (#278): a resume the deferral
//! queue says is due runs here, under the same permit a trigger runs
//! under, and a resume that is deferred again goes back on the queue.

use tracing::{debug, info, warn};

use super::TriggerDispatcher;
use crate::agent::AgentId;
use crate::control_plane::agent_cap::AgentSlot;
use crate::worker::{DueResume, ExecutorError, InvocationOutcome};

impl TriggerDispatcher {
    /// What becomes of an invocation's outcome once its trigger's fate
    /// is settled: a deferral goes on the queue (#278) — the trigger
    /// was acked at the first WAL write, the row is in flight, and the
    /// resume is this dispatcher's to run after the delay; an error is
    /// logged — the executor already emitted `failed`, the trigger is
    /// acked and the WAL owns recovery, so there is nothing to
    /// redeliver.
    ///
    /// `slot` is the invocation's claim on its agent's cap, handed over
    /// because this is where the invocation's fate is decided (#718): a
    /// deferral puts the claim on the queue with the resume, and every
    /// other outcome drops it here, which is the invocation ending.
    pub(super) fn conclude(
        &self,
        agent_id: AgentId,
        result: Result<InvocationOutcome, ExecutorError>,
        slot: AgentSlot,
    ) {
        match result {
            Ok(InvocationOutcome::Deferred {
                invocation_id,
                resume_after,
            }) => self
                .deferrals
                .defer(invocation_id, agent_id, resume_after, slot),
            Ok(_) => {}
            Err(err) => {
                warn!(
                    agent_id = %agent_id,
                    error = %err,
                    "executor returned an error for NATS-triggered run"
                );
                self.log_executor_error(&err);
            }
        }
    }

    /// Resume `due.invocation_id` now. The caller holds the concurrency
    /// permit; this is the invocation itself, however long it runs.
    ///
    /// **No entry into the count happens here (#718).** The invocation
    /// never left it: `due.slot` is the very claim its trigger was
    /// admitted under, carried across the sleep by the deferral queue.
    /// Taking a fresh one here — which is what this did before — made a
    /// deferral a hole in the cap, because the entry it took was the
    /// ungated one.
    pub(super) async fn resume_deferred(&self, due: DueResume) {
        let DueResume {
            invocation_id,
            agent_id,
            deferred_for,
            slot,
        } = due;
        let registry = self.registry.current();
        let Some(loaded) = registry.get_loaded(&agent_id) else {
            warn!(
                invocation_id = %invocation_id,
                agent_id = %agent_id,
                "deferred invocation's agent is no longer loaded; leaving its row for triage"
            );
            // `slot` drops with this return: an invocation nobody will
            // resume is not one of that agent's in-flight runs.
            return;
        };
        info!(
            invocation_id = %invocation_id,
            agent_id = %agent_id,
            deferred_for_ms = deferred_for.as_millis() as u64,
            "resuming a deferred invocation"
        );
        match self
            .worker
            .resume_invocation(&loaded.agent, self.llm.as_ref(), invocation_id)
            .await
        {
            Ok(InvocationOutcome::Deferred {
                invocation_id,
                resume_after,
            }) => self
                .deferrals
                .defer(invocation_id, agent_id, resume_after, slot),
            Ok(outcome) => debug!(
                invocation_id = %invocation_id,
                ?outcome,
                "deferred invocation resumed to an outcome"
            ),
            Err(err) => {
                warn!(
                    invocation_id = %invocation_id,
                    error = %err,
                    "deferred invocation's resume returned an error"
                );
                self.log_executor_error(&err);
            }
        }
    }
}
