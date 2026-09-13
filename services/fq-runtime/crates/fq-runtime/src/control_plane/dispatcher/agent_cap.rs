//! Admission control at the dispatcher, second rule (#718): a trigger
//! for an agent already at its `max_concurrent` is not started. It is
//! *held* — pulled, un-acked, kept alive with JetStream in-progress
//! acks — until one of that agent's invocations ends, then started as
//! the first attempt it still is.
//!
//! The mechanism is deliberately the one [`super::admission`] already
//! uses for a paused model, and for the same reasons: a NAK per refusal
//! would burn the trigger durable's bounded redeliveries, dead-letter a
//! trigger after four refusals, and stamp `attempt: N` into the
//! transcript preamble. An in-progress ack resets the ack window without
//! counting as a delivery, so a held trigger consumes nothing and starts
//! as `attempt: 1`. That module's doc is the reference for the trade;
//! this one says what is different.
//!
//! # A hold occupies no worker permit
//!
//! This is the one place the two holds must *not* behave alike, and it
//! decides whether the feature does what the issue asks or the exact
//! inverse of it.
//!
//! The consume loop takes a `[worker] max_concurrent_invocations` permit
//! *before* pulling a trigger, and the permit rides into the spawned
//! task. If a cap hold simply parked there, the hold would **be** a
//! permit: at a worker cap of 30 with `m0-issue-fix` at
//! `max_concurrent: 2`, thirty `status:ready` issues would leave two
//! builds running, twenty-eight parked, and *no permits left* — so the
//! loop would stop pulling and `doc-drift` and `m0-issue-triage` would
//! not run at all until the builds drained, for hours. The per-agent cap
//! would have made the fleet quieter than no cap at all, and `fq doctor`
//! would have reported nothing to fix.
//!
//! So the permit is given back for the length of the hold and a fresh
//! one is taken on the way out. The worker cap bounds *running*
//! invocations; waiting is free. What bounds the parked holds instead is
//! what is queued on the durable — each is one task and one un-acked
//! message, and the loop only pulls while a permit is free.
//!
//! **The order on the way out is agent slot first, then worker permit.**
//! A slot held while waiting for a permit is fine and cannot cycle:
//! permits are released by invocations finishing, and a finishing
//! invocation never waits on a slot. The reverse order is the one that
//! re-creates the bug.
//!
//! **A hold is a race, not a queue.** A freed slot is taken by whichever
//! parked hold polls first, or by a trigger the loop pulls in the same
//! instant; nothing is ordered and nothing is promised. Every parked
//! hold retries on every tick, so none waits long, but a trigger that
//! arrived later can start earlier.
//!
//! # What else is different
//!
//! **The cap is re-read every tick, not once.** The wait loop asks the
//! *current* registry for `max_concurrent` on each pass, so `fq reload`
//! reaches a trigger that is already waiting — an operator who raises a
//! cap to unstick a queue does not also have to restart the daemon. The
//! model-pause hold has no equivalent because a pause is not
//! configuration.
//!
//! **Admission is an acquisition, not a question.** The slot is taken
//! inside the same lock that finds it free
//! ([`AgentConcurrency`](crate::control_plane::agent_cap::AgentConcurrency)), so
//! two triggers racing for the last slot of a `max_concurrent: 1` agent
//! cannot both pass. Asking and then starting would be exactly the bug
//! the cap exists to prevent.
//!
//! **The order is pause first, then cap.** A trigger held for a paused
//! model has not taken a slot, so it is not occupying its agent's cap
//! while it waits on something unrelated.
//!
//! A drain or shutdown during a hold still leaves the delivery un-acked
//! for the next binary, exactly as [`super::admission`] documents.

use std::sync::Arc;
use std::time::Duration;

use async_nats::jetstream::AckKind;
use tokio::sync::OwnedSemaphorePermit;
use tracing::{debug, info, warn};

use super::{TriggerDispatcher, trigger_name};
use crate::agent::AgentId;
use crate::control_plane::agent_cap::AgentSlot;

/// How often a cap hold re-checks — and, because the check follows an
/// in-progress ack, how often the delivery is kept alive.
///
/// **Its own constant, not `admission::HOLD_KEEPALIVE`, because
/// the margin has to hold for a different length of time.** The trigger
/// durable's real first-delivery deadline is one second
/// (`TRIGGER_RETRY_BACKOFF[0]`; JetStream replaces `ack_wait` with
/// `backoff[0]` wherever a schedule is set), and one slipped tick means
/// a redelivery, a second `handle` for the same trigger, `attempt: 2`
/// and a duplicate invocation.
///
/// A pause hold is bounded by the pause — tens of ticks. A cap hold is
/// bounded by the *invocation ahead of it*, which for a build-bound
/// agent is a `just ci` pass of ~17 minutes on the same box: thousands
/// of ticks, every one of which has to land, on a machine that is busy
/// compiling. 250 ms leaves 750 ms of slack per tick rather than 600 ms,
/// which is the margin that number buys.
///
/// It is slack against a one-second window and not a fix for it. The
/// window itself belongs to
/// <https://github.com/bricef/factor-q/issues/327>, which owns the
/// duplicate-invocation class this guards against; widening it is that
/// issue's to do, not this module's.
pub(super) const CAP_POLL: Duration = Duration::from_millis(250);

impl TriggerDispatcher {
    /// Hold `msg` while `agent` is at its cap. Returns at once for an
    /// agent with no `max_concurrent` and for one with a slot free,
    /// which is every trigger on a quiet fleet.
    ///
    /// `permit` is the worker-cap permit the trigger was pulled under.
    /// On the fast path it is handed straight back; on a hold it is
    /// dropped for the length of the wait and a fresh one is taken
    /// before the invocation starts.
    ///
    /// `Some((slot, permit))` is the agent's claim on its own cap and
    /// its claim on the worker cap, both for as long as the invocation
    /// runs; `None` means a drain or shutdown landed during the hold and
    /// the delivery is being left for the next binary.
    pub(super) async fn admit_agent_slot(
        &self,
        msg: &async_nats::jetstream::Message,
        agent: &AgentId,
        trigger_id: Option<uuid::Uuid>,
        permit: OwnedSemaphorePermit,
    ) -> Option<(AgentSlot, OwnedSemaphorePermit)> {
        let cap = self.declared_cap(agent).await;
        if let Some(slot) = self.agent_caps.try_enter(agent.as_str(), cap) {
            return Some((slot, permit));
        }
        // Counted from here so `fq doctor` can say how many are waiting;
        // dropped on every exit below, including the interrupted one.
        let _waiting = self.agent_caps.hold(agent.as_str());
        info!(
            agent_id = %agent,
            trigger_id = %trigger_name(trigger_id),
            in_flight = self.agent_caps.in_flight(agent.as_str()),
            max_concurrent = cap.unwrap_or(0),
            "agent is at its concurrency cap; holding the trigger un-started until a slot frees"
        );
        // The blocker this module's doc is about: waiting is free, so
        // the loop can go on pulling and other agents go on running.
        drop(permit);
        loop {
            if self.stopping() {
                debug!(
                    agent_id = %agent,
                    trigger_id = %trigger_name(trigger_id),
                    "drain or shutdown during a cap hold; leaving the trigger for the next binary"
                );
                return None;
            }
            self.keep_alive(msg, trigger_id).await;
            tokio::time::sleep(CAP_POLL).await;
            // Re-read: a slot may have freed, and `fq reload` may have
            // changed the cap itself.
            let cap = self.declared_cap(agent).await;
            let Some(slot) = self.agent_caps.try_enter(agent.as_str(), cap) else {
                continue;
            };
            // Slot first, permit second. A permit is almost always free
            // the instant a slot is — the invocation that just ended
            // released both — but if the loop pulled with it first,
            // give the slot back rather than block a runnable agent
            // behind a waiter, and ask again next tick.
            match Arc::clone(&self.permits).try_acquire_owned() {
                Ok(permit) => {
                    debug!(
                        agent_id = %agent,
                        trigger_id = %trigger_name(trigger_id),
                        "a slot freed; starting the held trigger"
                    );
                    return Some((slot, permit));
                }
                Err(_) => drop(slot),
            }
        }
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
                "failed to keep a cap-held trigger alive"
            );
        }
    }

    /// `max_concurrent` as the *current* registry declares it, or `None`
    /// for an agent that declares none — which is also the answer for an
    /// agent a reload has just removed, so a vanished definition does
    /// not leave a trigger held forever. (The unknown-agent path is
    /// upstream of here; a removal mid-hold is the only way to reach
    /// this case.)
    async fn declared_cap(&self, agent: &AgentId) -> Option<u32> {
        self.registry
            .read()
            .await
            .get_loaded(agent)
            .and_then(|loaded| loaded.agent.max_concurrent())
    }
}
