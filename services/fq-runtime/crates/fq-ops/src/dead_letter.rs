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
    /// The original trigger's sequence on the **trigger** stream — the
    /// key that reconciles the inline and advisory emitters, and the
    /// selector `dead_letter.requeue` takes.
    ///
    /// Absent when the advisory arrived after the trigger had aged
    /// out, which is why it is not this atom's identity: an identity
    /// that can be missing is not one. The atom is addressed by its
    /// **event-log** sequence — see [`DeadLetterState`].
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
    /// watch had looked at it three times; a last look then found no start
    /// either. Such a dead letter may be false, since the run may have
    /// started or may yet start. `dead_letter.requeue` checks the records
    /// again before it re-runs it: it refuses a start on record, proceeds
    /// with nothing on record, and when it still cannot tell (this mark's
    /// claim gone included) refuses unless told the run was checked. That
    /// check is of the invocation named here: the run started if
    /// `fq invocation show` gives a `Live execution:` block, an
    /// `archived:` line, a `status:` other than `(no coordination row)` or
    /// a trigger line saying `durably started`. With no invocation named,
    /// find the run among the agent's `triggered` events. Absent on every
    /// other dead letter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_unchecked: Option<StartUnchecked>,
    /// Why this dead letter names no trigger, when the dead-letter watch
    /// wrote it for an advisory from an earlier incarnation of the trigger
    /// stream: one the broker emitted before the stream was last created.
    /// It gives when the advisory was emitted and when the stream was
    /// created. The advisory's sequence, `trigger_stream_seq` here, is a
    /// position on that earlier stream, whose messages went with it, and
    /// the message the stream holds at the same sequence now is another
    /// trigger, so the dead letter names none: it is recorded under the
    /// system sentinel with no subject, payload or trigger id, and
    /// `dead_letter.requeue` refuses it. Absent on every other dead letter,
    /// including one written under the system sentinel because its trigger
    /// had aged out of the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unattributed: Option<String>,
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
    /// it: the run to check (`fq invocation show`) when
    /// `dead_letter.requeue` cannot tell whether it started. Absent when
    /// the claim, or this mark, could not be read.
    pub invocation_id: Option<String>,
}

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
/// three reasons, in ascending order of force: the trigger sequence is
/// a coordinate on a *different* stream, it is not unique across
/// agents, and it can be absent altogether. The Event and Turn atoms
/// are keyed the same way, so a sequence read out of any of the three
/// streams means the same thing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DeadLetterState {
    pub seq: u64,
    /// The fact itself. Nested rather than flattened so the identity
    /// is visibly the atom's and not one of the trigger's own fields.
    pub dead_letter: DeadLetter,
}
