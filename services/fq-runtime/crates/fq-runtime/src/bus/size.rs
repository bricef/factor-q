//! The broker's size limit, counted the way the broker counts it.
//!
//! NATS refuses a message whose header block and body together exceed
//! the server's advertised `max_payload` (#4), and async-nats checks the
//! same sum before it sends. The bus sends one header with every event,
//! the `Nats-Msg-Id` JetStream deduplicates on, so what counts is the
//! event's JSON plus that header block. A guard that measured the JSON
//! alone let an event within the header's size of the limit through, to
//! be refused by the client as a generic publish error, which is
//! transient: the run it belonged to was left in flight, to be resumed
//! into the same refusal on every start (#828).
//!
//! The rule lives here once. [`EventBus::publish`](super::EventBus::publish)
//! guards with it and answers [`EventSink::check_size`](super::EventSink::check_size)
//! with it, so a caller that asks whether an event would be refused (the
//! run loop, before it stores a terminal event) and the publish that would
//! refuse it cannot disagree. The sim's sink applies the same rule.

use async_nats::HeaderMap;
use async_nats::jetstream::context::{PublishError, PublishErrorKind};

use super::BusError;
use crate::events::{Event, EventKind};

/// The one header the bus sends with an event: its id, which JetStream
/// deduplicates a republish on.
const MESSAGE_ID: &str = "Nats-Msg-Id";

/// The header block the bus sends with `event`.
pub(super) fn headers(event: &Event) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(MESSAGE_ID, event.envelope.event_id.to_string());
    headers
}

/// The bytes `event` takes on the wire when its JSON is `body_len` bytes:
/// the body and the header block [`headers`] builds, framed as it is sent
/// (`NATS/1.0\r\n`, `Nats-Msg-Id: <id>\r\n`, and a closing `\r\n`).
pub(crate) fn wire_len(event: &Event, body_len: usize) -> usize {
    let id_len = event.envelope.event_id.to_string().len();
    let header_len = "NATS/1.0\r\n".len() + MESSAGE_ID.len() + ": ".len() + id_len + 4;
    body_len + header_len
}

/// Refuse `event`, whose JSON is `body_len` bytes, when it would exceed
/// `limit` on the wire.
pub(crate) fn check_event_size(
    event: &Event,
    body_len: usize,
    limit: usize,
) -> Result<(), BusError> {
    let kind = EventKind::from(&event.payload);
    check_payload_size(kind, wire_len(event, body_len), limit)
}

/// Pre-flight payload size check: the pure comparison, factored out so
/// it can be tested without a live NATS server. Returns
/// [`BusError::PayloadTooLarge`] when `size` bytes on the wire would
/// exceed the server's advertised `max_payload`.
///
/// NATS rejects a message strictly greater than `max_payload`; one
/// exactly equal to the limit is accepted, so the guard uses a strict
/// `>` comparison to mirror the server.
pub(crate) fn check_payload_size(
    event_type: EventKind,
    size: usize,
    limit: usize,
) -> Result<(), BusError> {
    if size > limit {
        return Err(BusError::PayloadTooLarge {
            event_type,
            size,
            limit,
        });
    }
    Ok(())
}

/// What a failed publish of `event` becomes. The client's own size check
/// is a backstop behind [`check_event_size`]: they count the same bytes,
/// but the client measures against the limit as it stands now, which a
/// reconnect to a broker with a smaller one can lower. Its refusal is the
/// same permanent fact as the guard's and is reported as one; everything
/// else is what it always was.
pub(super) fn publish_error(
    err: PublishError,
    event: &Event,
    body_len: usize,
    limit: usize,
) -> BusError {
    match err.kind() {
        PublishErrorKind::MaxPayloadExceeded => BusError::PayloadTooLarge {
            event_type: EventKind::from(&event.payload),
            size: wire_len(event, body_len),
            limit,
        },
        _ => err.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shutdown() -> Event {
        let runtime_id = uuid::Uuid::now_v7();
        Event::system(
            runtime_id,
            crate::events::EventPayload::SystemShutdown(crate::events::SystemShutdownPayload {
                runtime_id,
                reason: "test".to_string(),
                clean: true,
            }),
        )
    }

    /// The count is the header [`headers`] actually builds, framed the way
    /// async-nats frames it (`NATS/1.0\r\n`, one `key: value\r\n` line,
    /// `\r\n`): 63 bytes for an event id.
    #[test]
    fn the_wire_length_is_the_body_and_the_header_block() {
        let event = shutdown();
        let mut framed = String::from("NATS/1.0\r\n");
        for (name, values) in headers(&event).iter() {
            for value in values {
                framed.push_str(&format!("{name}: {}\r\n", value.as_str()));
            }
        }
        framed.push_str("\r\n");
        assert_eq!(framed.len(), 63);
        assert_eq!(wire_len(&event, 1_000), 1_000 + framed.len());
    }

    /// The client's refusal is the same fact as the guard's; every other
    /// publish error is untouched.
    #[test]
    fn the_clients_size_refusal_is_reported_as_too_large() {
        let event = shutdown();
        let refused = PublishError::new(PublishErrorKind::MaxPayloadExceeded);
        match publish_error(refused, &event, 900, 512) {
            BusError::PayloadTooLarge {
                event_type,
                size,
                limit,
            } => {
                assert_eq!(event_type, EventKind::SystemShutdown);
                assert_eq!(size, 963);
                assert_eq!(limit, 512);
            }
            other => panic!("expected PayloadTooLarge, got {other:?}"),
        }
        let timed_out = PublishError::new(PublishErrorKind::TimedOut);
        assert!(matches!(
            publish_error(timed_out, &event, 900, 512),
            BusError::PublishTimedOut
        ));
    }
}
