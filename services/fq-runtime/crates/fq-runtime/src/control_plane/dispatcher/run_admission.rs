//! What a run is admitted under, as one value the daemon shares: the
//! worker cap and the per-agent counts (#994).
//!
//! # Every run takes a worker permit
//!
//! `[worker] max_concurrent_invocations` is one semaphore,
//! [`WorkerPermits`], and every run takes a permit from it before it
//! starts: a trigger at the end of `handle`'s admission (#733), a deferred
//! invocation's resume in `resume_when_permitted` (#278), and startup
//! recovery's resumes and `fq invocation resume` through
//! [`WorkerPermits::queue`]. Those last two used to spawn their resumes
//! directly (review C5), so a restart ran every recovered invocation
//! *plus* `max_concurrent_invocations` new ones, and at the default cap of
//! 1 a recovered run and the first trigger shared the one static
//! workspace directory at the same time.
//!
//! The permit rules are #733's and none of them changes here. The consume
//! loop holds no permit and a held trigger owns none; a permit is taken
//! last, by a fair wait — one waiter, registered once, served in order —
//! and nothing takes one with a non-blocking attempt, which can lose to
//! the loop for ever (it wedged the daemon at the default cap of 1 once).
//! Every wait lets go on a drain or a shutdown.
//!
//! # A place now, a permit later
//!
//! [`WorkerPermits::queue`] polls the wait exactly once before it returns.
//! The semaphore enqueues a waiter on its first poll, so when `queue`
//! returns the run either holds a permit or has its place in the queue,
//! fixed from that moment, and the caller knows which. The rest of the
//! wait runs wherever the caller moves the [`Place`] — a spawned task.
//!
//! That is what makes the order after a restart deliberate. Startup
//! recovery queues every run it found while the daemon is still being
//! assembled, before the dispatcher exists, so every recovered run is
//! ahead of every trigger by construction rather than by timing: the
//! first `max_concurrent_invocations` start at once, the rest wait in the
//! order recovery found them, and the first trigger the dispatcher pulls
//! queues behind them all. A recovered run was started before the daemon
//! stopped and no trigger was, so a backlog of triggers cannot starve it.
//!
//! # The agent half
//!
//! [`RunAdmission::agents`] is #718's per-agent count, unchanged. A resume
//! counts into it as soon as it is queued (`AgentConcurrency::enter`), so
//! the agent's triggers are held for as long as the resume waits or runs,
//! and it is not itself held by it: that is #718's documented exception
//! for work this process did not admit, and `agent_cap`'s module doc is
//! where it is argued.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{debug, info};
use uuid::Uuid;

use super::admission::{HOLD_KEEPALIVE, keep_alive};
use super::trigger_name;
use crate::control_plane::agent_cap::AgentConcurrency;
use crate::worker::{DrainState, Worker};

/// The two caps a run is admitted under, shared by the dispatcher, startup
/// recovery, `fq invocation resume` and the health reports. Cloning it
/// shares it.
#[derive(Clone)]
pub struct RunAdmission {
    agents: Arc<AgentConcurrency>,
    workers: Arc<WorkerPermits>,
}

impl RunAdmission {
    /// A fresh admission for a daemon running `max_concurrent` invocations
    /// at once on `worker`. The worker is the one asked whether a drain is
    /// under way, which every wait for a permit lets go on, so it is the
    /// worker the dispatcher runs on.
    pub fn new(worker: Arc<dyn Worker>, max_concurrent: usize) -> Self {
        Self {
            agents: AgentConcurrency::new(),
            workers: WorkerPermits::new(worker, max_concurrent),
        }
    }

    /// The per-agent in-flight counts (#718).
    pub fn agents(&self) -> &Arc<AgentConcurrency> {
        &self.agents
    }

    /// The worker cap.
    pub fn workers(&self) -> &Arc<WorkerPermits> {
        &self.workers
    }
}

/// `[worker] max_concurrent_invocations` as permits: one is held by every
/// running invocation, whatever started it.
pub struct WorkerPermits {
    pub(super) permits: Arc<Semaphore>,
    max_concurrent: usize,
    /// Set once the dispatcher's loop has seen its shutdown signal.
    shut_down: AtomicBool,
    /// Asked whether a drain is under way.
    worker: Arc<dyn Worker>,
}

impl WorkerPermits {
    /// `max_concurrent` permits, at least one.
    pub fn new(worker: Arc<dyn Worker>, max_concurrent: usize) -> Arc<Self> {
        let max_concurrent = max_concurrent.max(1);
        Arc::new(Self {
            permits: Arc::new(Semaphore::new(max_concurrent)),
            max_concurrent,
            shut_down: AtomicBool::new(false),
            worker,
        })
    }

    /// How many invocations may run at once.
    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    /// How many permits are held right now: the invocations running.
    pub fn in_flight(&self) -> usize {
        self.max_concurrent - self.permits.available_permits()
    }

    /// Whether the daemon is on its way out: the worker is draining, or
    /// the dispatcher has seen its shutdown signal. Every wait — for a
    /// permit, a paused model or an agent's slot — lets go on it.
    pub fn stopping(&self) -> bool {
        self.worker.drain_status() == DrainState::Draining || self.shut_down.load(Ordering::SeqCst)
    }

    /// The dispatcher has seen its shutdown signal: every wait lets go,
    /// including the resumes the daemon queued, and nothing starts again.
    pub(super) fn shut_down(&self) {
        self.shut_down.store(true, Ordering::SeqCst);
    }

    /// Take `waiter`'s place in the queue for a permit now, and wait for
    /// the permit later through the [`Place`]. For the runs that have no
    /// trigger delivery to keep alive: startup recovery's resumes and
    /// `fq invocation resume`.
    ///
    /// The wait is polled once here, which is when the semaphore enqueues
    /// it; the module doc says why that is the point. A daemon already on
    /// its way out queues nothing: [`Place::Stopping`] at once, rather than
    /// a wait that would only let go on its first keepalive tick.
    pub async fn queue(self: &Arc<Self>, waiter: Waiter) -> Place {
        if self.stopping() {
            return Place::Stopping;
        }
        let permits = Arc::clone(self);
        let mut wait: Pin<Box<dyn Future<Output = Option<OwnedSemaphorePermit>> + Send>> =
            Box::pin(async move { permits.wait(None, waiter).await });
        match std::future::poll_fn(|cx| Poll::Ready(wait.as_mut().poll(cx))).await {
            Poll::Ready(Some(permit)) => Place::Admitted(permit),
            Poll::Ready(None) => Place::Stopping,
            Poll::Pending => Place::Queued(QueuedRun(wait)),
        }
    }

    /// Wait for a permit, keeping `keepalive`'s delivery alive meanwhile
    /// (#733). `None` back is a drain or shutdown during the wait.
    ///
    /// **One waiter, registered once.** The acquire future is created
    /// before the loop and polled through the whole wait, so the waiter
    /// keeps its place in the semaphore's FIFO and the longest-parked one
    /// runs first; re-creating it each tick — which is what a naive
    /// `select!` in a loop does — would send it to the back of the queue
    /// every [`HOLD_KEEPALIVE`]. `biased` puts the permit first, so the
    /// fast path (a permit already free) takes it on the first poll and
    /// never arms the timer, never logs and never acks.
    pub(super) async fn wait(
        &self,
        keepalive: Option<&async_nats::jetstream::Message>,
        waiter: Waiter,
    ) -> Option<OwnedSemaphorePermit> {
        let acquire = Arc::clone(&self.permits).acquire_owned();
        tokio::pin!(acquire);
        let mut waited = false;
        loop {
            tokio::select! {
                biased;
                permit = &mut acquire => {
                    let permit = permit.expect("the worker semaphore is never closed");
                    // The drain frees permits too. Every runner releases
                    // its own as it suspends, and that release wakes the
                    // head of this FIFO — whose first-polled arm, under
                    // `biased`, is this one. Without the check a run would
                    // be started *into a draining worker* by the very drain
                    // that was meant to stop it. Dropping the permit hands
                    // it back to a semaphore nothing will take from again:
                    // the exit is process-terminal.
                    if self.stopping() {
                        drop(permit);
                        waiter.log_let_go("a worker permit freed during a drain or shutdown");
                        return None;
                    }
                    if waited {
                        waiter.log_admitted();
                    }
                    return Some(permit);
                }
                () = tokio::time::sleep(HOLD_KEEPALIVE) => {
                    if self.stopping() {
                        waiter.log_let_go("drain or shutdown while waiting for a worker permit");
                        return None;
                    }
                    if !waited {
                        waited = true;
                        waiter.log_waiting(self.in_flight(), self.max_concurrent);
                    }
                    if let Some(msg) = keepalive {
                        keep_alive(msg, waiter.trigger_id()).await;
                    }
                }
            }
        }
    }
}

/// Where a queued run stands: what [`WorkerPermits::queue`] returns.
#[derive(Debug)]
pub enum Place {
    /// A permit was free, and this is it: the run starts now.
    Admitted(OwnedSemaphorePermit),
    /// Every permit is held: the run has its place in the queue.
    Queued(QueuedRun),
    /// The daemon is draining or shutting down: nothing starts.
    Stopping,
}

impl Place {
    /// Whether the run has to wait for its permit.
    pub fn is_queued(&self) -> bool {
        matches!(self, Self::Queued(_))
    }

    /// The permit, once the run's turn comes, or `None` if a drain or
    /// shutdown comes first.
    pub async fn permit(self) -> Option<OwnedSemaphorePermit> {
        match self {
            Self::Admitted(permit) => Some(permit),
            Self::Queued(QueuedRun(wait)) => wait.await,
            Self::Stopping => None,
        }
    }
}

/// A wait for a permit that already has its place in the queue.
pub struct QueuedRun(Pin<Box<dyn Future<Output = Option<OwnedSemaphorePermit>> + Send>>);

impl fmt::Debug for QueuedRun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("QueuedRun")
    }
}

/// What is waiting for a permit, for the lines the wait logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waiter {
    /// A pulled trigger, by the id its publisher stamped, if any.
    Trigger(Option<Uuid>),
    /// A deferred invocation whose delay has run (#278).
    Deferred(Uuid),
    /// An invocation startup recovery found in flight.
    Recovered(Uuid),
    /// An invocation an operator re-drove with `fq invocation resume`.
    Operator(Uuid),
}

impl Waiter {
    fn trigger_id(self) -> Option<Uuid> {
        match self {
            Self::Trigger(id) => id,
            _ => None,
        }
    }

    /// The invocation a resume continues, and which resume it is.
    fn resume(self) -> Option<(Uuid, &'static str)> {
        match self {
            Self::Trigger(_) => None,
            Self::Deferred(id) => Some((id, "deferred")),
            Self::Recovered(id) => Some((id, "recovered")),
            Self::Operator(id) => Some((id, "operator")),
        }
    }

    fn log_waiting(self, in_flight: usize, max_concurrent: usize) {
        match self.resume() {
            None => info!(
                trigger_id = %trigger_name(self.trigger_id()),
                in_flight,
                max_concurrent,
                "worker permits exhausted; holding the trigger un-started"
            ),
            Some((invocation_id, resume)) => info!(
                %invocation_id,
                resume,
                in_flight,
                max_concurrent,
                "worker permits exhausted; the resume waits for one"
            ),
        }
    }

    fn log_admitted(self) {
        match self.resume() {
            None => debug!(
                trigger_id = %trigger_name(self.trigger_id()),
                "a worker permit freed; starting the waiting trigger"
            ),
            Some((invocation_id, resume)) => info!(
                %invocation_id,
                resume,
                "a worker permit freed; starting the waiting resume"
            ),
        }
    }

    fn log_let_go(self, why: &str) {
        match self.resume() {
            None => debug!(
                trigger_id = %trigger_name(self.trigger_id()),
                "{why}; nothing is started"
            ),
            // At `info`, because a resume has no delivery to requeue and
            // nothing on the broker to point at: only a WAL row, which the
            // next start's recovery picks up. Without this line the
            // invocation just stops appearing in the log.
            Some((invocation_id, resume)) => info!(
                %invocation_id,
                resume,
                "{why}; the invocation is left in flight for the next start's recovery"
            ),
        }
    }
}

#[cfg(test)]
mod tests;
