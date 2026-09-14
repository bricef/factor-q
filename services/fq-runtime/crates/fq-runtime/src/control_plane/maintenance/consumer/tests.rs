//! Unit tests for the redelivery ledger — the pure half of the
//! at-least-once guard, exercised without a broker.

use super::*;

fn resolved(run_id: &str) -> Resolved {
    Resolved::just(MaintenanceRunPayload {
        task: "ping".to_string(),
        run_id: run_id.to_string(),
        outcome: MaintenanceOutcome::Succeeded {
            detail: "pong".to_string(),
        },
        duration_ms: 0,
    })
}

/// The property the ledger exists for: an id it has seen is never
/// absent, so the caller never reaches the "run it" path twice.
#[test]
fn a_started_run_is_never_reported_as_unseen() {
    let mut ledger = RunLedger::new(4);
    assert!(ledger.lookup("a").is_none(), "unseen before it starts");
    ledger.start("a");
    assert!(ledger.lookup("a").is_some(), "seen once it starts");
    ledger.resolved("a");
    assert!(
        ledger.lookup("a").expect("still seen").is_none(),
        "resolved, so nothing is pending"
    );
}

/// A held outcome comes back so the redelivery can publish it instead
/// of re-running the task.
#[test]
fn a_held_outcome_is_returned_to_the_redelivery() {
    let mut ledger = RunLedger::new(4);
    ledger.start("a");
    ledger.hold("a", resolved("a"));
    let held = ledger.lookup("a").expect("seen").expect("held");
    assert_eq!(held.payload.run_id, "a");
    ledger.resolved("a");
    assert!(
        ledger.lookup("a").expect("still seen").is_none(),
        "once published there is nothing left to re-publish"
    );
}

/// Eviction is oldest-first and bounded: the ids that could still be
/// redelivered are the ones kept.
#[test]
fn the_ledger_evicts_the_oldest_and_stays_within_capacity() {
    let mut ledger = RunLedger::new(2);
    ledger.start("a");
    ledger.start("b");
    ledger.start("c");
    assert!(ledger.lookup("a").is_none(), "oldest evicted");
    assert!(ledger.lookup("b").is_some());
    assert!(ledger.lookup("c").is_some());
    assert_eq!(ledger.entries.len(), 2);
    assert_eq!(ledger.order.len(), 2);
}

/// Starting an id twice must not push a second order entry — otherwise
/// the eviction queue and the map would drift apart.
#[test]
fn starting_a_known_id_twice_is_a_no_op() {
    let mut ledger = RunLedger::new(4);
    ledger.start("a");
    ledger.hold("a", resolved("a"));
    ledger.start("a");
    assert_eq!(ledger.order.len(), 1, "no duplicate order entry");
    assert!(
        ledger.lookup("a").expect("seen").is_some(),
        "a repeat start must not erase a held outcome"
    );
}
