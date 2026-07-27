//! Ephemeral, cursor-addressed reads over the event stream — the
//! Turn stream's substrate (Phase 3d). Split from `bus.rs` to keep
//! that file inside its size budget; same `EventBus`, read-only
//! surface.

use crate::bus::{BusError, EventBus, STREAM_NAME};
use crate::events::Event;
use async_nats::jetstream::consumer;

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
