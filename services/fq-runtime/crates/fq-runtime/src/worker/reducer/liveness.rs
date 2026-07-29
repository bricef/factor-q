//! The runner's live registry: which invocations this process is
//! driving *right now*, whose work each one is, and which of them an
//! operator has asked to halt. Split from `runner.rs` (the `rounds`
//! precedent) because it is one concept, not three fields — and because
//! it is the concept two operator preconditions rest on.
//!
//! It is the system's only **zero-lag** authority on liveness. Every
//! durable answer to "is this invocation running" is behind it by
//! construction: the projection and the coordination owner rows are
//! folded by asynchronous durable consumers, and nothing on the
//! dispatch path writes an owner row at all. The process doing the work
//! knows first, and it knows without asking anyone.
//!
//! That is why the registry keys on the invocation and *values* the
//! agent rather than being a bare set. Liveness and identity are the
//! same fact here — this runner cannot be driving an invocation without
//! knowing whose it is — so an operator command that has established
//! "this is live" can resolve it without falling back to a store that
//! may not have heard yet. `invocation.drop` depends on exactly that:
//! having armed a halt on the strength of this registry, it must never
//! then report the invocation unknown (#107).
//!
//! Worker-local by construction; cross-worker liveness is the
//! #107/#374 coordination story.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use uuid::Uuid;

use crate::agent::AgentId;

/// The invocations this runner is driving, and the halts armed against
/// them. Entries live exactly as long as the drive does — see
/// [`ActiveInvocation`].
#[derive(Default)]
pub(crate) struct LiveRegistry {
    /// Invocation → the agent it is being driven for, registered for
    /// the duration of every run/resume entry.
    active: Mutex<HashMap<Uuid, AgentId>>,
    /// Invocation-scoped halt requests, consumed at the next step
    /// boundary. Unlike daemon drain, these terminate only the named
    /// invocation after the operator-drop event reconciles its durable
    /// row.
    halt_requested: Mutex<HashSet<Uuid>>,
}

impl LiveRegistry {
    /// The agent this runner is driving `invocation_id` for, or `None`
    /// when it is not driving it at all. Liveness and identity in one
    /// answer, from the one authority that has both.
    pub(crate) fn agent_for(&self, invocation_id: &Uuid) -> Option<AgentId> {
        self.active
            .lock()
            .expect("active set poisoned")
            .get(invocation_id)
            .cloned()
    }

    /// Whether this runner is currently driving `invocation_id`.
    pub(crate) fn is_active(&self, invocation_id: &Uuid) -> bool {
        self.agent_for(invocation_id).is_some()
    }

    /// Arm a halt for the next step boundary. Returns false without
    /// changing state when this runner is not driving the invocation —
    /// the test-and-set that keeps a halt from outliving its drive.
    pub(crate) fn request_halt(&self, invocation_id: Uuid) -> bool {
        // One lock for both halves: between a separate `is_active` and
        // the insert the drive could end, leaving an armed halt behind
        // for the next drive of the same id to consume.
        let active = self.active.lock().expect("active set poisoned");
        if !active.contains_key(&invocation_id) {
            return false;
        }
        self.halt_requested
            .lock()
            .expect("halt set poisoned")
            .insert(invocation_id);
        true
    }

    /// Consume a halt, if one is armed.
    pub(crate) fn take_halt(&self, invocation_id: Uuid) -> bool {
        self.halt_requested
            .lock()
            .expect("halt set poisoned")
            .remove(&invocation_id)
    }

    /// Register a drive of `invocation_id` for `agent`, for as long as
    /// the returned guard lives.
    pub(crate) fn enter<'a>(
        &'a self,
        invocation_id: Uuid,
        agent_id: AgentId,
        rounds: &'a super::rounds::RoundLedger,
    ) -> ActiveInvocation<'a> {
        self.active
            .lock()
            .expect("active set poisoned")
            .insert(invocation_id, agent_id);
        ActiveInvocation {
            live: self,
            rounds,
            id: invocation_id,
        }
    }
}

/// RAII entry in a [`LiveRegistry`]: removed on drop, so a panic or
/// early return can never leave a phantom "live" marker that would
/// block operator resume forever.
///
/// It clears the halt request on the same edge. A halt armed against an
/// invocation that then completes, fails, or panics before reaching a
/// step boundary would otherwise sit in the set for the daemon's
/// lifetime — and because `resume` re-drives the *same* invocation id, a
/// later drive of that id would consume the stale halt and suspend for
/// no reason. Tying both to the drive's lifetime makes that
/// unrepresentable.
pub(crate) struct ActiveInvocation<'a> {
    live: &'a LiveRegistry,
    rounds: &'a super::rounds::RoundLedger,
    id: Uuid,
}

impl Drop for ActiveInvocation<'_> {
    fn drop(&mut self) {
        self.live
            .active
            .lock()
            .expect("active set poisoned")
            .remove(&self.id);
        self.live
            .halt_requested
            .lock()
            .expect("halt set poisoned")
            .remove(&self.id);
        self.rounds.forget(self.id);
    }
}

#[cfg(test)]
mod tests;
