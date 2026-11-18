//! What is true of a dead letter that carries no trigger id, which
//! `dead_letter.requeue` refuses before it reads any record: which kind it
//! is, from what it records; whether its run may have started; and what
//! can be done with its work.

use fq_ops::dead_letter::{AN_UNLISTED_CLAIM_SHOWS_NOTHING, NOTHING_SHOWS_IT_DID_NOT_START};
use fq_runtime::dead_letter::{DeadLetter, UnattributedKind};

use super::verdict::STARTED_IF;

/// The kinds of dead letter with no trigger id, told apart by what each
/// records.
enum Kind<'a> {
    /// It names no trigger, and says why.
    Nameless(&'a str),
    /// It names no trigger and gives no reason.
    NamelessUnsaid,
    /// The watch read the trigger's message, but its subject names no
    /// legal agent, so the dead letter is filed under `system`.
    NoAgent,
    /// The watch's, for a trigger whose message carried no id it could
    /// read.
    HeaderLess,
    /// The dispatcher's, recorded before triggers were named.
    BeforeNaming,
}

impl<'a> Kind<'a> {
    fn of(dead: &'a DeadLetter) -> Self {
        let reason = dead.unattributed.as_deref().filter(|why| !why.is_empty());
        match (reason, dead.trigger_subject.is_empty()) {
            (Some(reason), _) => Kind::Nameless(reason),
            (None, true) => Kind::NamelessUnsaid,
            (None, false) if dead.agent_id == fq_runtime::AgentId::SYSTEM_STR => Kind::NoAgent,
            (None, false) if dead.source == "advisory" => Kind::HeaderLess,
            (None, false) => Kind::BeforeNaming,
        }
    }
}

/// The refusal's account of `dead`: why it carries no trigger id, whether
/// its start was checked, and what can be done with its work.
pub(super) fn what_is_true(dead: &DeadLetter) -> String {
    let kind = Kind::of(dead);
    [Some(why(dead, &kind)), start(dead), next(dead, &kind)]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Why `dead` carries no trigger id, in its own words when it says.
fn why(dead: &DeadLetter, kind: &Kind<'_>) -> String {
    match kind {
        Kind::Nameless(reason) => format!("It names no trigger, and says why: {reason}."),
        Kind::NamelessUnsaid if dead.unattributed_kind.is_none() => "It names no trigger and \
            gives no reason: a build before the dead-letter watch said why wrote it."
            .to_string(),
        Kind::NamelessUnsaid => "It names no trigger and gives no reason.".to_string(),
        Kind::NoAgent => format!(
            "Its trigger was published to `{}`, a subject that names no agent, so the \
             dead-letter watch filed it under `system`, with that subject and its payload and \
             no trigger id.",
            dead.trigger_subject
        ),
        Kind::HeaderLess => "It records its trigger's agent, subject and payload but no \
            trigger id, and gives no reason: the dead-letter watch wrote it for a trigger \
            published without an `Fq-Trigger-Id` header, or with one that is not a UUID, which \
            reads as none, and it reads the identity and never invents one."
            .to_string(),
        Kind::BeforeNaming => "It records its trigger's agent, subject and payload but no \
            trigger id, and gives no reason: the dispatcher recorded it before triggers were \
            named."
            .to_string(),
    }
}

/// Whether `dead`'s run may have started, when its start was not checked.
fn start(dead: &DeadLetter) -> Option<String> {
    if let Some(mark) = &dead.start_unchecked {
        let look = match &mark.invocation_id {
            Some(run) => format!(
                "read the run it names with `fq invocation show {run}` before you publish its \
                 work again: {STARTED_IF}"
            ),
            None => no_run_named(dead),
        };
        return Some(format!(
            "And its start was not checked ({}), so its run may have started: {look}.",
            mark.reason
        ));
    }
    (dead.unattributed_kind == Some(UnattributedKind::EarlierIncarnation)).then(|| {
        "And its start was never checked: the watch consulted no claim for an earlier \
         stream's advisory, so its run may have started."
            .to_string()
    })
}

/// What can tell whether the run of an unchecked `dead` started, when its
/// mark names no run. Not `fq doctor` alone: it lists a claim only while
/// it is still held, so its listing none shows nothing. The `triggered`
/// events can, for a dead letter that records what its run would have
/// been triggered with.
fn no_run_named(dead: &DeadLetter) -> String {
    let claims = format!(
        "it names no run. A trigger claim `fq doctor` lists at its sequence carries the id its \
         delivery reserved for the run, to read with `fq invocation show`, but \
         {AN_UNLISTED_CLAIM_SHOWS_NOTHING}"
    );
    let runs = match (dead.trigger_subject.is_empty(), dead.agent_id.as_str()) {
        (true, _) => {
            return format!(
                "{claims}. With no subject or payload on it to match among the `triggered` \
                 events, {NOTHING_SHOWS_IT_DID_NOT_START}"
            );
        }
        (false, fq_runtime::AgentId::SYSTEM_STR) => {
            "fq events query --event-type triggered".to_string()
        }
        (false, agent) => format!("fq events query --agent {agent} --event-type triggered"),
    };
    format!(
        "{claims}. To find its run before you publish its work again, list the runs with \
         `{runs}`, find the one whose trigger subject and payload are this dead letter's with \
         `fq events get <event-id>`, and check it with `fq invocation show <id>`: {STARTED_IF}"
    )
}

/// What can be done with `dead`'s work: published again from its payload,
/// or, when nothing on it can be published again, published afresh.
fn next(dead: &DeadLetter, kind: &Kind<'_>) -> Option<String> {
    match kind {
        Kind::HeaderLess | Kind::BeforeNaming => Some(
            "Its payload is on the record (`dead_letter.list`), so re-running it is \
             `trigger.publish` — new work, named honestly as new work."
                .to_string(),
        ),
        Kind::NoAgent => Some(
            "Nothing can publish it again as it is: `trigger.publish` addresses an agent, \
             and its subject names none. The work has to be published afresh to the agent it \
             was meant for."
                .to_string(),
        ),
        Kind::Nameless(_) | Kind::NamelessUnsaid => {
            let afresh = "It records no agent, subject or payload, so nothing on it can be \
                          published again: the work has to be published afresh by whatever \
                          published it.";
            Some(match (dead.unattributed_kind, dead.trigger_stream_seq) {
                (Some(UnattributedKind::MessageUnreadable), Some(seq)) => format!(
                    "{afresh} Unless its reason says the trigger stream was missing, the \
                     trigger's message may still be on the trigger stream at sequence {seq}, \
                     if that stream is still the one its trigger was on: its \
                     `trigger_stream_epoch` (in `fq events get <event-id> --json`) is the \
                     creation time, in nanoseconds since 1970, of the stream its trigger was \
                     on, and on a stream created since, sequence {seq} names another trigger."
                ),
                _ => afresh.to_string(),
            })
        }
    }
}
