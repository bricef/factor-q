//! The dead-letter watch's answer on an exhausted trigger whose claim says
//! it was consumed without a run (issue 1055): the verdict the records give
//! on a consumed claim ([`ConsumedClaim`]), answered in one function with
//! an arm per verdict.

use tracing::{error, info, warn};

use super::{AdvisoryOutcome, CONSUMER_NAME, Hold, MaxDeliverAdvisory};
use crate::bus::ConsumerLedger;
use crate::control_plane::dispatcher::ConsumedClaim;
use crate::control_plane::store::ConsumedWhy;

/// The watch's answer on an exhausted trigger whose claim says it was
/// consumed without a run (issue 1055). With no start on record it was
/// dealt with: no dead letter. With the records that could contradict the
/// claim unread, the verdict is held within the watch's limit like any
/// other; past it no dead letter is written either, since none is due
/// either way, and the claim is left unverified. Each verdict that writes
/// no dead letter is counted on `ledger` as kept out as consumed; a held
/// one is not yet a verdict. A claim consumed as `dead_lettered` is the
/// exception, checked or, past the limit, not: the dispatcher wrote that
/// trigger's dead letter as it consumed the claim, so the watch withheld
/// none, and it is answered as a dead letter already written
/// ([`already_dead_lettered`]).
pub(super) fn on_a_consumed_claim(
    advisory: &MaxDeliverAdvisory,
    subject: String,
    verdict: ConsumedClaim,
    looked_long_enough: bool,
    ledger: &ConsumerLedger,
) -> AdvisoryOutcome {
    let stream_seq = advisory.stream_seq;
    match verdict {
        ConsumedClaim::Checked {
            invocation_id,
            why: ConsumedWhy::DeadLettered,
            ..
        } => already_dead_lettered(advisory, subject, invocation_id.as_deref()),
        ConsumedClaim::Checked {
            invocation_id,
            why,
            operator_dropped,
        } => {
            warn!(consumer = CONSUMER_NAME, subject, stream_seq,
                deliveries = advisory.deliveries,
                consumed_invocation_id = %invocation_id.as_deref().unwrap_or("none"),
                consumed_why = %why, operator_dropped,
                "not dead-lettering a trigger whose deliveries ran out: it was consumed \
                 without a run, so it was dealt with, and a `trigger_exhausted` dead letter \
                 would be a second record of it");
            ledger.note_consumed_not_dead_lettered(CONSUMER_NAME);
            AdvisoryOutcome::Consumed
        }
        ConsumedClaim::Unchecked {
            invocation_id,
            unread,
            ..
        } if !looked_long_enough => AdvisoryOutcome::Held(Hold {
            why: unread,
            stream_seq,
            subject,
            invocation_id: Some(invocation_id),
        }),
        ConsumedClaim::Unchecked {
            invocation_id,
            why: ConsumedWhy::DeadLettered,
            ..
        } => already_dead_lettered(advisory, subject, Some(&invocation_id)),
        ConsumedClaim::Unchecked {
            invocation_id,
            why,
            operator_dropped,
            unread,
        } => {
            error!(consumer = CONSUMER_NAME, subject, stream_seq,
                deliveries = advisory.deliveries, consumed_invocation_id = invocation_id,
                consumed_why = %why, operator_dropped, reason = unread,
                "not dead-lettering a trigger whose deliveries ran out: its claim says it was \
                 consumed without a run, and the records that could say otherwise still cannot \
                 be read, so the claim is unverified; no dead letter is due either way, since a \
                 run on record started and one not on record was dealt with; check the run \
                 with `fq invocation show`");
            ledger.note_consumed_not_dead_lettered(CONSUMER_NAME);
            AdvisoryOutcome::ConsumedUnchecked
        }
    }
}

/// An exhausted trigger whose claim says it was consumed as dead-lettered:
/// the dispatcher published its dead letter as it consumed the claim, so
/// one is already written. It is answered as a dead letter already written
/// is, and counted in neither of the watch's counts, since the watch
/// withheld none. The watch reads the claim first, and the claim records
/// it, so no read of the log is needed. Its line, at info, says what the
/// claim records and names the run the claim names.
fn already_dead_lettered(
    advisory: &MaxDeliverAdvisory,
    subject: String,
    invocation_id: Option<&str>,
) -> AdvisoryOutcome {
    info!(consumer = CONSUMER_NAME, subject, stream_seq = advisory.stream_seq,
        consumed_invocation_id = %invocation_id.unwrap_or("none"),
        consumed_why = %ConsumedWhy::DeadLettered,
        "not dead-lettering a trigger whose deliveries ran out: its claim records that the \
         dispatcher published its dead letter, so one is already written");
    AdvisoryOutcome::AlreadySurfaced
}
