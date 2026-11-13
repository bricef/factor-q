//! The trigger-to-invocation link, read for an operator: which delivery
//! started an invocation, and which claimed deliveries never settled.
//!
//! The link is the dispatcher's claim row (ADR-0032): one per trigger
//! delivery, keyed on the message's place on the trigger stream, naming
//! the invocation it starts from the moment it is claimed. Nothing else
//! records it. The worker's WAL row is deleted once its archive is
//! acknowledged; a started claim row is kept until the retention sweep
//! deletes it, `[state] trigger_claim_retention_days` after it was claimed
//! (`control_plane::retention`).
//!
//! **What each read costs.** `invocation_id` and `state` have no index on
//! the claim table, so a read of it scans the rows the retention window
//! keeps. The invocation detail ([`Views::invocation`]) makes one such read for
//! the one invocation it shows, and no list makes any. The stranded-claim
//! check makes one statement over the table, then, for each `claimed` row
//! with no coordination row, a primary-key read of its run's WAL row (the
//! whole row, state blob included: the worker store has no narrower
//! read) and, when that misses, a primary-key read of the coordination
//! row. So it grows with the claims some process holds or left behind,
//! never with a list or with the started rows that make up nearly all of
//! the table. Indexing the table is its own change; nothing here needs it.
//!
//! The report's horizon ([`ClaimJudgement::horizon_ms`]) is the same
//! window the sweep deletes at, so a claim an exited process left stops
//! being listed at the age its row goes.

use std::time::Duration;

use fq_ops::health::{ClaimHolder, ClaimedMessage, StrandedClaimFinding, StrandedTriggerClaim};

use super::{TriggerClaimState, TriggerDeliveryView, Views, ViewsError};
use crate::control_plane::store::{TriggerClaimRow, UnsettledTriggerClaim};

/// How long an invocation's start may be on record while its claim still
/// says `claimed` before the check calls the claim unmarked.
///
/// The dispatcher marks the claim as soon as it sees the run's first WAL
/// write, so a report taken between the two must not call a healthy start
/// unmarked. The WAL row records neither that write's time nor the mark's,
/// so the grace runs from the row's `started_at`. That is taken after the
/// run's workspace and MCP servers are up and before its `triggered`
/// publish and first reduction, which the first write follows: a broker
/// round trip and one step, normally well under a second. A run whose
/// publish alone took longer than the grace could be called unmarked in
/// the moment before its mark lands.
pub const CLAIM_MARK_GRACE: Duration = Duration::from_secs(60);

/// What a claim is judged against: facts about the daemon answering and
/// the bounds it judges by, which the caller supplies the way every
/// threshold a view judges by is supplied.
#[derive(Debug, Clone, Copy)]
pub struct ClaimJudgement<'a> {
    /// The worker id this daemon claims triggers under.
    pub claimant: &'a str,
    /// When this daemon started dispatching, epoch ms.
    pub dispatching_since_ms: i64,
    /// How long after [`Self::dispatching_since_ms`] a deliverable trigger
    /// an exited process left unacked has been redelivered to this daemon,
    /// if it ever will be: [`crate::bus::TRIGGER_REDELIVERY_WINDOW`].
    pub redelivery_window_ms: i64,
    /// The incarnation of the trigger stream this daemon consumes: its
    /// creation time in nanoseconds ([`crate::bus::EventBus::trigger_stream_epoch`]).
    pub stream_epoch: i64,
    /// How long the trigger stream keeps a message
    /// ([`crate::bus::DEFAULT_TRIGGER_MAX_AGE`]).
    pub stream_max_age_ms: i64,
    /// How old a claim an exited process left may be and still be
    /// reported; `None` reports it for as long as its row exists. The
    /// daemon passes the age at which its retention sweep deletes such a
    /// claim ([`crate::config::StateConfig::trigger_claim_retention_ms`]).
    pub horizon_ms: Option<i64>,
    pub now_ms: i64,
}

impl ClaimJudgement<'_> {
    /// Whether a claim's message can still be delivered.
    ///
    /// A claim on another incarnation of the stream names a message on a
    /// stream that is gone. On this incarnation, a message older than the
    /// stream's maximum age has been removed by the broker; and a claim is
    /// never older than its message, because `claimed_at` is the
    /// dispatcher's clock when it handled a delivery of a message already
    /// published, and an adoption only moves it later (marking leaves it
    /// alone). So a claim older than the maximum age names a message the
    /// stream has dropped, or is about to on its next expiry pass. The
    /// claim's age is two readings of the daemon's wall clock, so this
    /// holds while that clock runs steadily: a clock stepped forward since
    /// the claim (or a claim written on a host whose clock ran behind this
    /// one's) makes a message look aged out up to that step early.
    fn message(&self, claim: &TriggerClaimRow) -> ClaimedMessage {
        if claim.stream_epoch != self.stream_epoch {
            ClaimedMessage::EarlierIncarnation
        } else if self.now_ms.saturating_sub(claim.claimed_at) >= self.stream_max_age_ms {
            ClaimedMessage::AgedOut
        } else {
            ClaimedMessage::Deliverable
        }
    }
}

/// What the stores record of the invocation a claim names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StartRecord {
    /// `in_flight`, the WAL row's terminal phase, or the coordination
    /// status.
    pub status: String,
    /// When a WAL row says the run started; `None` for a coordination
    /// row, which is only written once the run has finished or been
    /// found ambiguous, long after the claim should have been marked.
    pub started_at_ms: Option<i64>,
}

impl From<TriggerClaimRow> for TriggerDeliveryView {
    fn from(row: TriggerClaimRow) -> Self {
        TriggerDeliveryView {
            stream: row.stream,
            stream_epoch: row.stream_epoch.to_string(),
            stream_seq: row.stream_seq,
            state: if row.durably_started {
                TriggerClaimState::DurablyStarted
            } else {
                TriggerClaimState::Claimed
            },
            claimant: row.claimant,
            claimed_at_ms: row.claimed_at,
        }
    }
}

impl Views {
    /// The trigger delivery that started `invocation_id`, from the claim
    /// that names it. `None` when no claim does.
    pub(super) async fn invocation_trigger(
        &self,
        invocation_id: &str,
    ) -> Result<Option<TriggerDeliveryView>, ViewsError> {
        Ok(self
            .control_plane
            .trigger_claim_for_invocation(invocation_id)
            .await?
            .map(TriggerDeliveryView::from))
    }

    /// Every trigger claim that has not settled and will not settle on
    /// its own, oldest first. What qualifies is this module's `judge`;
    /// what the reads cost is in the module doc.
    pub async fn stranded_trigger_claims(
        &self,
        judgement: &ClaimJudgement<'_>,
    ) -> Result<Vec<StrandedTriggerClaim>, ViewsError> {
        let mut stranded = Vec::new();
        for unsettled in self.control_plane.unsettled_trigger_claims().await? {
            let record = self.start_record(&unsettled).await?;
            if let Some(claim) = judge(unsettled.claim, record, judgement) {
                stranded.push(claim);
            }
        }
        Ok(stranded)
    }

    /// What the stores record of the run a claim names: the coordination
    /// row the claim read joined, else the WAL row, else the coordination
    /// row read again. A run's record moves from the WAL to the
    /// coordination table when it is archived, so an archive landing
    /// between the joined read and the WAL read would otherwise leave the
    /// run with no record for this report.
    async fn start_record(
        &self,
        unsettled: &UnsettledTriggerClaim,
    ) -> Result<Option<StartRecord>, ViewsError> {
        let coordination = |status: crate::control_plane::store::OwnerStatus| StartRecord {
            status: status.as_str().to_string(),
            started_at_ms: None,
        };
        if let Some(status) = unsettled.owner_status()? {
            return Ok(Some(coordination(status)));
        }
        let Some(invocation_id) = unsettled.claim.invocation_id.as_deref() else {
            return Ok(None);
        };
        if let Some(row) = self.worker.get_invocation_state(invocation_id).await? {
            return Ok(Some(StartRecord {
                status: if row.terminal_at.is_some() {
                    row.phase
                } else {
                    "in_flight".to_string()
                },
                started_at_ms: Some(row.started_at),
            }));
        }
        Ok(self
            .control_plane
            .get_invocation_owner(invocation_id)
            .await?
            .map(|owner| coordination(owner.status)))
    }
}

/// Judge one `claimed` row. "Deliverable" is [`ClaimJudgement::message`];
/// a run "on record" is a [`StartRecord`].
///
/// | The claim | Verdict |
/// |---|---|
/// | Run on record, WAL row younger than [`CLAIM_MARK_GRACE`] | not listed: a mark that has not landed yet |
/// | Run on record, message deliverable | `StartedUnmarked` |
/// | Run on record, message gone | not listed: no redelivery can follow, so nothing can |
/// | No run on record, held by this daemon | not listed: held, starting or abandoned, and nothing on the row tells which |
/// | No run on record, exited holder, older than the horizon | not listed |
/// | No run on record, exited holder, no invocation id on the row | `StartUnknown` |
/// | No run on record, exited holder, deliverable, this daemon inside the redelivery window | `AwaitingRedelivery` |
/// | No run on record, exited holder, deliverable, past the window | `NeverStarted` |
/// | No run on record, exited holder, message gone | `NeverStarted` |
pub(crate) fn judge(
    claim: TriggerClaimRow,
    record: Option<StartRecord>,
    judgement: &ClaimJudgement<'_>,
) -> Option<StrandedTriggerClaim> {
    let holder = if claim.claimant == judgement.claimant {
        ClaimHolder::ThisDaemon
    } else {
        ClaimHolder::ExitedProcess
    };
    let message = judgement.message(&claim);
    let deliverable = message == ClaimedMessage::Deliverable;
    let now = judgement.now_ms;
    let (finding, invocation_status) = match record {
        Some(StartRecord {
            started_at_ms: Some(started),
            ..
        }) if now.saturating_sub(started) < CLAIM_MARK_GRACE.as_millis() as i64 => return None,
        Some(record) if deliverable => (StrandedClaimFinding::StartedUnmarked, Some(record.status)),
        Some(_) => return None,
        None if holder == ClaimHolder::ThisDaemon => return None,
        None if judgement
            .horizon_ms
            .is_some_and(|horizon| now.saturating_sub(claim.claimed_at) >= horizon) =>
        {
            return None;
        }
        None if claim.invocation_id.is_none() => (StrandedClaimFinding::StartUnknown, None),
        None if deliverable
            && now.saturating_sub(judgement.dispatching_since_ms)
                < judgement.redelivery_window_ms =>
        {
            (StrandedClaimFinding::AwaitingRedelivery, None)
        }
        None => (StrandedClaimFinding::NeverStarted, None),
    };
    let invocation_id = claim.invocation_id.clone();
    Some(StrandedTriggerClaim {
        delivery: TriggerDeliveryView::from(claim),
        invocation_id,
        holder,
        message,
        finding,
        invocation_status,
    })
}

#[cfg(test)]
mod tests;
