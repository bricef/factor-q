//! The cap invariant as a property, not as seven examples (#718).
//!
//! The unit tests beside this file are seven points in a space; this is
//! the space. A random sequence of the six things that can happen to an
//! agent's invocations — a trigger arrives, one finishes, one defers,
//! one wakes, the daemon restarts, an operator resumes one — is played
//! against the real [`AgentConcurrency`], and four things are checked:
//!
//! 1. `admitted(agent) ≤ max_concurrent(agent)`, after every step — the
//!    headline. What this daemon *admits* never exceeds the cap.
//! 2. `in_flight(agent) ≤ max(cap, recovered(agent)) +
//!    operator_resumed(agent)`, after every step — the bound
//!    `agent_cap`'s module doc states. `in_flight` is exactly admitted +
//!    recovered + operator-resumed, so this is `admitted + recovered ≤
//!    max(cap, recovered)`: a restart resumes what it finds and admits
//!    nothing new until it is back under cap. It says nothing about an
//!    operator's resume beyond its being counted.
//! 3. At every trigger: an admitted one leaves at most `cap` in flight,
//!    and a refused one found `cap` or more there already, counting every
//!    invocation however it came in. This is the one that says an
//!    operator's resume, never held itself, still counts against the
//!    limit and holds the agent's triggers back (issue 1009).
//! 4. The count agrees with the model on every agent, every step, so
//!    none of the assertions above can be satisfied by a count that has
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
//! | operator resume | `fq-daemon`'s `resume::redrive` | `enter(agent, cap)` — counted, never refused |
//!
//! The `defer`/`resume` pair is the one this file exists for. Before the
//! review of <https://github.com/bricef/factor-q/pull/731>, `defer`
//! dropped the slot and `resume` took a fresh one through `enter`, which
//! refuses nothing — [`the_pre_fix_deferral_wiring_breaks_the_property`]
//! plays that spelling and watches the invariant fail, so the property
//! above is not vacuous, and
//! [`the_shrunk_deferral_burst_is_refused_at_the_cap`] replays the
//! counterexample the property shrank to against it.

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
    /// Which entry took the slot.
    entry: Entry,
}

/// The way a claim came in: gated, or counted without being refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    /// A trigger's `try_enter`: the cap agreed to it.
    Admitted,
    /// Startup recovery's `enter`, at a restart.
    Recovered,
    /// `fq invocation resume`'s `enter`, at any time (issue 1009).
    OperatorResumed,
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
    /// An operator resumes one of the agent's ambiguous invocations,
    /// which held no slot while it waited for triage.
    OperatorResume(usize),
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        4 => (0usize..FLEET.len()).prop_map(Step::Trigger),
        2 => ((0usize..FLEET.len()), 0usize..8).prop_map(|(a, i)| Step::Finish(a, i)),
        3 => ((0usize..FLEET.len()), 0usize..8).prop_map(|(a, i)| Step::Defer(a, i)),
        3 => ((0usize..FLEET.len()), 0usize..8).prop_map(|(a, i)| Step::Resume(a, i)),
        1 => Just(Step::Restart),
        1 => (0usize..FLEET.len()).prop_map(Step::OperatorResume),
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

    fn count(&self, agent: usize, entry: Entry) -> usize {
        let of = |pool: &HashMap<usize, Vec<Claim>>| {
            pool.get(&agent)
                .map_or(0, |c| c.iter().filter(|c| c.entry == entry).count())
        };
        of(&self.running) + of(&self.sleeping)
    }

    /// Everything this process was holding dies with it; recovery then
    /// counts back in what the WAL says is still in flight — an operator's
    /// resume included, which is an invocation in flight like any other.
    /// Counted, never gated — the documented over-cap path at a restart.
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
                    entry: Entry::Recovered,
                });
            }
        }
    }
}

/// Assertions 1, 2 and 4, after every single step.
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
        let recovered = fleet.count(agent, Entry::Recovered);
        let operator_resumed = fleet.count(agent, Entry::OperatorResumed);
        let admitted = fleet.count(agent, Entry::Admitted);
        prop_assert!(
            admitted <= cap as usize,
            "{id} admitted {admitted} invocations past a cap of {cap}"
        );
        prop_assert!(
            live <= (cap as usize).max(recovered) + operator_resumed,
            "{id} has {live} in flight against a cap of {cap} with only {recovered} recovered \
             and {operator_resumed} resumed by an operator"
        );
    }
    Ok(())
}

/// Make the call `step`'s production path makes, and check assertion 3 at
/// a trigger.
fn play(
    step: Step,
    counts: &std::sync::Arc<AgentConcurrency>,
    fleet: &mut Fleet,
) -> Result<(), TestCaseError> {
    match step {
        // The trigger path: gated, and refused when full. Full counts every
        // invocation in flight, an operator's resume included.
        Step::Trigger(agent) => {
            let (id, cap) = (agent_id(agent), FLEET[agent].1);
            let before = fleet.live(agent);
            match counts.try_enter(&id, cap) {
                Some(slot) => {
                    fleet.running.entry(agent).or_default().push(Claim {
                        _slot: slot,
                        entry: Entry::Admitted,
                    });
                    if let Some(cap) = cap {
                        let after = fleet.live(agent);
                        prop_assert!(
                            after <= cap as usize,
                            "{id} admitted a trigger to {after} in flight against a cap of {cap}"
                        );
                    }
                }
                None => {
                    prop_assert!(cap.is_some(), "{id} has no cap, yet a trigger was refused");
                    let cap = cap.unwrap_or_default();
                    prop_assert!(
                        before >= cap as usize,
                        "{id} refused a trigger with {before} in flight against a cap of {cap}"
                    );
                }
            }
        }
        // `handle` returns: completed, failed, drained, dropped or
        // panicked — all the same `Drop`.
        Step::Finish(agent, which) => {
            drop(Fleet::take(&mut fleet.running, agent, which));
        }
        // `conclude` hands the slot to the deferral queue. The invocation
        // sleeps; the claim does not.
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
        Step::Restart => fleet.restart(counts),
        // `redrive`: counted at once, whatever the cap says.
        Step::OperatorResume(agent) => {
            let (id, cap) = (agent_id(agent), FLEET[agent].1);
            fleet.running.entry(agent).or_default().push(Claim {
                _slot: counts.enter(&id, cap),
                entry: Entry::OperatorResumed,
            });
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// The module doc's four assertions, across any sequence of triggers,
    /// deferrals, resumes, completions, restarts and operator resumes.
    #[test]
    fn the_cap_holds_across_triggers_deferrals_resumes_and_restarts(
        steps in prop::collection::vec(step(), 1..40)
    ) {
        let counts = AgentConcurrency::new();
        let mut fleet = Fleet::default();
        for step in steps {
            play(step, &counts, &mut fleet)?;
            check(&fleet, &counts)?;
        }
    }
}

/// The counterexample this property shrank to against the pre-fix
/// deferral wiring, in the review of
/// <https://github.com/bricef/factor-q/pull/731>, replayed as written. Two
/// invocations of the cap-2 agent defer, one wakes, and the trigger that
/// arrives then must be refused: the sleeping one still holds its slot.
///
/// It used to be a persisted proptest seed. A seed is replayed through
/// the strategy that generated it, and the strategy has since gained a
/// step (an operator's resume, issue 1009), so the seed no longer
/// produced these steps. As an example it cannot drift that way.
#[test]
fn the_shrunk_deferral_burst_is_refused_at_the_cap() -> Result<(), TestCaseError> {
    let counts = AgentConcurrency::new();
    let mut fleet = Fleet::default();
    for step in [
        Step::Trigger(1),
        Step::Defer(1, 0),
        Step::Trigger(1),
        Step::Defer(1, 0),
        Step::Resume(1, 0),
        Step::Trigger(1),
        Step::Resume(1, 0),
    ] {
        play(step, &counts, &mut fleet)?;
        check(&fleet, &counts)?;
    }
    assert_eq!(
        fleet.count(1, Entry::Admitted),
        2,
        "the third trigger was refused while both were in flight"
    );
    assert_eq!(counts.in_flight(&agent_id(1)), 2);
    Ok(())
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
