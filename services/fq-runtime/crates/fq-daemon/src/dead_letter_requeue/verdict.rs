//! What `dead_letter.requeue` asks before it re-runs a dead letter's
//! trigger: whether the trigger's run started (issue 1059).
//!
//! A dead letter says a trigger ran out of deliveries, not that its run
//! never started. The dead-letter watch writes one only once it has found
//! nothing of the run on record, except that it writes one anyway, marked
//! `start_unchecked`, when it could not take its verdict within its limit;
//! and a run can start after any dead letter is written, by a copy its
//! writer did not see. Requeueing a trigger whose run started runs the
//! work twice. So requeue asks the records the watch asks, through the
//! watch's own check ([`ExhaustionCheck`]), with the dead letter in hand
//! and before anything is reserved or published:
//!
//! - a start on record is refused, naming the run, whatever the request
//!   says;
//! - nothing on record proceeds, marked or not;
//! - when it cannot tell, the request is refused unless it says the
//!   operator has checked the run (`checked_not_started`).
//!
//! **What is read.** The trigger's claim, and for the run the claim names
//! the worker's WAL and the ownership table, as the check reads them for
//! the watch. When that leaves it unable to tell (no claim is there to
//! consult, the claims cannot be read, or a copy still handles the claim)
//! and the dead letter's mark names a run, those two records are read for
//! that run too, by its own id ([`ExhaustionCheck::start_of_run`]). A start
//! found in any of them is a start on record; only a start found counts,
//! so a read that fails is no answer, and the verdict stays "cannot tell".
//!
//! **Which claim, by what key.** The claim table keys a delivery on the
//! trigger stream, the stream's incarnation and the sequence. A dead letter
//! carries the sequence; the incarnation is the one the check's claims are
//! keyed on, the dispatcher's. A dead letter written before that
//! incarnation was created names a sequence of the earlier stream, and the
//! claim this one holds at that sequence, if any, is another trigger's, so
//! no claim is consulted: it has none to consult. (One the watch wrote for
//! an advisory of an earlier incarnation, or for a trigger that had aged
//! out of the stream, names no trigger at all, and the command refuses it
//! before it gets here.) The comparison is the dead letter's timestamp,
//! this daemon's clock when it was written, against the stream's creation,
//! the broker's: a skew between the two clocks moves the line by that much,
//! near a recreation. Issue 1077 is to record the incarnation on the dead
//! letter instead.
//!
//! **A claim that is gone.** A claim given back before its run started
//! leaves no row, and so does one the retention sweep deleted, `claimed`
//! by a process that has exited or marked started, once it was older than
//! `[state] trigger_claim_retention_days`. The two look the same, and only
//! the first is "nothing on record". When the check finds no claim
//! ([`Exhausted::NoClaim`]), or there is none to consult:
//!
//! - **An unmarked dead letter proceeds.** Its writer found nothing of the
//!   run on record: the dispatcher wrote it on the last delivery and gave
//!   the claim back before acking it, or the watch found no claim, or a
//!   claim with no start and no copy handling it. No delivery follows the
//!   last one, so a run starts after it only by a copy its writer did not
//!   see, such as a replay of the stream, and that run's claim is marked
//!   started: requeue refuses it while the claim is kept. A dead letter is
//!   an event, kept for `[events] max_age` (30 days by default), and a
//!   claim is kept for `[state] trigger_claim_retention_days` (90 by
//!   default, two at least), so with the defaults a dead letter leaves the
//!   log long before its claim can be swept. Only a claim window set below
//!   the event log's, or an event log kept longer than the claims, leaves
//!   a dead letter whose run's claim has gone, and that one reads as
//!   nothing on record (issue 1076).
//! - **A marked one cannot be told**, unless the run its mark names has a
//!   start on record by its own id. Its writer could not check, so nothing
//!   says what was on record when it was written, and a claim absent now
//!   is either one given back without a start or one swept after a start.
//!   With the windows above, the sweep can come while the dead letter is
//!   still listed only when the claim window is set below the event log's
//!   (near its two-day floor, within days of the dead letter, since the
//!   claim may be older than it by as long as the watch held its verdict
//!   and the daemon was down). The rule does not depend on which: a row
//!   absent for either reason looks the same.
//!
//! The dispatcher's own dead letter, written on the fifth failed delivery,
//! is the first kind: its claim is released as the delivery is answered,
//! so requeue finds no claim and proceeds, as it always did, before or
//! after any sweep. In the moment between the dead letter's publish and
//! that release, a copy is still handling the claim, and requeue says so.
//!
//! **The look.** [`ExhaustionCheck::last_look`], not the bare verdict: the
//! watch takes it when it is about to act on a verdict it could not take,
//! and requeue is always there, since it acts on its answer at once and has
//! no later look to wait for. It adds one read of the worker's WAL for the
//! run the claim names, and trusts only a start found, so it can only turn
//! "cannot tell" into a refusal the override does not pass.
//!
//! **The mark.** A start the check finds in the records, on a claim still
//! `claimed`, is marked on the claim, as it is for the watch, the
//! dispatcher and startup recovery. That is the one write a refused
//! requeue makes, and it is right on an operator's behalf for the reason
//! it is right for the others: the records already say the run started,
//! and until the claim says so too, a redelivered or replayed copy of the
//! trigger can adopt the claim and start the run a second time, the
//! double run requeue exists to refuse. The mark is a compare-and-set that
//! leaves a claim given back, taken again or marked already as it is, and
//! the refusal says what became of it. Not marking would mean a second
//! decision beside the check's, and the hazard left in place until the
//! daemon next starts. A start found by the run's own id marks nothing: no
//! claim was read.

use fq_edge::wire::WireError;
use fq_runtime::TriggerDispatcher;
use fq_runtime::control_plane::dispatcher::{ClaimMark, Exhausted, ExhaustionCheck};
use fq_runtime::dead_letter::DeadLetter;

use super::OP;

/// The records `dead_letter.requeue` asks whether a dead letter's
/// trigger started its run: the dispatcher's, through the check the
/// dead-letter watch takes its verdict by.
///
/// Built only from the dispatcher ([`Self::of`]), as the watch is, so the
/// operator surface cannot be assembled with a requeue that re-runs a
/// trigger without looking.
#[derive(Clone)]
pub struct TriggerRecords {
    /// `None` for a dispatcher with no claim store, a test's: every
    /// verdict is then "cannot tell".
    check: Option<ExhaustionCheck>,
}

impl TriggerRecords {
    /// The records `dispatcher` keeps of its triggers' runs.
    pub fn of(dispatcher: &TriggerDispatcher) -> Self {
        let check = dispatcher.exhaustion_check();
        if check.is_none() {
            tracing::warn!(
                op = OP,
                "the dispatcher holds no claim store, so `dead_letter.requeue` has no records to \
                 consult: it refuses every requeue unless the request says the operator checked \
                 the run"
            );
        }
        Self { check }
    }
}

/// What the records hold for one dead letter's trigger.
#[derive(Debug)]
enum Found {
    /// The check's last look at the claim at the dead letter's sequence.
    Look(Exhausted),
    /// The claims were not consulted, for the reason given: there is no
    /// claim of this stream to consult.
    NoClaimToConsult(&'static str),
    /// The dispatcher keeps no records.
    NoRecords,
}

/// What requeue does with one dead letter.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Nothing of the run is on record: requeue it.
    Proceed,
    /// The run has a start on record: never requeue it.
    Started {
        invocation_id: String,
        on_record: String,
        /// What became of the claim, or `None` for a start found by the
        /// run's own id, when no claim was read.
        mark: Option<ClaimMark>,
    },
    /// Cannot tell: requeue it only on the operator's word.
    CannotTell {
        why: String,
        /// The run to check: the one the claim names, else the one the
        /// dead letter's mark names.
        invocation_id: Option<String>,
        /// Whether the reason can clear, so that asking again later may
        /// let the daemon take the verdict itself.
        may_clear: bool,
    },
}

/// Why a dead letter written before the stream the claims are keyed on
/// has no claim to consult.
const EARLIER_STREAM: &str = "it was written before the trigger stream was last created, so its \
     sequence is the earlier stream's, and the claim at that sequence now, if there is one, is \
     another trigger's";

/// Why a dead letter that records no sequence has no claim to consult.
const NO_SEQUENCE: &str =
    "it records no sequence on the trigger stream, so no claim can be found for it";

/// Why a dead letter whose claim the check did not find cannot be told.
const NO_CLAIM: &str = "no claim names its delivery any more: a claim given back before its run \
     started leaves no row, and so does one the retention sweep deleted after its run started";

/// Refuse the requeue of `dead` unless the records allow it: a start on
/// record always, and "cannot tell" unless `checked_not_started`.
/// `selected` is whether the request named the dead letter by its
/// sequence, so a refusal of one taken by default can say so.
pub(super) async fn admit(
    records: &TriggerRecords,
    dead: &DeadLetter,
    selected: bool,
    checked_not_started: bool,
) -> Result<(), WireError> {
    let mut verdict = verdict_on(dead, look(records, dead).await);
    if matches!(verdict, Verdict::CannotTell { .. }) {
        verdict = with_the_marked_run(verdict, marked_run_start(records, dead).await);
    }
    match verdict {
        Verdict::Proceed => Ok(()),
        Verdict::Started {
            invocation_id,
            on_record,
            mark,
        } => {
            let claim = mark.as_ref().map_or_else(
                || "not read: found by the run's own id".to_string(),
                ClaimMark::to_string,
            );
            tracing::warn!(op = OP, event_id = %dead.event_id, agent_id = %dead.agent_id,
                trigger_stream_seq = ?dead.trigger_stream_seq, %invocation_id, run = %on_record,
                %claim, checked_not_started, "refused to requeue a dead letter whose trigger's \
                 run has a start on record");
            Err(started(
                dead,
                selected,
                &invocation_id,
                &on_record,
                mark.as_ref(),
            ))
        }
        // The only record that an operator overrode a verdict the daemon
        // could not take: why it could not, and the run it named.
        Verdict::CannotTell {
            why, invocation_id, ..
        } if checked_not_started => {
            tracing::warn!(op = OP, event_id = %dead.event_id, agent_id = %dead.agent_id,
                trigger_stream_seq = ?dead.trigger_stream_seq, reason = %why,
                invocation_id = %invocation_id.as_deref().unwrap_or("none"),
                "requeueing a dead letter whose trigger's start the records cannot tell, on the \
                 operator's word that its run did not start");
            Ok(())
        }
        Verdict::CannotTell {
            why,
            invocation_id,
            may_clear,
        } => Err(cannot_tell(
            dead,
            selected,
            &why,
            invocation_id.as_deref(),
            may_clear,
        )),
    }
}

/// What the records hold for `dead`'s trigger.
async fn look(records: &TriggerRecords, dead: &DeadLetter) -> Found {
    let Some(check) = &records.check else {
        return Found::NoRecords;
    };
    let Some(stream_seq) = dead.trigger_stream_seq else {
        return Found::NoClaimToConsult(NO_SEQUENCE);
    };
    if dead
        .timestamp
        .timestamp_nanos_opt()
        .is_some_and(|written| written < check.stream_epoch())
    {
        return Found::NoClaimToConsult(EARLIER_STREAM);
    }
    Found::Look(check.last_look(stream_seq).await)
}

/// The run `dead`'s mark names and where its start is on record, if the
/// records hold one by that run's own id.
async fn marked_run_start(records: &TriggerRecords, dead: &DeadLetter) -> Option<(String, String)> {
    let check = records.check.as_ref()?;
    let run = dead.start_unchecked.as_ref()?.invocation_id.clone()?;
    let on_record = check.start_of_run(&run).await?;
    Some((run, on_record))
}

/// A verdict that could not be taken from the claim, given what the
/// records hold by the id of the run the dead letter's mark names: a start
/// found there is a start on record. Pure, so it is tested without a store.
fn with_the_marked_run(verdict: Verdict, marked_run: Option<(String, String)>) -> Verdict {
    match (verdict, marked_run) {
        (Verdict::CannotTell { .. }, Some((invocation_id, on_record))) => Verdict::Started {
            invocation_id,
            on_record,
            mark: None,
        },
        (verdict, _) => verdict,
    }
}

/// The verdict on `dead`, from what its trigger's claim and the run the
/// claim names hold. Pure, so every arm is tested without a store.
fn verdict_on(dead: &DeadLetter, found: Found) -> Verdict {
    let mark = dead.start_unchecked.as_ref();
    let marked_invocation = || mark.and_then(|m| m.invocation_id.clone());
    // No claim to consult: nothing on record for an unmarked dead letter,
    // whose writer found none; cannot tell for a marked one, whose writer
    // could not look, unless the run its mark names is found started by
    // its own id (`with_the_marked_run`; the module doc has the why).
    let claim_gone = |why: &str| match mark {
        None => Verdict::Proceed,
        Some(mark) => Verdict::CannotTell {
            why: format!(
                "it is marked `start_unchecked` ({}), and {why}; {}",
                mark.reason,
                if mark.invocation_id.is_some() {
                    "nothing was found on record under the run its mark names"
                } else {
                    "its mark names no run to look up"
                }
            ),
            invocation_id: mark.invocation_id.clone(),
            may_clear: false,
        },
    };
    // Exhaustive on purpose. Issue 1055's claim of a trigger consumed
    // without its run starting cannot arrive as `NothingOnRecord`, since
    // the watch dead-letters on that: it will need an `Exhausted` variant
    // of its own, and this match an arm for it. It never ran, so that arm
    // proceeds.
    match found {
        Found::NoRecords => Verdict::CannotTell {
            why: "this daemon's dispatcher holds no claim store, so there are no records to \
                  consult"
                .to_string(),
            invocation_id: marked_invocation(),
            may_clear: false,
        },
        Found::NoClaimToConsult(why) => claim_gone(why),
        Found::Look(Exhausted::NoClaim) => claim_gone(NO_CLAIM),
        // A claim is there, and nothing of the run it names is on record.
        Found::Look(Exhausted::NothingOnRecord) => Verdict::Proceed,
        Found::Look(Exhausted::Started {
            invocation_id,
            on_record,
            mark,
        }) => Verdict::Started {
            invocation_id,
            on_record,
            mark: Some(mark),
        },
        Found::Look(Exhausted::Undecided { why, invocation_id }) => Verdict::CannotTell {
            why,
            invocation_id: invocation_id.or_else(marked_invocation),
            may_clear: true,
        },
    }
}

/// How a refusal names the dead letter it is about: its sequence and its
/// event, and, when the request named no sequence, that it was the
/// agent's newest.
fn which(dead: &DeadLetter, selected: bool) -> String {
    let at = match dead.trigger_stream_seq {
        Some(seq) => format!("at trigger sequence {seq}"),
        None => "with no trigger sequence".to_string(),
    };
    if selected {
        format!("the dead letter {at} (event `{}`)", dead.event_id)
    } else {
        format!(
            "the newest dead letter of agent `{}`, {at} (event `{}`), taken because the request \
             gave no `trigger_seq` (`--trigger-seq`),",
            dead.agent_id, dead.event_id
        )
    }
}

/// What the operator does once they have checked the run: the command
/// that requeues `dead` on their word, which names it by its sequence.
fn on_your_word(dead: &DeadLetter) -> String {
    match dead.trigger_stream_seq {
        Some(seq) => format!(
            "Once you have checked that it did not start, ask again with `checked_not_started` \
             set: `fq dead-letters requeue {} --trigger-seq {seq} --checked-not-started`.",
            dead.agent_id
        ),
        // The override names its dead letter by sequence, and this one has
        // none to be named by.
        None => "It records no trigger sequence, so the override, which names its dead letter \
                 by sequence, cannot reach it: once you have checked that the run did not start, \
                 publish the work again as new work with `trigger.publish`."
            .to_string(),
    }
}

/// What in `fq invocation show`'s output says a run started.
const STARTED_IF: &str = "it started if that shows a `Live execution:` block, an `archived:` \
     line, a `status:` other than `(no coordination row)` or a trigger line saying `durably \
     started`";

/// The refusal for a trigger whose run has a start on record. A
/// `Conflict`, as for a dead letter already requeued: the request was
/// right, the work ran, and no edit to the request changes that.
fn started(
    dead: &DeadLetter,
    selected: bool,
    invocation_id: &str,
    on_record: &str,
    mark: Option<&ClaimMark>,
) -> WireError {
    let claim = match mark {
        Some(mark) => format!("Its claim: {mark}."),
        None => "No claim answered for the trigger, so this was found under the run's own id, \
                 the one the dead letter's mark names, and no claim was marked."
            .to_string(),
    };
    WireError::Conflict {
        op: OP.into(),
        message: format!(
            "{} is for a trigger whose run has a start on record: invocation `{invocation_id}`, \
             {on_record}. Requeueing it would run the work twice, so nothing was reserved or \
             published, and `checked_not_started` (`--checked-not-started`) does not change \
             that: it is for a run the daemon cannot tell about, never one with a start on \
             record. Read the run with `fq invocation show {invocation_id}` (`invocation.get`). \
             To run the work again anyway, publish it as new work with `trigger.publish`. {claim}",
            which(dead, selected)
        ),
    }
}

/// The refusal for a trigger whose start the records cannot tell. An
/// `InvalidInput`, as `invocation.drop` refuses live work without `live`:
/// the edit that answers it is the override, once the run is checked.
fn cannot_tell(
    dead: &DeadLetter,
    selected: bool,
    why: &str,
    invocation_id: Option<&str>,
    may_clear: bool,
) -> WireError {
    // What asking again can do. For a marked dead letter it is less than
    // it looks: if the claim is given back meanwhile, the next answer is
    // "cannot tell" again, for the claim being gone.
    let clears = match (may_clear, dead.start_unchecked.is_some()) {
        (false, _) => "",
        (true, false) => {
            " That can clear: asked again once it has, the daemon takes the verdict \
             itself."
        }
        (true, true) => {
            " That can clear, but asked again once it has, the daemon takes the \
             verdict itself only while the claim is there: if the claim has been given back by \
             then, a dead letter marked like this one is still one it cannot tell about, unless \
             the run its mark names is on record."
        }
    };
    let check = match invocation_id {
        Some(id) => format!("Check the run with `fq invocation show {id}`: {STARTED_IF}."),
        None => format!(
            "No run is named, since no claim naming one could be read and the dead letter's mark \
             names none: list the agent's runs with `fq events query --agent {agent} --type \
             triggered`, find the one whose trigger payload is this dead letter's with `fq \
             events get <event-id>`, and check it with `fq invocation show <id>`: {STARTED_IF}.",
            agent = dead.agent_id
        ),
    };
    WireError::InvalidInput {
        op: OP.into(),
        message: format!(
            "{} may be for a trigger whose run started, and the daemon cannot tell: {why}. \
             Requeueing it would run the work twice if the run started, so nothing was reserved \
             or published.{clears} {check} A copy of the trigger can still start the run after \
             you look. {} A start on record is refused whatever you pass.",
            which(dead, selected),
            on_your_word(dead)
        ),
    }
}

#[cfg(test)]
mod tests;
