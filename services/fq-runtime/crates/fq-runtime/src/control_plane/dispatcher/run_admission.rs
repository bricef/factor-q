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
//! # A drop reaches a queued run before it starts
//!
//! A run waiting here is not one the runner drives, so `fq invocation
//! drop` takes it on the path for idle work: it publishes the drop, and the
//! WAL row turns terminal only once the coordination consumer folds that.
//! A permit arriving in between would start a dropped run. So the resumes
//! queued here — by startup recovery and by `invocation.resume`, through
//! [`WorkerPermits::queue`] — are listed by invocation id, each with a lock
//! of its own: the drop holds it while it publishes
//! ([`WorkerPermits::hold_for_drop`]) and marks the run dropped if the
//! publish succeeds, and the run takes the same lock to start
//! ([`Place::permit`]). After the drop's receipt the run cannot start in
//! this process; if the drop fails, the run is as it was.
//!
//! # A drop reaches a deferred run too
//!
//! A run deferred for a rate limit (#278) is listed the same way, from the
//! moment it is put down ([`WorkerPermits::list_deferred`]) until its
//! resume ends: while it sleeps out its delay, while its resume waits for
//! a permit in `resume_when_permitted`, and while that resume runs. One
//! listing across all three, so the delay running out is not a moment a
//! drop can miss. A drop that goes through wakes the sleeping run at once
//! ([`Deferral::sleep`]), which then ends there, giving its agent's slot
//! back at the drop rather than at the end of the delay; a resume already
//! due leaves the permit queue within a tick, as a queued resume does; and
//! one whose permit came first is starting, which the drop refuses like a
//! live run.
//!
//! One instant is not covered by a listing: the runner stops driving a
//! run when it returns `Deferred`, and the run is listed only when that
//! outcome reaches the deferral queue. A drop in between, a plain one or
//! a live one whose halt the run never reached a step boundary to see,
//! finds nothing to hold. So the permits remember every drop that went
//! through ([`WorkerPermits::note_dropped`]), and a deferral of an
//! invocation they remember is refused ([`WorkerPermits::list_deferred`]),
//! under the same lock: the run is never put down, its agent's slot goes
//! back at once, and nothing resumes it. A drop is remembered for
//! [`DROPS_REMEMBERED_FOR`], and forgotten the next time a drop is noted or
//! a deferral listed after that, so the memory holds at most the drops of
//! the last hour.
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
use std::time::Duration;

use tokio::time::Instant;

use tokio::sync::{Mutex as TurnLock, Notify, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};
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
    /// What a drop reaches here: the runs listed for it, and the drops
    /// that went through. See the module doc.
    queued: Mutex<Listings>,
}

/// How long the permits remember a drop that went through: the longest a
/// deferral that follows it can be refused. The instant it covers is the
/// time between a drop and the run it named returning `Deferred`, which a
/// live drop of a run inside its model call stretches to the rest of that
/// call: each attempt can last up to its request timeout (600 s by
/// default) and the retry layer makes up to four, waiting at most two
/// minutes between them (`[worker.llm_retry]`). An hour need not outlast
/// all of that: once the drop's fold has marked the run's row finished,
/// the runner itself refuses to put the run down (`mark_deferred` writes
/// only a row still in flight), so the memory covers only the fold's lag,
/// which is milliseconds.
pub const DROPS_REMEMBERED_FOR: Duration = Duration::from_secs(3_600);

/// What a drop reaches in [`WorkerPermits`], under one lock.
#[derive(Default)]
struct Listings {
    /// The runs listed for a drop to reach, by invocation id: the resumes
    /// queued here and the runs deferred for a rate limit, each until its
    /// run ends.
    runs: HashMap<Uuid, Listing>,
    /// The invocations a drop went through for, and when.
    dropped: HashMap<Uuid, Instant>,
}

impl Listings {
    /// Forget every drop older than [`DROPS_REMEMBERED_FOR`].
    fn forget_old_drops(&mut self, now: Instant) {
        self.dropped
            .retain(|_, at| now.duration_since(*at) < DROPS_REMEMBERED_FOR);
    }
}

/// Why a run was not listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotListed {
    /// Another run of the invocation is listed here, queued or running.
    AlreadyListed,
    /// A drop of the invocation went through, in the instant before the
    /// run was put down: it is not.
    Dropped,
}

/// How far a listed run has got, as a drop sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Turn {
    Waiting,
    /// A drop went through: it never starts in this process.
    Dropped,
    /// Its permit came and it is starting: live in all but name.
    Starting,
}

/// One run's listing: the lock a drop and the run's start both take, and
/// the wake a drop that goes through gives a run asleep on its deferral.
#[derive(Clone)]
struct Listing {
    turn: Arc<TurnLock<Turn>>,
    dropped: Arc<Notify>,
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
            queued: Mutex::new(Listings::default()),
        })
    }

    fn queued(&self) -> MutexGuard<'_, Listings> {
        self.queued
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Hold the listed run of `invocation_id`, a queued resume or a
    /// deferred run, if there is one, for a drop: until the hold is
    /// released the run cannot start, and [`QueuedHold::dropped`] makes
    /// sure it never does. `None` when no run of that id is listed here,
    /// which includes an id that does not parse.
    pub async fn hold_for_drop(&self, invocation_id: &str) -> Option<QueuedHold> {
        let id = Uuid::parse_str(invocation_id).ok()?;
        let listing = self.queued().runs.get(&id).cloned()?;
        Some(QueuedHold {
            turn: listing.turn.lock_owned().await,
            dropped: listing.dropped,
        })
    }

    /// List `invocation_id`, which a rate limit has just deferred, for a
    /// drop to reach from now until its resume ends. Refused when the
    /// invocation is already listed here, queued or running, since a
    /// deferral of it is then a second run of one invocation; and when a
    /// drop of it went through, in the instant before the run was put down
    /// ([`Self::note_dropped`]). Either way the run is not put down.
    pub fn list_deferred(self: &Arc<Self>, invocation_id: Uuid) -> Result<Deferral, NotListed> {
        // A deferred waiter is a resume, which `enter` always lists.
        QueuedEntry::enter(self, Waiter::Deferred(invocation_id))?
            .map(Deferral)
            .ok_or(NotListed::AlreadyListed)
    }

    /// Remember that a drop of `invocation_id` went through: its receipt
    /// is on its way to the operator. A deferral of it is refused from now
    /// on, for [`DROPS_REMEMBERED_FOR`], and a run of it listed here and
    /// still waiting is dropped, as [`QueuedHold::dropped`] drops one, a
    /// sleeping deferral woken to end. Nothing when the id does not parse.
    pub fn note_dropped(&self, invocation_id: &str) {
        let Ok(id) = Uuid::parse_str(invocation_id) else {
            return;
        };
        let now = Instant::now();
        let mut listings = self.queued();
        listings.forget_old_drops(now);
        listings.dropped.insert(id, now);
        if let Some(listing) = listings.runs.get(&id)
            && let Ok(mut turn) = listing.turn.try_lock()
            && *turn == Turn::Waiting
        {
            *turn = Turn::Dropped;
            listing.dropped.notify_one();
        }
    }

    /// Whether a drop has gone through for the listed run of `id`. A drop
    /// still in progress holds the lock, and is not one yet.
    fn is_dropped(&self, id: Uuid) -> bool {
        self.queued()
            .runs
            .get(&id)
            .is_some_and(|listing| listing.turn.try_lock().is_ok_and(|t| *t == Turn::Dropped))
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
    /// resume is listed for a drop to reach from here until its run ends,
    /// and one invocation is listed once: a second resume of an invocation
    /// already queued or running here is refused, not queued beside it.
    pub async fn queue(self: &Arc<Self>, waiter: Waiter) -> Place {
        if self.stopping() {
            return Place::never();
        }
        let Ok(entry) = QueuedEntry::enter(self, waiter) else {
            return Place {
                wait: Wait::AlreadyListed,
                entry: None,
            };
        };
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
    /// A resume's listing for a drop, from queueing until its run ends: it
    /// moves into the [`RunPermit`] when the run starts.
    entry: Option<QueuedEntry>,
}

enum Wait {
    /// A permit was free, and this is it.
    Now(OwnedSemaphorePermit),
    /// Every permit is held: the place in the queue, already taken.
    Later(Pin<Box<dyn Future<Output = Option<OwnedSemaphorePermit>> + Send>>),
    /// The daemon is draining or shutting down: nothing starts.
    Never,
    /// The invocation is already queued or running here: nothing starts.
    AlreadyListed,
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

    /// Whether the invocation was already queued or running here, so this
    /// second resume of it was refused: nothing starts.
    pub fn is_already_listed(&self) -> bool {
        matches!(self.wait, Wait::AlreadyListed)
    }

    /// The permit, once the run's turn comes. `None` if a drain or a
    /// shutdown comes first, or a drop: a resume starts under the lock a
    /// drop holds, so one that a drop has gone through for never starts.
    pub async fn permit(self) -> Option<RunPermit> {
        let Place { wait, entry } = self;
        let permit = match wait {
            Wait::Now(permit) => permit,
            Wait::Later(wait) => wait.await?,
            Wait::Never | Wait::AlreadyListed => return None,
        };
        start_on(permit, entry).await
    }
}

/// Start a run on `permit`, under the lock a drop holds if it is listed:
/// `None`, the permit going straight back, when a drop has gone through
/// for it.
async fn start_on(permit: OwnedSemaphorePermit, entry: Option<QueuedEntry>) -> Option<RunPermit> {
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

/// A run deferred for a rate limit, listed for a drop to reach
/// ([`WorkerPermits::list_deferred`]): carried by the deferral queue while
/// the run sleeps and by its resume while that waits for a permit, then
/// held by the [`RunPermit`] the resume runs under, until the run ends.
pub struct Deferral(QueuedEntry);

impl Deferral {
    /// Sleep out `delay`, unless a drop of the run goes through first.
    /// `true` is the delay run out: the resume is due. `false` is the drop,
    /// which wakes the run at once, so that what the sleeper holds, its
    /// agent's slot among it, is given back at the drop rather than at the
    /// end of the delay.
    pub(crate) async fn sleep(&self, delay: Duration) -> bool {
        tokio::select! {
            biased;
            () = self.0.listing.dropped.notified() => false,
            () = tokio::time::sleep(delay) => true,
        }
    }

    /// The permit, once the resume's turn comes, from the permits the run
    /// was listed in. `None` if a drain or a shutdown comes first, or a
    /// drop, as for a queued resume ([`Place::permit`]).
    pub(super) async fn permit(self) -> Option<RunPermit> {
        let Deferral(entry) = self;
        let permit = entry.permits.wait(None, entry.waiter).await?;
        start_on(permit, Some(entry)).await
    }
}

impl fmt::Debug for Deferral {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Deferral").field(&self.0.id).finish()
    }
}

impl fmt::Debug for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.wait {
            Wait::Now(_) => "Place::Admitted",
            Wait::Later(_) => "Place::Queued",
            Wait::Never => "Place::Stopping",
            Wait::AlreadyListed => "Place::AlreadyListed",
        })
    }
}

/// A run's worker permit, held for as long as it runs.
pub struct RunPermit {
    _permit: OwnedSemaphorePermit,
    _entry: Option<QueuedEntry>,
}

/// A drop's hold on a listed run, a queued resume or a deferred run
/// ([`WorkerPermits::hold_for_drop`]). Released without [`Self::dropped`]
/// — the drop failed — the run is as it was.
pub struct QueuedHold {
    turn: OwnedMutexGuard<Turn>,
    dropped: Arc<Notify>,
}

impl QueuedHold {
    /// Whether the run's permit came and it is starting: as live as a
    /// running one, which a drop takes through the runner, not here.
    pub fn is_starting(&self) -> bool {
        *self.turn == Turn::Starting
    }

    /// The drop went through: the run never starts in this process, and a
    /// deferred run asleep on its delay is woken to end now.
    pub fn dropped(mut self) {
        if *self.turn == Turn::Waiting {
            *self.turn = Turn::Dropped;
            self.dropped.notify_one();
        }
    }
}

/// A run's listing in [`WorkerPermits`]'s queued set, removed when it is
/// dropped — once its run has ended, or it never started.
struct QueuedEntry {
    permits: Arc<WorkerPermits>,
    waiter: Waiter,
    id: Uuid,
    listing: Listing,
}

impl QueuedEntry {
    /// List a resume; a trigger has nothing for a drop to reach here. `Err`
    /// is an invocation already listed: two runs of one invocation would
    /// share one listing, and a drop would hold only one of them.
    fn enter(permits: &Arc<WorkerPermits>, waiter: Waiter) -> Result<Option<Self>, NotListed> {
        let Some((id, _)) = waiter.resume() else {
            return Ok(None);
        };
        let listing = Listing {
            turn: Arc::new(TurnLock::new(Turn::Waiting)),
            dropped: Arc::new(Notify::new()),
        };
        let mut listings = permits.queued();
        if let Waiter::Deferred(_) = waiter {
            listings.forget_old_drops(Instant::now());
            if listings.dropped.contains_key(&id) {
                return Err(NotListed::Dropped);
            }
        }
        match listings.runs.entry(id) {
            std::collections::hash_map::Entry::Occupied(_) => return Err(NotListed::AlreadyListed),
            std::collections::hash_map::Entry::Vacant(vacant) => {
                vacant.insert(listing.clone());
            }
        }
        Ok(Some(Self {
            permits: Arc::clone(permits),
            waiter,
            id,
            listing,
        }))
    }

    /// Start, unless a drop has gone through: under the lock a drop holds
    /// while it publishes, so the two cannot interleave.
    async fn start(&self) -> bool {
        let mut turn = self.listing.turn.lock().await;
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
            .runs
            .get(&self.id)
            .is_some_and(|listing| Arc::ptr_eq(&listing.turn, &self.listing.turn))
        {
            queued.runs.remove(&self.id);
        }
    }
}

/// What is waiting for a permit, for the lines the wait logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waiter {
    /// A pulled trigger, by the id its publisher stamped, if any.
    Trigger(Option<Uuid>),
    /// A run deferred for a rate limit (#278): listed from the deferral,
    /// and waiting for a permit once its delay has run.
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
