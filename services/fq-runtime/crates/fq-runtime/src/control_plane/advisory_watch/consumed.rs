//! The dead-letter watch's answer on an exhausted trigger whose claim says
//! it was consumed without a run (issue 1055): the verdict the records give
//! on a consumed claim ([`ConsumedClaim`]), answered in one function with
//! an arm per verdict.

use tracing::{error, warn};

use super::{AdvisoryOutcome, CONSUMER_NAME, Hold, MaxDeliverAdvisory};
use crate::control_plane::dispatcher::ConsumedClaim;

/// The watch's answer on an exhausted trigger whose claim says it was
/// consumed without a run (issue 1055). With no start on record it was
/// dealt with: no dead letter. With the records that could contradict the
/// claim unread, the verdict is held within the watch's limit like any
/// other; past it no dead letter is written either, since none is due
/// either way, and the claim is left unverified.
pub(super) fn on_a_consumed_claim(
    advisory: &MaxDeliverAdvisory,
    subject: String,
    verdict: ConsumedClaim,
    looked_long_enough: bool,
) -> AdvisoryOutcome {
    let stream_seq = advisory.stream_seq;
    match verdict {
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
            AdvisoryOutcome::ConsumedUnchecked
        }
    }
}
