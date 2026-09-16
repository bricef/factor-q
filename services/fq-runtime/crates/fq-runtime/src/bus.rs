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
use crate::events::subjects::{ALL_MAINTENANCE, ALL_TRIGGERS};

mod consumers;
mod ledger;
pub mod retry;

pub use consumers::UNLIMITED_MAX_DELIVER;
pub use ledger::{ConsumerLedger, ConsumerRecord};
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

/// Name of the JetStream stream that holds pending maintenance
/// commands (#257) — the subjects an external scheduler (fq-cron)
/// publishes to ask this daemon to run a named housekeeping task.
///
/// **Its own stream, not the event stream.** Under ADR-0026 the event
/// log is the system of record for *facts*; a maintenance command is
/// an instruction, and admitting one to `fq-events` would put it in
/// front of every whole-stream durable there — the projector included
/// — which would read it as an event, find no envelope, and count it
/// malformed. It would also inherit the event stream's 30-day
/// retention and S2 compression, neither of which a command that is
/// stale within the hour wants.
///
/// **And not the trigger stream either**, though the delivery profile
/// matches: `fq-triggers` carries a finite `max_deliver`, a retry
/// backoff schedule and a MAX_DELIVERIES advisory capture, all of them
/// shaped around dispatching an agent invocation with a dead-letter
/// path. A maintenance command has none of that. Widening the trigger
/// stream's subject set would also not reach an existing deployment:
/// `ensure_trigger_stream` is `get_or_create` only, so a
/// broker that already holds `fq-triggers` would silently keep the old
/// subject list and every maintenance publish would fail with "no
/// stream matches subject".
///
/// Same shape as the trigger stream otherwise: `Limits` retention,
/// file storage, [`DEFAULT_MAINTENANCE_MAX_AGE`], no compression.
pub const MAINTENANCE_STREAM_NAME: &str = "fq-maintenance";

/// Retention for the maintenance stream. A maintenance command is
/// worth running late — a daemon restarted at 02:05 should still run
/// the 02:00 sweep — and worthless a day later, by which time the next
/// scheduled fire has been and gone. Matches the trigger stream's
/// window for the same reason: a safety net against a backlog, not a
/// promise that a command lives that long.
pub const DEFAULT_MAINTENANCE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

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
/// ack window is 30 seconds — not `[bus] ack_wait_ms` — giving the
/// 250 ms hold keepalive about 120 ticks to reach its first WAL write
/// without the trigger being redelivered underneath it. The tradeoff is
/// that a dispatcher crash before durable start is recovered after 30
/// seconds instead of one. This is a stopgap for the duplicate-invocation
/// storm of <https://github.com/bricef/factor-q/issues/327>, which owns
/// the durable design.
pub const TRIGGER_RETRY_BACKOFF: [std::time::Duration; 4] = [
    std::time::Duration::from_secs(30),
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

    /// Bytes read back off the stream are not an event this build can
    /// read: JSON that does not parse, or a supported schema version
    /// whose body does not match its shape.
    ///
    /// The read-side sibling of [`Self::Serialise`], which is the
    /// publish path's. One variant used to serve both, so every read
    /// failure printed "failed to serialise event" and sent the reader
    /// looking at the publisher — the wrong end of the pipe
    /// (<https://github.com/bricef/factor-q/issues/673>).
    #[error("failed to read event: {0}")]
    Deserialise(#[source] serde_json::Error),

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
    /// Retention stamped onto the payload-bearing event stream at connect.
    event_max_age: Duration,
    /// How every durable this bus creates paces redelivery, and how
    /// long the server waits for an ack before redelivering on its own.
    /// Held here because the bus is what *creates* the durables: the
    /// policy has to be in scope wherever a consumer config is stamped
    /// and wherever a transient handler failure is answered, and those
    /// are both reached through this handle. Defaults until
    /// [`Self::with_redelivery_policy`] applies `[bus]` from `fqd.toml`.
    redelivery: ConsumerRedeliveryPolicy,
    /// What the consumer loops run on this bus report about their
    /// parse boundary — a halt on a version this build cannot read,
    /// and the count of malformed messages acked. Held here for the
    /// same reason as the policy: every loop and every health probe
    /// reaches the bus, so this is the one handle through which the
    /// loop's account of itself can reach `fq doctor` without new
    /// wiring. Shared by every clone.
    ledger: ConsumerLedger,
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
        Self::connect_with_token_and_max_age(url, None, DEFAULT_MAX_AGE).await
    }

    /// Connect anonymously and apply an explicit event-stream retention.
    pub async fn connect_with_max_age(url: &str, max_age: Duration) -> Result<Self, BusError> {
        Self::connect_with_token_and_max_age(url, None, max_age).await
    }

    /// Connect to a NATS server, presenting `token` when the broker
    /// requires one, and ensure the event, trigger and advisory streams
    /// exist. The URL is logged as given: it carries no credential by
    /// construction (see `connect_options`).
    pub async fn connect_with_token(url: &str, token: Option<&str>) -> Result<Self, BusError> {
        Self::connect_with_token_and_max_age(url, token, DEFAULT_MAX_AGE).await
    }

    /// Connect with optional token auth and an explicit event-stream retention.
    pub async fn connect_with_token_and_max_age(
        url: &str,
        token: Option<&str>,
        event_max_age: Duration,
    ) -> Result<Self, BusError> {
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
            event_max_age,
            redelivery: ConsumerRedeliveryPolicy::default(),
            ledger: ConsumerLedger::default(),
        };
        bus.ensure_event_stream().await?;
        bus.ensure_trigger_stream().await?;
        bus.ensure_advisory_stream().await?;
        bus.ensure_maintenance_stream().await?;
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

    /// What the consumer loops on this bus have reported about their
    /// parse boundary: written by the loops, read by the health probe.
    pub fn consumer_ledger(&self) -> &ConsumerLedger {
        &self.ledger
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
            max_age: self.event_max_age,
            compression: Some(stream::Compression::S2),
            ..Default::default()
        };
        self.reconcile_managed_stream_config(config).await?;
        Ok(())
    }

    /// Create a stream when absent, or update only the fields factor-q manages.
    async fn reconcile_managed_stream_config(
        &self,
        desired: stream::Config,
    ) -> Result<(), BusError> {
        self.jetstream.get_or_create_stream(desired.clone()).await?;

        let mut existing = self
            .jetstream
            .get_stream(&desired.name)
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        let current = existing
            .info()
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?
            .config
            .clone();
        let changes = managed_stream_config_diff(&current, &desired);
        if changes.is_empty() {
            return Ok(());
        }

        let update = stream::Config {
            subjects: desired.subjects,
            retention: desired.retention,
            storage: desired.storage,
            max_age: desired.max_age,
            compression: desired.compression,
            ..current
        };
        self.jetstream.update_stream(&update).await?;
        info!(
            stream = %desired.name,
            changes = %changes.join(", "),
            "updated managed JetStream stream config"
        );
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

    /// Ensure the maintenance command stream exists (#257).
    ///
    /// Ensured unconditionally, even when `[maintenance] enabled` is
    /// false. A publisher must have somewhere to publish: fq-cron's
    /// durable publish of a job whose subject no stream matches is a
    /// *permanent* error on its side — the job is marked unhealthy
    /// until a reload (fq-cron D5) — so a daemon that creates the
    /// stream only when it intends to consume would turn "maintenance
    /// is switched off here" into "the scheduler is broken". With the
    /// stream always present, a disabled daemon simply leaves the
    /// commands to age out.
    ///
    /// Create-then-update, like [`Self::ensure_event_stream`] and
    /// unlike [`Self::ensure_trigger_stream`]: `get_or_create_stream`
    /// alone leaves an *existing* stream at whatever config the build
    /// that first created it asked for, so a later change to
    /// [`DEFAULT_MAINTENANCE_MAX_AGE`] or to the subject set would
    /// reach a fresh broker and never the deployed one. That is the
    /// class of defect this PR's own body cites for the trigger stream
    /// (<https://github.com/bricef/factor-q/issues/187>), so the new
    /// stream does not repeat it.
    async fn ensure_maintenance_stream(&self) -> Result<(), BusError> {
        debug!(
            stream = MAINTENANCE_STREAM_NAME,
            "ensuring JetStream maintenance stream exists"
        );
        let config = stream::Config {
            name: MAINTENANCE_STREAM_NAME.to_string(),
            subjects: vec![ALL_MAINTENANCE.to_string()],
            retention: stream::RetentionPolicy::Limits,
            storage: stream::StorageType::File,
            max_age: DEFAULT_MAINTENANCE_MAX_AGE,
            ..Default::default()
        };
        self.reconcile_managed_stream_config(config).await?;
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
                BusError::Deserialise(err)
            })
        });
        Ok(Box::pin(stream))
    }
}

/// Compare only stream fields owned by factor-q. The rendered entries are also
/// the operator-facing audit detail when reconciliation is required.
fn managed_stream_config_diff(current: &stream::Config, desired: &stream::Config) -> Vec<String> {
    let mut changes = Vec::new();
    if current.subjects != desired.subjects {
        changes.push(format!(
            "subjects: {:?} -> {:?}",
            current.subjects, desired.subjects
        ));
    }
    if current.retention != desired.retention {
        changes.push(format!(
            "retention: {:?} -> {:?}",
            current.retention, desired.retention
        ));
    }
    if current.storage != desired.storage {
        changes.push(format!(
            "storage: {:?} -> {:?}",
            current.storage, desired.storage
        ));
    }
    if current.max_age != desired.max_age {
        changes.push(format!(
            "max_age: {:?} -> {:?}",
            current.max_age, desired.max_age
        ));
    }
    if current.compression != desired.compression {
        changes.push(format!(
            "compression: {:?} -> {:?}",
            current.compression, desired.compression
        ));
    }
    changes
}

#[cfg(test)]
mod tests;
