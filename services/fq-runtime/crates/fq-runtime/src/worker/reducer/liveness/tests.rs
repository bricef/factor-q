//! The live registry's own invariants — pure, no broker, no store.

use super::*;

fn agent(name: &str) -> AgentId {
    AgentId::new(name).expect("agent id")
}

/// Liveness and identity are one answer: while a drive is entered the
/// registry names its agent, and the moment the guard drops it names
/// nothing — no phantom "live" marker survives the drive.
#[test]
fn a_drive_is_live_and_named_for_exactly_its_own_lifetime() {
    let live = LiveRegistry::default();
    let rounds = super::super::rounds::RoundLedger::default();
    let id = Uuid::now_v7();

    assert_eq!(live.agent_for(&id), None);
    assert!(!live.is_active(&id));

    {
        let _drive = live.enter(id, agent("researcher"), &rounds);
        assert!(live.is_active(&id));
        assert_eq!(
            live.agent_for(&id).map(AgentId::into_inner).as_deref(),
            Some("researcher")
        );
        // Concurrent drives are independent.
        let other = Uuid::now_v7();
        assert!(!live.is_active(&other));
    }

    assert!(!live.is_active(&id));
    assert_eq!(live.agent_for(&id), None);
}

/// A halt can only be armed against a live drive, is consumed once, and
/// never outlives the drive it was armed against — otherwise the next
/// drive of the same id (resume re-drives it) would suspend for no
/// reason.
#[test]
fn a_halt_needs_a_live_drive_and_dies_with_it() {
    let live = LiveRegistry::default();
    let rounds = super::super::rounds::RoundLedger::default();
    let id = Uuid::now_v7();

    // Nothing to halt: refused, and nothing recorded.
    assert!(!live.request_halt(id));
    assert!(!live.take_halt(id));

    {
        let _drive = live.enter(id, agent("researcher"), &rounds);
        assert!(live.request_halt(id));
        // Consumed once.
        assert!(live.take_halt(id));
        assert!(!live.take_halt(id));
        // Armed again, then abandoned by the drive ending.
        assert!(live.request_halt(id));
    }

    assert!(
        !live.take_halt(id),
        "a halt must not survive the drive it was armed against"
    );

    // A fresh drive of the same id starts clean.
    let _drive = live.enter(id, agent("researcher"), &rounds);
    assert!(!live.take_halt(id));
}
