//! The trigger-to-invocation link, read for an operator: which delivery
//! started an invocation, and which deliveries were taken and never
//! settled.
//!
//! The link is the dispatcher's claim row (ADR-0032): one per trigger
//! delivery, keyed on the message's place on the trigger stream, naming
//! the invocation it starts from the moment it is claimed. Nothing else
//! records it. The worker's WAL row is deleted once its archive is
//! acknowledged; the claim row is not deleted at all once it is started.
//!
//! **Neither read scales with a list.** `invocation_id` has no index on
//! the claim table and the table is not swept, so a lookup by
//! invocation reads it through. It is therefore made only where one
//! invocation is shown ([`Views::invocation`]), and the stranded-claim
//! check reads the table once, in one statement, for every row it
//! judges. Indexing and retention for the table are their own change;
//! nothing here needs either.

use std::time::Duration;

use fq_ops::health::{ClaimHolder, StrandedClaimFinding, StrandedTriggerClaim};

use super::{TriggerClaimState, TriggerDeliveryView, Views, ViewsError};
use crate::control_plane::store::{TriggerClaimRow, UnsettledTriggerClaim};

/// How long an invocation's start may be on record while its claim
/// still says `claimed` before the check calls the claim unmarked.
///
/// The dispatcher marks the claim as soon as it sees the first WAL write,
/// two awaits after it lands, so the gap is normally milliseconds; a
/// report taken inside it must not call a healthy start unmarked.
pub const CLAIM_MARK_GRACE: Duration = Duration::from_secs(60);

/// What a claim is judged against: facts about the daemon answering, and
/// the redelivery window, which the caller supplies the way every
/// threshold a view judges by is supplied.
#[derive(Debug, Clone, Copy)]
pub struct ClaimJudgement<'a> {
    /// The worker id this daemon claims triggers under.
    pub claimant: &'a str,
    /// When this daemon started dispatching, epoch ms.
    pub dispatching_since_ms: i64,
    /// How long after [`Self::dispatching_since_ms`] a trigger an exited
    /// process left unacked has certainly been redelivered to this daemon,
    /// if it ever will be: [`crate::bus::TRIGGER_REDELIVERY_WINDOW`].
    pub redelivery_window_ms: i64,
    pub now_ms: i64,
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
            stream_epoch: row.stream_epoch,
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
    /// its own, oldest first (see [`judge`] for what qualifies).
    ///
    /// One statement reads every `claimed` row with the coordination
    /// status of its invocation; a row with none is then looked up in the
    /// worker WAL by primary key. Neither touches the `durably_started`
    /// rows that make up nearly all of the table.
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

    async fn start_record(
        &self,
        unsettled: &UnsettledTriggerClaim,
    ) -> Result<Option<StartRecord>, ViewsError> {
        if let Some(status) = unsettled.owner_status {
            return Ok(Some(StartRecord {
                status: status.as_str().to_string(),
                started_at_ms: None,
            }));
        }
        let Some(invocation_id) = unsettled.claim.invocation_id.as_deref() else {
            return Ok(None);
        };
        Ok(self
            .worker
            .get_invocation_state(invocation_id)
            .await?
            .map(|row| StartRecord {
                status: if row.terminal_at.is_some() {
                    row.phase
                } else {
                    "in_flight".to_string()
                },
                started_at_ms: Some(row.started_at),
            }))
    }
}

/// Judge one `claimed` row.
///
/// - **Its invocation's start is on record:** stranded, `StartedUnmarked`,
///   whoever holds it — the claim should have been marked when the run
///   made its first WAL write. A WAL row younger than [`CLAIM_MARK_GRACE`]
///   is left alone, as a mark that has not landed yet.
/// - **No start on record, held by this daemon:** not stranded. The
///   trigger may be held behind a paused model, a full agent or the
///   worker cap, or starting; it may also be abandoned. Nothing on the
///   row tells those apart, so it is not reported at all.
/// - **No start on record, left by an exited process:** `AwaitingRedelivery`
///   until this daemon has been dispatching for the redelivery window,
///   `NeverStarted` after.
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
    let grace_ms = CLAIM_MARK_GRACE.as_millis() as i64;
    let window_ms = judgement.redelivery_window_ms;
    let (finding, invocation_status) = match record {
        Some(StartRecord {
            started_at_ms: Some(started),
            ..
        }) if judgement.now_ms.saturating_sub(started) < grace_ms => return None,
        Some(record) => (StrandedClaimFinding::StartedUnmarked, Some(record.status)),
        None => match holder {
            ClaimHolder::ThisDaemon => return None,
            ClaimHolder::ExitedProcess
                if judgement
                    .now_ms
                    .saturating_sub(judgement.dispatching_since_ms)
                    < window_ms =>
            {
                (StrandedClaimFinding::AwaitingRedelivery, None)
            }
            ClaimHolder::ExitedProcess => (StrandedClaimFinding::NeverStarted, None),
        },
    };
    let invocation_id = claim.invocation_id.clone();
    Some(StrandedTriggerClaim {
        delivery: TriggerDeliveryView::from(claim),
        invocation_id,
        holder,
        finding,
        invocation_status,
    })
}

#[cfg(test)]
mod tests;
