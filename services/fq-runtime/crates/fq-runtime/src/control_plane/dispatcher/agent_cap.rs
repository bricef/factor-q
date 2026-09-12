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
//! this one only says what is different.
//!
//! What is different is three things.
//!
//! **The cap is re-read every tick, not once.** The wait loop asks the
//! *current* registry for `max_concurrent` on each pass, so `fq reload`
//! reaches a trigger that is already waiting — an operator who raises a
//! cap to unstick a queue does not also have to restart the daemon. The
//! model-pause hold has no equivalent because a pause is not
//! configuration.
//!
//! **Admission is an acquisition, not a question.** The slot is taken
//! inside the same lock that finds it free ([`AgentConcurrency`]), so
//! two triggers racing for the last slot of a `max_concurrent: 1` agent
//! cannot both pass. Asking and then starting would be exactly the bug
//! the cap exists to prevent.
//!
//! **The order is pause first, then cap.** A trigger held for a paused
//! model has not taken a slot, so it is not occupying its agent's cap
//! while it waits on something unrelated.
//!
//! The cost is the same head-of-line blocking `admission` documents: a
//! held trigger occupies the dispatcher permit it was pulled under, and
//! a drain during a hold leaves the delivery un-acked for the next
//! binary.

use std::time::Duration;

use async_nats::jetstream::AckKind;
use tracing::{debug, info, warn};

use super::admission::HOLD_KEEPALIVE;
use super::{TriggerDispatcher, trigger_name};
use crate::agent::AgentId;
use crate::control_plane::agent_cap::AgentSlot;

impl TriggerDispatcher {
    /// Hold `msg` while `agent` is at its cap. Returns at once for an
    /// agent with no `max_concurrent` and for one with a slot free,
    /// which is every trigger on a quiet fleet.
    ///
    /// `Some(slot)` is the agent's claim on its own cap for as long as
    /// the invocation runs; `None` means a drain or shutdown landed
    /// during the hold and the delivery is being left for the next
    /// binary.
    pub(super) async fn admit_agent_slot(
        &self,
        msg: &async_nats::jetstream::Message,
        agent: &AgentId,
        trigger_id: Option<uuid::Uuid>,
    ) -> Option<AgentSlot> {
        let cap = self.declared_cap(agent).await;
        if let Some(slot) = self.agent_caps.try_enter(agent.as_str(), cap) {
            return Some(slot);
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
        loop {
            if self.stopping() {
                debug!(
                    agent_id = %agent,
                    trigger_id = %trigger_name(trigger_id),
                    "drain or shutdown during a cap hold; leaving the trigger for the next binary"
                );
                return None;
            }
            if let Err(err) = msg.ack_with(AckKind::Progress).await {
                // Keep holding: the worst case is the window expiring
                // and JetStream redelivering, which is where we would be
                // without the hold at all.
                warn!(
                    error = %err,
                    trigger_id = %trigger_name(trigger_id),
                    "failed to keep a cap-held trigger alive"
                );
            }
            tokio::time::sleep(CAP_POLL).await;
            // Re-read: a slot may have freed, and `fq reload` may have
            // changed the cap itself.
            let cap = self.declared_cap(agent).await;
            if let Some(slot) = self.agent_caps.try_enter(agent.as_str(), cap) {
                debug!(
                    agent_id = %agent,
                    trigger_id = %trigger_name(trigger_id),
                    "a slot freed; starting the held trigger"
                );
                return Some(slot);
            }
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

/// How often a cap hold looks again. Deliberately the keepalive
/// interval: the loop has to ack-progress that often anyway, and a
/// slot that freed a moment ago is worth 400ms of latency against
/// invocations that run for minutes.
const CAP_POLL: Duration = HOLD_KEEPALIVE;
