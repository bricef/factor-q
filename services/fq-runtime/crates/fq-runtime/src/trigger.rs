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
//!   verbatim, never re-minted ([`delivered`]).
//! - **A header-less external trigger** — assigned when the dispatcher
//!   first handles it, again by [`delivered`].
//!
//! [`Trigger::id`] is not optional and there is no constructor that
//! leaves it unset, so "a trigger the runtime is acting on" and "a
//! trigger with a name" are the same set by construction: an invocation
//! cannot be started from anything but a [`Trigger`], and a `Trigger`
//! cannot be built without an id.
//!
//! # Where a trigger is kept
//!
//! In the **projection**, indefinitely, payload included. A trigger is a
//! key domain event and its retention is not the trigger stream's 24
//! hours (a runaway-backlog safety net) nor the event log's 30 days: the
//! projection's `triggers` table is a trigger's permanent home, and the
//! retention sweep never reaches it. That follows the exemption cost-
//! bearing rows already have — `sweep_events` deletes on `timestamp < ?
//! AND total_cost IS NULL`, so spend outlives the log it was recorded on
//! — except that here the exemption is structural rather than a
//! predicate: the sweep only ever deletes from `events`, exactly as it
//! leaves `invocation_summary` alone.
//!
//! Because the row holds the payload, all three of Get, List and Stream
//! answer from that one store. Nothing hops to the log, so nothing can
//! be listed and then found missing — the failure mode the Event atom
//! has to name `Gone` for.
//!
//! # Where this lives
//!
//! The publish half is an `impl EventBus` block here rather than in
//! `bus.rs`, following `event_tail.rs`: same bus, one domain's slice of
//! its surface, in a file that has room to explain itself. The read half
//! is an `impl Views` block here for the same reason — `views.rs` is a
//! read model, not a place for one domain's queries, and it sits exactly
//! on its size budget. `Views::projection` is `pub(crate)` so this block
//! can reach it; the handle it hands out is opened read-only
//! (`ProjectionStore::open_read_only`), so the read path stays visibly
//! read-only despite crossing a module boundary.

// The shape and its names are `fq_ops::trigger`, re-exported so a
// caller reaches them by the same path as before. What stays is
// learning about a trigger from something only this crate can see: a
// broker message, or an event.
pub use fq_ops::trigger::{
    ALL_SUBJECTS, MAX_TRIGGER_PAYLOAD_BYTES, TRIGGER_ID_HEADER, Trigger, agent_id_from_subject,
    subject,
};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::debug;
use uuid::Uuid;

use crate::agent::AgentId;
use crate::bus::{BusError, EventBus};
use crate::dead_letter::{
    DEAD_LETTER_PAYLOAD_KEY, DEAD_LETTER_SUBJECT_KEY, DEAD_LETTER_TRIGGER_ID_KEY,
};
use crate::events::{Event, EventPayload, FailureKind, TriggerSource};
use crate::views::{Views, ViewsError};

/// One trigger as `trigger.list` hands it back: the Trigger atom's
/// **index** row.
///
/// **It carries no payload, deliberately.** A payload is opaque JSON a
/// producer chose, bounded only by [`MAX_TRIGGER_PAYLOAD_BYTES`], and
/// one List answer is one 8 MiB frame — so a page of whole triggers is
/// a page that can fail to encode. The split is what
/// `fq_ops::Atom::with_index` exists for, and the rule that makes it
/// safe is kept here: **the row carries the identity Get takes**, so
/// any row walks to the whole trigger through `trigger.get`.
///
/// Everything on it is bounded: two ids, an instant, and a source drawn
/// from a closed set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TriggerView {
    /// The identity — hand this to `trigger.get` unchanged.
    pub trigger_id: String,
    /// The agent the trigger was for.
    pub agent_id: String,
    /// When the record that named this trigger was written, RFC3339.
    /// Not when it was published: publishing appends nothing durable,
    /// so the first moment a trigger is a recorded fact is the moment
    /// something acted on it.
    pub recorded_at: String,
    /// How the runtime came by it: `manual` | `subject` | `schedule`.
    pub source: TriggerSource,
}

impl Views {
    /// One trigger by its identity — the whole of `trigger.get`.
    ///
    /// A single primary-key lookup against the trigger's permanent
    /// record. There is no second hop and so no second failure: the
    /// payload is in the row, which is why this atom needs neither the
    /// Event atom's `Unlocatable` (indexed, position unknown) nor its
    /// `Gone` (position known, log aged past it).
    ///
    /// `None` means **no durable record**, which is one state with more
    /// than one cause — a queued trigger nothing has consumed yet, a
    /// record written before this table existed, or an id that names
    /// nothing. The caller names those; see `trigger_command.rs`.
    pub async fn trigger(&self, trigger_id: &str) -> Result<Option<Trigger>, ViewsError> {
        Ok(self.projection.trigger(trigger_id).await?)
    }

    /// Triggers matching a narrowing, most recently recorded first —
    /// index rows, never payloads (see [`TriggerView`]).
    pub async fn triggers(
        &self,
        agent: Option<&str>,
        since: Option<&str>,
        limit: i64,
    ) -> Result<Vec<TriggerView>, ViewsError> {
        Ok(self.projection.query_triggers(agent, since, limit).await?)
    }

    /// One page of whole triggers at or after `from_seq`, in sequence
    /// order — `trigger.stream`'s read.
    pub async fn triggers_from(
        &self,
        agent: Option<&str>,
        since: Option<&str>,
        from_seq: u64,
        limit: i64,
    ) -> Result<Vec<(u64, Trigger)>, ViewsError> {
        Ok(self
            .projection
            .triggers_from(agent, since, from_seq, limit)
            .await?)
    }

    /// The highest log position any recorded trigger carries — where
    /// `from_seq = u64::MAX` seeks to.
    ///
    /// The tail of *this atom's population*, not of the log. The log's
    /// tip would be wrong in the one direction that loses data: the
    /// projection trails the log, so a caller told to resume at the
    /// log's tip would skip every trigger whose row has not been
    /// written yet.
    pub async fn trigger_tip(&self) -> Result<u64, ViewsError> {
        Ok(self.projection.max_trigger_seq().await?)
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

/// The accept rule: a body strictly over
/// [`MAX_TRIGGER_PAYLOAD_BYTES`] is refused.
///
/// **This is the single enforcement point**, and it is on the publish
/// seam rather than in the command handler because every edge publish
/// goes through here — including a requeue's republish. A limit a new
/// call site could route around would not be one.
///
/// A trigger arriving on NATS from an external publisher is not checked
/// and deliberately gets no check of its own: publishing straight to
/// `fq.trigger.*` is on a deprecation path in favour of the edge
/// (ADR-0006 D8 and Appendix C — NATS is internal infrastructure, not a
/// public API), and building rejection for it would be engineering
/// support for the thing this migration exists to remove. The broker's
/// own `max_payload` stays the backstop on that path while it lasts —
/// a broker-level protection for something being retired, not a domain
/// guarantee.
///
/// Strict `>`, mirroring the broker's own `max_payload` comparison
/// ([`EventBus::publish`]), so a body exactly at the limit is accepted
/// and the number means what it says.
fn check_payload_size(len: usize) -> Result<(), BusError> {
    if len > MAX_TRIGGER_PAYLOAD_BYTES {
        return Err(BusError::TriggerPayloadTooLarge {
            size: len,
            limit: MAX_TRIGGER_PAYLOAD_BYTES,
        });
    }
    Ok(())
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
    ///
    /// A body over [`MAX_TRIGGER_PAYLOAD_BYTES`] is **refused, never
    /// truncated**: a truncated payload is a different task, and an
    /// agent handed one would do the wrong work while every record said
    /// it did the right work. The publisher is told the limit and its
    /// own size and can decide.
    pub async fn publish_trigger(
        &self,
        agent: &AgentId,
        payload: &Value,
    ) -> Result<PublishedTrigger, BusError> {
        self.publish_trigger_named(agent, Uuid::now_v7(), payload)
            .await
    }

    /// Publish a trigger under an identity the caller already holds.
    ///
    /// The seam for republishing something that is already named —
    /// notably a requeued dead letter, which knows the id of the
    /// trigger it came from.
    pub async fn publish_trigger_named(
        &self,
        agent: &AgentId,
        id: Uuid,
        payload: &Value,
    ) -> Result<PublishedTrigger, BusError> {
        let subject = subject(agent);
        let body = serde_json::to_vec(payload)?;
        check_payload_size(body.len())?;
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

pub fn delivered(msg: &async_nats::jetstream::Message, payload: Value) -> Trigger {
    let subject = Some(msg.subject.to_string());
    match trigger_id_in(msg.headers.as_ref()) {
        Some(id) => Trigger::named(id, TriggerSource::Subject, subject, payload),
        None => Trigger::mint(TriggerSource::Subject, subject, payload),
    }
}

pub fn from_event(event: &Event) -> Option<Trigger> {
    match &event.payload {
        EventPayload::Triggered(p) => Some(Trigger::named(
            p.trigger_id?,
            p.trigger_source,
            p.trigger_subject.clone(),
            p.trigger_payload.clone(),
        )),
        // A dead letter's annotations are the same three facts under
        // different keys. `source` is `Subject` rather than the
        // annotation's `dead_letter_source` ("inline" | "advisory"),
        // which says which *emitter* noticed — a different question:
        // only a trigger that came off the trigger stream can
        // exhaust deliveries on it.
        EventPayload::Failed(p) if matches!(p.error_kind, FailureKind::TriggerExhausted) => {
            let annotation = |key: &str| event.annotations.0.get(key);
            let id = annotation(DEAD_LETTER_TRIGGER_ID_KEY)
                .and_then(|v| v.as_str())
                .and_then(|v| Uuid::parse_str(v).ok())?;
            Some(Trigger::named(
                id,
                TriggerSource::Subject,
                annotation(DEAD_LETTER_SUBJECT_KEY)
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                annotation(DEAD_LETTER_PAYLOAD_KEY)
                    .cloned()
                    .unwrap_or(Value::Null),
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::subjects;

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

    fn triggered_event(trigger_id: Option<Uuid>, payload: Value) -> Event {
        Event::new(
            crate::agent::AgentId::new("researcher").unwrap(),
            Uuid::now_v7(),
            EventPayload::Triggered(crate::events::TriggeredPayload {
                trigger_id,
                trigger_source: TriggerSource::Subject,
                trigger_subject: Some("fq.trigger.researcher".to_string()),
                trigger_payload: payload,
                config_snapshot: Default::default(),
            }),
        )
    }

    fn exhausted(kind: FailureKind) -> Event {
        Event::new(
            crate::agent::AgentId::new("researcher").unwrap(),
            Uuid::now_v7(),
            EventPayload::Failed(crate::events::FailedPayload {
                error_kind: kind,
                error_message: "trigger exhausted after 5 deliveries (limit 5)".into(),
                phase: crate::events::FailurePhase::Setup,
                partial_totals: Default::default(),
            }),
        )
    }

    /// The lens over an invocation's own record: the trigger the
    /// `triggered` event names, exactly as it was handed to the agent.
    #[test]
    fn a_triggered_event_names_the_trigger_that_caused_it() {
        let id = Uuid::now_v7();
        let payload = serde_json::json!({"task": "look at #12"});
        let trigger = crate::trigger::from_event(&triggered_event(Some(id), payload.clone()))
            .expect("a trigger");
        assert_eq!(trigger.id, id);
        assert_eq!(trigger.source, TriggerSource::Subject);
        assert_eq!(trigger.subject.as_deref(), Some("fq.trigger.researcher"));
        assert_eq!(trigger.payload, payload, "the body is verbatim");
    }

    /// **A dead-lettered trigger is still a trigger.** It may have no
    /// `triggered` event at all — a trigger for an agent this daemon
    /// does not have never starts an invocation — so the dead letter's
    /// annotations are the only record it ever gets, and reading them
    /// is what keeps `trigger.get` answering for it.
    #[test]
    fn a_dead_letter_names_the_trigger_that_died() {
        let id = Uuid::now_v7();
        let event = exhausted(FailureKind::TriggerExhausted)
            .annotate(
                DEAD_LETTER_TRIGGER_ID_KEY,
                serde_json::json!(id.to_string()),
            )
            .annotate(
                DEAD_LETTER_SUBJECT_KEY,
                serde_json::json!("fq.trigger.researcher"),
            )
            .annotate(DEAD_LETTER_PAYLOAD_KEY, serde_json::json!({"n": 1}));
        let trigger = crate::trigger::from_event(&event).expect("a trigger");
        assert_eq!(trigger.id, id);
        assert_eq!(trigger.payload, serde_json::json!({"n": 1}));
        assert_eq!(trigger.subject.as_deref(), Some("fq.trigger.researcher"));
        // `subject` and not the emitter word: `dead_letter_source` says
        // which path noticed the exhaustion, which is a different
        // question from how the runtime came by the trigger.
        assert_eq!(trigger.source, TriggerSource::Subject);
    }

    /// An event with no name in it is not a trigger this can be asked
    /// about — and the two ways that happens are both the pre-identity
    /// past rather than defects: `triggered` events written before the
    /// identity existed, and the advisory dead-letter path, which
    /// records no id rather than inventing one for a trigger that has
    /// aged off the stream.
    #[test]
    fn an_unnamed_record_yields_no_trigger() {
        assert!(
            crate::trigger::from_event(&triggered_event(None, Value::Null)).is_none(),
            "a pre-identity `triggered` event names no trigger"
        );
        assert!(
            crate::trigger::from_event(&exhausted(FailureKind::TriggerExhausted)).is_none(),
            "an unnamed dead letter names no trigger"
        );
        // An ordinary failure shares the subject and is not this at all,
        // named or otherwise.
        let ordinary = exhausted(FailureKind::RuntimeError).annotate(
            DEAD_LETTER_TRIGGER_ID_KEY,
            serde_json::json!(Uuid::now_v7().to_string()),
        );
        assert!(crate::trigger::from_event(&ordinary).is_none());
        // …and neither is any other event.
        assert!(
            crate::trigger::from_event(&Event::system(
                Uuid::now_v7(),
                EventPayload::WorkerHeartbeat(crate::events::WorkerHeartbeatPayload {
                    worker_id: crate::worker::WorkerId::new("w-1".to_string()).unwrap(),
                }),
            ))
            .is_none()
        );
    }

    /// **An oversized payload is refused, and the boundary is where the
    /// number says it is.** A trigger is kept indefinitely, so the
    /// ceiling is on what is *accepted*; and it is a refusal rather than
    /// a truncation because a shortened payload is a different task,
    /// which every record would then describe as the original one.
    ///
    /// Exactly at the limit is accepted — strict `>`, mirroring the
    /// broker's own `max_payload` comparison, so the limit is a size a
    /// publisher can actually send rather than one byte more than it may.
    #[test]
    fn a_payload_over_the_limit_is_refused_and_one_at_it_is_not() {
        assert!(check_payload_size(0).is_ok());
        assert!(check_payload_size(MAX_TRIGGER_PAYLOAD_BYTES - 1).is_ok());
        assert!(
            check_payload_size(MAX_TRIGGER_PAYLOAD_BYTES).is_ok(),
            "the limit is a size that may be sent, not one that may not"
        );
        let err = check_payload_size(MAX_TRIGGER_PAYLOAD_BYTES + 1)
            .expect_err("one byte over must be refused");
        assert!(
            matches!(err, BusError::TriggerPayloadTooLarge { size, limit }
                if size == MAX_TRIGGER_PAYLOAD_BYTES + 1 && limit == MAX_TRIGGER_PAYLOAD_BYTES),
            "the refusal must carry both numbers so the publisher can act; got {err:?}"
        );
    }

    /// The refusal happens on the publish path, before anything reaches
    /// the stream — so an oversized trigger never becomes a record, and
    /// the caller learns why rather than watching the broker refuse a
    /// message it cannot explain.
    ///
    /// The measure is the JSON **body**'s own bytes, which is what
    /// crosses the wire and what the permanent row then keeps: the two
    /// quotes around a JSON string are part of what a publisher is
    /// charged for.
    #[tokio::test]
    async fn an_oversized_trigger_never_reaches_the_stream() {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let agent = format!("oversize-{}", Uuid::now_v7().simple());

        let body = Value::String("x".repeat(MAX_TRIGGER_PAYLOAD_BYTES));
        let size = serde_json::to_vec(&body).expect("serialises").len();
        assert_eq!(size, MAX_TRIGGER_PAYLOAD_BYTES + 2, "the quotes count");

        let err = bus
            .publish_trigger(&AgentId::new(&agent).unwrap(), &body)
            .await
            .expect_err("an oversized trigger must be refused");
        assert!(
            matches!(err, BusError::TriggerPayloadTooLarge { size: got, .. } if got == size),
            "the refusal names the body's own size; got {err:?}"
        );
        // Nothing was published under that agent's subject, so nothing
        // can ever be recorded for it either.
        assert!(
            bus.jetstream()
                .get_stream(crate::bus::TRIGGER_STREAM_NAME)
                .await
                .expect("trigger stream")
                .get_last_raw_message_by_subject(&subjects::trigger(&agent))
                .await
                .is_err(),
            "a refused trigger must leave the stream untouched"
        );
    }

    /// The limit is a real bound rather than a round number nobody
    /// checked: generous against a real payload, under the edge frame a
    /// `trigger.get` answer has to fit inside, and under the broker
    /// ceiling the trigger has to cross to exist at all.
    ///
    /// Compile-time, because every input is a constant: the three
    /// quantities that frame the limit are facts about the transport
    /// and the workload, not runtime state, so a limit that violated
    /// one of them should stop the build rather than a test run.
    #[test]
    fn the_payload_limit_sits_between_a_real_payload_and_its_ceilings() {
        /// The production github-watcher task payload the DeadLetter
        /// atom's cap was measured against.
        const TYPICAL_PAYLOAD_BYTES: usize = 328;
        /// `LengthDelimitedCodec::new()`'s default max frame — one
        /// whole `trigger.get` answer.
        const EDGE_FRAME_BYTES: usize = 8 * 1024 * 1024;
        /// A stock `nats-server`'s default `max_payload`. The deployed
        /// broker raises it, but a limit that only holds on one
        /// deployment's config is not a domain guarantee.
        const STOCK_BROKER_MAX_PAYLOAD: usize = 1024 * 1024;
        const {
            assert!(
                MAX_TRIGGER_PAYLOAD_BYTES >= TYPICAL_PAYLOAD_BYTES * 1000,
                "`generous` has to mean something: the limit must be orders above a real payload"
            );
            assert!(
                MAX_TRIGGER_PAYLOAD_BYTES * 8 <= EDGE_FRAME_BYTES,
                "a Get answers with the whole payload, so the limit must leave the frame room"
            );
            assert!(
                MAX_TRIGGER_PAYLOAD_BYTES * 2 <= STOCK_BROKER_MAX_PAYLOAD,
                "an accepted trigger must fit the broker with room for its header, on any \
                 broker — otherwise the edge accepts what the transport then refuses, and the \
                 publisher gets an ack timeout instead of this limit's name"
            );
        }
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
            .publish_trigger(&AgentId::new(&agent).unwrap(), &body)
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
