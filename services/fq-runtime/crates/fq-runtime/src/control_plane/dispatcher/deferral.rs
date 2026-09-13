//! The dispatcher's half of a deferral (#278): a resume the deferral
//! queue says is due runs here, under the same permit a trigger runs
//! under, and a resume that is deferred again goes back on the queue.

use tracing::{debug, info, warn};

use super::TriggerDispatcher;
use crate::agent::AgentId;
use crate::worker::{DueResume, ExecutorError, InvocationOutcome};

impl TriggerDispatcher {
    /// What becomes of an invocation's outcome once its trigger's fate
    /// is settled: a deferral goes on the queue (#278) — the trigger
    /// was acked at the first WAL write, the row is in flight, and the
    /// resume is this dispatcher's to run after the delay; an error is
    /// logged — the executor already emitted `failed`, the trigger is
    /// acked and the WAL owns recovery, so there is nothing to
    /// redeliver.
    pub(super) fn conclude(
        &self,
        agent_id: AgentId,
        result: Result<InvocationOutcome, ExecutorError>,
    ) {
        match result {
            Ok(InvocationOutcome::Deferred {
                invocation_id,
                resume_after,
            }) => self.deferrals.defer(invocation_id, agent_id, resume_after),
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
    pub(super) async fn resume_deferred(&self, due: DueResume) {
        let registry = self.registry.read().await.clone();
        let Some(loaded) = registry.get_loaded(&due.agent_id) else {
            warn!(
                invocation_id = %due.invocation_id,
                agent_id = %due.agent_id,
                "deferred invocation's agent is no longer loaded; leaving its row for triage"
            );
            return;
        };
        // Counted, not gated (#718): the cap bounds what *starts*, and
        // this invocation was admitted when its trigger was. Holding it
        // back would keep the work on the host for longer, not less.
        let _agent_slot = self
            .agent_caps
            .enter(due.agent_id.as_str(), loaded.agent.max_concurrent());
        info!(
            invocation_id = %due.invocation_id,
            agent_id = %due.agent_id,
            deferred_for_ms = due.deferred_for.as_millis() as u64,
            "resuming a deferred invocation"
        );
        match self
            .worker
            .resume_invocation(&loaded.agent, self.llm.as_ref(), due.invocation_id)
            .await
        {
            Ok(InvocationOutcome::Deferred {
                invocation_id,
                resume_after,
            }) => self
                .deferrals
                .defer(invocation_id, due.agent_id, resume_after),
            Ok(outcome) => debug!(
                invocation_id = %due.invocation_id,
                ?outcome,
                "deferred invocation resumed to an outcome"
            ),
            Err(err) => {
                warn!(
                    invocation_id = %due.invocation_id,
                    error = %err,
                    "deferred invocation's resume returned an error"
                );
                self.log_executor_error(&err);
            }
        }
    }
}
