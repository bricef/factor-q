//! Which trigger an advisory names, as its dead letter records it: the
//! message at the advisory's sequence on the trigger stream, or nothing,
//! and why. Moved out of `advisory_watch.rs` as it stood, to keep that
//! file inside its size budget.

use tracing::info;
use uuid::Uuid;

use super::{AdvisoryWatch, AdvisoryWatchError, CONSUMER_NAME, MaxDeliverAdvisory};
use crate::agent::AgentId;
use crate::bus::TRIGGER_STREAM_NAME;
use crate::trigger::{agent_id_from_subject, trigger_id_in};

/// The trigger an advisory names, as its dead letter records it
/// ([`AdvisoryWatch::named_trigger`]).
pub(super) struct Named {
    /// The trigger's agent, or none: the dead letter goes under the system
    /// sentinel.
    pub(super) agent_id: Option<AgentId>,
    pub(super) subject: String,
    pub(super) payload: serde_json::Value,
    /// The id the trigger's publisher gave it, if it gave one.
    pub(super) trigger_id: Option<Uuid>,
    /// Why no trigger is named, when the advisory came from an earlier
    /// incarnation of the stream
    /// ([`crate::dead_letter::DEAD_LETTER_UNATTRIBUTED_KEY`]).
    pub(super) unattributed: Option<String>,
}

impl Named {
    /// No trigger: no agent, subject, payload or id.
    fn nothing() -> Self {
        Self {
            agent_id: None,
            subject: String::new(),
            payload: serde_json::Value::Null,
            trigger_id: None,
            unattributed: None,
        }
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
    pub(super) async fn named_trigger(
        &self,
        advisory: &MaxDeliverAdvisory,
        earlier: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Named, AdvisoryWatchError> {
        if let Some(emitted) = earlier {
            info!(consumer = CONSUMER_NAME, stream_seq = advisory.stream_seq,
                timestamp = ?advisory.timestamp,
                "an advisory from an earlier incarnation of the trigger stream: its sequence \
                 names a message of the stream before this one, so the watch reads neither the \
                 message nor the claim this stream has at that sequence, and dead-letters it \
                 naming no trigger");
            return Ok(Named {
                unattributed: Some(unattributed_reason(
                    emitted,
                    self.bus.trigger_stream_epoch(),
                )),
                ..Named::nothing()
            });
        }
        // The original trigger: subject → agent, payload → annotation.
        // Triggers age out after 24h — an older advisory still
        // surfaces, attributed to the system sentinel.
        let trigger = self
            .bus
            .jetstream()
            .get_stream(TRIGGER_STREAM_NAME)
            .await
            .map_err(|err| AdvisoryWatchError::Stream(err.to_string()))?
            .get_raw_message(advisory.stream_seq)
            .await
            .ok();
        // The identity is read, never assigned. This watch did not
        // dispatch the trigger, so it has no standing to name it: if
        // the publisher stamped a header the id is recorded, and if the
        // trigger arrived unnamed (or has aged out of the stream) the
        // dead letter says so by carrying no name, rather than
        // inventing one that matches nothing anywhere else.
        Ok(match trigger {
            Some(raw) => Named {
                agent_id: agent_id_from_subject(&raw.subject).and_then(|id| AgentId::new(id).ok()),
                subject: raw.subject.to_string(),
                payload: serde_json::from_slice(&raw.payload).unwrap_or(serde_json::Value::Null),
                trigger_id: trigger_id_in(Some(&raw.headers)),
                unattributed: None,
            },
            None => Named::nothing(),
        })
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
