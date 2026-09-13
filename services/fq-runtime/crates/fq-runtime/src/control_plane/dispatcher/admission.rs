//! Admission control at the dispatcher: the hold, and the first of the
//! two rules that use it.
//!
//! **The rule (#278, the third layer):** a trigger for a paused model is
//! not started. It is *held* — pulled, un-acked, kept alive with
//! JetStream in-progress acks — until the pause ends, then started as
//! the first attempt it still is.
//!
//! Why hold rather than NAK with a delay: the trigger durable's ack
//! window is one second (`TRIGGER_RETRY_BACKOFF[0]`, see `bus.rs`), and
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
//! - **the worker permit.** A cap hold gives its permit back for the
//!   length of the wait and takes a fresh one to run; a pause hold
//!   **keeps** its permit, so a pause on one model can hold the whole
//!   worker cap and stop agents on other models. That asymmetry is not
//!   a decision, it is where the work stopped —
//!   <https://github.com/bricef/factor-q/issues/733> gives the pause
//!   hold the same treatment, with the test that proves it;
//! - **what an interrupted delivery deserves.** A pause hold leaves it
//!   un-acked for the next binary; a cap hold, which can outlast a
//!   deploy, requeues it so the restart costs it no delivery.

use std::sync::atomic::Ordering;
use std::time::Duration;

use async_nats::jetstream::AckKind;
use tracing::{debug, info, warn};

use super::{TriggerDispatcher, trigger_name};
use crate::worker::DrainState;

/// How often a held delivery is kept alive — and, because the check
/// follows the ack, how often the hold asks whether it can stop.
///
/// **One constant for both holds.** The trigger durable's real
/// first-delivery deadline is one second (`TRIGGER_RETRY_BACKOFF[0]`;
/// JetStream replaces `ack_wait` with `backoff[0]` wherever a schedule
/// is set), and one slipped tick means a redelivery, a second `handle`
/// for the same trigger, `attempt: 2` and a duplicate invocation. 250 ms
/// leaves 750 ms of slack per tick.
///
/// The cap hold is what argued the number down from 400 ms: a pause hold
/// is bounded by the pause, tens of ticks, while a cap hold is bounded
/// by the *invocation ahead of it* — for a build-bound agent a ~17-minute
/// `just ci` pass on the same box, thousands of ticks, every one of which
/// has to land on a machine that is busy compiling. Nothing in that
/// argument is special to the cap, so both holds take the wider margin.
///
/// It is slack against a one-second window and not a fix for it. The
/// window itself belongs to
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
    /// The permit the trigger was pulled under is **kept** for the
    /// length of the hold — this call does not take it, so the caller's
    /// permit simply stays taken. That is head-of-line blocking, and
    /// <https://github.com/bricef/factor-q/issues/733> is where it gets
    /// the cap hold's treatment.
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

    /// Whether this dispatcher is on its way out: the worker is draining
    /// or the loop has seen its shutdown signal. Both holds — a paused
    /// model's and a full agent's (#718) — let go on it.
    pub(super) fn stopping(&self) -> bool {
        self.worker.drain_status() == DrainState::Draining || self.stopping.load(Ordering::SeqCst)
    }
}
