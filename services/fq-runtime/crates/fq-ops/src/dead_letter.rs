//! The DeadLetter atom's **shapes**: what `dead_letter.get`,
//! `dead_letter.list` and `dead_letter.stream` answer with
//! (`docs/design/committed/operator-surface-domain-model.md`).
//!
//! Shapes only. A dead letter is not a projection fold and has no store
//! of its own — it is one recorded `agent.failed` event, the terminal
//! one the dispatcher or the advisory watcher publishes when a trigger
//! runs out of deliveries, read back through a lens. **That lens stays
//! with the event vocabulary in the runtime**, along with the
//! annotation keys the two emitters write: recognising a dead letter
//! means reading an `Event`, and an `Event` is runtime machinery.
//!
//! What is here is the value that recognition produces — the thing the
//! daemon serialises and every client deserialises. Both ends naming
//! the same struct is what keeps the handler, the schema and the
//! rendering from drifting; a client that hand-rolled these fields
//! would be a second definition of a shape that already exists.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One dead-lettered trigger, as its terminal event records it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DeadLetter {
    pub event_id: String,
    /// When the exhaustion was recorded, RFC3339.
    ///
    /// Declared to the surface as a string rather than a reflected
    /// schema: that is exactly what it serialises as, and reflecting
    /// `chrono::DateTime` would need schemars' chrono integration —
    /// a wider change than this atom (the Event atom made the same
    /// call for its payload tree).
    #[schemars(with = "String")]
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub agent_id: String,
    pub trigger_subject: String,
    /// The original trigger's sequence on the **trigger** stream, and the
    /// selector `dead_letter.requeue` takes. With the incarnation of that
    /// stream, which the event records beside it, it is how the two
    /// emitters' dead letters for one trigger are told from others.
    ///
    /// Both emitters always record it, a dead letter that names no
    /// trigger included: there it is the sequence the broker's advisory
    /// gave, and `unattributed` says what it is a position on. No dead
    /// letter either emitter writes lacks it; it is optional so that a
    /// `trigger_exhausted` event recorded without it still lists. It is
    /// not this atom's identity: it is a coordinate on a different stream,
    /// and it is not unique, neither across incarnations of that stream,
    /// since a recreated stream numbers its messages from 1 again, nor
    /// across dead letters, since one exhaustion can be recorded by two at
    /// the same sequence. The atom is addressed by its **event-log**
    /// sequence — see [`DeadLetterState`].
    pub trigger_stream_seq: Option<u64>,
    /// Which emitter surfaced it: `"inline"` | `"advisory"`.
    pub source: String,
    pub trigger_payload: serde_json::Value,
    pub error_message: String,
    /// Set when the dead-letter watch wrote this without being able to
    /// check whether the trigger's run started. The watch held its
    /// verdict, because a copy of the trigger was still handling the
    /// claim, the run's records could not be read, or the claim changed
    /// between two reads of it, until the advisory was an hour old and the
    /// watch had looked at it three times (for an advisory with no
    /// timestamp the watch can read, three looks only); a last look then
    /// found no start either. Such a dead letter may be false, since the
    /// run may have started or may yet start. One that carries a trigger
    /// id, `dead_letter.requeue` checks against the records again before
    /// it re-runs it: it refuses a start on record, proceeds with nothing
    /// on record, and when it still cannot tell (this mark's claim gone
    /// included) refuses unless told the run was checked. One that carries
    /// no trigger id cannot be requeued at all, and no record is checked
    /// for it: one that names no trigger (`unattributed`); one for a
    /// trigger whose subject names no legal agent, filed under the system
    /// sentinel with that subject and its payload; and one the watch wrote
    /// for a trigger whose `Fq-Trigger-Id` header was missing or not a
    /// UUID, which still records its agent, subject and payload. Its work
    /// can be run again only as new work, which nothing checks, so check
    /// the run first: from its recorded payload with `trigger.publish` for
    /// the last, and published afresh for the others. That check is of the
    /// invocation named here, with `fq invocation show`: it started if that
    /// shows a `Live execution:` block, an `archived:` line or a trigger
    /// line saying `durably started`; a `status:` other than `(no
    /// coordination row)` is a start only under a trigger line that does
    /// not say `dropped by an operator`, since a run an operator dropped
    /// before its first WAL write has one, ended by the drop, without
    /// having started. With no invocation named, `fq doctor` cannot
    /// show that the run did not start: it lists a trigger claim only
    /// while it is still held and judged stranded, never one whose run
    /// started and marked it, though one it lists at the dead letter's
    /// trigger sequence carries the id its delivery reserved for the run.
    /// A dead letter that records a subject can be matched to its run by
    /// trigger subject and payload among the `triggered` events: its
    /// agent's, or every agent's for one under the system sentinel. For
    /// one that records neither, no check on the operator's surfaces can
    /// show that the run did not start, so treat its work as possibly run.
    /// Absent on every other dead letter, which does not make its
    /// start checked: one for an earlier incarnation's advisory
    /// (`unattributed_kind` `earlier_incarnation`) never is, since the
    /// watch consults no claim for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_unchecked: Option<StartUnchecked>,
    /// Why this dead letter names no trigger, in words; `unattributed_kind`
    /// gives the same as a kind to match on. The dead-letter watch sets it
    /// on every dead letter it writes without a trigger, which it records
    /// under the system sentinel with no subject, payload or trigger id,
    /// and which `dead_letter.requeue` refuses. There are three reasons.
    /// The broker answered that the trigger stream holds no message at the
    /// trigger's sequence: the reason gives when the watch read the stream,
    /// when the advisory was emitted and how long the stream keeps a
    /// message, since the message aged out unless it was deleted sooner
    /// (the broker answers the same for a message it cannot load). The
    /// advisory came from an earlier incarnation of the trigger stream, one
    /// the broker emitted before the stream was last created: the reason
    /// gives when the advisory was emitted and when the stream was created,
    /// `trigger_stream_seq` here is a position on that earlier stream,
    /// whose messages went with it, and the message the stream holds at
    /// the same sequence now is another trigger. Or the watch could not
    /// read the trigger's message, for a reason other than its absence,
    /// before its limit ran out (an hour after the advisory, and three
    /// looks): the reason gives what the last read answered and, unless
    /// that was that the trigger stream is missing, says the message may
    /// still be on the stream at `trigger_stream_seq`, which on a stream
    /// created since names another trigger (the event's
    /// `trigger_stream_epoch` names the stream its trigger was on). Absent
    /// on a dead letter that names its trigger, and on one an older build
    /// wrote without a trigger before it recorded why. Absent, too, on the
    /// one other dead letter filed under the system sentinel: one for a
    /// trigger whose subject names no legal agent, which records that
    /// subject and its payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unattributed: Option<String>,
    /// The kind of `unattributed`'s reason, set with it: `message_gone`,
    /// `earlier_incarnation` or `message_unreadable`. A kind a later build
    /// added reads as `unknown` to a reader built before it, and
    /// `unattributed` still says why in words. Absent when `unattributed`
    /// is, and on a dead letter an older build wrote before it recorded the
    /// kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unattributed_kind: Option<UnattributedKind>,
}

/// Why a dead letter names no trigger, as a kind a reader can match on
/// without parsing the reason ([`DeadLetter::unattributed_kind`]).
///
/// A kind this build does not know decodes as `unknown` rather than
/// failing the whole dead letter, and with it a whole page of them: a later
/// build may add a kind, and a client built before it still lists the dead
/// letters, each with its reason in words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UnattributedKind {
    /// The broker answered that the trigger stream holds no message at the
    /// trigger's sequence: it aged out, was deleted, or could not be
    /// loaded.
    MessageGone,
    /// The advisory came from an earlier incarnation of the trigger stream,
    /// so its sequence is not a position on this one.
    EarlierIncarnation,
    /// The trigger's message could not be read, for a reason other than
    /// its absence, before the watch's limit ran out. Unless the last read
    /// found the trigger stream missing, it may still be on the stream.
    MessageUnreadable,
    /// A kind this build does not know, written by a later one; the reason
    /// says why in words.
    #[serde(other)]
    Unknown,
}

/// Why a dead letter's trigger was not checked for a start, and where to
/// look ([`DeadLetter::start_unchecked`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StartUnchecked {
    /// Why the watch could not take its verdict: the claim could not be
    /// read; a copy of the trigger was still handling its claim; the
    /// worker could not say whether its WAL holds the run; the claim could
    /// not be read with its ownership row; or the claim changed while the
    /// run's records were read. A reader that cannot parse the mark says
    /// so here instead.
    pub reason: String,
    /// The invocation the trigger's claim named when the watch last read
    /// it: the run to check (`fq invocation show`) before the work is run
    /// again, when `dead_letter.requeue` cannot tell whether it started,
    /// and before publishing again the work of one that cannot be
    /// requeued. Absent when the claim, or this mark, could not be read.
    pub invocation_id: Option<String>,
}

/// What in `fq invocation show`'s output says a run started, in the one
/// set of words every surface that sends an operator to check a run gives:
/// `dead_letter.requeue`'s refusals, `fq dead-letters list`'s line, and
/// the description of [`DeadLetter::start_unchecked`], which carries it
/// word for word.
///
/// A run that started shows its live execution while the worker's WAL
/// holds it, its archive record once the worker has handed its archive
/// over, and a trigger line saying `durably started` once its claim is
/// marked. Its coordination row, from its durable start, says so too,
/// except for a run an operator dropped before its first WAL write: the
/// drop writes the run's row, ended by the drop, with its request, though
/// no archive record, and its trigger line says so. So the row counts only
/// under a trigger line that does not.
pub const STARTED_IF: &str = "it started if that shows a `Live execution:` block, an `archived:` \
     line or a trigger line saying `durably started`; a `status:` other than `(no coordination \
     row)` is a start only under a trigger line that does not say `dropped by an operator`, since \
     a run an operator dropped before its first WAL write has one, ended by the drop, without \
     having started";

/// Why `fq doctor` not listing a stranded trigger claim for a dead letter
/// does not show that its run did not start, in the words
/// `dead_letter.requeue`'s refusal and `fq dead-letters list` both give
/// it: the report lists a claim only while it is held and judged
/// stranded, and a run that started marks its claim.
pub const AN_UNLISTED_CLAIM_SHOWS_NOTHING: &str = "`fq doctor` lists only a claim still held and \
     judged stranded, never one whose run started and marked it, so its listing none does not \
     show that the run did not start";

/// What is left for a dead letter whose start was not checked and that
/// records nothing to find its run by, in the same words on both
/// surfaces.
pub const NOTHING_SHOWS_IT_DID_NOT_START: &str = "no check on the operator's surfaces can show \
     that its run did not start: treat its work as possibly run";

/// A dead letter addressed by its event-log sequence — the universal
/// cursor (P5): the same number that cursors `dead_letter.stream` and
/// feeds `min_seq` gates.
///
/// It does not ride in a command receipt's `AtomRef`, which names an
/// atom by identity — and this domain has no identity to give it, so
/// no command mints a DeadLetter reference. Addressing a dead letter
/// positionally is a known gap rather than a design: recreate the
/// stream and a stored sequence names a different letter.
///
/// `dead_letter.requeue` is the one command over this domain and it
/// does not close the gap — it steps around it. What a requeue makes is
/// a *trigger*, so its receipt names one, in a different domain from
/// the verb's own. A DeadLetter reference is still not a thing that
/// exists.
///
/// The identity is the log sequence and not the trigger sequence for
/// two reasons, in ascending order of force: the trigger sequence is a
/// coordinate on a *different* stream, and it is not unique. A recreated
/// trigger stream numbers its messages from 1 again, so one sequence can
/// name triggers of two incarnations; and one exhaustion can be recorded
/// by two dead letters at the same sequence, since the dead-letter watch
/// writes a second rather than risk none. No dead letter either emitter
/// writes lacks it; it is optional only so that a `trigger_exhausted`
/// event recorded without it still lists. The Event and Turn atoms are
/// keyed the same way, so a sequence read out of any of the three
/// streams means the same thing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DeadLetterState {
    pub seq: u64,
    /// The fact itself. Nested rather than flattened so the identity
    /// is visibly the atom's and not one of the trigger's own fields.
    pub dead_letter: DeadLetter,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row of a `dead_letter.list` page, as the daemon sends it, naming
    /// no trigger for `kind`.
    fn nameless(kind: &str) -> serde_json::Value {
        serde_json::json!({
            "seq": 9,
            "dead_letter": {
                "event_id": "00000000-0000-7000-8000-000000001066",
                "timestamp": "2026-01-02T03:04:05Z",
                "agent_id": "system",
                "trigger_subject": "",
                "trigger_stream_seq": 4,
                "source": "advisory",
                "trigger_payload": null,
                "error_message": "trigger exhausted after 5 deliveries (limit 5)",
                "unattributed": "why, in words",
                "unattributed_kind": kind,
            },
        })
    }

    /// **The published description of `start_unchecked` gives the words
    /// for telling a start that the refusals and the listing give**, word
    /// for word: one text everywhere, so a correction cannot reach one
    /// surface and miss another.
    #[test]
    fn start_uncheckeds_description_carries_the_shared_words_for_a_start() {
        let schema = serde_json::to_value(schemars::schema_for!(DeadLetter)).unwrap();
        let description = schema["properties"]["start_unchecked"]["description"]
            .as_str()
            .expect("start_unchecked is described");
        let flat = description.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains(STARTED_IF),
            "the description does not carry the shared words:\n{flat}"
        );
    }

    /// **A page holding one dead letter of a kind this build does not know
    /// still decodes**, as a client decodes the daemon's answer: that one
    /// reads as `unknown`, with its reason, and the rest as they are.
    #[test]
    fn a_page_with_a_kind_this_build_does_not_know_decodes() {
        let page: Vec<DeadLetterState> = serde_json::from_value(serde_json::json!([
            nameless("message_gone"),
            nameless("a_kind_a_later_build_added"),
        ]))
        .expect("the page decodes");
        let kinds: Vec<_> = page
            .iter()
            .map(|state| state.dead_letter.unattributed_kind)
            .collect();
        assert_eq!(
            kinds,
            [
                Some(UnattributedKind::MessageGone),
                Some(UnattributedKind::Unknown)
            ]
        );
        assert_eq!(
            page[1].dead_letter.unattributed.as_deref(),
            Some("why, in words")
        );
    }
}
