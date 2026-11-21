//! The archive sweeper's holds, read onto the invocation list and detail.

use super::*;
use fq_ops::health::ArchiveHoldReason;

fn row(id: &str, status: &str, archive_state: InvocationArchiveState) -> InvocationSummaryView {
    InvocationSummaryView {
        invocation_id: id.to_string(),
        agent_id: Some("researcher".to_string()),
        worker_id: "worker".to_string(),
        status: status.to_string(),
        assigned_at_ms: 0,
        started_at_ms: 0,
        archived: false,
        archive_state,
        ended_by: None,
        ended_at_ms: None,
        termination: None,
        summary: None,
    }
}

fn hold(id: &str) -> ArchiveHold {
    ArchiveHold {
        invocation_id: id.to_string(),
        agent_id: "researcher".to_string(),
        reason: ArchiveHoldReason::PayloadTooLarge {
            event_type: "invocation_archived".to_string(),
            size_bytes: 2_000_000,
            limit_bytes: 1_048_576,
        },
        remedy: "raise the broker's max_payload".to_string(),
    }
}

/// Only a row the store says is pending is marked held: a run the
/// sweeper holds has ended, and a row that reads otherwise is the
/// store's word, not the sweeper's.
#[test]
fn a_pending_row_the_sweeper_holds_reads_held_and_no_other() {
    let mut items = vec![
        row("held", "failed", InvocationArchiveState::Pending),
        row("pending", "completed", InvocationArchiveState::Pending),
        row("archived", "completed", InvocationArchiveState::Archived),
        row("running", "in_flight", InvocationArchiveState::NotDue),
    ];
    let holds = [hold("held"), hold("archived"), hold("running")];
    mark_held(&mut items, &holds);
    let states: Vec<_> = items.iter().map(|item| item.archive_state).collect();
    assert_eq!(
        states,
        [
            InvocationArchiveState::Held,
            InvocationArchiveState::Pending,
            InvocationArchiveState::Archived,
            InvocationArchiveState::NotDue,
        ]
    );
}

fn detail(owner: Option<InvocationSummaryView>) -> InvocationDetailView {
    InvocationDetailView {
        invocation_id: "held".to_string(),
        agent_id: Some("researcher".to_string()),
        owner,
        archive: None,
        live: None,
        recent_events: Vec::new(),
        has_transcript: false,
        summary: None,
        cost: None,
        trigger: None,
        archive_hold: None,
    }
}

#[test]
fn the_detail_of_a_held_run_carries_the_hold() {
    let mut held = detail(Some(row("held", "failed", InvocationArchiveState::Pending)));
    mark_held_detail(&mut held, &[hold("held")]);
    assert_eq!(
        held.owner.map(|owner| owner.archive_state),
        Some(InvocationArchiveState::Held)
    );
    assert_eq!(held.archive_hold, Some(hold("held")));

    let mut unheld = detail(Some(row("held", "failed", InvocationArchiveState::Pending)));
    mark_held_detail(&mut unheld, &[hold("another")]);
    assert_eq!(unheld.archive_hold, None);

    let mut no_row = detail(None);
    mark_held_detail(&mut no_row, &[hold("held")]);
    assert_eq!(no_row.archive_hold, None, "no row, nothing to mark");
}
