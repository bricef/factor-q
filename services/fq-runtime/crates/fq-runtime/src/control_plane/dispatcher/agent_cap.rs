//! Admission control at the dispatcher, second rule (#718): a trigger
//! for an agent already at its `max_concurrent` is not started. It is
//! *held* — pulled, un-acked, kept alive with JetStream in-progress
//! acks — until one of that agent's invocations ends, then started as
//! the first attempt it still is.
//!
//! The waiting itself is not this module's: each pass is
//! [`TriggerDispatcher::hold_tick`](super::admission), the one hold both
//! admission rules run on — one cadence, one keepalive, one
//! "still your first delivery" guarantee. A NAK per refusal would burn
//! the trigger durable's bounded redeliveries, dead-letter a trigger
//! after four refusals, and stamp `attempt: N` into the transcript
//! preamble; an in-progress ack resets the ack window without counting
//! as a delivery. `admission`'s doc is the reference for that trade.
//! What this module owns is the *cap*: what the hold waits on, what it
//! does with the worker permit, and what an interrupted delivery gets.
//!
//! # A hold occupies no worker permit
//!
//! This is the one place the two holds do *not* behave alike, and it
//! decides whether the feature does what the issue asks or the exact
//! inverse of it. (The pause hold keeps its permit and has the same
//! problem for the same reason —
//! <https://github.com/bricef/factor-q/issues/733>.)
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
//! invocations; waiting is free.
//!
//! What bounds the parked holds instead is the consumer's ack-pending
//! window, and it is worth stating as the number it is rather than as
//! "what is queued": the loop pulls whenever a permit is free and a hold
//! releases its permit, so triggers for a full agent are pulled and
//! parked until `max_ack_pending` — `max(2 × worker cap,
//! NATS_DEFAULT_MAX_ACK_PENDING)`, so **1000** on any ordinary
//! deployment. Each parked hold is one task, one un-acked message, one
//! registry read-lock and one in-progress ack per
//! [`HOLD_KEEPALIVE`](super::admission::HOLD_KEEPALIVE) tick. NATS will
//! not mind 4000 acks a second; the daemon's own reload latency is the
//! thing that would notice, and an event-driven wake off `AgentSlot`'s
//! `Drop` is the shape that would fix it if it ever does.
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
//! **The cap is re-read every tick, not once.** This module's poll asks
//! the *current* registry for `max_concurrent` on each pass, so `fq
//! reload` reaches a trigger that is already waiting — an operator who
//! raises a cap to unstick a queue does not also have to restart the
//! daemon. The model-pause hold has no equivalent because a pause is not
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
//! **A reload that removes the agent ends the hold.** The trigger is
//! acked and dropped, exactly as one arriving a second later would be —
//! see [`TriggerDispatcher::refuse_removed`], which is also where the
//! edited-definition case is written down.
//!
//! **A drain or shutdown during a hold requeues the trigger** rather
//! than leaving the delivery un-acked, because a cap hold can outlast a
//! deploy and an un-acked hold is charged a delivery on the way back —
//! see [`TriggerDispatcher::requeue_held`]. The pause hold in
//! [`super::admission`] still leaves its delivery un-acked; a pause is
//! tens of seconds, so it does not reach the same arithmetic.

use std::sync::Arc;

use tokio::sync::OwnedSemaphorePermit;
use tracing::{debug, info, warn};

use super::{TriggerDispatcher, trigger_name};
use crate::agent::AgentId;
use crate::control_plane::agent_cap::AgentSlot;

/// What the registry has to say about a held trigger's agent.
enum Declared {
    /// Still loaded; this is its `max_concurrent` (`None` = uncapped).
    Cap(Option<u32>),
    /// Gone — a reload removed the definition while the trigger waited.
    Removed,
}

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
    /// the trigger has been requeued for the next binary
    /// ([`Self::requeue_held`]).
    pub(super) async fn admit_agent_slot(
        &self,
        msg: &async_nats::jetstream::Message,
        agent: &AgentId,
        trigger_id: Option<uuid::Uuid>,
        payload: &serde_json::Value,
        permit: OwnedSemaphorePermit,
    ) -> Option<(AgentSlot, OwnedSemaphorePermit)> {
        let Declared::Cap(cap) = self.declared(agent).await else {
            return self.refuse_removed(msg, agent, trigger_id).await;
        };
        if let Some(slot) = self.agent_caps.try_enter(agent, cap) {
            return Some((slot, permit));
        }
        // Counted from here so `fq doctor` can say how many are waiting;
        // dropped on every exit below, including the interrupted one.
        let _waiting = self.agent_caps.hold(agent);
        info!(
            agent_id = %agent,
            trigger_id = %trigger_name(trigger_id),
            in_flight = self.agent_caps.in_flight(agent),
            max_concurrent = cap.unwrap_or(0),
            "agent is at its concurrency cap; holding the trigger un-started until a slot frees"
        );
        // The blocker this module's doc is about: waiting is free, so
        // the loop can go on pulling and other agents go on running.
        drop(permit);
        loop {
            if !self.hold_tick(msg, trigger_id).await {
                self.requeue_held(msg, agent, trigger_id, payload).await;
                return None;
            }
            // Re-read every tick: a slot may have freed, `fq reload` may
            // have changed the cap, and it may have taken the agent away.
            let Declared::Cap(cap) = self.declared(agent).await else {
                return self.refuse_removed(msg, agent, trigger_id).await;
            };
            let Some(slot) = self.agent_caps.try_enter(agent, cap) else {
                continue;
            };
            // Slot first, permit second. A permit is almost always free
            // the instant a slot is — the invocation that just ended
            // released both — but if the loop pulled with it first, give
            // the slot back rather than block a runnable agent behind a
            // waiter, and ask again next tick.
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

    /// Give a held trigger back to the stream, so a restart mid-hold
    /// costs it nothing.
    ///
    /// Leaving the delivery un-acked — what a drain did before — is free
    /// for a pause hold measured in tens of seconds and is *not* free
    /// here. A cap hold is bounded by the invocation ahead of it, which
    /// for a build-bound agent is a `just ci` pass; a backlog of ten
    /// issues at cap 2 is hours of holding, and the dogfood instance
    /// deploys hourly. Each deploy mid-hold would charge the trigger one
    /// delivery: `attempt: 2` in the transcript preamble (which the
    /// redelivery-storm notes read as the tell for a bug), and after
    /// [`TRIGGER_MAX_DELIVER`](crate::bus::TRIGGER_MAX_DELIVER)
    /// deliveries a `trigger_exhausted` dead letter for an issue nobody
    /// ever touched. A NAK is not the alternative — that counts as a
    /// delivery too.
    ///
    /// So the trigger is republished under the same
    /// `Fq-Trigger-Id` and the original delivery acked: the same trigger,
    /// by name, arriving at the next binary as the first attempt it
    /// still is. It goes to the back of the stream, which is what a hold
    /// already promises — "a hold is a race, not a queue".
    ///
    /// **Publish first, ack second.** A crash between the two redelivers
    /// the original, which is exactly where a requeue-less drain leaves
    /// us; the reverse order could ack a trigger that was never
    /// republished. A publish that fails leaves the delivery un-acked
    /// for the same reason — the old behaviour is the fallback, never a
    /// dropped trigger.
    ///
    /// **The consume loop must have stopped pulling first**, which is
    /// why it drops its message stream before it awaits the tasks that
    /// call this. A pull request left outstanding at the server takes
    /// the fresh copy the instant it is published, into a dispatcher
    /// that is on its way out and will never ack it — so the trigger
    /// reaches the next binary as `attempt: 2` after all, and the
    /// requeue buys a round trip and nothing else. That ordering is
    /// load-bearing, and the test below is what would catch it moving.
    async fn requeue_held(
        &self,
        msg: &async_nats::jetstream::Message,
        agent: &AgentId,
        trigger_id: Option<uuid::Uuid>,
        payload: &serde_json::Value,
    ) {
        // A header-less external trigger has no name until `delivered`
        // mints one, and it never got that far — so name it here and
        // requeue it under that, rather than sending an anonymous copy.
        let id = trigger_id.unwrap_or_else(uuid::Uuid::now_v7);
        match self.bus.publish_trigger_named(agent, id, payload).await {
            Ok(_) => {
                debug!(
                    agent_id = %agent,
                    trigger_id = %id,
                    "drain or shutdown during a cap hold; requeued the trigger \
                     for the next binary as its first delivery"
                );
                self.ack(msg, Some(id), "requeued from a cap hold").await;
            }
            Err(err) => warn!(
                agent_id = %agent,
                trigger_id = %trigger_name(trigger_id),
                error = %err,
                "failed to requeue a held trigger; leaving the delivery un-acked, \
                 which redelivers it to the next binary at the cost of one attempt"
            ),
        }
    }

    /// What the *current* registry says about a held trigger's agent.
    ///
    /// Asked on every tick, so `fq reload` reaches a trigger that is
    /// already waiting — including the reload that takes the agent away.
    async fn declared(&self, agent: &AgentId) -> Declared {
        match self.registry.read().await.get_loaded(agent) {
            Some(loaded) => Declared::Cap(loaded.agent.max_concurrent()),
            None => Declared::Removed,
        }
    }

    /// A reload removed the agent while its trigger waited.
    ///
    /// Refused the way the same trigger would be refused had it arrived
    /// one second later: acked and dropped, with the line `handle` logs
    /// for an unknown agent. Reading `None` as "no cap" — which is what
    /// this did — *admitted* the trigger and ran the definition clone
    /// taken before the hold, so an agent an operator had deleted kept
    /// starting invocations, and a trigger's fate depended on whether it
    /// arrived before or after the reload.
    ///
    /// The other half of a mid-hold reload is unchanged and deliberate:
    /// a definition that was *edited* runs as it was when the trigger
    /// was pulled (ADR-0020, refresh between invocations). Only the cap
    /// is re-read live, because that is the number an operator changes
    /// to unstick the queue this trigger is in.
    async fn refuse_removed(
        &self,
        msg: &async_nats::jetstream::Message,
        agent: &AgentId,
        trigger_id: Option<uuid::Uuid>,
    ) -> Option<(AgentSlot, OwnedSemaphorePermit)> {
        warn!(
            agent_id = %agent,
            trigger_id = %trigger_name(trigger_id),
            "agent was removed while its trigger was held; dropping the trigger \
             rather than running a definition that is no longer loaded"
        );
        self.ack(msg, trigger_id, "agent removed during a cap hold")
            .await;
        None
    }
}
