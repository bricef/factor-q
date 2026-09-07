//! The deferral queue (#278): where an invocation the runtime has put
//! down — rate-limited past what the retry layer will wait in place —
//! is picked up again.
//!
//! A deferral is decided in the worker (the runner returns
//! [`InvocationOutcome::Deferred`](super::InvocationOutcome::Deferred))
//! and carried out by whoever drives invocations: the trigger dispatcher
//! drains this queue under the same concurrency permit it runs triggers
//! on, so a resumed invocation counts against `max_concurrent_invocations`
//! like a fresh one. Startup recovery and `fq invocation resume` hand a
//! deferred outcome to the same queue.
//!
//! The queue is deliberately in-process and not durable: a deferred
//! invocation's WAL row stays in flight, so a daemon that stops before
//! the timer fires resumes it at the next start through ordinary
//! recovery. The timer is a convenience; the WAL is the truth.

use std::time::Duration;

use tokio::sync::mpsc;
use uuid::Uuid;

use crate::agent::AgentId;

/// One resume the control plane owes an invocation: who, and how long
/// it was put down for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueResume {
    pub invocation_id: Uuid,
    pub agent_id: AgentId,
    /// How long the invocation was deferred for — the delay that has
    /// now elapsed, kept for the log line.
    pub deferred_for: Duration,
}

/// The handle a deferral is handed to. Cloneable, so the dispatcher, the
/// startup recovery and the operator resume path all reach one queue.
#[derive(Debug, Clone)]
pub struct DeferralQueue {
    due: mpsc::Sender<DueResume>,
}

impl DeferralQueue {
    /// A queue and its drain end. The dispatcher takes the receiver;
    /// everything else clones the handle.
    pub fn new() -> (Self, mpsc::Receiver<DueResume>) {
        // Bounded, but the bound is never the interesting number: a
        // deferral is one message per invocation per pause, and the
        // dispatcher drains as fast as it can take a permit.
        let (due, receiver) = mpsc::channel(256);
        (Self { due }, receiver)
    }

    /// Put `invocation_id` down for `after`, then hand it to whoever is
    /// draining the queue. Fire-and-forget: if nobody is draining by the
    /// time it is due (the daemon is stopping), the send fails quietly
    /// and the next start's recovery resumes the row.
    pub fn defer(&self, invocation_id: Uuid, agent_id: AgentId, after: Duration) {
        let due = self.due.clone();
        tracing::info!(
            invocation_id = %invocation_id,
            agent_id = %agent_id,
            resume_in_ms = after.as_millis() as u64,
            "invocation deferred; a resume is scheduled"
        );
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            let resume = DueResume {
                invocation_id,
                agent_id,
                deferred_for: after,
            };
            if due.send(resume).await.is_err() {
                tracing::debug!(
                    invocation_id = %invocation_id,
                    "the deferral queue has no drain left; startup recovery resumes the row"
                );
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_deferral_is_delivered_when_its_delay_has_run() {
        let (queue, mut due) = DeferralQueue::new();
        let id = Uuid::now_v7();
        let agent = AgentId::new("deferred-agent").unwrap();
        queue.defer(id, agent.clone(), Duration::from_secs(30));
        let early = tokio::time::timeout(Duration::from_secs(29), due.recv()).await;
        assert!(early.is_err(), "not before the delay");
        let resume = tokio::time::timeout(Duration::from_secs(2), due.recv())
            .await
            .expect("due once the delay has run")
            .expect("the queue holds a sender");
        assert_eq!(
            resume,
            DueResume {
                invocation_id: id,
                agent_id: agent,
                deferred_for: Duration::from_secs(30),
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_deferral_nobody_drains_is_dropped_quietly() {
        let (queue, due) = DeferralQueue::new();
        drop(due);
        queue.defer(
            Uuid::now_v7(),
            AgentId::new("orphan").unwrap(),
            Duration::from_millis(10),
        );
        // The timer task finishes without panicking; nothing to observe
        // but the absence of a crash.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
