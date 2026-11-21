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
//! `archived` after, and `none` when an operator's drop said no archive of
//! the run will come. So the pairs a row can hold are `in_flight·not_due`,
//! `ambiguous·not_due`, `completed·pending`, `completed·archived`,
//! `failed·pending`, `failed·archived` and `failed·none`.
//!
//! Beside them the row keeps the termination record (issue 1070): who
//! ended the run, the run itself or an operator's drop (`ended_by`), and
//! when (`ended_at`, the ending event's own time); and the first
//! termination an operator requested, with its own time, who asked and
//! why. A row that ended before the record existed names no ender, which
//! reads "not recorded", and the table takes such a row as the run's. A
//! row's state, as the table reads it, is its pair, its ender and whether
//! a request is on record ([`RowState`]): no ender and no request on a run
//! that has not ended, and a drop as the ender only of a `failed` run
//! ([`RowState::is_valid`], the same states the store's checks admit).
//!
//! There is also no row at all, which is where a run stands before its
//! first WAL write. The dispatcher writes the row at that write, the
//! run's durable start, after it marks the trigger's claim started
//! (`dispatcher/claim/started_row.rs`, issue 721): the second writer of
//! the `no row → in_flight` edge, beside recovery's restore. A run whose
//! row that write could not make, or one started by a build before it,
//! has none until something else happens to it (it ends, recovery
//! restores it, it is reported ambiguous, an operator drops it). No event
//! removes a row, and nothing else does.
//!
//! Why the archive sweeper holds a `pending` row is not stored. A hold is
//! one daemon run's verdict, kept in the worker's memory and offered again
//! at the next start, so the daemon reads it where it builds a report.
//!
//! ## The transitions
//!
//! A column names a row's pair, and its ender where that matters: "by a
//! drop" is a `failed` row a drop ended; every other column is a row the
//! run ended, or one that records no ender. A cell names where the row
//! goes. "(run)" and "(drop)" are the ender a move records, at the event's
//! own time; a move that names none keeps the row's.
//! `invocation.ending_held` (issue 1071) is the run's own `completed` or
//! `failed`, which its worker holds, and folds as that ending.
//!
//! | event \ from | no row, in_flight, ambiguous | completed·pending | completed·archived | failed·pending | failed·archived | failed·none | failed·pending by a drop | failed·none by a drop | failed·archived by a drop |
//! |---|---|---|---|---|---|---|---|---|---|
//! | `triggered`, `archive_acked`, `operator_resumed`, `failed` in `setup` | ignored | ignored | ignored | ignored | ignored | ignored | ignored | ignored | ignored |
//! | the run durably starts | in_flight from no row; a duplicate on `in_flight`; refused on `ambiguous` | refused | refused | refused | refused | refused | refused | refused | refused |
//! | recovery restores the run | in_flight | refused | refused | refused | refused | refused | refused | refused | refused |
//! | `ambiguous` | ambiguous (a duplicate on `ambiguous`) | refused | refused | refused | refused | refused | refused | refused | refused |
//! | `completed` | completed·pending (run) | duplicate | duplicate | conflict | conflict | completed·pending (run) | completed·pending (run) | completed·pending (run) | conflict |
//! | `failed`, not in `setup` | failed·pending (run) | conflict | conflict | duplicate | duplicate | failed·pending (run) | failed·pending (run) | failed·pending (run) | duplicate |
//! | `archived`, phase `completed` | completed·archived (run) | completed·archived | duplicate | conflict: failed·archived | conflict | completed·archived (run) | completed·archived (run) | completed·archived (run) | conflict |
//! | `archived`, phase `failed` | failed·archived (run) | conflict: completed·archived | conflict | failed·archived | duplicate | failed·archived | failed·archived | failed·archived | duplicate |
//! | termination request (a drop) | failed·pending (drop), or failed·none (drop) when it says no archive will come | request | request | request | request | request | request | request | request |
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
//!   recorded ending stands, whichever came first on the stream. The
//!   consumer logs it at error: the run published two endings, or one
//!   after its drop's ending was made final, which a run whose drop's mark
//!   won never does. The one warn is a conflict on `failed·archived` with
//!   no ender on record, the row a drop from before the record left, where
//!   the drop may simply have come first. An archive's arrival is still
//!   recorded, as the cell after `conflict:` says, and the row then keeps
//!   its ending while the archive record holds the other: the two
//!   disagree with no operator involved (issue 1070).
//! - **Duplicate**: the row already says what the event says, as
//!   at-least-once delivery makes ordinary.
//! - **A termination request** (an operator's drop) ends a run that has
//!   not ended, `failed` by the drop, provisionally, and records the
//!   request beside the ending, the drop's own time as both. On a run that
//!   has ended, whoever ended it, it changes nothing but the request
//!   ("request" in the table): the first is recorded, and a later one is a
//!   duplicate. It never rewrites an ending, and writes no archive record:
//!   the worker's archive brings the run's real state.
//! - **A provisional ending gives way to the run's own.** A drop's ending
//!   is provisional while its archive is `pending` or `none`, and so is
//!   any ending whose archive is `none`, which says only that none was
//!   expected: the run's own `completed` or `failed`, or an archive saying
//!   `completed`, replaces it, and the run becomes the ender, at that
//!   event's own time (an archive's is the time the run ended,
//!   `terminal_at_ms`, not its hand-off's, and so is a held ending's,
//!   `ended_at_ms`). An archive saying `failed`
//!   makes a drop's ending final, `failed·archived` by the drop.
//! - **No archive to come**: a drop whose event says `archive: none`,
//!   because the run has no row on the dropping daemon's worker, none
//!   will be written there, and no archive is on record, ends a run that
//!   had not ended `failed·none` ([`ArchiveExpected`]). On a run that has
//!   ended it records the request, as any drop does.
//!
//! Who a row names: a row an event creates takes the event's worker id
//! (the worker's own on `invocation.archived`, on recovery's restore and
//! on a durable start, the holding worker's on a held ending, the agent id
//! standing in for it on every other event) and time (the run's own
//! start, the `started_at` its WAL row records, for recovery's restore, a
//! durable start and a held ending; the time the event is applied for
//! every other). A move
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
//!   drop marks the worker's WAL row terminal.
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

use super::store::{ArchiveState, EndedBy, OwnerRow, OwnerStatus};
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

    pub fn status(self) -> OwnerStatus {
        match self {
            Ending::Completed => OwnerStatus::Completed,
            Ending::Failed => OwnerStatus::Failed,
        }
    }
}

/// A row's state: its status and its archive's, who ended the run, and
/// whether an operator's termination request is on record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowState {
    pub status: OwnerStatus,
    pub archive: ArchiveState,
    /// Who ended the run; `None` while it has not, and on a row that ended
    /// before the termination record existed (not recorded).
    pub ended_by: Option<EndedBy>,
    /// Whether the row records an operator's termination request.
    pub requested: bool,
}

impl RowState {
    /// A run in progress.
    pub const IN_FLIGHT: Self = Self {
        status: OwnerStatus::InFlight,
        archive: ArchiveState::NotDue,
        ended_by: None,
        requested: false,
    };
    /// A run awaiting triage.
    pub const AMBIGUOUS: Self = Self {
        status: OwnerStatus::Ambiguous,
        archive: ArchiveState::NotDue,
        ended_by: None,
        requested: false,
    };

    /// A run that ended in `ending`, with its archive in `archive`, its
    /// ender not recorded and no request on record.
    pub fn ended(ending: Ending, archive: ArchiveState) -> Self {
        Self {
            status: ending.status(),
            archive,
            ended_by: None,
            requested: false,
        }
    }

    /// The same state, ended by `ender`.
    pub fn by(self, ender: EndedBy) -> Self {
        Self {
            ended_by: Some(ender),
            ..self
        }
    }

    /// The same state, with a termination request on record.
    pub fn with_request(self) -> Self {
        Self {
            requested: true,
            ..self
        }
    }

    /// The state a drop of a run that has not ended leaves: `failed` by the
    /// drop, its archive pending or, when the drop says so, none to come,
    /// and the request on record.
    pub fn dropped(expected: ArchiveExpected) -> Self {
        let archive = match expected {
            ArchiveExpected::Yes => ArchiveState::Pending,
            ArchiveExpected::No => ArchiveState::WillNotCome,
        };
        Self::ended(Ending::Failed, archive)
            .by(EndedBy::Drop)
            .with_request()
    }

    /// Whether the row's ending is provisional, one the run's own ending
    /// replaces: a drop's while its archive has not landed, and any ending
    /// whose archive is `none`, which says only that none was expected.
    pub fn is_provisional(self) -> bool {
        self.archive == ArchiveState::WillNotCome
            || (self.ended_by == Some(EndedBy::Drop) && self.archive != ArchiveState::Archived)
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

    /// Whether a row can hold the state: a run that has not ended has no
    /// archive due, no ender and no request; a run that has ended has an
    /// archive due; only a failed run can have none to come, and only a
    /// failed run can have been ended by a drop. These are the states the
    /// store's checks admit, so no move this table decides is one the store
    /// refuses.
    pub fn is_valid(self) -> bool {
        let pair = matches!(
            (self.status, self.archive),
            (
                OwnerStatus::InFlight | OwnerStatus::Ambiguous,
                ArchiveState::NotDue
            ) | (
                OwnerStatus::Completed,
                ArchiveState::Pending | ArchiveState::Archived
            ) | (
                OwnerStatus::Failed,
                ArchiveState::Pending | ArchiveState::Archived | ArchiveState::WillNotCome
            )
        );
        let ender = match self.ended_by {
            None => true,
            Some(EndedBy::Run) => self.ending().is_some(),
            Some(EndedBy::Drop) => self.status == OwnerStatus::Failed,
        };
        pair && ender && (!self.requested || self.ending().is_some())
    }
}

impl OwnerRow {
    /// The row's state, as [`transition`] takes it.
    pub fn state(&self) -> RowState {
        RowState {
            status: self.status,
            archive: self.archive_state,
            ended_by: self.ended_by,
            requested: self.termination.is_some(),
        }
    }
}

/// What reaches the row: an event the coordination consumer folds,
/// startup recovery's restore of a run it will resume, or a run's durable
/// start, which the dispatcher applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipEvent {
    /// `triggered`, published before the run's first WAL write.
    Triggered,
    /// The run made its first WAL write: it durably started, and the
    /// daemon that saw the write owns it from the run's start (issue 721).
    /// The dispatcher applies it once it has marked the trigger's claim.
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
    /// be terminated, a drop, with what its event says of the run's
    /// archive. It ends a run that has not ended, and is recorded beside
    /// the outcome of one that has (issue 1070).
    TerminationRequested(ArchiveExpected),
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
            OwnershipEvent::Ended(ending) | OwnershipEvent::Archived(ending) => Some(ending),
            OwnershipEvent::TerminationRequested(_)
            | OwnershipEvent::Triggered
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
        OwnershipEvent::TerminationRequested(ArchiveExpected::Yes),
        OwnershipEvent::TerminationRequested(ArchiveExpected::No),
        OwnershipEvent::OperatorResumed,
    ];
}

/// What an operator's drop says of the run's archive (issue 1070): whether
/// one may follow the drop, as the daemon that published it knew.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveExpected {
    /// One may: the drop said `pending`, said nothing (published by a
    /// build before the field), or said a value this build does not know.
    Yes,
    /// None will: the drop said `none`. The run had no row on the
    /// dropping daemon's worker, none was to be written there, and no
    /// archive was on record.
    No,
}

impl ArchiveExpected {
    /// What a drop's event says, its `archive` field read: only `none` is
    /// [`ArchiveExpected::No`]; everything else reads as an archive that
    /// may come, the answer that waits for it.
    pub fn of_drop(archive: Option<crate::events::DropArchive>) -> Self {
        match archive {
            Some(crate::events::DropArchive::WillNotCome) => ArchiveExpected::No,
            Some(crate::events::DropArchive::Pending | crate::events::DropArchive::Unknown)
            | None => ArchiveExpected::Yes,
        }
    }
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
                to: RowState::ended(ending, ArchiveState::Pending).by(EndedBy::Run),
                owner: Owner::Keep,
            },
            E::Archived(ending) => Decision::Move {
                to: RowState::ended(ending, ArchiveState::Archived).by(EndedBy::Run),
                owner: Owner::Keep,
            },
            E::TerminationRequested(expected) => Decision::Move {
                to: RowState::dropped(expected),
                owner: Owner::Keep,
            },
        };
    };
    // The run's own ending, where it replaces a provisional one: the run
    // is the ender, and the request stays on record.
    let the_runs_own = |ending: Ending, archive: ArchiveState| RowState {
        requested: row.requested,
        ..RowState::ended(ending, archive).by(EndedBy::Run)
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
            to: RowState::ended(reported, ArchiveState::Pending).by(EndedBy::Run),
            owner: Owner::Keep,
        },
        // A provisional ending gives way to the run's own, which brings its
        // archive.
        (E::Ended(reported), Some(_)) if row.is_provisional() => Decision::Move {
            to: the_runs_own(reported, ArchiveState::Pending),
            owner: Owner::Keep,
        },
        (E::Ended(reported), Some(recorded)) if reported == recorded => Decision::Duplicate,
        (E::Ended(_), Some(_)) => Decision::Conflict { then: None },
        (E::Archived(reported), None) => Decision::Move {
            to: RowState::ended(reported, ArchiveState::Archived).by(EndedBy::Run),
            owner: Owner::TakeWorker,
        },
        // An archive saying `completed` is the run's own ending.
        (E::Archived(Ending::Completed), Some(_)) if row.is_provisional() => Decision::Move {
            to: the_runs_own(Ending::Completed, ArchiveState::Archived),
            owner: Owner::TakeWorker,
        },
        (E::Archived(reported), Some(recorded)) => {
            // The archive lands on the ending the row records, its ender
            // and request kept.
            let arrived = (
                RowState {
                    archive: ArchiveState::Archived,
                    ..row
                },
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
        (E::TerminationRequested(expected), None) => Decision::Move {
            to: RowState::dropped(expected),
            owner: Owner::Keep,
        },
        // On a run that has ended, a drop records its request beside the
        // outcome, and changes nothing else; the first request is kept.
        (E::TerminationRequested(_), Some(_)) if row.requested => Decision::Duplicate,
        (E::TerminationRequested(_), Some(_)) => Decision::Move {
            to: row.with_request(),
            owner: Owner::Keep,
        },
    }
}

mod write;

pub use write::{APPLY_ATTEMPTS, Applied, OwnerFacts, OwnershipError, OwnershipStore, apply};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod apply_tests;
