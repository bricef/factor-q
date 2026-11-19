//! Writing a move the table decided: what [`apply`] needs of a store, the
//! compare-and-set loop that decides and writes, and what it answers. Moved
//! from `ownership.rs`, which the size cap holds at 800 production lines,
//! ahead of the table growing the termination record (issue 1070).

use super::{Decision, Owner, OwnershipEvent, RowState, transition};
use crate::control_plane::store::{
    ControlPlaneStore, ControlPlaneStoreError, MoveFacts, OwnerRow, TerminationRequest,
};

/// How many times [`apply`] decides and writes before it gives up on a row
/// that keeps changing under it.
///
/// Within one daemon little contends: the coordination consumer is the
/// row's only writer of moves while it runs, on one task. Startup
/// recovery's restores run beside it: the consumer starts first, so that
/// recovery decides over a folded backlog (issue 1070), and goes on
/// folding while recovery restores, a later event for the same run
/// included, or one it had not reached when recovery's wait for it ran
/// out (the held runs are restored again once it has). The compare-and-set
/// orders the two: whichever writes second decides again from the row the
/// first left. (An operator's drop of a row no projection event named an
/// agent for used to delete the row outside the table; it reads it now,
/// issue 1070.) Past that, a lost compare-and-set means another process
/// writes the same store, and three attempts tell a passing race from a
/// row that will not settle. When they are spent,
/// [`apply`] answers [`OwnershipError::Contended`] and has written
/// nothing. The coordination consumer takes that as a transient error: the
/// event is redelivered after the bus's backoff and decided again from the
/// row as it then is. Startup recovery stops the boot with it.
pub const APPLY_ATTEMPTS: usize = 3;

/// What an event says about the row's owner, for a row it creates or a
/// move that takes them ([`Owner`]), and what it brings to the
/// termination record.
#[derive(Debug, Clone, Copy)]
pub struct OwnerFacts<'a> {
    /// The worker the event names: the worker's own id on
    /// `invocation.archived`, on recovery's restore and on a durable
    /// start, the holding worker's on a held ending
    /// (`invocation.ending_held`), the agent id standing in for it on
    /// every other event.
    pub worker_id: &'a str,
    /// When: the run's start, the `started_at` its WAL row records, for
    /// recovery's restore and a durable start, and for a held ending
    /// (`invocation.ending_held` carries it); the time the event is
    /// applied otherwise.
    pub at_ms: i64,
    /// The ending's own time, which a move that records a new ender writes
    /// beside it: the event's own time for a run's ending and for a drop,
    /// and the time the run ended for an archive (`terminal_at_ms`) and for
    /// a held ending (`ended_at_ms`), never a hand-off's or the fold's. A
    /// move that records no new ender does not read it.
    pub ended_at_ms: i64,
    /// The termination request a drop's event carries, which a move that
    /// first records a request writes.
    pub request: Option<&'a TerminationRequest>,
}

impl<'a> OwnerFacts<'a> {
    /// What the move from `from` to `to` writes beside the state: the
    /// owner facts `owner` takes, the ending's time where `to` records an
    /// ender `from` did not, and the request where `to` first records one.
    fn written(&self, from: Option<RowState>, to: RowState, owner: Owner) -> MoveFacts<'a> {
        let (worker_id, assigned_at) = match owner {
            Owner::Keep => (None, None),
            Owner::TakeWorker => (Some(self.worker_id), None),
            Owner::TakeBoth => (Some(self.worker_id), Some(self.at_ms)),
        };
        let (ender, requested) = from.map_or((None, false), |from| (from.ended_by, from.requested));
        MoveFacts {
            worker_id,
            assigned_at,
            ended_at: (to.ended_by.is_some() && to.ended_by != ender).then_some(self.ended_at_ms),
            request: self.request.filter(|_| to.requested && !requested),
        }
    }
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
    /// The row records the run finished, or, for a durable start, awaits
    /// triage, and was left as it was.
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
        facts: MoveFacts<'_>,
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
        facts: MoveFacts<'_>,
    ) -> Result<bool, ControlPlaneStoreError> {
        ControlPlaneStore::compare_and_set_ownership(self, invocation_id, expected, to, facts).await
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
                    facts.written(None, to, Owner::TakeBoth),
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
        if store
            .compare_and_set_ownership(
                invocation_id,
                Some(row),
                to,
                facts.written(Some(row), to, owner),
            )
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
