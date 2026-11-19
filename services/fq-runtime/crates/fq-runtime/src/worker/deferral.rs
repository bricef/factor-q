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
//!
//! # A drop ends a deferral (issue 1070)
//!
//! Nothing drives a deferred invocation while it sleeps, so `fq invocation
//! drop` takes it as it takes a resume queued for a worker permit: the
//! deferral is listed in the worker permits from the moment it is put
//! down ([`Deferral`], travelling inside [`DueResume`] beside the slot),
//! and a drop that goes through wakes the sleep. The timer then ends
//! there: the invocation is not resumed, asks its model nothing more, and
//! its slot goes back at the drop, not when the delay would have run out.
//! A drop after the delay has run finds the same listing on the resume
//! waiting for its permit, which never starts.
//!
//! One instant has no listing to reach: the runner stops driving the
//! invocation when it returns `Deferred`, and it is listed only when its
//! outcome reaches [`DeferralQueue::defer`]. A drop in between, or a
//! `--live` one whose halt the run never reached a step boundary to see,
//! finds nothing to hold. The permits remember the drop instead
//! (`WorkerPermits::note_dropped`), and the deferral that follows is
//! refused: the invocation is never put down, its slot goes back at once,
//! and nothing resumes it. A run dropped live that way has announced its
//! deferral on the stream by then, after the drop; nothing follows it.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use uuid::Uuid;

use crate::agent::AgentId;
use crate::control_plane::agent_cap::AgentSlot;
use crate::control_plane::dispatcher::{Deferral, NotListed, WorkerPermits};

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
    /// The invocation's listing for a drop, held from the deferral until
    /// its resume ends: the resume waits for its permit and starts under
    /// it.
    pub deferral: Deferral,
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
    ///
    /// `workers` are the permits the resume will wait on, and where the
    /// invocation is listed for a drop to reach meanwhile. An invocation
    /// already listed there, queued or running, is not put down a second
    /// time, and one a drop went through for in the instant before is not
    /// put down at all: its slot goes back here and nothing is scheduled.
    pub fn defer(
        &self,
        workers: &Arc<WorkerPermits>,
        invocation_id: Uuid,
        agent_id: AgentId,
        after: Duration,
        slot: AgentSlot,
    ) {
        let deferral = match workers.list_deferred(invocation_id) {
            Ok(deferral) => deferral,
            // `slot` goes with these returns: nothing is put down.
            Err(NotListed::Dropped) => {
                tracing::info!(
                    invocation_id = %invocation_id,
                    agent_id = %agent_id,
                    "an operator dropped the invocation as it was put down; it is not resumed"
                );
                return;
            }
            Err(NotListed::AlreadyListed) => {
                tracing::warn!(
                    invocation_id = %invocation_id,
                    agent_id = %agent_id,
                    "invocation deferred while another run of it is queued or running in this \
                     daemon; no second resume is scheduled"
                );
                return;
            }
        };
        let due = self.due.clone();
        tracing::info!(
            invocation_id = %invocation_id,
            agent_id = %agent_id,
            resume_in_ms = after.as_millis() as u64,
            "invocation deferred; a resume is scheduled"
        );
        tokio::spawn(async move {
            if !deferral.sleep(after).await {
                // `slot` and `deferral` go with this return: the drop is
                // the invocation's end in this process.
                tracing::info!(
                    invocation_id = %invocation_id,
                    agent_id = %agent_id,
                    "an operator dropped the invocation while it was deferred; it is not resumed"
                );
                return;
            }
            let resume = DueResume {
                invocation_id,
                agent_id,
                deferred_for: after,
                slot,
                deferral,
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

    use crate::agent::Agent;
    use crate::control_plane::agent_cap::AgentConcurrency;
    use crate::control_plane::dispatcher::Waiter;
    use crate::llm::LlmClient;
    use crate::trigger::Trigger;
    use crate::worker::{
        DrainRequest, DrainState, DurableStart, ExecutorError, InvocationOutcome, Worker,
    };

    /// A worker that is never draining: all the permits ask of one.
    struct NeverDraining;

    #[async_trait::async_trait]
    impl Worker for NeverDraining {
        async fn run_invocation(
            &self,
            _agent: &Agent,
            _llm: &dyn LlmClient,
            _trigger: Trigger,
            _delivery_attempt: Option<u32>,
            _durable_start: DurableStart,
        ) -> Result<InvocationOutcome, ExecutorError> {
            unreachable!("nothing runs on these permits")
        }
        async fn request_drain(&self, _req: DrainRequest) {}
        fn drain_status(&self) -> DrainState {
            DrainState::Running
        }
    }

    /// The worker permits a deferral is listed in.
    fn permits() -> Arc<WorkerPermits> {
        WorkerPermits::new(Arc::new(NeverDraining), 1)
    }

    #[tokio::test(start_paused = true)]
    async fn a_deferral_is_delivered_when_its_delay_has_run() {
        let (queue, mut due) = DeferralQueue::new();
        let counts = AgentConcurrency::new();
        let id = Uuid::now_v7();
        let agent = AgentId::new("deferred-agent").unwrap();
        let slot = counts.try_enter(&agent, None).expect("no cap, no refusal");
        queue.defer(&permits(), id, agent.clone(), Duration::from_secs(30), slot);
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
        queue.defer(
            &permits(),
            Uuid::now_v7(),
            agent.clone(),
            Duration::from_secs(30),
            slot,
        );
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
        let workers = permits();
        let id = Uuid::now_v7();
        queue.defer(
            &workers,
            id,
            agent.clone(),
            Duration::from_millis(10),
            counts.try_enter(&agent, Some(1)).expect("the first fits"),
        );
        // The timer task finishes without panicking, and the slot it was
        // carrying goes with it — a queue with no drain left must not
        // wedge the agent at its cap.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(counts.in_flight(&agent), 0);
        assert!(
            workers.hold_for_drop(&id.to_string()).await.is_none(),
            "nor is the invocation still listed"
        );
    }

    /// **A drop ends a sleeping deferral at once (issue 1070).** The
    /// deferred invocation is listed for a drop to reach; one that goes
    /// through wakes the sleep, and the invocation's slot goes back then,
    /// not when the delay would have run out. Nothing is ever due.
    #[tokio::test(start_paused = true)]
    async fn a_drop_ends_a_sleeping_deferral_and_gives_its_slot_back_at_once() {
        let (queue, mut due) = DeferralQueue::new();
        let counts = AgentConcurrency::new();
        let agent = AgentId::new("dropped").unwrap();
        let workers = permits();
        let id = Uuid::now_v7();
        let slot = counts.try_enter(&agent, Some(1)).expect("the first fits");
        queue.defer(&workers, id, agent.clone(), Duration::from_secs(30), slot);

        let hold = workers
            .hold_for_drop(&id.to_string())
            .await
            .expect("a sleeping deferral is listed for a drop to reach");
        assert!(!hold.is_starting());
        hold.dropped();
        // The timer task runs once: no time passes.
        tokio::task::yield_now().await;
        assert_eq!(
            counts.in_flight(&agent),
            0,
            "the slot goes back at the drop, not at the end of the delay"
        );
        assert!(
            workers.hold_for_drop(&id.to_string()).await.is_none(),
            "and the invocation is no longer listed"
        );
        let resumed = tokio::time::timeout(Duration::from_secs(31), due.recv()).await;
        assert!(resumed.is_err(), "a dropped deferral is never due");
    }

    /// **A drop that fails leaves the deferral as it was**: released
    /// without `dropped`, the hold does not wake the sleep, and the resume
    /// is due when the delay has run, on the slot it had.
    #[tokio::test(start_paused = true)]
    async fn a_deferral_whose_drop_fails_is_due_as_before() {
        let (queue, mut due) = DeferralQueue::new();
        let counts = AgentConcurrency::new();
        let agent = AgentId::new("kept").unwrap();
        let workers = permits();
        let id = Uuid::now_v7();
        let slot = counts.try_enter(&agent, Some(1)).expect("the first fits");
        queue.defer(&workers, id, agent.clone(), Duration::from_secs(30), slot);

        drop(workers.hold_for_drop(&id.to_string()).await.expect("held"));
        tokio::task::yield_now().await;
        assert_eq!(counts.in_flight(&agent), 1, "the run still sleeps");
        let resume = tokio::time::timeout(Duration::from_secs(31), due.recv())
            .await
            .expect("due once the delay has run")
            .expect("the queue holds a sender");
        assert_eq!(resume.invocation_id, id);
        assert_eq!(counts.in_flight(&agent), 1, "on the slot it had");
    }

    /// **One invocation is put down once.** A deferral of an invocation
    /// already listed in the permits, queued or running there, would be a
    /// second run of it beside the first: nothing is scheduled, and its
    /// slot goes back at once.
    #[tokio::test(start_paused = true)]
    async fn a_deferral_of_an_invocation_already_listed_is_not_put_down() {
        let (queue, mut due) = DeferralQueue::new();
        let counts = AgentConcurrency::new();
        let agent = AgentId::new("twice").unwrap();
        let workers = permits();
        let id = Uuid::now_v7();
        let listed = workers.queue(Waiter::Recovered(id)).await;
        assert!(listed.is_admitted());

        let slot = counts.try_enter(&agent, None).expect("no cap, no refusal");
        queue.defer(&workers, id, agent.clone(), Duration::from_secs(30), slot);
        assert_eq!(counts.in_flight(&agent), 0, "its slot goes back at once");
        let resumed = tokio::time::timeout(Duration::from_secs(31), due.recv()).await;
        assert!(resumed.is_err(), "and nothing is due");
        drop(listed);
    }

    /// **A drop in the instant before the deferral is honoured** (issue
    /// 1070): the permits remember it, and the deferral that follows puts
    /// nothing down. The slot goes back at once, and nothing is ever due.
    #[tokio::test(start_paused = true)]
    async fn a_deferral_of_an_invocation_just_dropped_is_not_put_down() {
        let (queue, mut due) = DeferralQueue::new();
        let counts = AgentConcurrency::new();
        let agent = AgentId::new("dropped-first").unwrap();
        let workers = permits();
        let id = Uuid::now_v7();
        workers.note_dropped(&id.to_string());

        let slot = counts.try_enter(&agent, Some(1)).expect("the first fits");
        queue.defer(&workers, id, agent.clone(), Duration::from_secs(30), slot);
        assert_eq!(counts.in_flight(&agent), 0, "its slot goes back at once");
        assert!(workers.hold_for_drop(&id.to_string()).await.is_none());
        let resumed = tokio::time::timeout(Duration::from_secs(31), due.recv()).await;
        assert!(resumed.is_err(), "and nothing is due");
    }
}
