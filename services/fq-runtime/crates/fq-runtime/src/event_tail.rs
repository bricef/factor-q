//! Ephemeral, cursor-addressed reads over the event stream — the
//! Turn stream's substrate (Phase 3d). Split from `bus.rs` to keep
//! that file inside its size budget; same `EventBus`, read-only
//! surface.

use crate::bus::{BusError, EventBus, STREAM_NAME};
use crate::events::Event;
use async_nats::jetstream::consumer;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One recorded event, with the log position it was read at.
///
/// The Event atom's state is the event itself, unabridged. It is the
/// substrate every other resource folds from, so an atom that dropped
/// the payload would not be the fact; the projection's `events` table
/// is an *index* over these, not the atom. The event's **identity**
/// is `event.envelope.event_id`, which is what `event.get` takes.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EventState {
    /// Where in the log this event sits — the universal cursor (P5):
    /// the same number that cursors `event.stream`, feeds `min_seq`
    /// gates, and rides in a command receipt's `AtomRef`.
    ///
    /// A cursor, never an identity. It says where the read landed,
    /// and it is only meaningful against the log that produced it:
    /// recreate the stream and the number means something else. Ask
    /// for an event by `event_id`; use this to resume.
    pub seq: u64,
    /// The event exactly as published.
    ///
    /// Declared to the surface as an opaque object rather than a
    /// reflected schema: an event already names its own payload
    /// contract in `envelope.schema_id` (`factor-q/llm_response@1`),
    /// which is the versioned reference a reader resolves, and
    /// reflecting the whole payload tree here would need schemars'
    /// chrono and uuid integrations — a wider change than this atom.
    #[schemars(with = "serde_json::Value")]
    pub event: Event,
}

impl EventBus {
    /// An ephemeral, ordered, ack-less consumer over the event stream
    /// starting at `from_seq` (`0` reads from the beginning), scoped
    /// to `filter_subject`. Yields `(stream_sequence, event)` pairs —
    /// the Turn stream's substrate. Leaves no durable state behind
    /// (the `list_dead_letters` pattern, with a start sequence).
    pub async fn events_from(
        &self,
        filter_subject: &str,
        from_seq: u64,
    ) -> Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = Result<(u64, Event), BusError>> + Send>>,
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
            let event: Event = serde_json::from_slice(&msg.payload)?;
            Ok((seq, event))
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
