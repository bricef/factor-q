//! The ownership row's lifecycle, written down once (issue 1024).
//!
//! The control plane keeps one ownership row per run,
//! `coordination_invocation_owner`, which `fq invocation list`, the
//! recovery views and startup recovery read. Every move of it is decided
//! by [`transition`], a pure function from the row's state and one event
//! to a [`Decision`], and [`apply`] writes the move as a compare-and-set on
//! the state it was decided from. It is the first small instance of the
//! explicit lifecycles issue 1067 proposes.
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
//! ambiguous, an operator drops it). The table below has a run's durable
//! start, its first WAL write, as a second writer of the
//! `no row → in_flight` edge beside recovery's restore (issue 721).
//! Nothing writes it yet: the dispatcher is to apply it at the run's
//! first WAL write, after it marks the trigger's claim started, so that
//! a running run has a row from that moment. No event removes a row. The
//! one delete is outside the table: an operator's drop of a row no
//! projection event names an agent for, which deletes the row before the
//! drop's own fold writes it again ([`APPLY_ATTEMPTS`] says how a move
//! meets it). Changing what a drop does is issue 1070's.
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
//! | the run durably starts | in_flight | duplicate | refused | refused | refused | refused | refused |
//! | recovery restores the run | in_flight | in_flight | in_flight | refused | refused | refused | refused |
//! | `ambiguous` | ambiguous | ambiguous | duplicate | refused | refused | refused | refused |
//! | `completed` | completed·pending | completed·pending | completed·pending | duplicate | duplicate | conflict | conflict |
//! | `failed`, not in `setup` | failed·pending | failed·pending | failed·pending | conflict | conflict | duplicate | duplicate |
//! | `archived`, phase `completed` | completed·archived | completed·archived | completed·archived | completed·archived | duplicate | conflict: failed·archived | conflict |
//! | `archived`, phase `failed` | failed·archived | failed·archived | failed·archived | conflict: completed·archived | conflict | failed·archived | duplicate |
//! | termination request (a drop), phase `failed` | failed·archived | failed·archived | failed·archived | failed·archived | failed·archived | failed·archived | duplicate |
//! | termination request, phase `completed` | completed·archived | completed·archived | completed·archived | completed·archived | duplicate | completed·archived | completed·archived |
//!
//! - **Ignored**: the event never touches the row. `triggered` is
//!   published before the run's first WAL write, also by a run refused
//!   before it starts. A `failed` in `setup` is published only while the
//!   run's WAL holds no row: the runner stamps that phase from the WAL
//!   row itself (`worker/reducer/runner/publish_failure.rs`). That is
//!   before the run's first WAL write, and a row is what startup
//!   recovery, the dispatcher's own-claim arm and the dead-letter watch
//!   read as "the run started" (`RunRecords`), so a row written for such
//!   a run would mark its trigger's claim started, and drop every later
//!   copy of the trigger. Or it is after an operator's live drop whose
//!   archive was handed off, when the drop is already the ending on
//!   record. A reducer whose first step errors is one such run: its
//!   `failed` is in `setup`, and the runner's archive step after it logs
//!   `archive publish skipped: invocation_state row missing` at warn, as
//!   it did before this table. The rule holds for events from builds
//!   that read the phase from the WAL; a consumer that replays the stream
//!   from its start meets earlier builds' `failed` events in another
//!   phase for runs that never started, and writes each such run a
//!   `failed` row, its archive pending, which no archive ever follows.
//!   `operator_resumed` is ignored too: the resume it records hands the
//!   run back to its worker, whose next ending moves the row, and it
//!   carries no worker id to move the row to `in_flight` under, so a run
//!   an operator resumed reads `ambiguous` until it ends, as the
//!   operating guide says.
//! - **Refused**: the row records the run finished, and the event would
//!   put it back in progress. A report of ambiguity, or recovery's
//!   restore, can arrive after the run's ending; the ending stands. A
//!   durable start is refused by an `ambiguous` row too: see the next
//!   item.
//! - **The durable start** creates the row, `in_flight`, for a run that
//!   has made its first WAL write, and only where there is no row. It
//!   comes after that write, so the WAL already records the start that the
//!   row then repeats, and a run refused before it never gets one. Its
//!   run's id is new at each delivery, so a row already there was written
//!   for that run by someone else first. An `in_flight` row is the same
//!   start, written already (a retry of a write that landed): a duplicate,
//!   which keeps the worker and time it names. An `ambiguous` row awaits
//!   an operator's triage, and an ended row records an ending, an
//!   operator's drop among them: either refuses it and keeps what it
//!   says, since a start must neither take a run from triage nor put a
//!   finished run back in progress. [`apply`] answers
//!   [`Applied::Refused`] with the row it found, logs nothing itself, and
//!   writes nothing; whoever applies the start logs the refusal.
//! - **Conflict**: an ending that contradicts the one recorded. The
//!   recorded ending stands, whichever came first on the stream: a run's
//!   own ending that reaches a row an operator's drop has already ended
//!   and archived keeps the drop. The consumer logs a conflict on
//!   `failed·archived`, the state a drop leaves, at warn, since dropping
//!   a run as it finishes is no fault; the worker's own failure leaves the
//!   same state once archived, so a second ending there is a warn too.
//!   Anywhere else, `completed·archived` included, the run published two
//!   endings, which is logged at error. An archive's
//!   arrival is still recorded, as the cell after `conflict:` says, and
//!   the row then keeps its ending while the archive record holds the
//!   other: the two disagree with no operator involved (issue 1070).
//! - **Duplicate**: the row already says what the event says, as
//!   at-least-once delivery makes ordinary.
//! - **A termination request wins**: an operator's drop ends the run from
//!   every state, as it always has, and can turn a finished run to
//!   `failed`. What that leaves inconsistent in the archive record is
//!   issue 1070's.
//!
//! Who a row names: a row an event creates takes the event's worker id
//! (the worker's own on `invocation.archived`, on recovery's restore and
//! on a durable start, the agent id standing in for it on every other
//! event) and time (the run's own start, the `started_at` its WAL row
//! records, for recovery's restore and a durable start; the time the
//! event is applied for every other). A move
//! keeps the row's worker id and time, with two exceptions:
//! `invocation.archived` brings the worker's real id ([`Owner::TakeWorker`]),
//! and recovery's restore re-owns the run for this daemon from the run's
//! start ([`Owner::TakeBoth`]).
//!
//! ## A row this build cannot read
//!
//! A status or an archive state this build does not know, which only a
//! hand edit or a newer build writes, cannot be moved by compare-and-set:
//! [`apply`] answers [`OwnershipError::Unreadable`] and writes nothing.
//! The writers of the row and the paths that must not stall follow one
//! rule: the row is left as it is, the event or the run goes on without
//! it, and nothing waits for it. Who follows it:
//!
//! - **The coordination consumer** refuses the event as permanent, which
//!   its shared loop logs at warn and acks, because a retry would hold
//!   every later event behind it on that strict-order consumer for good.
//!   What the event does besides moving the row still happens:
//!   `invocation.archived` writes the archive record and is acknowledged
//!   to the worker, so the worker lets its terminal row go; an operator's
//!   drop writes its archive record, with the drop's time as the run's
//!   start, and marks the worker's WAL row terminal.
//! - **Startup recovery** logs the row at error and classifies the run
//!   from its WAL row alone: `reconcile_terminal_owners` finds no ending
//!   to apply, and the restore before a resume leaves the row as it is
//!   and resumes the run, since the WAL, not the row, says whether a run
//!   can be resumed. The boot goes on.
//! - **The stuck sweep and `fq doctor`'s executions** read the row as not
//!   closing the run (`liveness::is_closed_by_control_plane`), and go on
//!   to the rows after it.
//!
//! What does not follow it yet, and fails on such a row instead: the
//! invocation list with no status filter, `fq invocation show` of that
//! run, the worker list and detail, `fq doctor`, and the retention sweep
//! once a stale worker owns the row. An operator's drop of the run still
//! stops it, but the row keeps reading as it did, and only a hand edit of
//! the row clears it. Issue 1082 is those readers and the drop.

use super::store::{ArchiveState, OwnerRow, OwnerStatus};
use crate::events::{FailurePhase, HeldOutcome};

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    Completed,
    Failed,
}

/// The ending `invocation.ending_held` reports (issue 1071), the outcome
/// of a run whose own ending the worker holds.
impl From<HeldOutcome> for Ending {
    fn from(outcome: HeldOutcome) -> Self {
        match outcome {
            HeldOutcome::Completed => Ending::Completed,
            HeldOutcome::Failed => Ending::Failed,
        }
    }
}

/// What a worker holding a run's ending reports: the ending its row's
/// phase names, read by [`Ending::of_archived_phase`], the rule the
/// run's `invocation.archived` is read by, so the two never disagree.
impl From<Ending> for HeldOutcome {
    fn from(ending: Ending) -> Self {
        match ending {
            Ending::Completed => HeldOutcome::Completed,
            Ending::Failed => HeldOutcome::Failed,
        }
    }
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

    /// The state as log lines name it: `status/archive_state`.
    pub fn name(self) -> String {
        format!("{}/{}", self.status.as_str(), self.archive.as_str())
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

/// What reaches the row: an event the coordination consumer folds,
/// startup recovery's restore of a run it will resume, or a run's durable
/// start, which nothing applies yet (issue 721's next change makes the
/// dispatcher apply it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipEvent {
    /// `triggered`, published before the run's first WAL write.
    Triggered,
    /// The run made its first WAL write: it durably started, and the
    /// daemon that saw the write owns it from the run's start (issue 721).
    /// Nothing applies it yet.
    DurablyStarted,
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
    /// `invocation.operator_recovered`: an operator asked for the run to
    /// be terminated, a drop, with the ending its `final_phase` names.
    /// The request ends the run from every state; issue 1070 records it
    /// beside the run's outcome instead.
    TerminationRequested(Ending),
    /// `invocation.operator_resumed`.
    OperatorResumed,
}

impl OwnershipEvent {
    /// What a `failed` in `phase` is to the row. A `failed` in `setup` is
    /// published only while the run's WAL holds no row: before its first
    /// WAL write, when a row here would record a start that never
    /// happened, or after a live drop whose archive was handed off, when
    /// the drop is already the recorded ending. Every other phase is an
    /// ending. One arm per phase, so a phase added later has to be
    /// placed here.
    pub fn of_failed(phase: FailurePhase) -> Self {
        match phase {
            FailurePhase::Setup => OwnershipEvent::SetupFailed,
            FailurePhase::Reducer
            | FailurePhase::HostStepBudget
            | FailurePhase::Budget
            | FailurePhase::LlmRequest
            | FailurePhase::LlmResponse
            | FailurePhase::ToolCall
            | FailurePhase::ToolResult => OwnershipEvent::Ended(Ending::Failed),
        }
    }

    /// The ending the event reports, if it reports one.
    pub fn ending(self) -> Option<Ending> {
        match self {
            OwnershipEvent::Ended(ending)
            | OwnershipEvent::Archived(ending)
            | OwnershipEvent::TerminationRequested(ending) => Some(ending),
            OwnershipEvent::Triggered
            | OwnershipEvent::DurablyStarted
            | OwnershipEvent::Restored
            | OwnershipEvent::Ambiguous
            | OwnershipEvent::SetupFailed
            | OwnershipEvent::ArchiveAcked
            | OwnershipEvent::OperatorResumed => None,
        }
    }

    /// Every event, for the tests that drive each from every state.
    pub const ALL: [OwnershipEvent; 13] = [
        OwnershipEvent::Triggered,
        OwnershipEvent::DurablyStarted,
        OwnershipEvent::Restored,
        OwnershipEvent::Ambiguous,
        OwnershipEvent::Ended(Ending::Completed),
        OwnershipEvent::Ended(Ending::Failed),
        OwnershipEvent::SetupFailed,
        OwnershipEvent::Archived(Ending::Completed),
        OwnershipEvent::Archived(Ending::Failed),
        OwnershipEvent::ArchiveAcked,
        OwnershipEvent::TerminationRequested(Ending::Completed),
        OwnershipEvent::TerminationRequested(Ending::Failed),
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
    /// in progress; or, for a durable start, the row awaits triage.
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
            E::DurablyStarted | E::Restored => Decision::Move {
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
            E::Archived(ending) | E::TerminationRequested(ending) => Decision::Move {
                to: RowState::ended(ending, ArchiveState::Archived),
                owner: Owner::Keep,
            },
        };
    };
    match (event, row.ending()) {
        (E::Triggered | E::SetupFailed | E::ArchiveAcked | E::OperatorResumed, _) => {
            Decision::Ignore
        }
        (E::DurablyStarted, None) if row.status == OwnerStatus::InFlight => Decision::Duplicate,
        (E::DurablyStarted, _) => Decision::Refused,
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
        (E::TerminationRequested(ending), _) => {
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

mod write;

pub use write::{APPLY_ATTEMPTS, Applied, OwnerFacts, OwnershipError, OwnershipStore, apply};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod apply_tests;
