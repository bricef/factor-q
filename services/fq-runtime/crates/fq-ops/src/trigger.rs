//! The Trigger atom: what asked an agent to run.
//!
//! The shape and the names it travels under — the identity header, the
//! payload ceiling, and the subject vocabulary. Learning about a trigger
//! from a broker message or an event stays in `fq-runtime`, beside the
//! things it reads.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::agent::AgentId;
use crate::events::TriggerSource;
use crate::events::subjects;

/// The NATS header a trigger's identity travels in.
///
/// Named rather than reserved-prefixed (`Nats-*` belongs to the
/// server): a header is the one place a fact can ride a message whose
/// body is contractually opaque. Its value is the id's canonical
/// hyphenated UUID text.
pub const TRIGGER_ID_HEADER: &str = "Fq-Trigger-Id";

/// The largest trigger payload `trigger.publish` will accept, in bytes
/// of the JSON body as it goes on the wire.
///
/// **A trigger is kept forever, so its payload needs a ceiling.**
/// Unbounded retention of an unbounded field is the one combination
/// that cannot be allowed to ship; before this, the only bound on a
/// payload was the broker's.
///
/// **The number is derived, not chosen.** Four quantities frame it:
///
/// - One `trigger.get` answer is one edge frame, and both ends of the
///   edge frame with `LengthDelimitedCodec::new()`, whose default
///   ceiling is **8 MiB** (8,388,608 bytes). Get answers with the whole
///   trigger, so a payload has to fit inside that with its envelope.
/// - The trigger crosses NATS, and `max_payload` is where the
///   *deployment* draws its own line: the dogfood broker sets **16 MB**
///   (`ops/dogfood/infra/nats.conf`), but a stock `nats-server` defaults
///   to **1 MiB** — and JetStream charges the body *plus* the
///   `Fq-Trigger-Id` header against it.
/// - A real payload is a task description. The production row the
///   DeadLetter atom's cap was measured against carried a
///   github-watcher task (`task`/`refs`/`constraints`/`done_criteria`/
///   `github`) in **328 bytes**.
///
/// 512 KiB (524,288 bytes) is ~1,600x that production payload — so
/// "generous" is honest rather than a word — sits **16x under the edge
/// frame**, so a Get plus its envelope is never near it, and sits at
/// **half a stock broker's default `max_payload`**, so the header and
/// JetStream's framing have room whatever broker the runtime is pointed
/// at.
///
/// **That last quantity is why this is not 1 MiB**, which was the
/// obvious landing spot and is the one value that is exactly wrong: it
/// is the first size at which the edge accepts a trigger that a
/// default-configured broker then refuses, and the publisher's answer
/// for it is a publish-ack timeout rather than this limit's name. A
/// domain guarantee that only holds on one broker's config is not one.
/// Verified rather than reasoned: a 1 MiB publish against the test
/// broker fails on the ack, which is what sent this number down.
///
/// What this cap does **not** fix: `dead_letter.list` embeds a trigger
/// payload per row and caps a page at 500, so a page of maximal
/// payloads still outgrows the frame. That was already true with no
/// cap at all and is the dead-letter atom's own bound to make; this one
/// bounds what is *accepted*, which is the part that is kept forever.
pub const MAX_TRIGGER_PAYLOAD_BYTES: usize = 512 * 1024;

/// The subject one agent's triggers travel on.
///
/// **Takes an [`AgentId`], not a `&str`, and that is the point.**
/// `AgentId::new` runs `subjects::validate_token`, so a value of this
/// type cannot contain a `.`, `*`, `>` or whitespace — the four
/// characters that would turn one agent's subject into a wildcard over
/// somebody else's. The subject this builds is therefore well-formed
/// *by construction* rather than by the caller having remembered.
///
/// Before #43 this was `bus::trigger_subject(&str)`: the transport
/// module owned the trigger domain's names, and owned them in a form
/// that accepted any string at all. The wire spelling now lives with
/// the rest of the vocabulary in [`subjects`]; what lives here is the
/// domain's contract about who may name a trigger.
pub fn subject(agent: &AgentId) -> String {
    subjects::trigger(agent.as_str())
}

/// Every agent's triggers — the trigger stream's capture pattern and
/// the dispatcher's default consumer filter.
pub const ALL_SUBJECTS: &str = subjects::ALL_TRIGGERS;

/// Recover the agent id from a trigger subject, or `None` if the
/// subject is not one. The inverse of [`subject`], for the dispatcher
/// reading a delivered message back off the wire.
pub fn agent_id_from_subject(subject: &str) -> Option<&str> {
    subjects::agent_id_from_trigger(subject)
}

/// One trigger, named — the request an invocation answers.
///
/// This is the value the control-plane hands a worker; the worker
/// records [`Trigger::id`] on the invocation's `triggered` event, which
/// is what finally lets a reader say *this invocation came from that
/// trigger* rather than inferring it from matching content.
///
/// It is also the Trigger atom's **state** — what `trigger.get` and
/// `trigger.stream` answer with, whole. Not a wrapper around it with a
/// log position bolted on, as [`crate::dead_letter::DeadLetterState`]
/// and `EventState` are: those atoms are addressed by position and this
/// one is addressed by name, which is the entire point of the identity
/// landing first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Trigger {
    /// The trigger's identity: a UUIDv7, like every other identity the
    /// runtime mints (`event_id`, `invocation_id`). Time-sortable, so
    /// ids order the way the triggers did.
    ///
    /// Declared to the surface as a string rather than a reflected
    /// schema: that is exactly what it serialises as, and reflecting
    /// `Uuid` would need schemars' uuid integration — a wider change
    /// than this atom ([`crate::dead_letter::DeadLetter`] made the same
    /// call for its chrono timestamp).
    #[schemars(with = "String")]
    pub id: Uuid,
    /// How the runtime came by this trigger.
    pub source: TriggerSource,
    /// The subject it arrived on, when it arrived on one.
    pub subject: Option<String>,
    /// The trigger body, verbatim — an opaque JSON value the target
    /// agent interprets (wire contract §The payload).
    pub payload: Value,
    /// The trigger this one was requeued from, when it was one —
    /// `dead_letter.requeue`'s record of what it re-ran.
    ///
    /// **Absent on every trigger the runtime learns about from the
    /// wire**, and that is structural rather than an omission: lineage
    /// rides no header and the body is contractually opaque, so there
    /// is nowhere on the wire it could come from. [`Trigger::
    /// requeue_of`] is the only constructor that sets it, which makes
    /// "this trigger is a requeue" a claim only the requeue path can
    /// make.
    ///
    /// It is also the idempotency key `dead_letter.requeue` turns on:
    /// the column behind this field is uniquely indexed, so a dead
    /// letter can be requeued at most once and the second attempt is
    /// answered with the trigger the first one made.
    ///
    /// `serde(default)` because trigger rows written before requeues
    /// were recorded read NULL, and a required field would break them.
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub requeued_from: Option<Uuid>,
}

impl Trigger {
    /// Take responsibility for a trigger that has no name yet, minting
    /// one. The direct-run paths (`fq`-driven manual runs, tests, the
    /// sim) enter here: nothing published them, so nothing named them.
    pub fn mint(source: TriggerSource, subject: Option<String>, payload: Value) -> Self {
        Self::named(Uuid::now_v7(), source, subject, payload)
    }

    /// Adopt a trigger that is **already** named — the publisher's id
    /// wins, always. Re-minting here would silently fork the identity
    /// of a trigger someone else can already name.
    pub fn named(id: Uuid, source: TriggerSource, subject: Option<String>, payload: Value) -> Self {
        Self {
            id,
            source,
            subject,
            payload,
            requeued_from: None,
        }
    }

    /// The trigger a requeue mints from one that dead-lettered: the
    /// same work, a new name, and a record of where it came from.
    ///
    /// **A new identity, not the original's.** Republishing under the
    /// old id would write no new row — the `triggers` table is
    /// `INSERT OR IGNORE` on the identity — so nothing would
    /// distinguish "published once" from "published, then requeued",
    /// and there would be no record for a second requeue to fail on. A
    /// requeue is also genuinely a later trigger, so a fresh UUIDv7
    /// sorts where it belongs.
    ///
    /// The payload is the original's, verbatim. Anything else would be
    /// a different task wearing this one's provenance.
    pub fn requeue_of(original: &Trigger, subject: String) -> Self {
        Self {
            id: Uuid::now_v7(),
            // How the runtime will come by it: the requeue puts it back
            // on the agent's trigger subject, which is where the
            // dispatcher reads it from.
            source: TriggerSource::Subject,
            subject: Some(subject),
            payload: original.payload.clone(),
            requeued_from: Some(original.id),
        }
    }
}
