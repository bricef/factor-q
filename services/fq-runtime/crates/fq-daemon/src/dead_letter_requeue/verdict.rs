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
//! **A consumed claim** (issue 1055) says the trigger was dealt with and
//! its run never started. It is not taken on trust: a start on record for
//! the run it names is a start, as above. With none, the claim's reason
//! decides. Consumed as dead-lettered, the dead letter is the dispatcher's
//! own, and it proceeds. Requeued from a hold, the trigger's replacement
//! was published under the same trigger id, so it is refused whatever the
//! request says, as a start on record is. For any other reason the daemon
//! writes no dead letter, so this one and the claim disagree, and one of
//! them may be about another trigger: it is refused unless the operator
//! says they checked that the dead letter's own work did not run, and the
//! refusal says what the daemon found, not that the run may have started.
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
use fq_runtime::control_plane::dispatcher::{ClaimMark, ConsumedClaim, Exhausted, ExhaustionCheck};
use fq_runtime::control_plane::store::ConsumedWhy;
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
        /// What the refusal says of the trigger's claim.
        claim: OfTheClaim,
    },
    /// The trigger was requeued from a hold, so its replacement went out
    /// under the same trigger id: never requeue it.
    Replaced {
        /// The run the claim names, for the log line.
        invocation_id: Option<String>,
    },
    /// The trigger was consumed for a reason that writes no dead letter,
    /// and no start is on record for the run its claim names: this dead
    /// letter and the claim disagree, so requeue it only on the operator's
    /// word that its own work did not run.
    Disagrees {
        why: ConsumedWhy,
        /// The run the claim names.
        invocation_id: Option<String>,
        /// Whether the claim records an operator's drop of that run.
        operator_dropped: bool,
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

/// What a refusal of a start on record says of the trigger's claim.
#[derive(Debug, PartialEq, Eq)]
enum OfTheClaim {
    /// The start was found through the claim, and this became of it.
    Marked(ClaimMark),
    /// No claim answered, so the start was found under the run the dead
    /// letter's mark names, by its own id.
    NoAnswer,
    /// The claim answered that the trigger was consumed without a run as
    /// `why`, an answer that writes no dead letter, so it disagrees with
    /// this one, and the start was found under the run the dead letter's
    /// mark names.
    Disagreed { why: ConsumedWhy },
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
    if matches!(
        verdict,
        Verdict::CannotTell { .. } | Verdict::Disagrees { .. }
    ) {
        verdict = with_the_marked_run(verdict, marked_run_start(records, dead).await);
    }
    answered(dead, selected, checked_not_started, verdict)
}

/// What requeue answers on `verdict`: the refusal, or `Ok` to go on. Pure
/// but for its log lines, so every refusal is tested without a store.
fn answered(
    dead: &DeadLetter,
    selected: bool,
    checked_not_started: bool,
    verdict: Verdict,
) -> Result<(), WireError> {
    match verdict {
        Verdict::Proceed => Ok(()),
        Verdict::Started {
            invocation_id,
            on_record,
            claim,
        } => {
            let field = match &claim {
                OfTheClaim::Marked(mark) => mark.to_string(),
                OfTheClaim::NoAnswer => "not read: found by the run's own id".to_string(),
                OfTheClaim::Disagreed { why } => {
                    format!("consumed as `{why}`, disagreeing: found by the run's own id")
                }
            };
            tracing::warn!(op = OP, event_id = %dead.event_id, agent_id = %dead.agent_id,
                trigger_stream_seq = ?dead.trigger_stream_seq, %invocation_id, run = %on_record,
                claim = %field, checked_not_started, "refused to requeue a dead letter whose \
                 trigger's run has a start on record");
            Err(started(dead, selected, &invocation_id, &on_record, &claim))
        }
        Verdict::Replaced { invocation_id } => {
            tracing::warn!(op = OP, event_id = %dead.event_id, agent_id = %dead.agent_id,
                trigger_stream_seq = ?dead.trigger_stream_seq,
                invocation_id = %invocation_id.as_deref().unwrap_or("none"), checked_not_started,
                "refused to requeue a dead letter whose trigger was requeued from a hold: its \
                 replacement was published under the same trigger id");
            Err(replaced(dead, selected))
        }
        // The only record that an operator overrode a claim that disagrees
        // with its dead letter: the claim's reason, and the run it names.
        Verdict::Disagrees {
            why, invocation_id, ..
        } if checked_not_started => {
            tracing::warn!(op = OP, event_id = %dead.event_id, agent_id = %dead.agent_id,
                trigger_stream_seq = ?dead.trigger_stream_seq, consumed_why = %why,
                invocation_id = %invocation_id.as_deref().unwrap_or("none"),
                "requeueing a dead letter whose trigger's claim says it was consumed for a reason \
                 that writes no dead letter, on the operator's word that its work did not run");
            Ok(())
        }
        Verdict::Disagrees {
            why,
            invocation_id,
            operator_dropped,
        } => Err(disagrees(
            dead,
            selected,
            &why,
            invocation_id.as_deref(),
            operator_dropped,
        )),
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
            claim: OfTheClaim::NoAnswer,
        },
        (Verdict::Disagrees { why, .. }, Some((invocation_id, on_record))) => Verdict::Started {
            invocation_id,
            on_record,
            claim: OfTheClaim::Disagreed { why },
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
    // Exhaustive on purpose: a state added to the claim is an arm here.
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
        Found::Look(Exhausted::Consumed(consumed)) => on_a_consumed_claim(consumed),
        Found::Look(Exhausted::Started {
            invocation_id,
            on_record,
            mark,
        }) => Verdict::Started {
            invocation_id,
            on_record,
            claim: OfTheClaim::Marked(mark),
        },
        Found::Look(Exhausted::Undecided { why, invocation_id }) => Verdict::CannotTell {
            why,
            invocation_id: invocation_id.or_else(marked_invocation),
            may_clear: true,
        },
    }
}

/// The verdict on a dead letter whose trigger's claim says it was consumed
/// without its run starting, and no start found says otherwise (issue
/// 1055). Only a trigger consumed as dead-lettered has a dead letter by
/// design, its own, and it proceeds. One requeued from a hold never does,
/// checked or not: its replacement went out under the same trigger id. For
/// any other reason this dead letter and the claim disagree, and it waits
/// for the operator's word. A claim whose records could not be read cannot
/// be told, and only for one consumed as dead-lettered can that clear: read,
/// the records would leave any other reason disagreeing all the same.
fn on_a_consumed_claim(consumed: ConsumedClaim) -> Verdict {
    match consumed {
        ConsumedClaim::Checked {
            why: ConsumedWhy::DeadLettered,
            ..
        } => Verdict::Proceed,
        ConsumedClaim::Checked {
            why: ConsumedWhy::RequeuedFromHold,
            invocation_id,
            ..
        } => Verdict::Replaced { invocation_id },
        ConsumedClaim::Unchecked {
            why: ConsumedWhy::RequeuedFromHold,
            invocation_id,
            ..
        } => Verdict::Replaced {
            invocation_id: Some(invocation_id),
        },
        ConsumedClaim::Checked {
            why,
            invocation_id,
            operator_dropped,
        } => Verdict::Disagrees {
            why,
            invocation_id,
            operator_dropped,
        },
        ConsumedClaim::Unchecked {
            invocation_id,
            why,
            unread,
            ..
        } => Verdict::CannotTell {
            may_clear: why == ConsumedWhy::DeadLettered,
            why: format!(
                "its trigger's claim says it was consumed without a run as `{why}`, and {unread}"
            ),
            invocation_id: Some(invocation_id),
        },
    }
}

/// How a refusal names the dead letter it is about: its sequence and its
/// event, and, when the request named no sequence, that it was the
/// agent's newest.
pub(super) fn which(dead: &DeadLetter, selected: bool) -> String {
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

/// What the operator does once they have checked that `checked` (what the
/// refusal asked them to check): the command that requeues `dead` on their
/// word, which names it by its sequence.
fn on_your_word(dead: &DeadLetter, checked: &str) -> String {
    match dead.trigger_stream_seq {
        Some(seq) => format!(
            "Once you have checked that {checked}, ask again with `checked_not_started` set: `fq \
             dead-letters requeue {} --trigger-seq {seq} --checked-not-started`.",
            dead.agent_id
        ),
        // The override names its dead letter by sequence, and this one has
        // none to be named by. Only a dead letter no claim was consulted
        // for comes here: a claim is found by the sequence.
        None => "It records no trigger sequence, so the override, which names its dead letter \
                 by sequence, cannot reach it: once you have checked that the run did not start, \
                 publish the work again as new work with `trigger.publish`."
            .to_string(),
    }
}

/// What in `fq invocation show`'s output says a run started.
pub(super) const STARTED_IF: &str = "it started if that shows a `Live execution:` block, an `archived:` \
     line, a `status:` other than `(no coordination row)` or a trigger line saying `durably \
     started`";

/// What in `fq invocation show`'s output says a run started, for the runs
/// of a trigger whose claim says it was consumed (issue 1055). An operator
/// may have dropped such a run before its first WAL write, and the drop
/// writes the `archived:` record and the coordination status that
/// [`STARTED_IF`] counts as a start; the trigger line tells the two apart.
const STARTED_IF_DROPPED_SHOWN: &str = "it started if that shows a `Live execution:` block or a \
     trigger line saying `durably started`. An `archived:` line or a `status:` other than `(no \
     coordination row)` is a start only under a trigger line that does not say `dropped by an \
     operator`: a run an operator dropped before its first WAL write shows both without having \
     started, and its trigger line says it was consumed without a run and dropped by an operator";

/// The refusal for a trigger whose run has a start on record. A
/// `Conflict`, as for a dead letter already requeued: the request was
/// right, the work ran, and no edit to the request changes that.
fn started(
    dead: &DeadLetter,
    selected: bool,
    invocation_id: &str,
    on_record: &str,
    claim: &OfTheClaim,
) -> WireError {
    let claim = match claim {
        OfTheClaim::Marked(mark) => format!("Its claim: {mark}."),
        OfTheClaim::NoAnswer => "No claim answered for the trigger, so this was found under the \
                                 run's own id, the one the dead letter's mark names, and no \
                                 claim was marked."
            .to_string(),
        OfTheClaim::Disagreed { why } => format!(
            "Its claim answered that the trigger was consumed without a run as `{why}`, which \
             disagrees with this dead letter, since no dead letter is written for that answer: \
             the start was found under the run the dead letter's mark names, and no claim was \
             marked."
        ),
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
             names none: list the agent's runs with `fq events query --agent {agent} \
             --event-type triggered`, find the one whose trigger payload is this dead letter's \
             with `fq events get <event-id>`, and check it with `fq invocation show <id>`: \
             {STARTED_IF}.",
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
            on_your_word(dead, "it did not start")
        ),
    }
}

/// The refusal for a trigger requeued from a hold. A `Conflict`, as for a
/// start on record: the work was handed to the replacement, and no edit to
/// the request changes that.
fn replaced(dead: &DeadLetter, selected: bool) -> WireError {
    WireError::Conflict {
        op: OP.into(),
        message: format!(
            "{} is for a trigger requeued from a hold: its claim says it was consumed without a \
             run as `requeued_from_hold`, so its replacement was already published under the \
             same trigger id. Requeueing this dead letter would run the work twice, so nothing \
             was reserved or published, and `checked_not_started` (`--checked-not-started`) \
             does not change that: it is for a run the daemon cannot tell about, and this \
             trigger's work was handed to its replacement. To run the work again anyway, \
             publish it as new work with `trigger.publish`.",
            which(dead, selected)
        ),
    }
}

/// The refusal for a trigger consumed for a reason that writes no dead
/// letter, with no start on record for the run its claim names. An
/// `InvalidInput`, as for "cannot tell": the edit that answers it is the
/// override, once the operator has checked this dead letter's own work. It
/// says what the daemon read, since that is not in doubt: the doubt is
/// whether the claim and the dead letter are about the same trigger.
fn disagrees(
    dead: &DeadLetter,
    selected: bool,
    why: &ConsumedWhy,
    invocation_id: Option<&str>,
    operator_dropped: bool,
) -> WireError {
    let found = match (invocation_id, operator_dropped) {
        (None, _) => "it names no run, so no record could show a start".to_string(),
        (Some(id), true) => format!(
            "the daemon found no start on record for the run the claim names, `{id}`, in the \
             worker's WAL, the one record that shows a start for a run the claim records an \
             operator dropped, since the drop writes the run's ownership row"
        ),
        (Some(id), false) => format!(
            "the daemon found no start on record for the run the claim names, `{id}`, in either \
             the worker's WAL or the ownership table"
        ),
    };
    WireError::InvalidInput {
        op: OP.into(),
        message: format!(
            "{} is not requeued on the records alone: its trigger's claim says it was consumed \
             without a run as `{why}`, and {found}. But the daemon writes no dead letter for a \
             trigger it consumes as `{why}`, so this dead letter and the claim disagree, and one \
             of them may be about another trigger: what the claim says of its run settles \
             nothing about this dead letter's work. Nothing was reserved or published. Before \
             you override, check that this dead letter's own work did not run: list the \
             agent's runs with `fq events query --agent {agent} --event-type triggered`, find any \
             whose trigger payload is this dead letter's with `fq events get <event-id>`, and \
             check each with `fq invocation show <id>`: {STARTED_IF_DROPPED_SHOWN}. {} A start \
             on record is refused whatever you pass.",
            which(dead, selected),
            on_your_word(dead, "its work did not run"),
            agent = dead.agent_id
        ),
    }
}

#[cfg(test)]
mod tests;
