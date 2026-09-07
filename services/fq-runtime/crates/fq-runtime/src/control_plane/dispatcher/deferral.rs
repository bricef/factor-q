//! The dispatcher's half of a deferral (#278): a resume the deferral
//! queue says is due runs here, under the same permit a trigger runs
//! under, and a resume that is deferred again goes back on the queue.

use tracing::{debug, info, warn};

use super::TriggerDispatcher;
use crate::worker::{DueResume, InvocationOutcome};

impl TriggerDispatcher {
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
