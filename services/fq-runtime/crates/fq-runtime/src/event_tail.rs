//! Ephemeral, cursor-addressed reads over the event stream — the
//! Turn stream's substrate (Phase 3d). Split from `bus.rs` to keep
//! that file inside its size budget; same `EventBus`, read-only
//! surface.
//!
//! The tailing is here; what a tail *yields* is not. [`EventState`]
//! is a wire shape — one event and the position it was read at — so
//! it lives in `fq-ops` with the rest of the event vocabulary, and is
//! re-exported here so `crate::event_tail::EventState` still names it.

use crate::bus::{BusError, EventBus, STREAM_NAME};
use crate::events::{Event, EventParseError};
use async_nats::jetstream::consumer;
use tracing::trace;

pub use fq_ops::events::EventState;

/// One message off the tail: where it sits on the stream, and the
/// event when this build can read it.
///
/// **`event` is `None` for history this build does not read.** The
/// wire boundary refuses a message whose envelope declares a schema
/// version outside `SUPPORTED_SCHEMA_VERSIONS` before it looks at the
/// body ([#409](https://github.com/bricef/factor-q/issues/409)), and
/// what that means is the reader's to decide: a durable consumer halts
/// on one, because acking it would erase history from every projection
/// built after it; an ephemeral listing skips it, because the message
/// is not one of the things it was asked for and refusing to answer at
/// all is the larger loss.
///
/// The tail used to hand the bytes straight to serde, so a reader had
/// no third answer to give: a v2 message anywhere in an agent's history
/// failed `turn.list` for *every* invocation of that agent, however
/// recent, and the dashboard's transcript page 503'd on all of them
/// ([#673](https://github.com/bricef/factor-q/issues/673)).
///
/// The sequence is carried either way, so a scan bounded by a tip still
/// terminates on a message it cannot read.
#[derive(Debug)]
pub struct TailedMessage {
    /// The stream sequence the message was read at.
    pub seq: u64,
    /// The event, when this build reads its schema version.
    pub event: Option<Event>,
}

impl EventBus {
    /// An ephemeral, ordered, ack-less consumer over the event stream
    /// starting at `from_seq` (`0` reads from the beginning), scoped
    /// to `filter_subject`. Yields [`TailedMessage`]s — the Turn
    /// stream's substrate. Leaves no durable state behind (the
    /// `list_dead_letters` pattern, with a start sequence).
    ///
    /// Reads through [`Event::from_wire`], the same boundary every
    /// other consumer of the log uses: version first, then shape. A
    /// message in a version this build does not read is *yielded*, with
    /// no event, rather than erroring the whole tail — see
    /// [`TailedMessage`] for why the caller is the one that decides.
    /// Bytes that are malformed *within* a version this build reads
    /// are still an error; that is a bug, not history.
    pub async fn events_from(
        &self,
        filter_subject: &str,
        from_seq: u64,
    ) -> Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = Result<TailedMessage, BusError>> + Send>>,
        BusError,
    > {
        use futures::StreamExt;
        let stream = self
            .jetstream()
            .get_stream(STREAM_NAME)
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        let deliver_policy = if from_seq <= 1 {
            consumer::DeliverPolicy::All
        } else {
            consumer::DeliverPolicy::ByStartSequence {
                start_sequence: from_seq,
            }
        };
        let consumer = stream
            .create_consumer(consumer::pull::OrderedConfig {
                filter_subject: filter_subject.to_string(),
                deliver_policy,
                ..Default::default()
            })
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        let messages = consumer
            .messages()
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        Ok(Box::pin(messages.map(|msg| {
            let msg = msg.map_err(|err| BusError::Stream(err.to_string()))?;
            let seq = msg
                .info()
                .map_err(|err| BusError::Stream(err.to_string()))?
                .stream_sequence;
            match Event::from_wire(&msg.payload) {
                Ok(event) => Ok(TailedMessage {
                    seq,
                    event: Some(event),
                }),
                Err(EventParseError::UnsupportedSchemaVersion { found, .. }) => {
                    // `trace`, not `debug`: this fires once per
                    // unreadable message per tail, and a page load
                    // walks an agent's whole subject — on an instance
                    // with a wire break in its history that is
                    // thousands of lines for one transcript.
                    trace!(
                        seq,
                        found,
                        "tail skipped a message in a schema version this build does not read"
                    );
                    Ok(TailedMessage { seq, event: None })
                }
                Err(EventParseError::Malformed(err)) => Err(BusError::Deserialise(err)),
            }
        })))
    }

    /// The last sequence **matching `filter_subject`** — the bound a
    /// filtered scan must stop at.
    ///
    /// [`last_event_seq`](Self::last_event_seq) is the wrong bound for
    /// a filtered read: a scan that walks until it sees the stream's
    /// last sequence never sees it when that message is one the filter
    /// excludes (a heartbeat, another agent's turn), and waits for a
    /// message that will never be delivered. This asks the server for
    /// the last message the *filter* matches instead, so the scan's
    /// end is a sequence it is guaranteed to be handed.
    ///
    /// `0` when nothing matches — an empty read, not an error. An
    /// empty `filter_subject` means the whole stream.
    pub async fn last_event_seq_matching(&self, filter_subject: &str) -> Result<u64, BusError> {
        let pattern = if filter_subject.is_empty() {
            ">"
        } else {
            filter_subject
        };
        let stream = self
            .jetstream()
            .get_stream(STREAM_NAME)
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        // "No message found" is the empty answer, not a failure: a
        // subject nothing has ever been published to is a legitimate
        // (and common) filter.
        match stream.get_last_raw_message_by_subject(pattern).await {
            Ok(message) => Ok(message.sequence),
            Err(_) => Ok(0),
        }
    }

    /// The event stream's last sequence — where a tail starts.
    pub async fn last_event_seq(&self) -> Result<u64, BusError> {
        let stream = self
            .jetstream()
            .get_stream(STREAM_NAME)
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        let mut stream = stream;
        let info = stream
            .info()
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        Ok(info.state.last_sequence)
    }
}

#[cfg(test)]
mod tests;
