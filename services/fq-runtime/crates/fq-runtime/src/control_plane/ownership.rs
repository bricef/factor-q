//! The ownership row's lifecycle, written down once (issue 1024).
//!
//! The control plane keeps one ownership row per run,
//! `coordination_invocation_owner`, which `fq invocation list`, the
//! recovery views and startup recovery read. Every move of it is decided
//! by [`transition`], a pure function from the row's state and one event
//! to a [`Decision`]. It is the first small instance of the explicit
//! lifecycles issue 1067 proposes.
//!
//! ## The row's states
//!
//! `status` is `in_flight`, `ambiguous`, `completed` or `failed`, and
//! `archive_state` ([`ArchiveState`]) records beside it whether the control
//! plane holds the run's archive record: `not_due` while the run has not
//! ended, `pending` once it has and until the record is written,
//! `archived` after. So the pairs a row can hold are `in_flight·not_due`,
//! `ambiguous·not_due`, `completed·pending`, `completed·archived`,
//! `failed·pending` and `failed·archived` ([`RowState::is_valid`]).
//!
//! There is also no row at all, which is where a triggered run still
//! running stands: its row is written when it ends, or when something
//! else happens to it first (recovery restores it, it is reported
//! ambiguous, an operator drops it). Whether a run should have a row from
//! its durable start is issue 721's question; it would be one more writer
//! of the `no row → in_flight` edge below.
//!
//! Why the archive sweeper holds a `pending` row is not stored. A hold is
//! one daemon run's verdict, kept in the worker's memory and offered again
//! at the next start, so the daemon reads it where it builds a report.
//!
//! ## The transitions
//!
//! | event \ from | no row | in_flight | ambiguous | completed·pending | completed·archived | failed·pending | failed·archived |
//! |---|---|---|---|---|---|---|---|
//! | `triggered`, `archive_acked`, `operator_resumed` | ignored | ignored | ignored | ignored | ignored | ignored | ignored |
//! | `failed` in `setup` | ignored | ignored | ignored | ignored | ignored | ignored | ignored |
//! | recovery restores the run | in_flight | in_flight | in_flight | refused | refused | refused | refused |
//! | `ambiguous` | ambiguous | ambiguous | duplicate | refused | refused | refused | refused |
//! | `completed` | completed·pending | completed·pending | completed·pending | duplicate | duplicate | conflict | conflict |
//! | `failed`, not in `setup` | failed·pending | failed·pending | failed·pending | conflict | conflict | duplicate | duplicate |
//! | `archived`, phase `completed` | completed·archived | completed·archived | completed·archived | completed·archived | duplicate | conflict: failed·archived | conflict |
//! | `archived`, phase `failed` | failed·archived | failed·archived | failed·archived | conflict: completed·archived | conflict | failed·archived | duplicate |
//! | operator's drop, phase `failed` | failed·archived | failed·archived | failed·archived | failed·archived | failed·archived | failed·archived | duplicate |
//! | operator's recovery, phase `completed` | completed·archived | completed·archived | completed·archived | completed·archived | duplicate | completed·archived | completed·archived |
//!
//! - **Ignored**: the event never touches the row. `triggered` is
//!   published before the run's first WAL write, also by a run refused
//!   before it starts. A `failed` in `setup` is published only for a run
//!   that never made its first WAL write: the runner stamps that phase
//!   from the WAL row itself, in one place
//!   (`worker/reducer/runner/publish_failure.rs`), and its tests refuse
//!   every publish of a run in turn to keep it so. A row is what startup
//!   recovery, the dispatcher's own-claim arm and the dead-letter watch
//!   read as "the run started" (`RunRecords`), so a row written for such
//!   a run would mark its trigger's claim started, and drop every later
//!   copy of the trigger.
//! - **Refused**: the row records the run finished, and the event would
//!   put it back in progress. A report of ambiguity, or recovery's
//!   restore, can arrive after the run's ending; the ending stands.
//! - **Conflict**: an ending that contradicts the one recorded, so two
//!   endings were published for one run. The first stands; an archive's
//!   arrival is still recorded, as the cell after `conflict:` says.
//! - **Duplicate**: the row already says what the event says, as
//!   at-least-once delivery makes ordinary.
//! - **An operator's drop wins** from every state, as it always has, and
//!   can turn a finished run to `failed`. What that leaves inconsistent in
//!   the archive record is issue 1070's.
//!
//! Who a row names: a row an event creates takes the event's worker id
//! (the worker's own on `invocation.archived` and on recovery's restore,
//! the agent id standing in for it on every other event) and time. A move
//! keeps the row's worker id and time, with two exceptions:
//! `invocation.archived` brings the worker's real id ([`Owner::TakeWorker`]),
//! and recovery's restore re-owns the run for this daemon from the run's
//! start ([`Owner::TakeBoth`]).

use super::store::{ArchiveState, OwnerRow, OwnerStatus};

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    Completed,
    Failed,
}

impl Ending {
    /// The ending `invocation.archived`'s `final_phase` names: `failed` is
    /// a failure and anything else a completion, as the archive hand-off
    /// has always read it.
    pub fn of_archived_phase(final_phase: &str) -> Self {
        if final_phase == "failed" {
            Ending::Failed
        } else {
            Ending::Completed
        }
    }

    /// The ending an operator's `final_phase` names: `completed`, or a
    /// failure for anything else, so an audit trail is kept.
    pub fn of_operator_phase(final_phase: &str) -> Self {
        if final_phase == "completed" {
            Ending::Completed
        } else {
            Ending::Failed
        }
    }

    pub fn status(self) -> OwnerStatus {
        match self {
            Ending::Completed => OwnerStatus::Completed,
            Ending::Failed => OwnerStatus::Failed,
        }
    }
}

/// A row's state: its status and its archive's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowState {
    pub status: OwnerStatus,
    pub archive: ArchiveState,
}

impl RowState {
    /// A run in progress.
    pub const IN_FLIGHT: Self = Self {
        status: OwnerStatus::InFlight,
        archive: ArchiveState::NotDue,
    };
    /// A run awaiting triage.
    pub const AMBIGUOUS: Self = Self {
        status: OwnerStatus::Ambiguous,
        archive: ArchiveState::NotDue,
    };

    /// A run that ended in `ending`, with its archive in `archive`.
    pub fn ended(ending: Ending, archive: ArchiveState) -> Self {
        Self {
            status: ending.status(),
            archive,
        }
    }

    /// The ending the row records, if it records one.
    pub fn ending(self) -> Option<Ending> {
        match self.status {
            OwnerStatus::Completed => Some(Ending::Completed),
            OwnerStatus::Failed => Some(Ending::Failed),
            OwnerStatus::InFlight | OwnerStatus::Ambiguous => None,
        }
    }

    /// Whether a row can hold the pair: a run that has not ended has no
    /// archive due, and a run that has ended has one.
    pub fn is_valid(self) -> bool {
        self.ending().is_none() == (self.archive == ArchiveState::NotDue)
    }
}

impl OwnerRow {
    /// The row's state, as [`transition`] takes it.
    pub fn state(&self) -> RowState {
        RowState {
            status: self.status,
            archive: self.archive_state,
        }
    }
}

/// What reaches the row: an event the coordination consumer folds, or
/// startup recovery's restore of a run it will resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipEvent {
    /// `triggered`, published before the run's first WAL write.
    Triggered,
    /// Startup recovery restores a run its WAL holds in flight, to resume
    /// it.
    Restored,
    /// `invocation.ambiguous`.
    Ambiguous,
    /// `completed`, or a `failed` not in `setup`.
    Ended(Ending),
    /// A `failed` in `setup`: the run never made its first WAL write.
    SetupFailed,
    /// `invocation.archived`, with the ending its `final_phase` names.
    Archived(Ending),
    /// `invocation.archive_acked`, the control plane's own answer to it.
    ArchiveAcked,
    /// `invocation.operator_recovered`: an operator's drop, or another
    /// operator-issued ending.
    OperatorEnded(Ending),
    /// `invocation.operator_resumed`.
    OperatorResumed,
}

impl OwnershipEvent {
    /// Every event, for the tests that drive each from every state.
    pub const ALL: [OwnershipEvent; 12] = [
        OwnershipEvent::Triggered,
        OwnershipEvent::Restored,
        OwnershipEvent::Ambiguous,
        OwnershipEvent::Ended(Ending::Completed),
        OwnershipEvent::Ended(Ending::Failed),
        OwnershipEvent::SetupFailed,
        OwnershipEvent::Archived(Ending::Completed),
        OwnershipEvent::Archived(Ending::Failed),
        OwnershipEvent::ArchiveAcked,
        OwnershipEvent::OperatorEnded(Ending::Completed),
        OwnershipEvent::OperatorEnded(Ending::Failed),
        OwnershipEvent::OperatorResumed,
    ];
}

/// Which of the event's facts replace the row's in a move. A row an event
/// creates always takes both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// The row keeps its worker id and time.
    Keep,
    /// The event's worker id replaces the row's; the time is kept.
    TakeWorker,
    /// The event's worker id and time replace the row's.
    TakeBoth,
}

/// What an event does to a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Nothing, from any state: the event is not about the row.
    Ignore,
    /// Move the row to `to`, taking `owner` from the event.
    Move { to: RowState, owner: Owner },
    /// The row already says what the event says.
    Duplicate,
    /// The row records the run finished, and the event would put it back
    /// in progress.
    Refused,
    /// The event reports an ending the row contradicts. The row keeps its
    /// status; `then`, when present, is the move the event still makes
    /// (an archive's arrival).
    Conflict { then: Option<(RowState, Owner)> },
}

/// The one definition of the row's lifecycle: what `event` does to a row
/// in `current` (`None`: there is no row). The table in the module doc is
/// this function, and its tests drive every event from every state. With
/// no row, an event either creates one ([`Decision::Move`]) or is ignored.
pub fn transition(current: Option<RowState>, event: OwnershipEvent) -> Decision {
    use OwnershipEvent as E;
    let Some(row) = current else {
        return match event {
            E::Triggered | E::SetupFailed | E::ArchiveAcked | E::OperatorResumed => {
                Decision::Ignore
            }
            E::Restored => Decision::Move {
                to: RowState::IN_FLIGHT,
                owner: Owner::TakeBoth,
            },
            E::Ambiguous => Decision::Move {
                to: RowState::AMBIGUOUS,
                owner: Owner::Keep,
            },
            E::Ended(ending) => Decision::Move {
                to: RowState::ended(ending, ArchiveState::Pending),
                owner: Owner::Keep,
            },
            E::Archived(ending) | E::OperatorEnded(ending) => Decision::Move {
                to: RowState::ended(ending, ArchiveState::Archived),
                owner: Owner::Keep,
            },
        };
    };
    match (event, row.ending()) {
        (E::Triggered | E::SetupFailed | E::ArchiveAcked | E::OperatorResumed, _) => {
            Decision::Ignore
        }
        (E::Restored, None) => Decision::Move {
            to: RowState::IN_FLIGHT,
            owner: Owner::TakeBoth,
        },
        (E::Ambiguous, None) if row.status == OwnerStatus::Ambiguous => Decision::Duplicate,
        (E::Ambiguous, None) => Decision::Move {
            to: RowState::AMBIGUOUS,
            owner: Owner::Keep,
        },
        (E::Restored | E::Ambiguous, Some(_)) => Decision::Refused,
        (E::Ended(reported), None) => Decision::Move {
            to: RowState::ended(reported, ArchiveState::Pending),
            owner: Owner::Keep,
        },
        (E::Ended(reported), Some(recorded)) if reported == recorded => Decision::Duplicate,
        (E::Ended(_), Some(_)) => Decision::Conflict { then: None },
        (E::Archived(reported), None) => Decision::Move {
            to: RowState::ended(reported, ArchiveState::Archived),
            owner: Owner::TakeWorker,
        },
        (E::Archived(reported), Some(recorded)) => {
            let arrived = (
                RowState::ended(recorded, ArchiveState::Archived),
                Owner::TakeWorker,
            );
            match (row.archive, reported == recorded) {
                (ArchiveState::Archived, true) => Decision::Duplicate,
                (ArchiveState::Archived, false) => Decision::Conflict { then: None },
                (_, true) => Decision::Move {
                    to: arrived.0,
                    owner: arrived.1,
                },
                (_, false) => Decision::Conflict {
                    then: Some(arrived),
                },
            }
        }
        (E::OperatorEnded(ending), _) => {
            let to = RowState::ended(ending, ArchiveState::Archived);
            if row == to {
                Decision::Duplicate
            } else {
                Decision::Move {
                    to,
                    owner: Owner::Keep,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
