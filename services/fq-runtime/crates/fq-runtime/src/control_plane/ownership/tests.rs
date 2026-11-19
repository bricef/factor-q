//! The transition table, cell by cell, and the properties every sequence
//! of events keeps.

use super::*;
use proptest::prelude::*;

/// Every state a row can be in, `None` for no row.
const STATES: [Option<RowState>; 7] = [
    None,
    Some(RowState::IN_FLIGHT),
    Some(RowState::AMBIGUOUS),
    Some(RowState {
        status: OwnerStatus::Completed,
        archive: ArchiveState::Pending,
    }),
    Some(RowState {
        status: OwnerStatus::Completed,
        archive: ArchiveState::Archived,
    }),
    Some(RowState {
        status: OwnerStatus::Failed,
        archive: ArchiveState::Pending,
    }),
    Some(RowState {
        status: OwnerStatus::Failed,
        archive: ArchiveState::Archived,
    }),
];

/// What a cell of the table says, as the module doc writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cell {
    Ignored,
    To(&'static str),
    Duplicate,
    Refused,
    Conflict,
    ConflictThen(&'static str),
}

fn short(state: RowState) -> &'static str {
    match (state.status, state.archive) {
        (OwnerStatus::InFlight, ArchiveState::NotDue) => "in_flight",
        (OwnerStatus::Ambiguous, ArchiveState::NotDue) => "ambiguous",
        (OwnerStatus::Completed, ArchiveState::Pending) => "completed·pending",
        (OwnerStatus::Completed, ArchiveState::Archived) => "completed·archived",
        (OwnerStatus::Failed, ArchiveState::Pending) => "failed·pending",
        (OwnerStatus::Failed, ArchiveState::Archived) => "failed·archived",
        other => panic!("not a state a row can hold: {other:?}"),
    }
}

fn cell(decision: Decision) -> Cell {
    match decision {
        Decision::Ignore => Cell::Ignored,
        Decision::Move { to, .. } => Cell::To(short(to)),
        Decision::Duplicate => Cell::Duplicate,
        Decision::Refused => Cell::Refused,
        Decision::Conflict { then: None } => Cell::Conflict,
        Decision::Conflict {
            then: Some((to, _)),
        } => Cell::ConflictThen(short(to)),
    }
}

/// The table in the module doc, row by row, in the order of [`STATES`]:
/// no row, in_flight, ambiguous, completed·pending, completed·archived,
/// failed·pending, failed·archived.
fn expected(event: OwnershipEvent) -> [Cell; 7] {
    use Cell::{Conflict, ConflictThen, Duplicate, Ignored, Refused, To};
    use OwnershipEvent as E;
    match event {
        E::Triggered | E::ArchiveAcked | E::OperatorResumed | E::SetupFailed => [Ignored; 7],
        E::DurablyStarted => [
            To("in_flight"),
            Duplicate,
            Refused,
            Refused,
            Refused,
            Refused,
            Refused,
        ],
        E::Restored => [
            To("in_flight"),
            To("in_flight"),
            To("in_flight"),
            Refused,
            Refused,
            Refused,
            Refused,
        ],
        E::Ambiguous => [
            To("ambiguous"),
            To("ambiguous"),
            Duplicate,
            Refused,
            Refused,
            Refused,
            Refused,
        ],
        E::Ended(Ending::Completed) => [
            To("completed·pending"),
            To("completed·pending"),
            To("completed·pending"),
            Duplicate,
            Duplicate,
            Conflict,
            Conflict,
        ],
        E::Ended(Ending::Failed) => [
            To("failed·pending"),
            To("failed·pending"),
            To("failed·pending"),
            Conflict,
            Conflict,
            Duplicate,
            Duplicate,
        ],
        E::Archived(Ending::Completed) => [
            To("completed·archived"),
            To("completed·archived"),
            To("completed·archived"),
            To("completed·archived"),
            Duplicate,
            ConflictThen("failed·archived"),
            Conflict,
        ],
        E::Archived(Ending::Failed) => [
            To("failed·archived"),
            To("failed·archived"),
            To("failed·archived"),
            ConflictThen("completed·archived"),
            Conflict,
            To("failed·archived"),
            Duplicate,
        ],
        E::OperatorEnded(Ending::Failed) => [
            To("failed·archived"),
            To("failed·archived"),
            To("failed·archived"),
            To("failed·archived"),
            To("failed·archived"),
            To("failed·archived"),
            Duplicate,
        ],
        E::OperatorEnded(Ending::Completed) => [
            To("completed·archived"),
            To("completed·archived"),
            To("completed·archived"),
            To("completed·archived"),
            Duplicate,
            To("completed·archived"),
            To("completed·archived"),
        ],
    }
}

/// Every event from every state, against the table.
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

#[test]
fn every_event_from_every_state_does_what_the_table_says() {
    let mut cells = 0;
    for event in OwnershipEvent::ALL {
        for (state, want) in STATES.iter().zip(expected(event)) {
            assert_eq!(
                cell(transition(*state, event)),
                want,
                "{event:?} from {:?}",
                state.map(short)
            );
            cells += 1;
        }
    }
    assert_eq!(cells, 7 * 13, "the table covers every cell");
}

/// Who the row names after each move: a move keeps the row's worker
/// unless it is an archive's arrival (the worker's own id) or recovery's
/// restore (this daemon, from the run's start). A durable start never
/// moves a row: it only creates one, which takes the start's worker and
/// time ([`a_durable_start_creates_a_row_only_where_there_is_none`]).
#[test]
fn a_move_keeps_the_rows_worker_but_for_an_archive_and_a_restore() {
    for event in OwnershipEvent::ALL {
        for state in STATES.into_iter().flatten() {
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
            assert_eq!(owner, want, "{event:?} from {}", short(state));
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
    for state in STATES.into_iter().flatten() {
        let decision = transition(Some(state), OwnershipEvent::DurablyStarted);
        assert!(
            matches!(decision, Decision::Duplicate | Decision::Refused),
            "{} answers {decision:?}",
            short(state)
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

proptest! {
    /// Whatever events arrive, in whatever order and however often.
    #[test]
    fn every_sequence_of_events_keeps_the_row_true(events in proptest::collection::vec(event(), 0..24)) {
        let mut row: Option<RowState> = None;
        for event in events {
            let next = after(row, event);
            let twice = after(next, event);
            prop_assert_eq!(twice, next, "{:?} applied twice is applied once", event);
            if let Some(state) = next {
                prop_assert!(state.is_valid(), "{:?} is not a pair a row can hold", state);
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
            if let Some(ending) = row.and_then(RowState::ending) {
                let after_ending = next.and_then(RowState::ending);
                prop_assert!(after_ending.is_some(), "a finished run never goes back in progress");
                if !matches!(event, OwnershipEvent::OperatorEnded(_)) {
                    prop_assert_eq!(after_ending, Some(ending), "only an operator changes an ending");
                }
            }
            if row.map(|state| state.archive) == Some(ArchiveState::Archived) {
                prop_assert_eq!(
                    next.map(|state| state.archive),
                    Some(ArchiveState::Archived),
                    "an archive written stays written"
                );
            }
            if let OwnershipEvent::OperatorEnded(ending) = event {
                prop_assert_eq!(next, Some(RowState::ended(ending, ArchiveState::Archived)));
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
