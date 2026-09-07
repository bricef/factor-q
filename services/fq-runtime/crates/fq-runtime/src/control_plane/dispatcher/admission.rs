//! Admission control at the dispatcher (#278, the third layer): a
//! trigger for a paused model is not started. It is *held* — pulled,
//! un-acked, kept alive with JetStream in-progress acks — until the
//! pause ends, then started as the first attempt it still is.
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
//! The cost is head-of-line blocking: a held trigger occupies the
//! dispatcher permit it was pulled under. A drain or shutdown during a
//! hold leaves the delivery un-acked for the next binary, exactly as a
//! drain that lands after a pull does today.

use std::sync::atomic::Ordering;
use std::time::Duration;

use async_nats::jetstream::AckKind;
use tracing::{debug, info, warn};

use super::{TriggerDispatcher, trigger_name};
use crate::worker::DrainState;

/// How often a held delivery is kept alive. Well inside the one-second
/// ack window, so a scheduling hiccup does not turn a hold into a
/// redelivery.
pub(super) const HOLD_KEEPALIVE: Duration = Duration::from_millis(400);

/// What the hold decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Admission {
    /// The model is not paused (any more): start the invocation.
    Start,
    /// A drain or shutdown landed during the hold: leave the delivery
    /// un-acked so it reaches the next binary.
    Interrupted,
}

impl TriggerDispatcher {
    /// Hold `msg` while `model` is paused. Returns at once for an
    /// unpaused model, which is every trigger on a healthy day.
    pub(super) async fn admit(
        &self,
        msg: &async_nats::jetstream::Message,
        model: &str,
        trigger_id: Option<uuid::Uuid>,
    ) -> Admission {
        let Some(mut remaining) = self.throttle.pause_remaining(model) else {
            return Admission::Start;
        };
        info!(
            model,
            trigger_id = %trigger_name(trigger_id),
            paused_for_ms = remaining.as_millis() as u64,
            "model is paused; holding the trigger un-started until the pause ends"
        );
        loop {
            if self.stopping() {
                debug!(
                    model,
                    trigger_id = %trigger_name(trigger_id),
                    "drain or shutdown during a hold; leaving the trigger for the next binary"
                );
                return Admission::Interrupted;
            }
            if let Err(err) = msg.ack_with(AckKind::Progress).await {
                // Keep holding: the worst case is the window expiring and
                // JetStream redelivering, which is where we would be
                // without the hold at all.
                warn!(
                    error = %err,
                    trigger_id = %trigger_name(trigger_id),
                    "failed to keep a held trigger alive"
                );
            }
            tokio::time::sleep(remaining.min(HOLD_KEEPALIVE)).await;
            match self.throttle.pause_remaining(model) {
                None => {
                    debug!(
                        model,
                        trigger_id = %trigger_name(trigger_id),
                        "pause ended; starting the held trigger"
                    );
                    return Admission::Start;
                }
                Some(still) => remaining = still,
            }
        }
    }

    /// Whether this dispatcher is on its way out: the worker is draining
    /// or the loop has seen its shutdown signal.
    fn stopping(&self) -> bool {
        self.worker.drain_status() == DrainState::Draining
            || self.stopping.load(Ordering::SeqCst)
    }
}
