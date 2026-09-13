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
//!
//! # A deferral is not an exit
//!
//! A deferred invocation keeps its agent's concurrency slot (#718): the
//! [`AgentSlot`] travels *inside* [`DueResume`], from the `handle` that
//! admitted the trigger to the resume that finishes it. The queue owns
//! the claim while the invocation sleeps.
//!
//! That is what makes the cap an invariant rather than a hope. The first
//! shape released the slot at deferral and took a fresh, ungated one at
//! resume, which let a persistently 429'ing model convert a capped
//! agent's held backlog into an uncapped burst: two run, both defer,
//! two more start, defer… until the whole backlog is sleeping, and every
//! one of them resumes at once when the pause lifts. Sleeping is not
//! finishing — the WAL row is still `in_flight` and `control.doctor`
//! still folds it — so the slot stays taken, and `Drop` still covers
//! every way the resume can end, including a queue nobody drains.

use std::time::Duration;

use tokio::sync::mpsc;
use uuid::Uuid;

use crate::agent::AgentId;
use crate::control_plane::agent_cap::AgentSlot;

/// One resume the control plane owes an invocation: who, how long it was
/// put down for, and the agent-cap slot it never let go of.
///
/// Not `Clone`/`Eq`: the slot is a unique claim on the agent's cap, so a
/// due resume is a value that moves rather than one that copies.
#[derive(Debug)]
pub struct DueResume {
    pub invocation_id: Uuid,
    pub agent_id: AgentId,
    /// How long the invocation was deferred for — the delay that has
    /// now elapsed, kept for the log line.
    pub deferred_for: Duration,
    /// The claim the invocation was admitted under, carried across the
    /// sleep. Dropped with this value if nobody drains the queue, which
    /// is the daemon stopping — the next start's recovery counts the row
    /// again.
    pub slot: AgentSlot,
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
    ///
    /// `slot` is the invocation's claim on its agent's cap. It rides the
    /// queue rather than being released here: a sleeping invocation is
    /// still one of that agent's in-flight runs (#718), and dropping it
    /// at the deferral would let the resume come back through an ungated
    /// entry.
    pub fn defer(&self, invocation_id: Uuid, agent_id: AgentId, after: Duration, slot: AgentSlot) {
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
                slot,
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

    use crate::control_plane::agent_cap::AgentConcurrency;

    #[tokio::test(start_paused = true)]
    async fn a_deferral_is_delivered_when_its_delay_has_run() {
        let (queue, mut due) = DeferralQueue::new();
        let counts = AgentConcurrency::new();
        let id = Uuid::now_v7();
        let agent = AgentId::new("deferred-agent").unwrap();
        let slot = counts.try_enter(&agent, None).expect("no cap, no refusal");
        queue.defer(id, agent.clone(), Duration::from_secs(30), slot);
        let early = tokio::time::timeout(Duration::from_secs(29), due.recv()).await;
        assert!(early.is_err(), "not before the delay");
        let resume = tokio::time::timeout(Duration::from_secs(2), due.recv())
            .await
            .expect("due once the delay has run")
            .expect("the queue holds a sender");
        assert_eq!(resume.invocation_id, id);
        assert_eq!(resume.agent_id, agent);
        assert_eq!(resume.deferred_for, Duration::from_secs(30));
    }

    /// The cap invariant across a deferral (#718): the sleeping
    /// invocation still counts, so the agent's next trigger is refused
    /// for as long as it sleeps — and the resume runs on the slot it
    /// already had rather than taking a second one.
    #[tokio::test(start_paused = true)]
    async fn a_sleeping_invocation_still_holds_its_agents_slot() {
        let (queue, mut due) = DeferralQueue::new();
        let counts = AgentConcurrency::new();
        let agent = AgentId::new("builder").unwrap();
        let slot = counts.try_enter(&agent, Some(1)).expect("the first fits");
        queue.defer(Uuid::now_v7(), agent.clone(), Duration::from_secs(30), slot);
        assert_eq!(
            counts.in_flight(&agent),
            1,
            "a deferral is not an exit: the WAL row is still in flight"
        );
        assert!(
            counts.try_enter(&agent, Some(1)).is_none(),
            "a cap-1 agent with a sleeping invocation admits no second trigger"
        );
        let resume = tokio::time::timeout(Duration::from_secs(31), due.recv())
            .await
            .expect("due once the delay has run")
            .expect("the queue holds a sender");
        assert_eq!(
            counts.in_flight(&agent),
            1,
            "the resume arrives holding the one slot, not asking for another"
        );
        drop(resume);
        assert_eq!(counts.in_flight(&agent), 0, "and dropping it releases it");
    }

    #[tokio::test(start_paused = true)]
    async fn a_deferral_nobody_drains_is_dropped_quietly() {
        let (queue, due) = DeferralQueue::new();
        let counts = AgentConcurrency::new();
        let agent = AgentId::new("orphan").unwrap();
        drop(due);
        queue.defer(
            Uuid::now_v7(),
            agent.clone(),
            Duration::from_millis(10),
            counts.try_enter(&agent, Some(1)).expect("the first fits"),
        );
        // The timer task finishes without panicking, and the slot it was
        // carrying goes with it — a queue with no drain left must not
        // wedge the agent at its cap.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(counts.in_flight(&agent), 0);
    }
}
