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
//! # A drop reaches a queued run before it starts
//!
//! A run waiting here is not one the runner drives, so `fq invocation
//! drop` takes it on the path for idle work: it publishes the drop, and the
//! WAL row turns terminal only once the coordination consumer folds that.
//! A permit arriving in between would start a dropped run. So the resumes
//! queued here are listed by invocation id, each with a lock of its own:
//! the drop holds it while it publishes ([`WorkerPermits::hold_for_drop`])
//! and marks the run dropped if the publish succeeds, and the run takes the
//! same lock to start ([`Place::permit`]). After the drop's receipt the run
//! cannot start in this process; if the drop fails, the run is as it was.
//!
//! # The agent half
//!
//! [`RunAdmission::agents`] is #718's per-agent count, unchanged. A resume
//! counts into it as soon as it is queued (`AgentConcurrency::enter`), so
//! the agent's triggers are held for as long as the resume waits or runs,
//! and it is not itself held by it: that is #718's documented exception
//! for work this process did not admit, and `agent_cap`'s module doc is
//! where it is argued.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::Poll;

use tokio::sync::{Mutex as TurnLock, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};
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
    /// The resumes queued here and not yet started, by invocation id:
    /// what a drop reaches before they start. See the module doc.
    queued: Mutex<HashMap<Uuid, Arc<TurnLock<Turn>>>>,
}

/// How far a queued resume has got, as a drop sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Turn {
    Waiting,
    /// A drop went through: it never starts in this process.
    Dropped,
    /// Its permit came and it is starting: live in all but name.
    Starting,
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
            queued: Mutex::new(HashMap::new()),
        })
    }

    fn queued(&self) -> MutexGuard<'_, HashMap<Uuid, Arc<TurnLock<Turn>>>> {
        self.queued
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Hold the queued resume of `invocation_id`, if there is one, for a
    /// drop: until the hold is released the run cannot start, and
    /// [`QueuedHold::dropped`] makes sure it never does. `None` when no run
    /// of that id is queued here, which includes an id that does not parse.
    pub async fn hold_for_drop(&self, invocation_id: &str) -> Option<QueuedHold> {
        let id = Uuid::parse_str(invocation_id).ok()?;
        let turn = self.queued().get(&id).cloned()?;
        Some(QueuedHold(turn.lock_owned().await))
    }

    /// Whether a drop has gone through for the queued resume of `id`. A
    /// drop still in progress holds the lock, and is not one yet.
    fn is_dropped(&self, id: Uuid) -> bool {
        self.queued()
            .get(&id)
            .is_some_and(|turn| turn.try_lock().is_ok_and(|t| *t == Turn::Dropped))
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
    /// its way out queues nothing: a stopping [`Place`] at once, rather
    /// than a wait that would only let go on its first keepalive tick. A
    /// resume is listed for a drop to reach from here until it starts.
    pub async fn queue(self: &Arc<Self>, waiter: Waiter) -> Place {
        if self.stopping() {
            return Place::never();
        }
        let entry = QueuedEntry::enter(self, waiter);
        let permits = Arc::clone(self);
        let mut wait: Pin<Box<dyn Future<Output = Option<OwnedSemaphorePermit>> + Send>> =
            Box::pin(async move { permits.wait(None, waiter).await });
        let wait = match std::future::poll_fn(|cx| Poll::Ready(wait.as_mut().poll(cx))).await {
            Poll::Ready(Some(permit)) => Wait::Now(permit),
            Poll::Ready(None) => return Place::never(),
            Poll::Pending => Wait::Later(wait),
        };
        Place { wait, entry }
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
                    // Not the guarantee — starting takes the lock a drop
                    // holds — but a dropped run leaves the queue and gives
                    // its agent's slot back within a tick.
                    if let Some((id, _)) = waiter.resume()
                        && self.is_dropped(id)
                    {
                        waiter.log_dropped();
                        return None;
                    }
                    if !waited {
                        waited = true;
                        waiter.log_waiting(self.permits.available_permits(), self.max_concurrent);
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
pub struct Place {
    wait: Wait,
    /// A resume's listing for a drop, from queueing until it starts.
    entry: Option<QueuedEntry>,
}

enum Wait {
    /// A permit was free, and this is it.
    Now(OwnedSemaphorePermit),
    /// Every permit is held: the place in the queue, already taken.
    Later(Pin<Box<dyn Future<Output = Option<OwnedSemaphorePermit>> + Send>>),
    /// The daemon is draining or shutting down: nothing starts.
    Never,
}

impl Place {
    fn never() -> Self {
        Self {
            wait: Wait::Never,
            entry: None,
        }
    }

    /// Whether a permit was free when the run was queued.
    pub fn is_admitted(&self) -> bool {
        matches!(self.wait, Wait::Now(_))
    }

    /// Whether the run has to wait for its permit.
    pub fn is_queued(&self) -> bool {
        matches!(self.wait, Wait::Later(_))
    }

    /// Whether the daemon was already on its way out: nothing starts.
    pub fn is_stopping(&self) -> bool {
        matches!(self.wait, Wait::Never)
    }

    /// The permit, once the run's turn comes. `None` if a drain or a
    /// shutdown comes first, or a drop: a resume starts under the lock a
    /// drop holds, so one that a drop has gone through for never starts.
    pub async fn permit(self) -> Option<RunPermit> {
        let Place { wait, entry } = self;
        let permit = match wait {
            Wait::Now(permit) => permit,
            Wait::Later(wait) => wait.await?,
            Wait::Never => return None,
        };
        if let Some(entry) = &entry
            && !entry.start().await
        {
            entry.waiter.log_dropped();
            return None;
        }
        Some(RunPermit {
            _permit: permit,
            _entry: entry,
        })
    }
}

impl fmt::Debug for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.wait {
            Wait::Now(_) => "Place::Admitted",
            Wait::Later(_) => "Place::Queued",
            Wait::Never => "Place::Stopping",
        })
    }
}

/// A run's worker permit, held for as long as it runs.
pub struct RunPermit {
    _permit: OwnedSemaphorePermit,
    _entry: Option<QueuedEntry>,
}

/// A drop's hold on a queued resume ([`WorkerPermits::hold_for_drop`]).
/// Released without [`Self::dropped`] — the drop failed — the run is as it
/// was.
pub struct QueuedHold(OwnedMutexGuard<Turn>);

impl QueuedHold {
    /// Whether the run's permit came and it is starting: as live as a
    /// running one, which a drop takes through the runner, not here.
    pub fn is_starting(&self) -> bool {
        *self.0 == Turn::Starting
    }

    /// The drop went through: the run never starts in this process.
    pub fn dropped(mut self) {
        if *self.0 == Turn::Waiting {
            *self.0 = Turn::Dropped;
        }
    }
}

/// A resume's listing in [`WorkerPermits`]'s queued set, removed when it
/// is dropped — once its run has ended, or it never started.
struct QueuedEntry {
    permits: Arc<WorkerPermits>,
    waiter: Waiter,
    id: Uuid,
    turn: Arc<TurnLock<Turn>>,
}

impl QueuedEntry {
    /// List a resume; a trigger has nothing for a drop to reach here.
    fn enter(permits: &Arc<WorkerPermits>, waiter: Waiter) -> Option<Self> {
        let (id, _) = waiter.resume()?;
        let turn = Arc::new(TurnLock::new(Turn::Waiting));
        permits.queued().insert(id, Arc::clone(&turn));
        Some(Self {
            permits: Arc::clone(permits),
            waiter,
            id,
            turn,
        })
    }

    /// Start, unless a drop has gone through: under the lock a drop holds
    /// while it publishes, so the two cannot interleave.
    async fn start(&self) -> bool {
        let mut turn = self.turn.lock().await;
        if *turn == Turn::Dropped {
            return false;
        }
        *turn = Turn::Starting;
        true
    }
}

impl Drop for QueuedEntry {
    fn drop(&mut self) {
        let mut queued = self.permits.queued();
        if queued
            .get(&self.id)
            .is_some_and(|turn| Arc::ptr_eq(turn, &self.turn))
        {
            queued.remove(&self.id);
        }
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

    fn log_waiting(self, available_permits: usize, max_concurrent: usize) {
        let in_flight = max_concurrent - available_permits;
        match self.resume() {
            None => info!(
                trigger_id = %trigger_name(self.trigger_id()),
                available_permits,
                in_flight,
                max_concurrent,
                "worker permits exhausted; holding the trigger un-started"
            ),
            Some((invocation_id, resume)) => info!(
                %invocation_id,
                resume,
                available_permits,
                in_flight,
                max_concurrent,
                "worker permits exhausted; the resume waits for one"
            ),
        }
    }

    /// At `info` for the same reason as [`Self::log_let_go`]'s resume line.
    fn log_dropped(self) {
        if let Some((invocation_id, resume)) = self.resume() {
            info!(
                %invocation_id,
                resume,
                "an operator dropped the invocation while it waited for a worker permit; \
                 it is not resumed"
            );
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
