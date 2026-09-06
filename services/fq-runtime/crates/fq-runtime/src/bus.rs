//! NATS-backed event bus.
//!
//! Provides a typed interface for publishing and subscribing to factor-q
//! events. All events flow through a single JetStream stream, using
//! subject-based filtering for consumption.
//!
//! See `docs/design/committed/event-schema.md` for the event schema and subject
//! hierarchy.

use async_nats::jetstream::{self, stream};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::events::Event;
use crate::events::subjects::ALL_TRIGGERS;

mod consumers;
pub mod retry;

pub use consumers::UNLIMITED_MAX_DELIVER;
pub use retry::{ConsumerRedeliveryPolicy, RedeliveryLog};

/// The narrowest seam over event publication (reducer verification
/// plan, slice 3; widened to the archive sweeper in slice 5). The
/// reducer runner and the archive retry sweeper publish through this
/// trait so the hermetic sim can capture events in memory and inject
/// publish faults; production wires [`EventBus`], the NATS
/// implementation. Envelope timestamps are stamped by `Event::new`
/// from the system clock either way — the trace oracle and the
/// equivalence checks treat them as volatile.
#[async_trait::async_trait]
pub trait EventSink: Send + Sync {
    /// Publish one event; returns its sequence on the event stream —
    /// the coordinate receipts and the projection watermark speak.
    async fn publish(&self, event: &Event) -> Result<u64, BusError>;
}

#[async_trait::async_trait]
impl EventSink for EventBus {
    async fn publish(&self, event: &Event) -> Result<u64, BusError> {
        EventBus::publish(self, event).await
    }
}

/// Name of the JetStream stream that holds all factor-q events.
pub const STREAM_NAME: &str = "fq-events";

/// Subjects captured by the event stream.
///
/// We narrow this from the original `fq.>` so the separate trigger
/// stream (`fq.trigger.>`) can claim its subject without overlap.
/// NATS does not allow two JetStream streams to claim overlapping
/// subjects. `fq.worker.>` is captured here so worker-scoped events
/// (heartbeats, archive acks) reach JetStream consumers and
/// `bus.publish` receives a Pub-Ack — see `worker_heartbeat` and
/// `worker_invocation_archive_acked` in `events::subjects`.
pub const EVENT_STREAM_SUBJECTS: &[&str] = &["fq.agent.>", "fq.system.>", "fq.worker.>"];

/// Default retention for the event stream.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60); // 30 days

/// Name of the JetStream stream that holds pending agent triggers.
/// Separate from the event stream because triggers have different
/// semantics: work-queue delivery (one consumer per message), short
/// retention, and no compression. See
/// `docs/design/committed/storage-and-scaling.md` for the rationale.
pub const TRIGGER_STREAM_NAME: &str = "fq-triggers";

// The subject set the trigger stream captures is
// `subjects::ALL_TRIGGERS`, imported at the top of this file. The
// `fq.trigger.*` vocabulary belongs to the trigger domain and is
// spelled in `crate::events::subjects` (#43): the transport binds a
// stream to a subject set, it does not get to name it. A caller after
// one agent's subject wants `crate::trigger::subject`.

/// The NATS server's default `max_ack_pending` for explicit-ack
/// consumers. The dispatcher never sizes its ack window *below* this:
/// running deployments' durable consumers already carry it as their
/// effective value, and `get_or_create` won't rewrite an existing
/// consumer's config — going lower would mean a config the server
/// silently ignores on existing deployments.
pub const NATS_DEFAULT_MAX_ACK_PENDING: i64 = 1000;

/// Maximum times JetStream may deliver a trigger before it is surfaced
/// as exhausted, re-exported from the contract crate where it is
/// declared. The number is what "exhausted" *means* on the operator
/// surface, so the client that renders it and the consumer config that
/// applies it read the same constant; what lives here is the applying.
pub use fq_ops::surface::TRIGGER_MAX_DELIVER;

/// JetStream stream capturing `MAX_DELIVERIES` advisories for the
/// trigger stream (#169). Advisories are core-NATS fire-and-forget,
/// and the crash that exhausts a trigger also kills any live
/// subscriber — with the retry backoff, the delivery-5 advisory can
/// fire minutes after the crash — so they are captured durably and
/// drained by the control-plane's advisory watch. Same Limits/24h
/// retention as the trigger stream: an advisory only needs to
/// outlive daemon downtime.
pub const ADVISORY_STREAM_NAME: &str = "fq-advisories";

/// Subject the JetStream server publishes when a message on the
/// trigger stream can no longer be delivered (delivery bound
/// reached). One token per consumer name at the tail.
pub fn trigger_max_deliveries_advisory_subject() -> String {
    format!("$JS.EVENT.ADVISORY.CONSUMER.MAX_DELIVERIES.{TRIGGER_STREAM_NAME}.>")
}

/// Escalating redelivery schedule paired with [`TRIGGER_MAX_DELIVER`]:
/// entry N delays redelivery N+1. Applied twice — as the consumer's
/// `backoff` (paces ack-wait redelivery when a dispatcher crashes
/// mid-delivery) and as the dispatcher's explicit NAK delay (a bare
/// `Nak(None)` redelivers immediately, overriding the consumer
/// schedule). JetStream requires `max_deliver` > the schedule length,
/// so four entries cover the four retries after the first delivery.
///
/// **The first entry is also the trigger durable's real first-delivery
/// deadline.** JetStream *replaces* a consumer's `ack_wait` with
/// `backoff[0]` wherever a schedule is set, so the trigger consumer's
/// ack window is one second — not `[bus] ack_wait_ms` — and a
/// dispatcher that takes longer than that to reach its first WAL write
/// has its trigger redelivered underneath it. That is the duplicate
/// -invocation storm of <https://github.com/bricef/factor-q/issues/327>,
/// which owns its own design; moving this number belongs there and not
/// to whoever is next reading this line.
pub const TRIGGER_RETRY_BACKOFF: [std::time::Duration; 4] = [
    std::time::Duration::from_secs(1),
    std::time::Duration::from_secs(5),
    std::time::Duration::from_secs(30),
    std::time::Duration::from_secs(120),
];

/// Default retention for the trigger stream. Triggers are short-lived
/// — the dispatcher consumes them within seconds under normal
/// operation. A 24h window is a safety net against a runaway
/// backlog, not a promise that normal triggers live that long.
pub const DEFAULT_TRIGGER_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60); // 24 hours

/// Errors from the event bus.
#[derive(Debug, thiserror::Error)]
pub enum BusError {
    #[error("failed to connect to NATS: {0}")]
    Connect(#[from] async_nats::ConnectError),

    #[error("failed to ensure stream: {0}")]
    Stream(String),

    #[error("failed to publish event: {0}")]
    Publish(String),

    #[error("failed to subscribe: {0}")]
    Subscribe(String),

    #[error("failed to serialise event: {0}")]
    Serialise(#[from] serde_json::Error),

    /// The serialised event exceeds the NATS server's advertised
    /// `max_payload`. Returned by the pre-flight guard in
    /// [`EventBus::publish`] *before* any bytes reach the wire, so an
    /// oversized event never trips a NATS "Maximum Payload Violation"
    /// nor poisons the archive-retry sweep. See issue #4.
    #[error("event payload of {size} bytes exceeds NATS max_payload of {limit} bytes")]
    PayloadTooLarge { size: usize, limit: usize },

    /// The trigger body exceeds
    /// [`crate::trigger::MAX_TRIGGER_PAYLOAD_BYTES`]. A domain limit,
    /// not the broker's: a trigger is retained indefinitely, so the
    /// ceiling is on what is *accepted* rather than on what the wire
    /// happens to carry. Refused before publishing, so an oversized
    /// trigger never reaches the stream and never becomes a record.
    #[error(
        "trigger payload of {size} bytes exceeds the {limit}-byte limit on an accepted trigger"
    )]
    TriggerPayloadTooLarge { size: usize, limit: usize },
}

impl From<async_nats::jetstream::context::CreateStreamError> for BusError {
    fn from(err: async_nats::jetstream::context::CreateStreamError) -> Self {
        BusError::Stream(err.to_string())
    }
}

impl From<async_nats::jetstream::context::PublishError> for BusError {
    fn from(err: async_nats::jetstream::context::PublishError) -> Self {
        BusError::Publish(err.to_string())
    }
}

impl From<async_nats::SubscribeError> for BusError {
    fn from(err: async_nats::SubscribeError) -> Self {
        BusError::Subscribe(err.to_string())
    }
}

/// The factor-q event bus. Wraps a NATS client and JetStream context.
#[derive(Clone)]
pub struct EventBus {
    client: async_nats::Client,
    jetstream: jetstream::Context,
    /// The server's advertised `max_payload`, read from the NATS
    /// server INFO at connect time. Every publish is size-checked
    /// against this at the shared seam ([`Self::publish`]) so a
    /// known limit is enforced at the boundary rather than
    /// discovered through a runtime protocol violation (Design
    /// Principle 7; issue #4).
    max_payload: usize,
    /// How every durable this bus creates paces redelivery, and how
    /// long the server waits for an ack before redelivering on its own.
    /// Held here because the bus is what *creates* the durables: the
    /// policy has to be in scope wherever a consumer config is stamped
    /// and wherever a transient handler failure is answered, and those
    /// are both reached through this handle. Defaults until
    /// [`Self::with_redelivery_policy`] applies `[bus]` from `fqd.toml`.
    redelivery: ConsumerRedeliveryPolicy,
}

/// Connect options for the broker: token auth when a token is given,
/// anonymous otherwise. The credential arrives as its own argument and
/// never as URL userinfo — `NatsConfig::validate` refuses that shape and
/// nothing here parses it (`async_nats` ignores userinfo too) — so a URL
/// that reaches a log line, the banner or the `system.startup` event is
/// printable by construction (#540).
fn connect_options(token: Option<&str>) -> async_nats::ConnectOptions {
    match token {
        Some(token) => async_nats::ConnectOptions::with_token(token.to_string()),
        None => async_nats::ConnectOptions::new(),
    }
}

/// Pre-flight payload size check: the pure heart of the publish
/// guard, factored out so it can be tested without a live NATS
/// server. Returns [`BusError::PayloadTooLarge`] when the serialised
/// event would exceed the server's advertised `max_payload`.
///
/// NATS rejects a publish whose body is strictly greater than
/// `max_payload`; a body exactly equal to the limit is accepted, so
/// the guard uses a strict `>` comparison to mirror the server.
fn check_payload_size(size: usize, limit: usize) -> Result<(), BusError> {
    if size > limit {
        return Err(BusError::PayloadTooLarge { size, limit });
    }
    Ok(())
}

impl EventBus {
    /// Connect anonymously — a private test broker or an unauthenticated
    /// deployment. See [`Self::connect_with_token`].
    pub async fn connect(url: &str) -> Result<Self, BusError> {
        Self::connect_with_token(url, None).await
    }

    /// Connect to a NATS server, presenting `token` when the broker
    /// requires one, and ensure the event, trigger and advisory streams
    /// exist. The URL is logged as given: it carries no credential by
    /// construction (see `connect_options`).
    pub async fn connect_with_token(url: &str, token: Option<&str>) -> Result<Self, BusError> {
        info!(
            nats_url = url,
            token_auth = token.is_some(),
            "connecting to NATS"
        );
        let client = connect_options(token).connect(url).await?;
        let max_payload = client.server_info().max_payload;
        info!(max_payload, "NATS server max_payload");
        let jetstream = jetstream::new(client.clone());

        let bus = Self {
            client,
            jetstream,
            max_payload,
            redelivery: ConsumerRedeliveryPolicy::default(),
        };
        bus.ensure_event_stream().await?;
        bus.ensure_trigger_stream().await?;
        bus.ensure_advisory_stream().await?;
        Ok(bus)
    }

    /// Apply an operator's `[bus]` settings to this handle. Applied
    /// after connect rather than passed through it because connecting
    /// only ensures streams; nothing durable has been created yet, so
    /// every consumer this bus goes on to make sees the policy.
    ///
    /// The handle is cloned into each consumer task, and the policy
    /// travels with the clone — there is no way to hold a bus and reach
    /// a different policy than the one its durables were stamped with.
    pub fn with_redelivery_policy(mut self, policy: ConsumerRedeliveryPolicy) -> Self {
        self.redelivery = policy;
        self
    }

    /// This bus's redelivery policy — what a consumer loop NAKs with,
    /// and what health measures "stuck" against.
    pub fn redelivery_policy(&self) -> ConsumerRedeliveryPolicy {
        self.redelivery
    }

    /// A clone of the bus's JetStream context, so co-resident consumers
    /// (the read service's health probe) share the daemon's one NATS
    /// connection instead of opening their own.
    pub fn jetstream(&self) -> jetstream::Context {
        self.jetstream.clone()
    }

    /// Ensure the factor-q event stream exists, creating it if necessary.
    ///
    /// S2 compression is enabled on creation. Events are text-heavy
    /// (JSON with large system prompts and tool outputs) and compress
    /// 2–4x with negligible CPU cost, which meaningfully extends the
    /// retention window at a given storage budget. See
    /// `docs/design/committed/storage-and-scaling.md` for the rationale.
    async fn ensure_event_stream(&self) -> Result<(), BusError> {
        debug!(
            stream = STREAM_NAME,
            "ensuring JetStream event stream exists"
        );
        let config = stream::Config {
            name: STREAM_NAME.to_string(),
            subjects: EVENT_STREAM_SUBJECTS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            retention: stream::RetentionPolicy::Limits,
            storage: stream::StorageType::File,
            max_age: DEFAULT_MAX_AGE,
            compression: Some(stream::Compression::S2),
            ..Default::default()
        };
        // Create-or-update: get_or_create creates a fresh stream
        // but won't change the config of an existing one, so a
        // pre-existing cluster with stale `subjects` (e.g. a
        // pre-`fq.worker.>` deployment) would silently drop
        // worker-scoped publishes. update_stream applies the
        // current config to whatever the server has.
        self.jetstream.get_or_create_stream(config.clone()).await?;
        self.jetstream.update_stream(&config).await?;
        Ok(())
    }

    /// Ensure the factor-q trigger stream exists, creating it if
    /// necessary.
    ///
    /// Uses `Limits` retention with a short `max_age` rather than
    /// `WorkQueue`. Work-queue streams disallow overlapping consumer
    /// filters at the NATS level (error code 10100), which makes
    /// parallel test consumers and broad production consumers
    /// fundamentally incompatible. `Limits` retention allows any
    /// number of consumers with any filters — each consumer just
    /// tracks its own position, and messages age out after the
    /// retention window.
    ///
    /// For phase 1 single-runtime deployments this is equivalent in
    /// practice: the production dispatcher consumes each trigger
    /// quickly, and the 24h retention window ensures space is
    /// reclaimed even if the runtime is down. If horizontal
    /// scaling of dispatchers becomes a concern, we will revisit
    /// (likely with an explicit queue-group pattern on top of the
    /// limits stream, or a separate stream per runtime instance).
    ///
    /// Unlike the event stream, the trigger stream is not compressed
    /// — messages are short-lived and small, so the CPU cost of
    /// compression is not justified.
    async fn ensure_trigger_stream(&self) -> Result<(), BusError> {
        debug!(
            stream = TRIGGER_STREAM_NAME,
            "ensuring JetStream trigger stream exists"
        );
        self.jetstream
            .get_or_create_stream(stream::Config {
                name: TRIGGER_STREAM_NAME.to_string(),
                subjects: vec![ALL_TRIGGERS.to_string()],
                retention: stream::RetentionPolicy::Limits,
                storage: stream::StorageType::File,
                max_age: DEFAULT_TRIGGER_MAX_AGE,
                ..Default::default()
            })
            .await?;
        Ok(())
    }

    /// Ensure the advisory capture stream exists (#169). Capture must
    /// be server-side and always-on: the advisory a crashed dispatcher
    /// leaves behind fires while no subscriber is alive to hear it.
    async fn ensure_advisory_stream(&self) -> Result<(), BusError> {
        debug!(
            stream = ADVISORY_STREAM_NAME,
            "ensuring JetStream advisory capture stream exists"
        );
        self.jetstream
            .get_or_create_stream(stream::Config {
                name: ADVISORY_STREAM_NAME.to_string(),
                subjects: vec![trigger_max_deliveries_advisory_subject()],
                retention: stream::RetentionPolicy::Limits,
                storage: stream::StorageType::File,
                max_age: DEFAULT_TRIGGER_MAX_AGE,
                ..Default::default()
            })
            .await?;
        Ok(())
    }

    /// Publish an event to the bus.
    ///
    /// The event's subject is derived from its payload type via
    /// [`Event::subject`]. Publishing awaits the JetStream ack, confirming
    /// the event was durably stored, and returns the event's sequence on
    /// the `fq-events` stream — the coordinate a command's receipt hands
    /// back for read-your-writes against the projection watermark
    /// (mirrors [`EventBus::publish_trigger`]).
    pub async fn publish(&self, event: &Event) -> Result<u64, BusError> {
        let subject = event.subject();
        let payload = serde_json::to_vec(event)?;
        debug!(subject = %subject, event_id = %event.envelope.event_id, "publishing event");

        // Pre-flight payload guard (issue #4). This is the single
        // seam every event publish passes through — the live
        // invocation path and the archive-retry sweeper (which
        // republishes through `EventSink::publish` -> here) both hit
        // it. Reject an oversized event with a clear, attributable
        // error *before* the bytes reach NATS, rather than tripping a
        // "Maximum Payload Violation" that errors the invocation and
        // poisons the retry loop.
        check_payload_size(payload.len(), self.max_payload)?;

        let ack = self
            .jetstream
            .publish(subject, Bytes::from(payload))
            .await?
            .await?;
        Ok(ack.sequence)
    }

    /// Subscribe to events matching a subject filter.
    ///
    /// Uses core NATS subscribe (not a durable JetStream consumer), so the
    /// stream only delivers events published after the subscription is
    /// established. Suitable for live tailing.
    ///
    /// Each item in the returned stream is either a deserialised [`Event`]
    /// or a [`BusError`] if deserialisation fails.
    pub async fn subscribe(
        &self,
        subject_filter: impl Into<String>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Event, BusError>> + Send>>, BusError> {
        let subject = subject_filter.into();
        debug!(subject = %subject, "subscribing to events");

        let subscriber = self.client.subscribe(subject).await?;
        let stream = subscriber.map(|msg| {
            serde_json::from_slice::<Event>(&msg.payload).map_err(|err| {
                warn!(error = %err, "failed to deserialise event");
                BusError::Serialise(err)
            })
        });
        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests;
