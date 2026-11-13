//! The once-per-run decision, at its seam. The sweeper writes a hold's
//! line at error level exactly when [`ArchiveHolds::hold`] says the hold is
//! news, and at debug otherwise; what the line says is read off the real
//! daemon's log in `fq-daemon/tests/archive_holds.rs`.

use super::*;

fn too_large(limit_bytes: u64) -> ArchiveHoldReason {
    ArchiveHoldReason::PayloadTooLarge {
        event_type: "invocation_archived".to_string(),
        size_bytes: 1_252_461,
        limit_bytes,
    }
}

fn refused() -> ArchiveHoldReason {
    ArchiveHoldReason::StoredEventRefused {
        detail: "stored terminal event is unreadable".to_string(),
    }
}

/// A row is news the first time it is held, and not again for the same
/// reason: one error line per daemon run however many passes hold it.
#[test]
fn a_first_hold_is_news_and_the_same_hold_again_is_not() {
    let holds = ArchiveHolds::default();
    assert!(holds.hold("inv-1", "agent", refused()), "first: news");
    for _ in 0..3 {
        assert!(!holds.hold("inv-1", "agent", refused()), "again: not news");
    }
    assert!(holds.hold("inv-2", "agent", refused()), "another row: news");
    assert_eq!(holds.snapshot().len(), 2);
}

/// Held for something new (another limit, another event, another cause)
/// is news, so the line says what changed.
#[test]
fn a_hold_for_another_reason_is_news() {
    let holds = ArchiveHolds::default();
    assert!(holds.hold("inv-1", "agent", too_large(1_048_576)));
    assert!(
        holds.hold("inv-1", "agent", too_large(2_097_152)),
        "new limit"
    );
    assert!(holds.hold("inv-1", "agent", refused()), "new cause");
    let listed = holds.snapshot();
    assert_eq!(listed.len(), 1, "still one row: {listed:?}");
    assert_eq!(
        listed[0].reason,
        refused(),
        "listed under its latest reason"
    );
}

/// A row that went out and is held again later in the run is news again.
#[test]
fn a_released_row_held_again_is_news() {
    let holds = ArchiveHolds::default();
    assert!(holds.hold("inv-1", "agent", refused()));
    holds.release("inv-1");
    assert!(holds.snapshot().is_empty());
    assert!(holds.hold("inv-1", "agent", refused()));
}

/// Rows the store no longer lists are forgotten; the rest are kept.
#[test]
fn rows_no_longer_pending_are_forgotten() {
    let holds = ArchiveHolds::default();
    holds.hold("inv-1", "agent", refused());
    holds.hold("inv-2", "agent", refused());
    holds.retain_pending(&HashSet::from(["inv-2"]));
    let ids: Vec<String> = holds
        .snapshot()
        .into_iter()
        .map(|h| h.invocation_id)
        .collect();
    assert_eq!(ids, ["inv-2"]);
}

/// Only a size hold sits passes out, so only it reports a limit.
#[test]
fn only_a_size_hold_names_the_limit_it_was_refused_under() {
    let holds = ArchiveHolds::default();
    holds.hold("big", "agent", too_large(1_048_576));
    holds.hold("bad", "agent", refused());
    assert_eq!(holds.too_large_under("big"), Some(1_048_576));
    assert_eq!(holds.too_large_under("bad"), None);
    assert_eq!(holds.too_large_under("not-held"), None);
}
