//! The stranded-trigger-claims block of `fq doctor` (#954): claimed
//! deliveries that have not settled, each with what the report can stand
//! behind about it.
//!
//! Its own module because the wording is the substance. Every line is a
//! claim about a delivery, a message or a run, and each is worded for what
//! the claim row and the stores actually show: a delivery that never
//! started a run is not "work that never ran", since the same work may
//! have run under another delivery this claim cannot see.

use fq_ops::health::{ClaimHolder, ClaimedMessage, StrandedClaimFinding, StrandedTriggerClaim};
use fq_ops::surface::DoctorTriggerClaims;

/// Pure: the block. One line per stranded claim — the delivery, who
/// claimed it and when, and the invocation id it names — and one or two
/// lines under it saying what that means. "none" when every claim has
/// settled or is one the report does not judge, so the block is always on
/// the page.
///
/// The holder is named in words rather than left to an id the reader
/// would have to compare with this daemon's: "a process that has exited"
/// is the fact the verdict rests on.
pub(super) fn render_trigger_claims(claims: &DoctorTriggerClaims) -> String {
    if claims.stranded.is_empty() {
        return "Stranded trigger claims: none\n".to_string();
    }
    let issues = claims
        .stranded
        .iter()
        .filter(|c| c.finding.is_issue())
        .count();
    let mut out = format!(
        "Stranded trigger claims: {} ({issues} issues) — claimed trigger deliveries that have \
         not settled\n",
        claims.stranded.len()
    );
    for claim in &claims.stranded {
        out.push_str(&claim_line(claim));
        out.push_str(&finding_lines(claim, claims));
    }
    out
}

/// The line naming the delivery: its stream and sequence (with the
/// stream's incarnation when it is not the one this daemon consumes,
/// since the same sequence there is another message), who claimed it and
/// when, and the invocation id it names — labelled as the id reserved for
/// a run wherever no run is known to exist.
fn claim_line(claim: &StrandedTriggerClaim) -> String {
    let d = &claim.delivery;
    let incarnation = match claim.message {
        ClaimedMessage::EarlierIncarnation => match created_at(&d.stream_epoch) {
            Some(created) => format!(" (an earlier incarnation of the stream, created {created})"),
            None => format!(
                " (an earlier incarnation of the stream, epoch {})",
                d.stream_epoch
            ),
        },
        ClaimedMessage::Deliverable | ClaimedMessage::AgedOut => String::new(),
    };
    let holder = match claim.holder {
        ClaimHolder::ThisDaemon => "this daemon".to_string(),
        ClaimHolder::ExitedProcess => format!("{}, a process that has exited", d.claimant),
    };
    let invocation = match (&claim.invocation_id, claim.finding) {
        (None, _) => "no invocation id on the claim".to_string(),
        (Some(id), StrandedClaimFinding::StartedUnmarked) => format!("invocation {id}"),
        (Some(id), _) => format!("reserved invocation id {id}"),
    };
    format!(
        "  {} seq {}{incarnation}: claimed {} by {holder}; {invocation}\n",
        d.stream,
        d.stream_seq,
        utc_seconds(d.claimed_at_ms)
    )
}

/// What the finding means, and what to do about it.
fn finding_lines(claim: &StrandedTriggerClaim, claims: &DoctorTriggerClaims) -> String {
    let window = format_span(claims.redelivery_window_ms);
    let invocation = claim.invocation_id.as_deref().unwrap_or("<id>");
    let started = match claim.invocation_status.as_deref() {
        Some(status) => format!("the run started ({status})"),
        None => "the run started".to_string(),
    };
    match (claim.finding, claim.holder) {
        (StrandedClaimFinding::AwaitingRedelivery, _) => format!(
            "  -> not started; if its message is still deliverable, this daemon adopts it and \
             starts a run when the broker redelivers it, which is due within {window} of this \
             daemon starting\n"
        ),
        (StrandedClaimFinding::NeverStarted, _) => {
            let why = match claim.message {
                ClaimedMessage::Deliverable => format!(
                    "it has not been redelivered within {window} of this daemon starting, so its \
                     last delivery was most likely dead-lettered (`fq dead-letters list`)"
                ),
                ClaimedMessage::EarlierIncarnation => {
                    "its message belonged to an earlier incarnation of the stream".to_string()
                }
                ClaimedMessage::AgedOut => format!(
                    "its message is older than the stream keeps messages ({})",
                    format_span(claims.stream_max_age_ms)
                ),
            };
            let until = match claims.horizon_ms {
                Some(horizon) => format!("until the claim is {} old", format_span(horizon)),
                None => "while the claim row exists".to_string(),
            };
            format!(
                "  -> this delivery never started a run and will not be redelivered: {why}\n  \
                 -> work requeued or published again runs under a new sequence this claim cannot \
                 show, so this line stays {until}\n"
            )
        }
        (StrandedClaimFinding::StartedUnmarked, ClaimHolder::ThisDaemon) => format!(
            "  -> {started} but its claim was never marked started: this daemon drops its \
             redeliveries, and the trigger will be dead-lettered as exhausted although its run \
             started; do not requeue that dead letter — `fq invocation show {invocation}`\n"
        ),
        (StrandedClaimFinding::StartedUnmarked, ClaimHolder::ExitedProcess) => format!(
            "  -> {started} but its claim was never marked started: a redelivery would be adopted \
             and start the work again, and a false `trigger_exhausted` dead letter may already \
             exist — `fq invocation show {invocation}`\n"
        ),
        (StrandedClaimFinding::StartUnknown, _) => "  -> the claim was written before claims \
             named their run, so this report cannot tell whether it started one; not an issue\n"
            .to_string(),
    }
}

/// A stream incarnation, a creation time in nanoseconds carried as a
/// decimal string, as that time in UTC.
fn created_at(stream_epoch: &str) -> Option<String> {
    let nanos: i64 = stream_epoch.parse().ok()?;
    Some(utc_seconds(nanos.div_euclid(1_000_000)))
}

fn utc_seconds(at_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(at_ms)
        .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| format!("{at_ms}ms"))
}

/// A span as the largest unit that states it exactly: `30d`, `24h`, or
/// `19m30s` (`20m` when it is whole minutes).
pub(super) fn format_span(ms: i64) -> String {
    const DAY: i64 = 86_400_000;
    const HOUR: i64 = 3_600_000;
    if ms > 0 && ms % DAY == 0 {
        return format!("{}d", ms / DAY);
    }
    if ms > 0 && ms % HOUR == 0 {
        return format!("{}h", ms / HOUR);
    }
    let (minutes, seconds) = (ms / 60_000, (ms % 60_000) / 1_000);
    if seconds == 0 {
        format!("{minutes}m")
    } else {
        format!("{minutes}m{seconds}s")
    }
}
