//! The transition table, cell by cell, and the properties every sequence
//! of events keeps.

use super::*;
use crate::control_plane::store::EndedBy;
use proptest::prelude::*;

/// Every state a row can hold, `None` for no row: each pair, with each
/// ender it admits, with and without a request on record.
fn states() -> Vec<Option<RowState>> {
    let mut states = vec![None];
    for status in [
        OwnerStatus::InFlight,
        OwnerStatus::Ambiguous,
        OwnerStatus::Completed,
        OwnerStatus::Failed,
    ] {
        for archive in [
            ArchiveState::NotDue,
            ArchiveState::Pending,
            ArchiveState::Archived,
            ArchiveState::WillNotCome,
        ] {
            for ended_by in [None, Some(EndedBy::Run), Some(EndedBy::Drop)] {
                for requested in [false, true] {
                    let state = RowState {
                        status,
                        archive,
                        ended_by,
                        requested,
                    };
                    if state.is_valid() {
                        states.push(Some(state));
                    }
                }
            }
        }
    }
    states
}

/// The states: two that have not ended, and, of the seven pairs a run
/// that ended can hold, two `completed` ones with no ender or the run, and
/// three `failed` ones with no ender, the run or a drop, each with and
/// without a request.
#[test]
fn the_states_are_every_one_a_row_can_hold() {
    assert_eq!(states().len(), 1 + 2 + 2 * 2 * 2 + 3 * 3 * 2);
}

/// What a cell of the table says, as the module doc writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cell {
    Ignored,
    /// A move to the pair, recording `ender`; `None` keeps the row's.
    To(&'static str, Option<EndedBy>),
    Duplicate,
    Refused,
    Conflict,
    /// A conflict, and the archive's arrival still moves the row to the
    /// pair, its ender and request kept.
    ConflictThen(&'static str),
    /// The request recorded and nothing else changed, or a duplicate where
    /// one already is.
    Request,
}

fn pair(state: RowState) -> &'static str {
    match (state.status, state.archive) {
        (OwnerStatus::InFlight, ArchiveState::NotDue) => "in_flight",
        (OwnerStatus::Ambiguous, ArchiveState::NotDue) => "ambiguous",
        (OwnerStatus::Completed, ArchiveState::Pending) => "completed·pending",
        (OwnerStatus::Completed, ArchiveState::Archived) => "completed·archived",
        (OwnerStatus::Failed, ArchiveState::Pending) => "failed·pending",
        (OwnerStatus::Failed, ArchiveState::Archived) => "failed·archived",
        (OwnerStatus::Failed, ArchiveState::WillNotCome) => "failed·none",
        other => panic!("not a pair a row can hold: {other:?}"),
    }
}

/// The table's column for a state, in the order [`expected`] lists them:
/// no row, in_flight, ambiguous, completed·pending, completed·archived,
/// failed·pending, failed·archived, failed·none, then failed·pending,
/// failed·none and failed·archived by a drop.
fn column(state: Option<RowState>) -> usize {
    let Some(state) = state else {
        return 0;
    };
    let by_a_drop = state.ended_by == Some(EndedBy::Drop);
    match (pair(state), by_a_drop) {
        ("in_flight", _) => 1,
        ("ambiguous", _) => 2,
        ("completed·pending", _) => 3,
        ("completed·archived", _) => 4,
        ("failed·pending", false) => 5,
        ("failed·archived", false) => 6,
        ("failed·none", false) => 7,
        ("failed·pending", true) => 8,
        ("failed·none", true) => 9,
        ("failed·archived", true) => 10,
        other => panic!("no column for {other:?}"),
    }
}

/// The table in the module doc, row by row, column by column.
fn expected(event: OwnershipEvent) -> [Cell; 11] {
    use Cell::{Conflict, ConflictThen, Duplicate, Ignored, Refused, Request, To};
    use EndedBy::{Drop, Run};
    use OwnershipEvent as E;
    let completed_pending = To("completed·pending", Some(Run));
    let failed_pending = To("failed·pending", Some(Run));
    let completed_archived = To("completed·archived", Some(Run));
    match event {
        E::Triggered | E::ArchiveAcked | E::OperatorResumed | E::SetupFailed => [Ignored; 11],
        E::DurablyStarted => [
            To("in_flight", None),
            Duplicate,
            Refused,
            Refused,
            Refused,
            Refused,
            Refused,
            Refused,
            Refused,
            Refused,
            Refused,
        ],
        E::Restored => [
            To("in_flight", None),
            To("in_flight", None),
            To("in_flight", None),
            Refused,
            Refused,
            Refused,
            Refused,
            Refused,
            Refused,
            Refused,
            Refused,
        ],
        E::Ambiguous => [
            To("ambiguous", None),
            To("ambiguous", None),
            Duplicate,
            Refused,
            Refused,
            Refused,
            Refused,
            Refused,
            Refused,
            Refused,
            Refused,
        ],
        E::Ended(Ending::Completed) => [
            completed_pending,
            completed_pending,
            completed_pending,
            Duplicate,
            Duplicate,
            Conflict,
            Conflict,
            completed_pending,
            completed_pending,
            completed_pending,
            Conflict,
        ],
        E::Ended(Ending::Failed) => [
            failed_pending,
            failed_pending,
            failed_pending,
            Conflict,
            Conflict,
            Duplicate,
            Duplicate,
            failed_pending,
            failed_pending,
            failed_pending,
            Duplicate,
        ],
        E::Archived(Ending::Completed) => [
            completed_archived,
            completed_archived,
            completed_archived,
            To("completed·archived", None),
            Duplicate,
            ConflictThen("failed·archived"),
            Conflict,
            completed_archived,
            completed_archived,
            completed_archived,
            Conflict,
        ],
        E::Archived(Ending::Failed) => [
            To("failed·archived", Some(Run)),
            To("failed·archived", Some(Run)),
            To("failed·archived", Some(Run)),
            ConflictThen("completed·archived"),
            Conflict,
            To("failed·archived", None),
            Duplicate,
            To("failed·archived", None),
            To("failed·archived", None),
            To("failed·archived", None),
            Duplicate,
        ],
        E::TerminationRequested(ArchiveExpected::Yes) => [
            To("failed·pending", Some(Drop)),
            To("failed·pending", Some(Drop)),
            To("failed·pending", Some(Drop)),
            Request,
            Request,
            Request,
            Request,
            Request,
            Request,
            Request,
            Request,
        ],
        E::TerminationRequested(ArchiveExpected::No) => [
            To("failed·none", Some(Drop)),
            To("failed·none", Some(Drop)),
            To("failed·none", Some(Drop)),
            Request,
            Request,
            Request,
            Request,
            Request,
            Request,
            Request,
            Request,
        ],
    }
}

/// Whether `decision`, for `event` from `from`, is what `cell` says. A
/// move keeps the row's request, and a drop records one.
fn holds(from: Option<RowState>, event: OwnershipEvent, decision: Decision, cell: Cell) -> bool {
    let requested = from.is_some_and(|state| state.requested);
    let ender = from.and_then(|state| state.ended_by);
    let drop = matches!(event, OwnershipEvent::TerminationRequested(_));
    match (cell, decision) {
        (Cell::Ignored, Decision::Ignore)
        | (Cell::Duplicate, Decision::Duplicate)
        | (Cell::Refused, Decision::Refused)
        | (Cell::Conflict, Decision::Conflict { then: None }) => true,
        (Cell::To(to_pair, by), Decision::Move { to, .. }) => {
            pair(to) == to_pair
                && to.ended_by == by.or(ender)
                && to.requested == (requested || drop)
        }
        (
            Cell::ConflictThen(to_pair),
            Decision::Conflict {
                then: Some((to, _)),
            },
        ) => pair(to) == to_pair && to.ended_by == ender && to.requested == requested,
        (Cell::Request, Decision::Duplicate) => requested,
        (Cell::Request, Decision::Move { to, .. }) => {
            !requested && Some(to) == from.map(RowState::with_request)
        }
        _ => false,
    }
}

/// Which `failed` is an ending, phase by phase: `setup` alone is not (the
/// run never made its first WAL write), every other phase is.
#[test]
fn a_failed_is_an_ending_in_every_phase_but_setup() {
    use crate::events::FailurePhase as P;
    for (phase, expected) in [
        (P::Setup, OwnershipEvent::SetupFailed),
        (P::Reducer, OwnershipEvent::Ended(Ending::Failed)),
        (P::HostStepBudget, OwnershipEvent::Ended(Ending::Failed)),
        (P::Budget, OwnershipEvent::Ended(Ending::Failed)),
        (P::LlmRequest, OwnershipEvent::Ended(Ending::Failed)),
        (P::LlmResponse, OwnershipEvent::Ended(Ending::Failed)),
        (P::ToolCall, OwnershipEvent::Ended(Ending::Failed)),
        (P::ToolResult, OwnershipEvent::Ended(Ending::Failed)),
    ] {
        assert_eq!(OwnershipEvent::of_failed(phase), expected, "{phase:?}");
    }
}

/// Every event from every state, against the table.
#[test]
fn every_event_from_every_state_does_what_the_table_says() {
    let mut cells = 0;
    for event in OwnershipEvent::ALL {
        let row = expected(event);
        for state in states() {
            let cell = row[column(state)];
            let decision = transition(state, event);
            assert!(
                holds(state, event, decision, cell),
                "{event:?} from {state:?}: decided {decision:?}, the table says {cell:?}"
            );
            cells += 1;
        }
    }
    assert_eq!(cells, 29 * 13, "the table covers every state and event");
}

/// Who the row names after each move: a move keeps the row's worker
/// unless it is an archive's arrival (the worker's own id) or recovery's
/// restore (this daemon, from the run's start). A durable start never
/// moves a row: it only creates one, which takes the start's worker and
/// time ([`a_durable_start_creates_a_row_only_where_there_is_none`]).
#[test]
fn a_move_keeps_the_rows_worker_but_for_an_archive_and_a_restore() {
    for event in OwnershipEvent::ALL {
        for state in states().into_iter().flatten() {
            let owner = match transition(Some(state), event) {
                Decision::Move { owner, .. } => owner,
                Decision::Conflict {
                    then: Some((_, owner)),
                } => owner,
                _ => continue,
            };
            let want = match event {
                OwnershipEvent::Archived(_) => Owner::TakeWorker,
                OwnershipEvent::Restored => Owner::TakeBoth,
                _ => Owner::Keep,
            };
            assert_eq!(owner, want, "{event:?} from {state:?}");
        }
    }
}

/// **A durable start creates a row only where there is none**, owned by
/// the daemon and from the time the start names; every row already there
/// is left as it is.
#[test]
fn a_durable_start_creates_a_row_only_where_there_is_none() {
    assert_eq!(
        transition(None, OwnershipEvent::DurablyStarted),
        Decision::Move {
            to: RowState::IN_FLIGHT,
            owner: Owner::TakeBoth,
        }
    );
    for state in states().into_iter().flatten() {
        let decision = transition(Some(state), OwnershipEvent::DurablyStarted);
        assert!(
            matches!(decision, Decision::Duplicate | Decision::Refused),
            "{state:?} answers {decision:?}"
        );
        assert_eq!(
            after(Some(state), OwnershipEvent::DurablyStarted),
            Some(state)
        );
    }
}

/// The row after `event`, from `current`.
fn after(current: Option<RowState>, event: OwnershipEvent) -> Option<RowState> {
    match transition(current, event) {
        Decision::Move { to, .. }
        | Decision::Conflict {
            then: Some((to, _)),
        } => Some(to),
        Decision::Ignore
        | Decision::Duplicate
        | Decision::Refused
        | Decision::Conflict { then: None } => current,
    }
}

fn event() -> impl Strategy<Value = OwnershipEvent> {
    (0..OwnershipEvent::ALL.len()).prop_map(|i| OwnershipEvent::ALL[i])
}

fn state() -> impl Strategy<Value = Option<RowState>> {
    let states = states();
    (0..states.len()).prop_map(move |i| states[i])
}

proptest! {
    /// Whatever events arrive, in whatever order and however often, from
    /// whatever state the row starts in, a row from before the termination
    /// record included.
    #[test]
    fn every_sequence_of_events_keeps_the_row_true(
        start in state(),
        events in proptest::collection::vec(event(), 0..24),
    ) {
        let mut row = start;
        for event in events {
            let next = after(row, event);
            let twice = after(next, event);
            prop_assert_eq!(twice, next, "{:?} applied twice is applied once", event);
            if let Some(state) = next {
                prop_assert!(state.is_valid(), "{:?} is not a state a row can hold", state);
            }
            if row.is_some() {
                prop_assert!(next.is_some(), "no event removes a row");
            }
            if matches!(
                event,
                OwnershipEvent::SetupFailed
                    | OwnershipEvent::Triggered
                    | OwnershipEvent::ArchiveAcked
                    | OwnershipEvent::OperatorResumed
            ) {
                prop_assert_eq!(next, row, "{:?} never writes or moves a row", event);
            }
            if row.is_some_and(|state| state.requested) {
                prop_assert!(
                    next.is_some_and(|state| state.requested),
                    "a request on record stays on record"
                );
            }
            if let Some(state) = next
                && state.ending().is_some()
                && row.is_none_or(|state| state.ending().is_none())
            {
                prop_assert!(state.ended_by.is_some(), "an ending this table records names its ender");
            }
            if let Some(before) = row
                && let Some(ending) = before.ending()
            {
                let next = next.expect("a row stays");
                prop_assert!(next.ending().is_some(), "a finished run never goes back in progress");
                if !before.is_provisional() {
                    prop_assert_eq!(
                        (next.ending(), next.ended_by),
                        (Some(ending), before.ended_by),
                        "only a provisional ending is replaced"
                    );
                }
                if next.ending() != Some(ending) || next.ended_by != before.ended_by {
                    prop_assert_eq!(
                        next.ended_by,
                        Some(EndedBy::Run),
                        "what replaces a provisional ending is the run's own"
                    );
                }
            }
            if row.map(|state| state.archive) == Some(ArchiveState::Archived) {
                prop_assert_eq!(
                    next.map(|state| state.archive),
                    Some(ArchiveState::Archived),
                    "an archive written stays written"
                );
            }
            if let OwnershipEvent::TerminationRequested(expected) = event {
                let want = match row {
                    Some(state) if state.ending().is_some() => state.with_request(),
                    _ => RowState::dropped(expected),
                };
                prop_assert_eq!(
                    next,
                    Some(want),
                    "a drop ends a run not ended, and only records its request on one that has"
                );
            }
            row = next;
        }
    }

    /// A run whose only failure is in `setup` never gets a row, whatever
    /// else arrives that does not concern the row.
    #[test]
    fn a_setup_failure_never_creates_a_row(
        events in proptest::collection::vec(
            prop_oneof![
                Just(OwnershipEvent::SetupFailed),
                Just(OwnershipEvent::Triggered),
                Just(OwnershipEvent::ArchiveAcked),
                Just(OwnershipEvent::OperatorResumed),
            ],
            1..12,
        )
    ) {
        let row = events.into_iter().fold(None, after);
        prop_assert_eq!(row, None);
    }
}
