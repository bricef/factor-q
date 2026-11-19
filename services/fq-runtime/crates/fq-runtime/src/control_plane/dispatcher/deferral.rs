//! The dispatcher's half of a deferral (#278): a resume the deferral
//! queue says is due runs here, under a worker permit its task waited
//! for like any trigger, and a resume that is deferred again goes back
//! on the queue.

use std::time::Duration;

use tracing::{debug, info, warn};
use uuid::Uuid;

use super::TriggerDispatcher;
use super::run_admission::RunPermit;
use crate::agent::AgentId;
use crate::control_plane::agent_cap::AgentSlot;
use crate::worker::{ExecutorError, InvocationOutcome};

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
                .defer(&self.workers, invocation_id, agent_id, resume_after, slot),
            Ok(_) => {}
            Err(err) => {
                // A drop's stop is not an error of the run: its own info
                // line is the only one (issue 1070).
                if !matches!(err, ExecutorError::FinishedElsewhere { .. }) {
                    warn!(
                        agent_id = %agent_id,
                        error = %err,
                        "executor returned an error for NATS-triggered run"
                    );
                }
                self.log_executor_error(&err);
            }
        }
    }

    /// Resume `invocation_id` now, on `permit`, the worker permit its
    /// resume waited for. This is the invocation itself, however long it
    /// runs; the permit goes back when it returns, before its outcome is
    /// handled, so a deferral again lists it afresh.
    ///
    /// **No entry into the count happens here (#718).** The invocation
    /// never left it: `slot` is the very claim its trigger was admitted
    /// under, carried across the sleep by the deferral queue. Taking a
    /// fresh one here — which is what this did before — made a deferral a
    /// hole in the cap, because the entry it took was the ungated one.
    pub(super) async fn resume_deferred(
        &self,
        invocation_id: Uuid,
        agent_id: AgentId,
        deferred_for: Duration,
        slot: AgentSlot,
        permit: RunPermit,
    ) {
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
        let resumed = self
            .worker
            .resume_invocation(&loaded.agent, self.llm.as_ref(), invocation_id)
            .await;
        // The run is over: its worker goes back, and its listing with it,
        // which a deferral below takes again.
        drop(permit);
        match resumed {
            Ok(InvocationOutcome::Deferred {
                invocation_id,
                resume_after,
            }) => self
                .deferrals
                .defer(&self.workers, invocation_id, agent_id, resume_after, slot),
            Ok(outcome) => debug!(
                invocation_id = %invocation_id,
                ?outcome,
                "deferred invocation resumed to an outcome"
            ),
            Err(err) => {
                if !matches!(err, ExecutorError::FinishedElsewhere { .. }) {
                    warn!(
                        invocation_id = %invocation_id,
                        error = %err,
                        "deferred invocation's resume returned an error"
                    );
                }
                self.log_executor_error(&err);
            }
        }
    }
}
