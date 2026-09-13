//! The per-agent concurrency count (#718): how many invocations of each
//! agent this daemon is running, and how many triggers are waiting for a
//! slot.
//!
//! One [`AgentConcurrency`] lives in the daemon and is shared by every
//! place an invocation actually starts — the dispatcher's trigger path,
//! startup recovery's resume tasks and `fq invocation resume`. The
//! dispatcher reads it to decide whether a new trigger may start; `fq
//! doctor` reads it to say what is being held.
//!
//! # The invariant, and its one exception
//!
//! **`in_flight(agent) ≤ max_concurrent(agent)` for everything this
//! daemon starts.** [`AgentConcurrency::try_enter`] is the only way in
//! for a trigger, and the check and the increment are one critical
//! section, so the bound cannot be raced past. A *deferral* does not
//! leave the count: the [`AgentSlot`] travels with the sleeping
//! invocation inside `DueResume` and comes back with its resume, so the
//! deferral path needs no entry of its own (see
//! `worker::deferral`). That was the hole the first shape of this module
//! had — a resume re-entered through [`AgentConcurrency::enter`], which
//! refuses nothing, so a persistently throttled model turned a capped
//! agent's held backlog into an uncapped burst.
//!
//! The exception is a resume of work *this process did not admit*:
//! startup recovery and `fq invocation resume`. A restart resumes what
//! it finds, and it may find more than the cap now allows; it admits
//! nothing new until it is back under cap, which is exactly what
//! `try_enter` gives. Refusing those would leave a half-done invocation
//! unresumed while it still occupies the host. So the property that
//! holds unconditionally is
//! `in_flight(agent) ≤ max(cap, invocations recovered at start)`, and
//! `control_plane::agent_cap::proptests` is that sentence as a test.
//!
//! # Why a count here and not a query of the WAL
//!
//! The live execution table `control.doctor` folds (`Views::executions`
//! over `find_in_flight_invocations`) is the authority on what is in
//! flight — but it cannot be the *admission* authority, for one reason
//! that is not a matter of taste: a WAL row appears at the invocation's
//! **first WAL write**, seconds into a run (that lag is exactly why the
//! trigger is acked there and not at dispatch). Two triggers for the
//! same agent are handled on two spawned tasks, so both would read a
//! table that still showed zero and both would start. A cap of 1 would
//! admit two invocations on the first burst — the ten-issues-labelled-at
//! -once case the cap exists for.
//!
//! Counting in memory closes that window: the increment *is* the
//! admission, taken under one lock, so the second trigger sees the first.
//!
//! The cost is that this count must be taken everywhere an invocation
//! runs, including the paths that never touch the dispatcher. That is
//! what [`AgentConcurrency::enter`] is for, and the two callers outside
//! the dispatcher — `recovery::spawn_resume_tasks` and the
//! `invocation.resume` handler — take it so a restart that resumes three
//! builds is not immediately joined by three more.
//!
//! # Why a guard rather than a decrement
//!
//! Every exit path must decrement or the agent wedges at its cap
//! forever, and the exit paths are not a list anyone can keep complete:
//! completed, failed, deferred, suspended by a drain, dropped by the
//! operator, and a panicking task. [`AgentSlot`] decrements in `Drop`,
//! so the paths are covered by construction rather than by inspection —
//! including the ones added later.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use fq_ops::health::AgentAtCap;

use crate::agent::AgentId;

/// Per-agent in-flight counts, shared across the daemon.
///
/// Cheap to ask: one short critical section over a map keyed by
/// [`AgentId`] — the validated value every caller already holds, rather
/// than a `String` each of them has to spell out of it — with no entry
/// at all for an agent that is doing nothing.
#[derive(Debug, Default)]
pub struct AgentConcurrency {
    agents: Mutex<HashMap<AgentId, AgentState>>,
}

/// What is known about one agent right now. Dropped from the map the
/// moment it goes quiet, so an idle daemon carries no state and the
/// snapshot has nothing uninteresting to filter out.
#[derive(Debug, Default)]
struct AgentState {
    /// Invocations running: slots held.
    in_flight: u32,
    /// Triggers pulled and waiting for a slot.
    held: u32,
    /// The cap last read off the registry for this agent. `None` for an
    /// agent with no `max_concurrent`, and for a resume that counted
    /// without consulting one.
    cap: Option<u32>,
}

impl AgentState {
    fn is_quiet(&self) -> bool {
        self.in_flight == 0 && self.held == 0
    }
}

impl AgentConcurrency {
    /// A fresh, empty count.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Take a slot for `agent` if its cap allows, recording `cap` as
    /// what the current registry says. `cap = None` — the agent
    /// declares no `max_concurrent` — always succeeds.
    ///
    /// The check and the increment happen under one lock: two triggers
    /// racing for the last slot of a `max_concurrent: 1` agent cannot
    /// both see it free.
    pub fn try_enter(self: &Arc<Self>, agent: &AgentId, cap: Option<u32>) -> Option<AgentSlot> {
        let mut agents = self.lock();
        let state = agents.entry(agent.clone()).or_default();
        state.cap = cap;
        match cap {
            Some(cap) if state.in_flight >= cap => None,
            _ => {
                state.in_flight += 1;
                Some(AgentSlot {
                    counts: Arc::clone(self),
                    agent: agent.clone(),
                })
            }
        }
    }

    /// Count an invocation this process did not admit — a resume of
    /// work that was in flight before this daemon (or this operator
    /// command) reached it. Never refused, and the **only** way the
    /// count can exceed a cap.
    ///
    /// Its two callers are startup recovery (`recovery::spawn_resume
    /// _tasks`) and `fq invocation resume`. Both are finishing an
    /// invocation whose WAL row is already in flight: holding it back
    /// would extend the time the host stays occupied rather than
    /// shorten it, and for startup recovery it would leave a half-done
    /// invocation unresumed.
    ///
    /// It is deliberately *not* the deferral path's entry. A deferred
    /// invocation never leaves the count — its [`AgentSlot`] rides the
    /// deferral queue — because a resume that re-entered here would be
    /// an admission the cap never made.
    pub fn enter(self: &Arc<Self>, agent: &AgentId, cap: Option<u32>) -> AgentSlot {
        let mut agents = self.lock();
        let state = agents.entry(agent.clone()).or_default();
        if cap.is_some() {
            state.cap = cap;
        }
        state.in_flight += 1;
        AgentSlot {
            counts: Arc::clone(self),
            agent: agent.clone(),
        }
    }

    /// Mark a trigger as waiting for a slot, for as long as the
    /// returned ticket lives. Visibility only — it grants nothing.
    pub fn hold(self: &Arc<Self>, agent: &AgentId) -> HeldTrigger {
        self.lock().entry(agent.clone()).or_default().held += 1;
        HeldTrigger {
            counts: Arc::clone(self),
            agent: agent.clone(),
        }
    }

    /// What `fq doctor` reports: every agent with a declared cap that is
    /// either full or holding a trigger, agent id first.
    ///
    /// An agent running *under* its cap with nothing waiting is left out
    /// on purpose. It is not the answer to any question an operator is
    /// asking here — "why has nothing picked this up" — and listing it
    /// would bury the one line that is.
    ///
    /// The two reasons to be listed are kept apart:
    /// [`AgentAtCap::is_at_cap`] decides the first, and a renderer asks
    /// it rather than re-deriving the comparison. They come apart for
    /// real, in the window between a slot freeing and the held trigger's
    /// next poll: the agent is under its cap and a trigger is still
    /// waiting, and saying "at cap" of it would be wrong.
    pub fn snapshot(&self) -> Vec<AgentAtCap> {
        let agents = self.lock();
        let mut listed: Vec<AgentAtCap> = agents
            .iter()
            .filter_map(|(agent, state)| {
                Some(AgentAtCap {
                    // The report row is where the id becomes a string:
                    // it is a wire shape, and the runtime is not.
                    agent: agent.as_str().to_string(),
                    in_flight: state.in_flight,
                    cap: state.cap?,
                    held: state.held,
                })
            })
            .filter(|a| a.is_at_cap() || a.held > 0)
            .collect();
        listed.sort_by(|a, b| a.agent.cmp(&b.agent));
        listed
    }

    /// Invocations of `agent` running right now. For tests and for any
    /// caller that wants the raw number rather than the report.
    pub fn in_flight(&self, agent: &AgentId) -> u32 {
        self.lock().get(agent).map_or(0, |s| s.in_flight)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<AgentId, AgentState>> {
        self.agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn release(&self, agent: &AgentId, what: fn(&mut AgentState)) {
        let mut agents = self.lock();
        let Some(state) = agents.get_mut(agent) else {
            return;
        };
        what(state);
        if state.is_quiet() {
            agents.remove(agent);
        }
    }
}

/// One running invocation's claim on its agent's cap. Releasing it is
/// `Drop`'s job, so every way an invocation can end — including a
/// panicking task — gives the slot back.
#[derive(Debug)]
pub struct AgentSlot {
    counts: Arc<AgentConcurrency>,
    agent: AgentId,
}

impl Drop for AgentSlot {
    fn drop(&mut self) {
        self.counts
            .release(&self.agent, |s| s.in_flight = s.in_flight.saturating_sub(1));
    }
}

/// One trigger waiting for a slot. Exists so `fq doctor` can say how
/// many are waiting; it confers nothing and imposes no order.
#[derive(Debug)]
pub struct HeldTrigger {
    counts: Arc<AgentConcurrency>,
    agent: AgentId,
}

impl Drop for HeldTrigger {
    fn drop(&mut self) {
        self.counts
            .release(&self.agent, |s| s.held = s.held.saturating_sub(1));
    }
}

/// The invariant as a property over random histories, in its own file
/// because it is a different kind of test from the examples below:
/// those pin one behaviour each, this one pins the sentence in this
/// module's doc.
#[cfg(test)]
mod proptests;

#[cfg(test)]
mod tests {
    use super::*;

    /// The count is keyed by the validated id, so the tests name
    /// their agents the way the daemon does.
    fn id(name: &str) -> AgentId {
        AgentId::new(name).expect("a legal agent id")
    }

    #[test]
    fn an_uncapped_agent_is_never_refused_and_never_reported() {
        let counts = AgentConcurrency::new();
        let slots: Vec<_> = (0..50)
            .map(|_| {
                counts
                    .try_enter(&id("chatty"), None)
                    .expect("no cap, no refusal")
            })
            .collect();
        assert_eq!(counts.in_flight(&id("chatty")), 50);
        assert!(
            counts.snapshot().is_empty(),
            "an agent with no declared cap is not a cap line"
        );
        drop(slots);
        assert_eq!(counts.in_flight(&id("chatty")), 0);
    }

    #[test]
    fn a_cap_refuses_the_invocation_past_it_and_admits_again_when_one_ends() {
        let counts = AgentConcurrency::new();
        let first = counts
            .try_enter(&id("builder"), Some(2))
            .expect("first fits");
        let second = counts
            .try_enter(&id("builder"), Some(2))
            .expect("second fits");
        assert!(
            counts.try_enter(&id("builder"), Some(2)).is_none(),
            "the third must not start while two are running"
        );
        drop(first);
        let third = counts
            .try_enter(&id("builder"), Some(2))
            .expect("a freed slot admits the next one");
        drop((second, third));
        assert_eq!(counts.in_flight(&id("builder")), 0);
    }

    /// The guard is the decrement, so the exit paths are covered by
    /// construction. A dropped slot inside a panicking scope is the
    /// hardest of them, and it is the same `Drop`.
    #[test]
    fn a_slot_is_released_even_when_its_scope_panics() {
        let counts = AgentConcurrency::new();
        let result = std::panic::catch_unwind({
            let counts = Arc::clone(&counts);
            move || {
                let _slot = counts.try_enter(&id("builder"), Some(1)).unwrap();
                panic!("the invocation task died");
            }
        });
        assert!(result.is_err());
        assert_eq!(counts.in_flight(&id("builder")), 0);
        assert!(counts.try_enter(&id("builder"), Some(1)).is_some());
    }

    /// A resume counts against the cap but is never refused by it: the
    /// work was already admitted, and blocking it would only keep the
    /// slot occupied longer.
    #[test]
    fn a_resume_counts_but_is_never_refused() {
        let counts = AgentConcurrency::new();
        let recovered = counts.enter(&id("builder"), Some(1));
        assert!(
            counts.try_enter(&id("builder"), Some(1)).is_none(),
            "a recovery-resumed run fills the cap for new triggers"
        );
        let also_recovered = counts.enter(&id("builder"), Some(1));
        assert_eq!(counts.in_flight(&id("builder")), 2);
        drop((recovered, also_recovered));
        assert!(counts.try_enter(&id("builder"), Some(1)).is_some());
    }

    #[test]
    fn the_snapshot_names_the_full_agents_and_what_is_waiting() {
        let counts = AgentConcurrency::new();
        let _full = counts.try_enter(&id("m0-issue-fix"), Some(1)).unwrap();
        let _held = counts.hold(&id("m0-issue-fix"));
        let _room = counts.try_enter(&id("doc-drift"), Some(4)).unwrap();
        let listed = counts.snapshot();
        assert_eq!(listed.len(), 1, "only the full agent is a line: {listed:?}");
        assert_eq!(listed[0].agent, "m0-issue-fix");
        assert_eq!(listed[0].in_flight, 1);
        assert_eq!(listed[0].cap, 1);
        assert_eq!(listed[0].held, 1);
        assert!(listed[0].is_at_cap());
        assert_eq!(listed[0].cap_summary(), "1/1 in flight, 1 trigger(s) held");
    }

    /// A reload that raises the cap is picked up by the next admission,
    /// because the cap travels with the call rather than being cached
    /// per agent.
    #[test]
    fn the_cap_is_whatever_the_caller_passes_this_time() {
        let counts = AgentConcurrency::new();
        let _one = counts.try_enter(&id("builder"), Some(1)).unwrap();
        assert!(counts.try_enter(&id("builder"), Some(1)).is_none());
        let _two = counts
            .try_enter(&id("builder"), Some(2))
            .expect("a raised cap admits the next trigger");
        assert_eq!(
            counts.snapshot()[0].cap,
            2,
            "and the report says the new one"
        );
    }

    #[test]
    fn a_quiet_agent_leaves_no_entry_behind() {
        let counts = AgentConcurrency::new();
        drop(counts.try_enter(&id("transient"), Some(1)));
        drop(counts.hold(&id("transient")));
        assert!(counts.snapshot().is_empty());
        assert_eq!(counts.in_flight(&id("transient")), 0);
    }
}
