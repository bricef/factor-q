//! Which trigger an advisory names, as its dead letter records it: the
//! message at the advisory's sequence on the trigger stream, or nothing,
//! and why.
//!
//! An advisory from an earlier incarnation of the stream is not read at
//! all (issue 1054). For one of this incarnation, the broker answers the
//! read in one of three ways (pinned against a real broker in
//! `read_tests`), and they mean different things (issue 1066):
//!
//! - the message;
//! - no message at that sequence, the client's
//!   `RawMessageErrorKind::NoMessageFound` (the broker's error code 10037):
//!   the broker's word for a message that aged out of the stream, was
//!   deleted, or never was, and also for one its store failed to load,
//!   which the client cannot tell apart. The watch takes it as gone and
//!   writes the dead letter at once, naming none and saying so, with when
//!   it read the stream and when the advisory was emitted;
//! - anything else: the request timed out, the stream is missing (code
//!   10059), the broker erred. The message may well be on the stream, and a
//!   later read may name it, so the verdict is held, as one is when the
//!   records cannot be read, and bounded the same way: past the watch's
//!   limit the dead letter is written naming no trigger, and says the
//!   message could not be read.
//!
//! The stream handle is made without asking the broker
//! (`get_stream_no_info`), so the read is one request and its one answer
//! decides.
//!
//! Every dead letter that names no trigger says why, in words and as a kind
//! ([`Unattributed`]): [`Named::Nothing`] is the only way to name nothing,
//! and it carries both.

use async_nats::jetstream::ErrorCode;
use async_nats::jetstream::message::StreamMessage;
use async_nats::jetstream::stream::RawMessageErrorKind;
use tracing::info;
use uuid::Uuid;

use super::{AdvisoryWatch, CONSUMER_NAME, Hold, MaxDeliverAdvisory};
use crate::agent::AgentId;
use crate::bus::{DEFAULT_TRIGGER_MAX_AGE, TRIGGER_STREAM_NAME};
use crate::dead_letter::{
    DEAD_LETTER_UNATTRIBUTED_KEY, DEAD_LETTER_UNATTRIBUTED_KIND_KEY, UnattributedKind,
};
use crate::events::Event;
use crate::trigger::{agent_id_from_subject, trigger_id_in};

/// The trigger an advisory names, as its dead letter records it
/// ([`AdvisoryWatch::named_trigger`]): its message, or nothing and why.
pub(super) enum Named {
    /// The trigger's message, read off the stream.
    Trigger(TriggerMessage),
    /// No trigger, so no agent, subject, payload or id; and why.
    Nothing(Unattributed),
}

impl Named {
    /// No trigger, for the reason of `kind`.
    fn nothing(kind: UnattributedKind, reason: String) -> Self {
        Self::Nothing(Unattributed { kind, reason })
    }
}

/// What the trigger's message gives its dead letter.
pub(super) struct TriggerMessage {
    /// The trigger's agent, or none when the subject names no valid agent:
    /// the dead letter then goes under the system sentinel, still with the
    /// subject and payload.
    pub(super) agent_id: Option<AgentId>,
    pub(super) subject: String,
    pub(super) payload: serde_json::Value,
    /// The id the trigger's publisher gave it, if it gave one.
    pub(super) trigger_id: Option<Uuid>,
}

/// Why a dead letter names no trigger: the kind, and the reason in words.
pub(super) struct Unattributed {
    kind: UnattributedKind,
    reason: String,
}

impl Unattributed {
    /// The reason, in words.
    pub(super) fn reason(&self) -> &str {
        &self.reason
    }

    /// `event`, a dead letter that names no trigger, annotated with why
    /// ([`DEAD_LETTER_UNATTRIBUTED_KEY`], [`DEAD_LETTER_UNATTRIBUTED_KIND_KEY`]).
    pub(super) fn annotate(&self, event: Event) -> Event {
        event
            .annotate(
                DEAD_LETTER_UNATTRIBUTED_KEY,
                serde_json::Value::String(self.reason.clone()),
            )
            .annotate(
                DEAD_LETTER_UNATTRIBUTED_KIND_KEY,
                serde_json::to_value(self.kind).unwrap_or_default(),
            )
    }
}

impl AdvisoryWatch {
    /// The trigger `advisory` names, as its dead letter records it: the
    /// message at the advisory's sequence, read off the trigger stream as
    /// it is now, when the advisory was emitted for this incarnation of the
    /// stream. `earlier` is when it was emitted if that was before this
    /// incarnation was created ([`MaxDeliverAdvisory::emitted_before`]).
    ///
    /// An advisory from an earlier incarnation is not read at all (issue
    /// 1054). Its sequence is a position on the stream before this one,
    /// whose messages went with it, and the message this stream holds at
    /// the same sequence is another trigger: attributed to it, the dead
    /// letter would name a trigger that may have run, or may never have
    /// been exhausted, and requeuing it would run that work again. So it
    /// names no trigger, as one whose message aged out of the stream does,
    /// and says why ([`crate::dead_letter::DEAD_LETTER_UNATTRIBUTED_KEY`]),
    /// and the exhaustion is still recorded.
    ///
    /// A message the read cannot get, for any reason but its absence (issue
    /// 1066), may still be on the stream, so the verdict is held (the
    /// `Err`) until the watch has looked `looked_long_enough`: then the
    /// trigger is not named, and the dead letter says the message could not
    /// be read.
    pub(super) async fn named_trigger(
        &self,
        advisory: &MaxDeliverAdvisory,
        earlier: Option<chrono::DateTime<chrono::Utc>>,
        looked_long_enough: bool,
    ) -> Result<Named, Hold> {
        if let Some(emitted) = earlier {
            info!(consumer = CONSUMER_NAME, stream_seq = advisory.stream_seq,
                timestamp = ?advisory.timestamp,
                "an advisory from an earlier incarnation of the trigger stream: its sequence \
                 names a message of the stream before this one, so the watch reads neither the \
                 message nor the claim this stream has at that sequence, and dead-letters it \
                 naming no trigger");
            return Ok(Named::nothing(
                UnattributedKind::EarlierIncarnation,
                unattributed_reason(emitted, self.bus.trigger_stream_epoch()),
            ));
        }
        // The original trigger: subject → agent, payload → annotation.
        // Triggers age out after 24h — an older advisory still
        // surfaces, attributed to the system sentinel.
        //
        // The identity is read, never assigned. This watch did not
        // dispatch the trigger, so it has no standing to name it: if
        // the publisher stamped a header the id is recorded, and if the
        // trigger arrived unnamed (or has aged out of the stream) the
        // dead letter says so by carrying no name, rather than
        // inventing one that matches nothing anywhere else.
        match read_trigger(&self.bus.jetstream(), advisory.stream_seq).await {
            TriggerRead::Found(raw) => Ok(Named::Trigger(TriggerMessage {
                agent_id: agent_id_from_subject(&raw.subject).and_then(|id| AgentId::new(id).ok()),
                subject: raw.subject.to_string(),
                payload: serde_json::from_slice(&raw.payload).unwrap_or(serde_json::Value::Null),
                trigger_id: trigger_id_in(Some(&raw.headers)),
            })),
            TriggerRead::Gone => Ok(Named::nothing(
                UnattributedKind::MessageGone,
                gone_reason(chrono::Utc::now(), advisory.emitted_at()),
            )),
            TriggerRead::Unreadable { answer, .. } if !looked_long_enough => Err(Hold {
                why: held_while_unreadable(&answer),
                stream_seq: advisory.stream_seq,
                subject: String::new(),
                invocation_id: None,
            }),
            TriggerRead::Unreadable {
                answer,
                stream_missing,
            } => Ok(Named::nothing(
                UnattributedKind::MessageUnreadable,
                unreadable_at_the_limit(&answer, stream_missing),
            )),
        }
    }

    /// The subject of the message at `stream_seq` on the trigger stream,
    /// for a log line that names the trigger, or empty when the read does
    /// not find one: a line is no reason to hold a verdict.
    pub(super) async fn subject_at(&self, stream_seq: u64) -> String {
        match read_trigger(&self.bus.jetstream(), stream_seq).await {
            TriggerRead::Found(raw) => raw.subject.to_string(),
            TriggerRead::Gone | TriggerRead::Unreadable { .. } => String::new(),
        }
    }
}

/// Why the dead letter for an advisory emitted at `emitted`, before the
/// incarnation of the trigger stream created at `stream_epoch`
/// (nanoseconds), names no trigger
/// ([`crate::dead_letter::DEAD_LETTER_UNATTRIBUTED_KEY`]).
pub(super) fn unattributed_reason(
    emitted: chrono::DateTime<chrono::Utc>,
    stream_epoch: i64,
) -> String {
    let at = |time: chrono::DateTime<chrono::Utc>| {
        time.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
    };
    format!(
        "its advisory came from an earlier incarnation of the trigger stream (emitted {}, before \
         the stream was created at {}), so its sequence is that stream's, whose messages went \
         with it",
        at(emitted),
        at(chrono::DateTime::from_timestamp_nanos(stream_epoch)),
    )
}

/// What reading the trigger's message gave.
enum TriggerRead {
    /// The message.
    Found(StreamMessage),
    /// The broker says the stream holds no message at that sequence.
    Gone,
    /// The read failed for another reason: decide again later.
    Unreadable {
        /// What the read answered.
        answer: String,
        /// Whether the answer was that the trigger stream is missing (code
        /// 10059), which leaves no message to find on it.
        stream_missing: bool,
    },
}

/// Read the message at `stream_seq` on the trigger stream.
async fn read_trigger(jetstream: &async_nats::jetstream::Context, stream_seq: u64) -> TriggerRead {
    let stream = match jetstream.get_stream_no_info(TRIGGER_STREAM_NAME).await {
        Ok(stream) => stream,
        Err(err) => {
            return TriggerRead::Unreadable {
                answer: err.to_string(),
                stream_missing: false,
            };
        }
    };
    match stream.get_raw_message(stream_seq).await {
        Ok(raw) => TriggerRead::Found(raw),
        Err(err) => match err.kind() {
            RawMessageErrorKind::NoMessageFound => TriggerRead::Gone,
            kind => TriggerRead::Unreadable {
                answer: err.to_string(),
                stream_missing: matches!(
                    kind,
                    RawMessageErrorKind::JetStream(js) if js.error_code() == ErrorCode::STREAM_NOT_FOUND
                ),
            },
        },
    }
}

/// Why the watch holds its verdict while the trigger's message cannot be
/// read, `err` being what the read answered.
fn held_while_unreadable(err: &str) -> String {
    format!("the trigger's message could not be read: {err}")
}

/// Why a dead letter written at the watch's limit, with the trigger's
/// message still unreadable, names no trigger: `answer` is what the last
/// read answered. The message may still be on the stream unless the answer
/// was that the stream is missing, and only then does the reason say so.
pub(super) fn unreadable_at_the_limit(answer: &str, stream_missing: bool) -> String {
    if stream_missing {
        format!(
            "the trigger's message could not be read by the watch's limit: the trigger stream \
             was missing (the last read answered: {answer}), so the trigger is not named"
        )
    } else {
        format!(
            "the trigger's message could not be read off the trigger stream by the watch's limit \
             (the last read answered: {answer}), so the trigger is not named, though its message \
             may still be on the stream"
        )
    }
}

/// Why a dead letter names no trigger when the broker said the stream held
/// no message at the trigger's sequence as the watch last read it, at
/// `read_at`, for an advisory emitted at `emitted` (if the watch can read
/// its timestamp). The last read is the one the dead letter is written on:
/// for a verdict held on the records it can be an hour after the first read
/// that found the message gone.
///
/// The reason says what the broker answered, then what the watch makes of
/// it: the broker gives the same answer for a message its store failed to
/// load (and for a sequence the stream never held, which an advisory of
/// this incarnation does not name), so that it aged out is the likeliest
/// reading, not a fact.
pub(super) fn gone_reason(
    read_at: chrono::DateTime<chrono::Utc>,
    emitted: Option<chrono::DateTime<chrono::Utc>>,
) -> String {
    let at = |time: chrono::DateTime<chrono::Utc>| {
        time.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
    };
    let advisory = match emitted {
        Some(emitted) => format!("for an advisory emitted at {}", at(emitted)),
        None => "for an advisory that carries no timestamp the watch can read".to_string(),
    };
    format!(
        "the broker answered that the trigger stream held no message at this sequence when the \
         watch last read it, at {}, {advisory}: the stream keeps a message {}h after its \
         publish, so it most likely aged out, or was deleted sooner; the broker answers the same \
         for a message it cannot load",
        at(read_at),
        DEFAULT_TRIGGER_MAX_AGE.as_secs() / 3600,
    )
}
