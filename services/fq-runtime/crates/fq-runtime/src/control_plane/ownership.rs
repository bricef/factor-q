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
//! ambiguous, an operator drops it). Whether a run should have a row from
//! its durable start is issue 721's question; it would be one more writer
//! of the `no row → in_flight` edge below. No event removes a row. The
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
//! - **Conflict**: an ending that contradicts the one recorded. The
//!   recorded ending stands, whichever came first on the stream: a run's
//!   own ending that reaches a row an operator's drop has already ended
//!   and archived keeps the drop, which the consumer logs at warn, since
//!   dropping a run as it finishes is no fault. Anywhere else the run
//!   published two endings, which is logged at error. An archive's
//!   arrival is still recorded, as the cell after `conflict:` says, and
//!   the row then keeps its ending while the archive record holds the
//!   other: the two disagree with no operator involved (issue 1070).
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
//!
//! ## A row this build cannot read
//!
//! A status or an archive state this build does not know, which only a
//! hand edit or a newer build writes, cannot be moved by compare-and-set:
//! [`apply`] answers [`OwnershipError::Unreadable`] and writes nothing.
//! The rule is the same everywhere: the row is left as it is, the event
//! or the run goes on without it, and nothing waits for it.
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

use super::store::{
    ArchiveState, ControlPlaneStore, ControlPlaneStoreError, OwnerRow, OwnerStatus,
};
use crate::events::FailurePhase;

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
            | OwnershipEvent::OperatorEnded(ending) => Some(ending),
            OwnershipEvent::Triggered
            | OwnershipEvent::Restored
            | OwnershipEvent::Ambiguous
            | OwnershipEvent::SetupFailed
            | OwnershipEvent::ArchiveAcked
            | OwnershipEvent::OperatorResumed => None,
        }
    }

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

/// How many times [`apply`] decides and writes before it gives up on a row
/// that keeps changing under it.
///
/// Within one daemon little contends: the coordination consumer is the
/// row's only writer of moves while it runs, on one task, and startup
/// recovery writes before that consumer starts. The one other writer is an
/// operator's drop of a row no projection event names an agent for:
/// `operator::drop_invocation` deletes that row itself, to tell it from an
/// id that never existed, before it publishes the drop, whose fold writes
/// the row again as `failed`, archived. A move decided over the row just
/// before the delete loses its compare-and-set and is decided again from
/// no row. Past that, a lost compare-and-set means another process writes
/// the same store, and three attempts tell a passing race from a row that
/// will not settle. When they are spent,
/// [`apply`] answers [`OwnershipError::Contended`] and has written
/// nothing. The coordination consumer takes that as a transient error: the
/// event is redelivered after the bus's backoff and decided again from the
/// row as it then is. Startup recovery stops the boot with it.
pub const APPLY_ATTEMPTS: usize = 3;

/// What an event says about the row's owner, for a row it creates or a
/// move that takes them ([`Owner`]).
#[derive(Debug, Clone, Copy)]
pub struct OwnerFacts<'a> {
    /// The worker the event names: the worker's own id on
    /// `invocation.archived` and on recovery's restore, the agent id
    /// standing in for it on every other event.
    pub worker_id: &'a str,
    /// When: the run's start for recovery's restore, the time the event
    /// is applied otherwise.
    pub at_ms: i64,
}

/// What [`apply`] did. Each answer but `Ignored` names the state it was
/// decided from, so the caller can say it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// The event does not concern the row, which was left as it was.
    Ignored,
    /// The row moved from `from` (`None`: there was no row) to `to`.
    Moved {
        from: Option<RowState>,
        to: RowState,
    },
    /// The row already said what the event says, and was left as it was.
    Duplicate { at: RowState },
    /// The row records the run finished, and was left as it was.
    Refused { recorded: RowState },
    /// The event reported an ending the row contradicts. The row keeps its
    /// status; `moved_to` is where it went when an archive's arrival still
    /// moved it.
    Conflict {
        recorded: RowState,
        moved_to: Option<RowState>,
    },
}

/// Why [`apply`] wrote nothing.
#[derive(Debug, thiserror::Error)]
pub enum OwnershipError {
    #[error(transparent)]
    Store(ControlPlaneStoreError),
    /// The row holds a status or an archive state this build cannot read.
    #[error("the ownership row of {invocation_id} cannot be read: {detail}")]
    Unreadable {
        invocation_id: String,
        detail: String,
    },
    /// The row changed between the read and the write on every attempt.
    #[error(
        "the ownership row of {invocation_id} changed under each of {attempts} attempts to \
         move it"
    )]
    Contended {
        invocation_id: String,
        attempts: usize,
    },
}

/// What [`apply`] needs of a store: the row, and a write that lands only
/// on the state the move was decided from. The control-plane store is the
/// one implementation; a test gives another to make the race happen.
#[async_trait::async_trait]
pub trait OwnershipStore: Sync {
    async fn ownership(
        &self,
        invocation_id: &str,
    ) -> Result<Option<OwnerRow>, ControlPlaneStoreError>;

    async fn compare_and_set_ownership(
        &self,
        invocation_id: &str,
        expected: Option<RowState>,
        to: RowState,
        worker_id: Option<&str>,
        assigned_at: Option<i64>,
    ) -> Result<bool, ControlPlaneStoreError>;
}

#[async_trait::async_trait]
impl OwnershipStore for ControlPlaneStore {
    async fn ownership(
        &self,
        invocation_id: &str,
    ) -> Result<Option<OwnerRow>, ControlPlaneStoreError> {
        self.get_invocation_owner(invocation_id).await
    }

    async fn compare_and_set_ownership(
        &self,
        invocation_id: &str,
        expected: Option<RowState>,
        to: RowState,
        worker_id: Option<&str>,
        assigned_at: Option<i64>,
    ) -> Result<bool, ControlPlaneStoreError> {
        ControlPlaneStore::compare_and_set_ownership(
            self,
            invocation_id,
            expected,
            to,
            worker_id,
            assigned_at,
        )
        .await
    }
}

/// Apply `what` to `invocation_id`'s row: read the row, decide with
/// [`transition`], and write the move as a compare-and-set on the state
/// it was decided from. A row that changed between the read and the
/// write is read again and decided again, at most [`APPLY_ATTEMPTS`]
/// times in all.
pub async fn apply(
    store: &impl OwnershipStore,
    invocation_id: &str,
    what: OwnershipEvent,
    facts: OwnerFacts<'_>,
) -> Result<Applied, OwnershipError> {
    for _ in 0..APPLY_ATTEMPTS {
        let current = match store.ownership(invocation_id).await {
            Ok(row) => row.map(|row| row.state()),
            Err(ControlPlaneStoreError::Malformed(detail)) => {
                return Err(OwnershipError::Unreadable {
                    invocation_id: invocation_id.to_string(),
                    detail,
                });
            }
            Err(err) => return Err(OwnershipError::Store(err)),
        };
        let decision = transition(current, what);
        let Some(row) = current else {
            // With no row, an event either creates one or is ignored.
            let Decision::Move { to, .. } = decision else {
                return Ok(Applied::Ignored);
            };
            let created = store
                .compare_and_set_ownership(
                    invocation_id,
                    None,
                    to,
                    Some(facts.worker_id),
                    Some(facts.at_ms),
                )
                .await
                .map_err(OwnershipError::Store)?;
            if created {
                return Ok(Applied::Moved { from: None, to });
            }
            continue;
        };
        let (to, owner) = match decision {
            Decision::Ignore => return Ok(Applied::Ignored),
            Decision::Duplicate => return Ok(Applied::Duplicate { at: row }),
            Decision::Refused => return Ok(Applied::Refused { recorded: row }),
            Decision::Conflict { then: None } => {
                return Ok(Applied::Conflict {
                    recorded: row,
                    moved_to: None,
                });
            }
            Decision::Move { to, owner }
            | Decision::Conflict {
                then: Some((to, owner)),
            } => (to, owner),
        };
        let (worker_id, assigned_at) = match owner {
            Owner::Keep => (None, None),
            Owner::TakeWorker => (Some(facts.worker_id), None),
            Owner::TakeBoth => (Some(facts.worker_id), Some(facts.at_ms)),
        };
        if store
            .compare_and_set_ownership(invocation_id, Some(row), to, worker_id, assigned_at)
            .await
            .map_err(OwnershipError::Store)?
        {
            return Ok(match decision {
                Decision::Conflict { .. } => Applied::Conflict {
                    recorded: row,
                    moved_to: Some(to),
                },
                _ => Applied::Moved {
                    from: Some(row),
                    to,
                },
            });
        }
    }
    Err(OwnershipError::Contended {
        invocation_id: invocation_id.to_string(),
        attempts: APPLY_ATTEMPTS,
    })
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod apply_tests;
