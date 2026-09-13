//! The cap invariant as a property, not as seven examples (#718).
//!
//! The unit tests beside this file are seven points in a space; this is
//! the space. A random sequence of the five things that can happen to an
//! agent's invocations — a trigger arrives, one finishes, one defers,
//! one wakes, the daemon restarts — is played against the real
//! [`AgentConcurrency`], and the invariant is checked after **every**
//! step:
//!
//! 1. `admitted(agent) ≤ max_concurrent(agent)` — the headline. What
//!    this daemon *admits* never exceeds the cap.
//! 2. `in_flight(agent) ≤ max(cap, recovered(agent))` — the honest
//!    total, including the one over-cap path: a restart resumes what it
//!    finds and admits nothing new until it is back under cap.
//! 3. The count agrees with the model on every agent, every step, so
//!    neither assertion above can be satisfied by a count that has
//!    quietly lost track.
//!
//! # Fidelity: which call each step makes
//!
//! The point of the property is the *wiring*, so each step makes the
//! call the production path makes, and nothing else:
//!
//! | step | production site | call |
//! | --- | --- | --- |
//! | trigger | `dispatcher::admit_agent_slot` | `try_enter(agent, cap)` |
//! | finish/fail/drop | `dispatcher::handle` returns | drop the slot |
//! | defer | `dispatcher::conclude` → `DeferralQueue::defer` | move the slot into the queue |
//! | resume | `dispatcher::resume_deferred` | run on the slot the queue carried — **no entry** |
//! | restart | process death, then `recovery::spawn_resume_tasks` | drop everything, then `enter(...)` per recovered row |
//!
//! The `defer`/`resume` pair is the one this file exists for. Before the
//! review of <https://github.com/bricef/factor-q/pull/731>, `defer`
//! dropped the slot and `resume` took a fresh one through `enter`, which
//! refuses nothing — [`the_pre_fix_deferral_wiring_breaks_the_property`]
//! plays that spelling and watches the invariant fail, so the property
//! above is not vacuous.

use std::collections::HashMap;

use proptest::prelude::*;

use super::{AgentConcurrency, AgentSlot};
use crate::agent::AgentId;

/// Three agents: a build-bound one at 1, a middling one at 2, and an
/// LLM-bound one with no cap at all (the case that must stay free).
const FLEET: [(&str, Option<u32>); 3] = [
    ("m0-issue-fix", Some(1)),
    ("doc-drift", Some(2)),
    ("probe", None),
];

/// The fleet's ids, validated once — the count is keyed by [`AgentId`].
fn agent_id(index: usize) -> AgentId {
    AgentId::new(FLEET[index].0).expect("the fleet's ids are legal")
}

/// One live claim, and whether the cap ever agreed to it.
struct Claim {
    /// Held only for its `Drop`: giving the slot back *is* the
    /// invocation ending, so the field is never read.
    _slot: AgentSlot,
    /// `true` for a slot taken by startup recovery — the one entry that
    /// is counted rather than gated.
    recovered: bool,
}

/// What can happen to an agent's invocations. `usize` fields are
/// arbitrary and are taken modulo the live population, so every
/// generated sequence is executable.
#[derive(Debug, Clone, Copy)]
enum Step {
    Trigger(usize),
    Finish(usize, usize),
    Defer(usize, usize),
    Resume(usize, usize),
    Restart,
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        4 => (0usize..FLEET.len()).prop_map(Step::Trigger),
        2 => ((0usize..FLEET.len()), 0usize..8).prop_map(|(a, i)| Step::Finish(a, i)),
        3 => ((0usize..FLEET.len()), 0usize..8).prop_map(|(a, i)| Step::Defer(a, i)),
        3 => ((0usize..FLEET.len()), 0usize..8).prop_map(|(a, i)| Step::Resume(a, i)),
        1 => Just(Step::Restart),
    ]
}

/// The daemon as far as the cap can see it: what is running, and what is
/// asleep in the deferral queue holding its slot.
#[derive(Default)]
struct Fleet {
    running: HashMap<usize, Vec<Claim>>,
    sleeping: HashMap<usize, Vec<Claim>>,
}

impl Fleet {
    fn take(pool: &mut HashMap<usize, Vec<Claim>>, agent: usize, which: usize) -> Option<Claim> {
        let live = pool.get_mut(&agent)?;
        if live.is_empty() {
            return None;
        }
        Some(live.remove(which % live.len()))
    }

    fn live(&self, agent: usize) -> usize {
        self.running.get(&agent).map_or(0, Vec::len) + self.sleeping.get(&agent).map_or(0, Vec::len)
    }

    fn count(&self, agent: usize, recovered: bool) -> usize {
        let of = |pool: &HashMap<usize, Vec<Claim>>| {
            pool.get(&agent)
                .map_or(0, |c| c.iter().filter(|c| c.recovered == recovered).count())
        };
        of(&self.running) + of(&self.sleeping)
    }

    /// Everything this process was holding dies with it; recovery then
    /// counts back in what the WAL says is still in flight. Counted,
    /// never gated — the documented over-cap path.
    fn restart(&mut self, counts: &std::sync::Arc<AgentConcurrency>) {
        let mut found: Vec<(usize, usize)> = Vec::new();
        for agent in 0..FLEET.len() {
            found.push((agent, self.live(agent)));
        }
        self.running.clear();
        self.sleeping.clear();
        for (agent, in_flight) in found {
            let (id, cap) = (agent_id(agent), FLEET[agent].1);
            for _ in 0..in_flight {
                self.running.entry(agent).or_default().push(Claim {
                    _slot: counts.enter(&id, cap),
                    recovered: true,
                });
            }
        }
    }
}

/// The three assertions, after every single step.
fn check(fleet: &Fleet, counts: &AgentConcurrency) -> Result<(), TestCaseError> {
    for (agent, (id, cap)) in FLEET.iter().enumerate() {
        let live = fleet.live(agent);
        prop_assert_eq!(
            counts.in_flight(&agent_id(agent)) as usize,
            live,
            "the count and the fleet disagree about {}",
            id
        );
        let Some(cap) = *cap else { continue };
        let recovered = fleet.count(agent, true);
        let admitted = fleet.count(agent, false);
        prop_assert!(
            admitted <= cap as usize,
            "{id} admitted {admitted} invocations past a cap of {cap}"
        );
        prop_assert!(
            live <= (cap as usize).max(recovered),
            "{id} has {live} in flight against a cap of {cap} with only {recovered} recovered"
        );
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Never more than `max_concurrent` in flight per agent, across any
    /// sequence of triggers, deferrals, resumes, completions and
    /// restarts.
    #[test]
    fn the_cap_holds_across_triggers_deferrals_resumes_and_restarts(
        steps in prop::collection::vec(step(), 1..40)
    ) {
        let counts = AgentConcurrency::new();
        let mut fleet = Fleet::default();
        for step in steps {
            match step {
                // The trigger path: gated, and refused when full.
                Step::Trigger(agent) => {
                    let (id, cap) = (agent_id(agent), FLEET[agent].1);
                    if let Some(slot) = counts.try_enter(&id, cap) {
                        fleet.running.entry(agent).or_default().push(Claim { _slot: slot, recovered: false });
                    }
                }
                // `handle` returns: completed, failed, drained, dropped
                // or panicked — all the same `Drop`.
                Step::Finish(agent, which) => {
                    drop(Fleet::take(&mut fleet.running, agent, which));
                }
                // `conclude` hands the slot to the deferral queue. The
                // invocation sleeps; the claim does not.
                Step::Defer(agent, which) => {
                    if let Some(claim) = Fleet::take(&mut fleet.running, agent, which) {
                        fleet.sleeping.entry(agent).or_default().push(claim);
                    }
                }
                // `resume_deferred` runs on the slot the queue carried.
                Step::Resume(agent, which) => {
                    if let Some(claim) = Fleet::take(&mut fleet.sleeping, agent, which) {
                        fleet.running.entry(agent).or_default().push(claim);
                    }
                }
                Step::Restart => fleet.restart(&counts),
            }
            check(&fleet, &counts)?;
        }
    }
}

/// The witness that the property above is not vacuous.
///
/// This is the shape the code had before the review of
/// <https://github.com/bricef/factor-q/pull/731>: a deferral released
/// the slot (`handle` returned after `conclude` queued the resume) and
/// the resume took a fresh one through [`AgentConcurrency::enter`],
/// which is documented as never refused. The four steps below are one
/// `max_concurrent: 1` agent ending up with two invocations running —
/// the outcome the cap exists to prevent, reached through the throttle
/// instead of through the trigger path.
///
/// Kept as a test rather than deleted with the bug, so that a change
/// which re-introduces an ungated entry on the deferral path has
/// something here that says what it costs.
#[test]
fn the_pre_fix_deferral_wiring_breaks_the_property() {
    let counts = AgentConcurrency::new();
    let builder = agent_id(0);
    // One invocation of a cap-1 agent is running.
    let running = counts.try_enter(&builder, Some(1)).expect("the first fits");
    // Its model 429s, so it defers. Pre-fix, `handle` returned and the
    // slot went with it — the sleeping invocation stopped counting.
    drop(running);
    // The held trigger behind it sees a free slot and starts.
    let _next = counts
        .try_enter(&builder, Some(1))
        .expect("the cap thinks the agent is idle");
    // The pause lifts and the deferred invocation resumes, through the
    // entry that refuses nothing.
    let _resumed = counts.enter(&builder, Some(1));
    assert_eq!(
        counts.in_flight(&builder),
        2,
        "two invocations of a max_concurrent: 1 agent — what carrying the slot across the \
         deferral prevents"
    );
}
