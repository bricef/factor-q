//! Admission control at the dispatcher: the hold, and the first of the
//! two rules that use it.
//!
//! **The rule (#278, the third layer):** a trigger for a paused model is
//! not started. It is *held* — pulled, un-acked, kept alive with
//! JetStream in-progress acks — until the pause ends, then started as
//! the first attempt it still is.
//!
//! Why hold rather than NAK with a delay: the trigger durable's ack
//! window is 30 seconds (`TRIGGER_RETRY_BACKOFF[0]`, see `bus.rs`), and
//! every redelivery counts toward `TRIGGER_MAX_DELIVER`. A NAK per pause
//! would dead-letter a trigger after four pauses of a persistently
//! throttled model — a `failed` event the watcher counts, the outcome
//! #278 exists to prevent — and would stamp `attempt: N` into the
//! transcript preamble, the tell the redelivery-storm notes rely on. An
//! in-progress ack resets the window without counting as a delivery, so
//! a held trigger consumes nothing and starts as `attempt: 1`.
//!
//! # One hold, two rules
//!
//! [`TriggerDispatcher::hold_tick`] is the mechanism, and both
//! admission rules run on it: this module's pause hold and the per-agent
//! cap hold in [`super::agent_cap`] (#718). One loop, one keepalive
//! cadence ([`HOLD_KEEPALIVE`]), one "still your first delivery"
//! guarantee. Two cadences for one ack window was two things to get
//! wrong, which is what the review of
//! <https://github.com/bricef/factor-q/pull/731> called it.
//!
//! What the two rules still decide for themselves is at their call
//! sites, because it is genuinely different in each:
//!
//! - **what the hold waits on** — a pause ending, or a slot freeing;
//! - **what an interrupted delivery deserves.** A pause hold leaves it
//!   un-acked for the next binary; a cap hold, which can outlast a
//!   deploy, requeues it so the restart costs it no delivery.
//!
//! # Neither hold owns a worker permit
//!
//! A hold takes no `[worker] max_concurrent_invocations` permit and
//! gives none back, because it never had one: the permit is taken
//! *after* both holds, by [`TriggerDispatcher::acquire_run_permit`], at
//! the one moment a trigger is otherwise ready to run. The consume loop
//! does not hold one either — it pulls, and what bounds how much it
//! pulls is `max_ack_pending`, not the worker cap.
//!
//! That is the whole of <https://github.com/bricef/factor-q/issues/733>.
//! Before it, the loop took a permit before every pull and each hold
//! decided for itself what to do with the one it inherited, which at the
//! default worker cap of 1 meant a held trigger either blocked the fleet
//! (keeping it) or wedged forever (releasing it: the loop, queued first
//! on the fair semaphore, took every permit a hold gave back). Now there
//! is one rule, one place, and one thing a permit means — a worker is
//! running this.
//!
//! The permit wait itself is the third user of [`HOLD_KEEPALIVE`]: a
//! trigger waiting for a permit is held exactly as one waiting on a
//! pause is, and lets go on the same `stopping` flag.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use async_nats::jetstream::AckKind;
use tokio::sync::OwnedSemaphorePermit;
use tracing::{debug, info, warn};

use super::{TriggerDispatcher, trigger_name};
use crate::agent::AgentId;
use crate::worker::{DrainState, DueResume};

/// How often a held delivery is kept alive — and, because the check
/// follows the ack, how often the hold asks whether it can stop.
///
/// **One constant for every wait** — both holds and the wait for a
/// worker permit. The trigger durable's real
/// first-delivery deadline is 30 seconds (`TRIGGER_RETRY_BACKOFF[0]`;
/// JetStream replaces `ack_wait` with `backoff[0]` wherever a schedule
/// is set), and one slipped tick means a redelivery, a second `handle`
/// for the same trigger, `attempt: 2` and a duplicate invocation. 250 ms
/// gives each window about 120 keepalive ticks.
///
/// The cap hold is what argued the number down from 400 ms: a pause hold
/// is bounded by the pause, tens of ticks, while a cap hold is bounded
/// by the *invocation ahead of it* — for a build-bound agent a ~17-minute
/// `just ci` pass on the same box, thousands of ticks, every one of which
/// has to land on a machine that is busy compiling. Nothing in that
/// argument is special to the cap, so both holds take the wider margin.
///
/// It is slack against a 30-second window and not a durable fix. The
/// tradeoff is that a crash before durable start is recovered after 30
/// seconds instead of one. The window itself belongs to
/// <https://github.com/bricef/factor-q/issues/327>, which owns the
/// duplicate-invocation class this guards against.
pub(super) const HOLD_KEEPALIVE: Duration = Duration::from_millis(250);

/// What the pause hold decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Admission {
    /// The model is not paused (any more): start the invocation.
    Start,
    /// A drain or shutdown landed during the hold: leave the delivery
    /// un-acked so it reaches the next binary.
    Interrupted,
}

impl TriggerDispatcher {
    /// The whole task the consume loop spawns for a **pulled trigger**.
    ///
    /// Owned rather than borrowed, and `Arc<Self>` rather than `&self`,
    /// because `JoinSet::spawn` needs a `'static` future. That is the
    /// only reason it exists as a method: it keeps the loop's arm a
    /// single line and puts the spawned body next to the admission
    /// rules it runs.
    pub(super) async fn dispatch_pulled(self: Arc<Self>, msg: async_nats::jetstream::Message) {
        self.handle(&msg).await;
    }

    /// The whole task the consume loop spawns for a **due resume**
    /// (#278): queue for a worker permit, then resume under it.
    ///
    /// A resume queues like everything else, with no delivery to keep
    /// alive — its trigger was acked at the first WAL write — so it can
    /// neither jump the triggers already waiting nor be jumped by them.
    pub(super) async fn resume_when_permitted(self: Arc<Self>, resume: DueResume) {
        let Some(_permit) = self.acquire_run_permit(None, None).await else {
            return;
        };
        self.resume_deferred(resume).await;
    }

    /// The trigger's body as JSON, or `None` for one that will never
    /// parse — acked and dropped, because a redelivery would only fail
    /// the same way.
    ///
    /// An empty body is `null` rather than an error: a trigger with
    /// nothing to say is a legitimate trigger.
    ///
    /// This is admission's business even though it is not a hold: it is
    /// a refusal on the trigger's own merits, and it is deliberately
    /// asked *before* the cap hold so a poison payload does not occupy
    /// a slot for however long the agent stays full.
    pub(super) async fn parse_payload(
        &self,
        msg: &async_nats::jetstream::Message,
        agent: &AgentId,
    ) -> Option<serde_json::Value> {
        if msg.payload.is_empty() {
            return Some(serde_json::Value::Null);
        }
        match serde_json::from_slice(&msg.payload) {
            Ok(payload) => Some(payload),
            Err(err) => {
                warn!(
                    agent_id = %agent,
                    error = %err,
                    "trigger payload is not valid JSON, dropping"
                );
                self.ack(
                    msg,
                    crate::trigger::trigger_id_in(msg.headers.as_ref()),
                    "invalid payload",
                )
                .await;
                None
            }
        }
    }

    /// One pass of a hold: `false` once this dispatcher is on its way
    /// out and the trigger must not be started, `true` when the caller
    /// should ask its own question again.
    ///
    /// A pass is: give up if stopping, keep the delivery alive, wait
    /// [`HOLD_KEEPALIVE`]. The keepalive comes *before* the sleep so a
    /// hold that starts late in the ack window still resets it, and the
    /// caller's re-check comes after, so a hold that can end at once
    /// still costs one tick rather than spinning on the registry.
    ///
    /// The `loop` itself stays at the two call sites rather than being
    /// swallowed by a `hold_until(…, poll)` taking an async closure —
    /// which is the shape this wanted. A generic async callback carries
    /// no `Send` bound that survives `tokio::spawn`ing the `handle`
    /// future around it (`AsyncFnMut::CallRefFuture` is not nameable on
    /// stable), and a boxed poll would trade the whole point of the
    /// unification for an allocation per tick. Everything that has to
    /// agree between the two holds — the cadence, the keepalive, the
    /// meaning of "stopping" — is here; what differs is the question
    /// each one asks, which is the part that should differ.
    pub(super) async fn hold_tick(
        &self,
        msg: &async_nats::jetstream::Message,
        trigger_id: Option<uuid::Uuid>,
    ) -> bool {
        if self.stopping() {
            debug!(
                trigger_id = %trigger_name(trigger_id),
                "drain or shutdown during a hold; the trigger is not started"
            );
            return false;
        }
        self.keep_alive(msg, trigger_id).await;
        tokio::time::sleep(HOLD_KEEPALIVE).await;
        true
    }

    /// Reset the delivery's ack window without counting as a delivery.
    /// A failure keeps the hold going: the worst case is the window
    /// expiring and JetStream redelivering, which is where we would be
    /// without the hold at all.
    async fn keep_alive(
        &self,
        msg: &async_nats::jetstream::Message,
        trigger_id: Option<uuid::Uuid>,
    ) {
        if let Err(err) = msg.ack_with(AckKind::Progress).await {
            warn!(
                error = %err,
                trigger_id = %trigger_name(trigger_id),
                "failed to keep a held trigger alive"
            );
        }
    }

    /// Hold `msg` while `model` is paused. Returns at once for an
    /// unpaused model, which is every trigger on a healthy day.
    ///
    /// No worker permit is involved, in either direction: a held trigger
    /// has none to give back and takes none to leave with. It asks for
    /// one only once it is ready to run
    /// ([`Self::acquire_run_permit`]), so a paused model's backlog
    /// cannot stop agents on other models (#733).
    pub(super) async fn admit(
        &self,
        msg: &async_nats::jetstream::Message,
        model: &str,
        trigger_id: Option<uuid::Uuid>,
    ) -> Admission {
        let Some(remaining) = self.throttle.pause_remaining(model) else {
            return Admission::Start;
        };
        info!(
            model,
            trigger_id = %trigger_name(trigger_id),
            paused_for_ms = remaining.as_millis() as u64,
            "model is paused; holding the trigger un-started until the pause ends"
        );
        loop {
            if !self.hold_tick(msg, trigger_id).await {
                return Admission::Interrupted;
            }
            if self.throttle.pause_remaining(model).is_none() {
                debug!(
                    model,
                    trigger_id = %trigger_name(trigger_id),
                    "pause ended; starting the held trigger"
                );
                return Admission::Start;
            }
        }
    }

    /// Wait for the worker-cap permit a trigger runs under, keeping its
    /// delivery alive meanwhile (#733). The last gate in `handle`, and
    /// the only place a permit is taken.
    ///
    /// `msg` is the delivery to keep alive, and `None` is the one caller
    /// that has nothing to keep alive: a **due resume**, whose trigger
    /// was acked at its first WAL write and whose row is the WAL's. It
    /// queues for a permit like everything else, so a resume cannot jump
    /// the triggers already waiting, nor they it.
    ///
    /// `None` back is a drain or shutdown during the wait — the same
    /// answer [`Admission::Interrupted`] gives, and it earns the same
    /// treatment: the caller returns without acking, leaving the
    /// delivery for the next binary. **Both** arms answer it, because a
    /// drain is also how a permit comes free: suspending runners release
    /// theirs, so a waiter can be handed one by the very drain that
    /// should have stopped it.
    ///
    /// **One waiter, registered once.** The acquire future is created
    /// before the loop and polled through the whole wait, so the trigger
    /// keeps its place in the semaphore's FIFO and the longest-parked
    /// one runs first; re-creating it each tick — which is what a naive
    /// `select!` in a loop does — would send it to the back of the queue
    /// every [`HOLD_KEEPALIVE`]. `biased` puts the permit first, so the
    /// fast path (a permit already free) takes it on the first poll and
    /// never arms the timer, never logs and never acks.
    pub(super) async fn acquire_run_permit(
        &self,
        msg: Option<&async_nats::jetstream::Message>,
        trigger_id: Option<uuid::Uuid>,
    ) -> Option<OwnedSemaphorePermit> {
        let acquire = Arc::clone(&self.permits).acquire_owned();
        tokio::pin!(acquire);
        let mut waited = false;
        loop {
            tokio::select! {
                biased;
                permit = &mut acquire => {
                    let permit = permit.expect("dispatcher semaphore is never closed");
                    // The drain frees permits too. Every runner releases
                    // its own as it suspends, and that release wakes the
                    // head of this FIFO — whose first-polled arm, under
                    // `biased`, is this one. Without the check a trigger
                    // would be started *into a draining worker* by the
                    // very drain that was meant to stop it. Dropping the
                    // permit here hands it back to a semaphore nothing
                    // will take from again: the exit is process-terminal.
                    if self.stopping() {
                        drop(permit);
                        debug!(
                            trigger_id = %trigger_name(trigger_id),
                            "a worker permit freed during a drain or shutdown; \
                             nothing is started"
                        );
                        return None;
                    }
                    if waited {
                        debug!(
                            trigger_id = %trigger_name(trigger_id),
                            "a worker permit freed; starting the waiting trigger"
                        );
                    }
                    return Some(permit);
                }
                _ = tokio::time::sleep(HOLD_KEEPALIVE) => {
                    if self.stopping() {
                        debug!(
                            trigger_id = %trigger_name(trigger_id),
                            "drain or shutdown while waiting for a worker permit; \
                             nothing is started"
                        );
                        return None;
                    }
                    if !waited {
                        waited = true;
                        info!(
                            trigger_id = %trigger_name(trigger_id),
                            available_permits = self.permits.available_permits(),
                            in_flight = self.max_concurrent
                                - self.permits.available_permits(),
                            "worker permits exhausted; holding the trigger un-started"
                        );
                    }
                    if let Some(msg) = msg {
                        self.keep_alive(msg, trigger_id).await;
                    }
                }
            }
        }
    }

    /// Whether this dispatcher is on its way out: the worker is draining
    /// or the loop has seen its shutdown signal. Every wait — a paused
    /// model's hold, a full agent's (#718), and the wait for a worker
    /// permit (#733) — lets go on it.
    pub(super) fn stopping(&self) -> bool {
        self.worker.drain_status() == DrainState::Draining || self.stopping.load(Ordering::SeqCst)
    }
}
