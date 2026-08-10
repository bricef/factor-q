//! Triggers — the request that starts an agent invocation, and the
//! identity the runtime knows one by.
//!
//! # Why the identity is a header
//!
//! The trigger wire contract
//! (`docs/design/committed/trigger-wire-contract.md`) makes the message
//! **body** the trigger payload itself: one opaque JSON value, written
//! directly by external publishers in any language (the Go
//! `github-watcher`, `fq-cron`, anything with a NATS client). An
//! identity in the body would be a field inside a value the runtime
//! does not own and must not interpret — so it rides *beside* the body,
//! as the [`TRIGGER_ID_HEADER`] NATS header. A publisher that has never
//! heard of it is unaffected; one that has can name its own trigger
//! before the runtime ever sees it.
//!
//! # When the identity exists
//!
//! From the moment the system takes responsibility for the trigger,
//! which is exactly three moments:
//!
//! - **`trigger.publish`** — the daemon mints the id as it publishes
//!   ([`EventBus::publish_trigger`]) and hands it back in the
//!   [`PublishedTrigger`], so the caller learns the name of the thing
//!   it just queued.
//! - **An inbound trigger that already carries the header** — honoured
//!   verbatim, never re-minted ([`Trigger::delivered`]).
//! - **A header-less external trigger** — assigned when the dispatcher
//!   first handles it, again by [`Trigger::delivered`].
//!
//! [`Trigger::id`] is not optional and there is no constructor that
//! leaves it unset, so "a trigger the runtime is acting on" and "a
//! trigger with a name" are the same set by construction: an invocation
//! cannot be started from anything but a [`Trigger`], and a `Trigger`
//! cannot be built without an id.
//!
//! # Where this lives
//!
//! The publish half is an `impl EventBus` block here rather than in
//! `bus.rs`, following `event_tail.rs`: same bus, one domain's slice of
//! its surface, in a file that has room to explain itself.

use bytes::Bytes;
use serde_json::Value;
use tracing::debug;
use uuid::Uuid;

use crate::bus::{BusError, EventBus, trigger_subject};
use crate::events::TriggerSource;

/// The NATS header a trigger's identity travels in.
///
/// Named rather than reserved-prefixed (`Nats-*` belongs to the
/// server): a header is the one place a fact can ride a message whose
/// body is contractually opaque. Its value is the id's canonical
/// hyphenated UUID text.
pub const TRIGGER_ID_HEADER: &str = "Fq-Trigger-Id";

/// One trigger, named — the request an invocation answers.
///
/// This is the value the control-plane hands a worker; the worker
/// records [`Trigger::id`] on the invocation's `triggered` event, which
/// is what finally lets a reader say *this invocation came from that
/// trigger* rather than inferring it from matching content.
#[derive(Debug, Clone)]
pub struct Trigger {
    /// The trigger's identity: a UUIDv7, like every other identity the
    /// runtime mints (`event_id`, `invocation_id`). Time-sortable, so
    /// ids order the way the triggers did.
    pub id: Uuid,
    /// How the runtime came by this trigger.
    pub source: TriggerSource,
    /// The subject it arrived on, when it arrived on one.
    pub subject: Option<String>,
    /// The trigger body, verbatim — an opaque JSON value the target
    /// agent interprets (wire contract §The payload).
    pub payload: Value,
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
        }
    }

    /// The trigger a delivered JetStream message stands for: honour the
    /// header if the publisher stamped one, assign an id if not.
    ///
    /// This is the dispatcher's *first handling* of an inbound trigger
    /// and the only way a delivered message becomes something an
    /// invocation can be started from, so neither branch of the
    /// honour-or-assign rule can be skipped by adding a consumer — a
    /// new consumer either calls this or has no `Trigger` to run.
    ///
    /// `payload` is passed in already decoded because decoding it is
    /// also the dispatcher's poison check: a body that is not valid
    /// JSON is dropped rather than dispatched (wire contract §The
    /// payload), and a dropped trigger never becomes one of these.
    pub fn delivered(msg: &async_nats::jetstream::Message, payload: Value) -> Self {
        let subject = Some(msg.subject.to_string());
        match trigger_id_in(msg.headers.as_ref()) {
            Some(id) => Self::named(id, TriggerSource::Subject, subject, payload),
            None => Self::mint(TriggerSource::Subject, subject, payload),
        }
    }
}

/// The identity a NATS message carries, if it carries a readable one.
///
/// A malformed header value is treated as absent rather than as an
/// error: the trigger itself is still perfectly good work, and refusing
/// it would let a publisher's typo silence an agent. The caller decides
/// what "absent" means — the dispatcher assigns, the advisory watch
/// records nothing.
pub fn trigger_id_in(headers: Option<&async_nats::HeaderMap>) -> Option<Uuid> {
    headers
        .and_then(|h| h.get(TRIGGER_ID_HEADER))
        .and_then(|v| Uuid::parse_str(v.as_str()).ok())
}

/// What a publish onto the trigger stream leaves behind.
///
/// Two coordinates that are deliberately not interchangeable: `id`
/// names the trigger, `stream_seq` says where it landed. The sequence
/// is a position in a log — it is what `fq dead-letters` reconciles on
/// and what a requeue selects by — and is never an identity (see
/// [`crate::dead_letter::DeadLetterState`] for the same distinction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishedTrigger {
    /// The identity now on the message, minted here or supplied.
    pub id: Uuid,
    /// The trigger's sequence on the trigger stream.
    pub stream_seq: u64,
}

impl EventBus {
    /// Publish a trigger for a given agent, minting its identity.
    ///
    /// The JSON-encoded payload becomes the message body — unchanged,
    /// per the wire contract — and the identity rides the
    /// [`TRIGGER_ID_HEADER`] header. The delivery is ack'd by JetStream
    /// once durably accepted, so this returns only after the trigger is
    /// persisted, and the returned [`PublishedTrigger`] is how a caller
    /// learns what it just queued was called.
    pub async fn publish_trigger(
        &self,
        agent_id: &str,
        payload: &Value,
    ) -> Result<PublishedTrigger, BusError> {
        self.publish_trigger_named(agent_id, Uuid::now_v7(), payload)
            .await
    }

    /// Publish a trigger under an identity the caller already holds.
    ///
    /// The seam for republishing something that is already named —
    /// notably a requeued dead letter, which knows the id of the
    /// trigger it came from.
    pub async fn publish_trigger_named(
        &self,
        agent_id: &str,
        id: Uuid,
        payload: &Value,
    ) -> Result<PublishedTrigger, BusError> {
        let subject = trigger_subject(agent_id);
        let body = serde_json::to_vec(payload)?;
        let mut headers = async_nats::HeaderMap::new();
        headers.insert(TRIGGER_ID_HEADER, id.to_string());
        debug!(subject = %subject, trigger_id = %id, "publishing trigger");
        let ack = self
            .jetstream()
            .publish_with_headers(subject, headers, Bytes::from(body))
            .await?
            .await?;
        Ok(PublishedTrigger {
            id,
            stream_seq: ack.sequence,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with(value: &str) -> async_nats::HeaderMap {
        let mut headers = async_nats::HeaderMap::new();
        headers.insert(TRIGGER_ID_HEADER, value);
        headers
    }

    #[test]
    fn a_minted_trigger_is_time_sortable() {
        let first = Trigger::mint(TriggerSource::Manual, None, Value::Null);
        let second = Trigger::mint(TriggerSource::Manual, None, Value::Null);
        assert_eq!(first.id.get_version_num(), 7, "identities are UUIDv7");
        assert!(first.id < second.id, "v7 ids sort in minting order");
    }

    #[test]
    fn a_header_bearing_message_keeps_its_publisher_s_id() {
        let id = Uuid::now_v7();
        assert_eq!(
            trigger_id_in(Some(&headers_with(&id.to_string()))),
            Some(id)
        );
    }

    #[test]
    fn an_unreadable_or_absent_header_reads_as_no_id() {
        assert_eq!(trigger_id_in(None), None);
        assert_eq!(trigger_id_in(Some(&headers_with("not-a-uuid"))), None);
        assert_eq!(trigger_id_in(Some(&async_nats::HeaderMap::new())), None);
    }

    /// The publish half of the contract, against a live broker: the
    /// caller learns the id, the message carries it as a header, and the
    /// **body is still exactly the payload** — the one thing external
    /// publishers depend on.
    #[tokio::test]
    async fn a_published_trigger_is_named_on_the_wire_and_to_its_caller() {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let agent = format!("publish-names-{}", Uuid::now_v7().simple());
        let body = serde_json::json!({"task": "look at #12"});

        let published = bus
            .publish_trigger(&agent, &body)
            .await
            .expect("publish trigger");
        assert_eq!(
            published.id.get_version_num(),
            7,
            "minted identities are UUIDv7"
        );

        let raw = bus
            .jetstream()
            .get_stream(crate::bus::TRIGGER_STREAM_NAME)
            .await
            .expect("trigger stream")
            .get_raw_message(published.stream_seq)
            .await
            .expect("the published trigger");
        assert_eq!(
            trigger_id_in(Some(&raw.headers)),
            Some(published.id),
            "the id the caller learned is the id on the message"
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&raw.payload).expect("body is JSON"),
            body,
            "the body is the payload itself — the wire contract is unchanged"
        );
    }
}
