//! NATS-backed event bus.
//!
//! Provides a typed interface for publishing and subscribing to factor-q
//! events. All events flow through a single JetStream stream, using
//! subject-based filtering for consumption.
//!
//! See `docs/design/committed/event-schema.md` for the event schema and subject
//! hierarchy.

use async_nats::jetstream::{self, consumer, consumer::FromConsumer, stream};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::events::Event;

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

/// Subject pattern matching all agent triggers.
pub const ALL_TRIGGERS_SUBJECT: &str = "fq.trigger.>";

/// The NATS server's default `max_ack_pending` for explicit-ack
/// consumers. The dispatcher never sizes its ack window *below* this:
/// running deployments' durable consumers already carry it as their
/// effective value, and `get_or_create` won't rewrite an existing
/// consumer's config — going lower would mean a config the server
/// silently ignores on existing deployments.
pub const NATS_DEFAULT_MAX_ACK_PENDING: i64 = 1000;

/// Maximum times JetStream may deliver a trigger before it is surfaced as
/// exhausted. This bounds poison-trigger retries while still allowing a few
/// transient failures to recover.
pub const TRIGGER_MAX_DELIVER: i64 = 5;

/// Escalating redelivery schedule paired with [`TRIGGER_MAX_DELIVER`]:
/// entry N delays redelivery N+1. Applied twice — as the consumer's
/// `backoff` (paces ack-wait redelivery when a dispatcher crashes
/// mid-delivery) and as the dispatcher's explicit NAK delay (a bare
/// `Nak(None)` redelivers immediately, overriding the consumer
/// schedule). JetStream requires `max_deliver` > the schedule length,
/// so four entries cover the four retries after the first delivery.
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

pub const TRIGGER_RETRY_BACKOFF: [std::time::Duration; 4] = [
    std::time::Duration::from_secs(1),
    std::time::Duration::from_secs(5),
    std::time::Duration::from_secs(30),
    std::time::Duration::from_secs(120),
];

/// Control subject a running `fq run` daemon subscribes to for a
/// hot-reload of agent definitions. Published by `fq reload`
/// (fire-and-forget, core NATS — deliberately NOT one of the
/// JetStream stream subjects, so a reload signal is ephemeral: if
/// no daemon is listening it is simply a no-op, never a queued
/// backlog). A reload affects the NEXT trigger only; in-flight
/// invocations keep the config they snapshotted at trigger time
/// (ADR-0020 refresh-between-invocations precedent).
pub const CONTROL_RELOAD_SUBJECT: &str = "fq.control.reload";

/// Core-NATS subject an operator-initiated clean stop is requested on
/// (`fq down`, issue #63). Ephemeral like reload — no daemon
/// listening is a silent no-op. The message body selects the mode:
/// [`DOWN_MODE_DRAIN`] (drain in-flight work to a step boundary, then
/// exit) or [`DOWN_MODE_NOW`] (clean infra teardown + deregister + exit
/// immediately, the `--now` escape hatch). Either way the daemon
/// deregisters its worker and publishes `fq.system.shutdown` on exit, so
/// `fq down` can confirm the process actually stopped.
pub const CONTROL_DOWN_SUBJECT: &str = "fq.control.down";

/// `fq down` mode marker: drain in-flight work to the next step boundary
/// (bounded by `drain_deadline_ms`), then exit — the default.
pub const DOWN_MODE_DRAIN: &str = "drain";

/// `fq down --now` mode marker: skip the drain — clean infra teardown,
/// worker deregister, and immediate exit (equivalent to today's SIGINT,
/// but as a proper confirmable command).
pub const DOWN_MODE_NOW: &str = "now";

/// Default retention for the trigger stream. Triggers are short-lived
/// — the dispatcher consumes them within seconds under normal
/// operation. A 24h window is a safety net against a runaway
/// backlog, not a promise that normal triggers live that long.
pub const DEFAULT_TRIGGER_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60); // 24 hours

/// Build a trigger subject for a given agent id.
pub fn trigger_subject(agent_id: &str) -> String {
    format!("fq.trigger.{agent_id}")
}

/// Extract an agent id from a trigger subject of the form
/// `fq.trigger.<agent_id>`. Agent ids are validated to contain no
/// dots (see `AgentId::new`), so the subject has exactly three
/// dot-separated tokens.
pub fn agent_id_from_trigger_subject(subject: &str) -> Option<&str> {
    let mut parts = subject.splitn(3, '.');
    let first = parts.next()?;
    let second = parts.next()?;
    let third = parts.next()?;
    if first != "fq" || second != "trigger" || third.is_empty() {
        return None;
    }
    Some(third)
}

/// Parse a control-down message body into "drain now?" — `true` for
/// [`DOWN_MODE_NOW`], `false` otherwise. Any unrecognised body (including
/// an empty one) falls back to the safe default: drain to a step boundary
/// rather than a surprise hard stop. Pure so the daemon's dispatch is
/// unit-testable without NATS.
pub fn down_mode_now_from_body(body: &[u8]) -> bool {
    body == DOWN_MODE_NOW.as_bytes()
}

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
}

/// Extract auth credentials from a NATS URL's userinfo.
/// `async_nats::connect` ignores URL userinfo entirely; factor-q
/// honours it so credentials can travel inside `FQ_NATS_URL` /
/// `fq.toml`'s `url` without a separate secret channel (project
/// assessment 2026-07-05, critique #4):
/// `nats://TOKEN@host` selects token auth,
/// `nats://USER:PASS@host` selects user/password auth.
fn url_credentials(url: &str) -> Option<(String, Option<String>)> {
    let parsed = url::Url::parse(url).ok()?;
    let user = parsed.username();
    if user.is_empty() {
        return None;
    }
    Some((user.to_string(), parsed.password().map(str::to_string)))
}

/// Connect a raw NATS client, honouring URL userinfo (see
/// `url_credentials` above). `EventBus::connect` and the CLI's
/// direct client path both route through this.
pub async fn connect_with_url_credentials(
    url: &str,
) -> Result<async_nats::Client, async_nats::ConnectError> {
    let options = match url_credentials(url) {
        Some((user, Some(password))) => {
            async_nats::ConnectOptions::with_user_and_password(user, password)
        }
        Some((token, None)) => async_nats::ConnectOptions::with_token(token),
        None => async_nats::ConnectOptions::new(),
    };
    options.connect(url).await
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
    /// Connect to a NATS server and ensure both the event and
    /// trigger streams exist.
    pub async fn connect(url: &str) -> Result<Self, BusError> {
        info!(nats_url = url, "connecting to NATS");
        let client = connect_with_url_credentials(url).await?;
        let max_payload = client.server_info().max_payload;
        info!(max_payload, "NATS server max_payload");
        let jetstream = jetstream::new(client.clone());

        let bus = Self {
            client,
            jetstream,
            max_payload,
        };
        bus.ensure_event_stream().await?;
        bus.ensure_trigger_stream().await?;
        bus.ensure_advisory_stream().await?;
        Ok(bus)
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
                subjects: vec![ALL_TRIGGERS_SUBJECT.to_string()],
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

    /// Durable consumer over the advisory capture stream (#169).
    /// Bounded like the trigger consumer — a poison advisory must not
    /// redeliver forever either.
    pub async fn advisory_consumer(&self, name: &str) -> Result<consumer::PullConsumer, BusError> {
        debug!(
            consumer = name,
            "getting/creating durable advisory consumer"
        );
        let stream = self
            .jetstream
            .get_stream(ADVISORY_STREAM_NAME)
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        stream
            .get_or_create_consumer(
                name,
                consumer::pull::Config {
                    durable_name: Some(name.to_string()),
                    ack_policy: consumer::AckPolicy::Explicit,
                    max_deliver: TRIGGER_MAX_DELIVER,
                    backoff: TRIGGER_RETRY_BACKOFF.to_vec(),
                    ..Default::default()
                },
            )
            .await
            .map_err(|err| BusError::Stream(err.to_string()))
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

    /// Publish a trigger for a given agent. The JSON-encoded
    /// payload becomes the message body. The delivery is ack'd by
    /// JetStream once it's durably accepted, so this returns only
    /// after the trigger is persisted.
    /// Returns the trigger's sequence on the trigger stream — the
    /// operator-facing handle (`fq dead-letters` reconciles on it).
    pub async fn publish_trigger(
        &self,
        agent_id: &str,
        payload: &serde_json::Value,
    ) -> Result<u64, BusError> {
        let subject = trigger_subject(agent_id);
        let body = serde_json::to_vec(payload)?;
        debug!(subject = %subject, "publishing trigger");
        let ack = self
            .jetstream
            .publish(subject, Bytes::from(body))
            .await?
            .await?;
        Ok(ack.sequence)
    }

    /// Create (or open) a durable JetStream pull consumer on the
    /// trigger stream, filtered to all trigger subjects.
    pub async fn trigger_consumer(
        &self,
        name: &str,
        max_ack_pending: i64,
    ) -> Result<consumer::PullConsumer, BusError> {
        self.trigger_consumer_with_filter(name, ALL_TRIGGERS_SUBJECT, max_ack_pending)
            .await
    }

    /// Create (or open) a durable JetStream pull consumer on the
    /// trigger stream with an explicit filter subject.
    ///
    /// Work-queue streams require every consumer to be "filtered".
    /// Production callers use [`Self::trigger_consumer`] which
    /// passes the broad `fq.trigger.>` pattern. Tests use
    /// narrower filters (e.g. a specific agent's trigger subject)
    /// so that parallel test consumers do not compete for each
    /// other's messages on the same work-queue stream. NATS
    /// delivers each published trigger to exactly one consumer
    /// whose filter matches; with disjoint per-test filters, tests
    /// do not cross-talk.
    pub async fn trigger_consumer_with_filter(
        &self,
        name: &str,
        filter_subject: &str,
        max_ack_pending: i64,
    ) -> Result<consumer::PullConsumer, BusError> {
        debug!(
            consumer = name,
            filter = filter_subject,
            max_ack_pending,
            "getting/creating durable trigger consumer"
        );
        let stream = self
            .jetstream
            .get_stream(TRIGGER_STREAM_NAME)
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        let config = consumer::pull::Config {
            durable_name: Some(name.to_string()),
            ack_policy: consumer::AckPolicy::Explicit,
            filter_subject: filter_subject.to_string(),
            // Explicit ack window, sized by the caller from its
            // concurrency bound (see NATS_DEFAULT_MAX_ACK_PENDING
            // for the floor rationale).
            max_ack_pending,
            // Never retry a poison trigger indefinitely. The dispatcher
            // emits a terminal failure on the last delivery before
            // acknowledging it.
            max_deliver: TRIGGER_MAX_DELIVER,
            // Paces ack-wait redelivery (a crashed dispatcher never
            // reaches the explicit NAK delay in the handle path).
            backoff: TRIGGER_RETRY_BACKOFF.to_vec(),
            ..Default::default()
        };
        let mut consumer = stream
            .get_or_create_consumer(name, config)
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;

        // Existing durable consumers retain their old configuration when
        // opened. Upgrade their retry policy too, otherwise a deployment
        // made before this limit would keep retrying poison triggers forever.
        let existing = consumer
            .info()
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?
            .config
            .clone();
        if existing.max_deliver != TRIGGER_MAX_DELIVER
            || existing.backoff != TRIGGER_RETRY_BACKOFF.to_vec()
        {
            let mut config = consumer::pull::Config::try_from_consumer_config(existing)
                .map_err(|err| BusError::Stream(err.to_string()))?;
            config.max_deliver = TRIGGER_MAX_DELIVER;
            config.backoff = TRIGGER_RETRY_BACKOFF.to_vec();
            return stream
                .update_consumer(config)
                .await
                .map_err(|err| BusError::Stream(err.to_string()));
        }
        Ok(consumer)
    }

    /// Create (or open) a durable JetStream pull consumer on the
    /// factor-q event stream.
    ///
    /// Durable consumers remember their position across restarts, so
    /// the projection consumer can be stopped and restarted without
    /// losing events or redelivering old ones. The returned
    /// [`consumer::PullConsumer`] can be used with `.messages()` to
    /// iterate over delivered messages.
    pub async fn durable_consumer(&self, name: &str) -> Result<consumer::PullConsumer, BusError> {
        debug!(
            consumer = name,
            "getting/creating durable JetStream consumer"
        );
        let stream = self
            .jetstream
            .get_stream(STREAM_NAME)
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        let consumer = stream
            .get_or_create_consumer(
                name,
                consumer::pull::Config {
                    durable_name: Some(name.to_string()),
                    ack_policy: consumer::AckPolicy::Explicit,
                    ..Default::default()
                },
            )
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        Ok(consumer)
    }

    /// [`EventBus::durable_consumer`], with **resolved-contiguous
    /// delivery**: `max_ack_pending = 1`, so the server never delivers
    /// a message until the previous one is resolved (acked — success
    /// or permanent skip). After a NAK the next delivery is the retry
    /// of the same message, which is what makes an advance-on-success
    /// watermark contiguous by construction: sequence S is never
    /// exposed while an earlier sequence is still pending redelivery.
    /// Costs throughput — one outstanding message per round-trip —
    /// which the projection's fold accepts as the price of
    /// read-your-writes.
    ///
    /// `get_or_create` keeps an existing durable's settings, so a
    /// pre-existing consumer is updated in place when its
    /// `max_ack_pending` differs (the same drift-repair the trigger
    /// consumer does for its retry policy).
    pub async fn durable_consumer_strict(
        &self,
        name: &str,
    ) -> Result<consumer::PullConsumer, BusError> {
        debug!(
            consumer = name,
            "getting/creating strict-order durable JetStream consumer"
        );
        let stream = self
            .jetstream
            .get_stream(STREAM_NAME)
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        let consumer = stream
            .get_or_create_consumer(
                name,
                consumer::pull::Config {
                    durable_name: Some(name.to_string()),
                    ack_policy: consumer::AckPolicy::Explicit,
                    max_ack_pending: 1,
                    ..Default::default()
                },
            )
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        let existing = consumer.cached_info().config.clone();
        if existing.max_ack_pending != 1 {
            let mut config = consumer::pull::Config::try_from_consumer_config(existing)
                .map_err(|err| BusError::Stream(err.to_string()))?;
            config.max_ack_pending = 1;
            return stream
                .update_consumer(config)
                .await
                .map_err(|err| BusError::Stream(err.to_string()));
        }
        Ok(consumer)
    }

    /// Durable JetStream consumer scoped to a subject filter.
    ///
    /// Used by the coordination consumer (step 7) which only
    /// cares about a small subset of events
    /// (`fq.agent.*.invocation.*`); subscribing to the whole
    /// event stream would force every coordination consumer
    /// instance to handle messages it doesn't act on.
    pub async fn durable_consumer_with_filter(
        &self,
        name: &str,
        filter_subject: &str,
    ) -> Result<consumer::PullConsumer, BusError> {
        debug!(
            consumer = name,
            filter = filter_subject,
            "getting/creating filtered durable JetStream consumer"
        );
        let stream = self
            .jetstream
            .get_stream(STREAM_NAME)
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        let consumer = stream
            .get_or_create_consumer(
                name,
                consumer::pull::Config {
                    durable_name: Some(name.to_string()),
                    filter_subject: filter_subject.to_string(),
                    ack_policy: consumer::AckPolicy::Explicit,
                    ..Default::default()
                },
            )
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        Ok(consumer)
    }

    /// Durable JetStream consumer scoped to *several* subject
    /// filters. Same shape as
    /// [`Self::durable_consumer_with_filter`], for consumers that
    /// react to a handful of unrelated event types (the summary
    /// consumer, #216: triggered + llm_response + completed +
    /// failed) — a single-wildcard filter would force it to churn
    /// through the tool-event firehose it never acts on.
    pub async fn durable_consumer_with_filters(
        &self,
        name: &str,
        filter_subjects: &[&str],
    ) -> Result<consumer::PullConsumer, BusError> {
        debug!(
            consumer = name,
            filters = ?filter_subjects,
            "getting/creating multi-filter durable JetStream consumer"
        );
        let stream = self
            .jetstream
            .get_stream(STREAM_NAME)
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        let consumer = stream
            .get_or_create_consumer(
                name,
                consumer::pull::Config {
                    durable_name: Some(name.to_string()),
                    filter_subjects: filter_subjects.iter().map(|s| s.to_string()).collect(),
                    ack_policy: consumer::AckPolicy::Explicit,
                    ..Default::default()
                },
            )
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        Ok(consumer)
    }

    /// Like [`Self::durable_consumer_with_filters`] but starting
    /// from new messages only. Test-oriented, mirroring
    /// [`Self::durable_consumer_with_filter_from_new`].
    pub async fn durable_consumer_with_filters_from_new(
        &self,
        name: &str,
        filter_subjects: &[String],
    ) -> Result<consumer::PullConsumer, BusError> {
        debug!(
            consumer = name,
            filters = ?filter_subjects,
            "getting/creating multi-filter durable JetStream consumer (deliver_policy=new)"
        );
        let stream = self
            .jetstream
            .get_stream(STREAM_NAME)
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        let consumer = stream
            .get_or_create_consumer(
                name,
                consumer::pull::Config {
                    durable_name: Some(name.to_string()),
                    filter_subjects: filter_subjects.to_vec(),
                    ack_policy: consumer::AckPolicy::Explicit,
                    deliver_policy: consumer::DeliverPolicy::New,
                    ..Default::default()
                },
            )
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        Ok(consumer)
    }

    /// Like [`Self::durable_consumer_with_filter`] but the
    /// consumer starts from new messages only (skips the
    /// stream's historical messages on first creation).
    ///
    /// Test-oriented: the acceptance harness needs fresh
    /// consumers per test, but the stream is shared across
    /// runs and contains thousands of historical messages.
    /// Starting from `New` avoids the catch-up wait while
    /// keeping production's recovery-from-history semantics
    /// untouched.
    ///
    /// Note: `get_or_create_consumer` returns the existing
    /// consumer's config if `name` already exists, so this
    /// only affects the first creation. Pair with a unique
    /// per-test consumer name to actually get the new
    /// behaviour.
    pub async fn durable_consumer_with_filter_from_new(
        &self,
        name: &str,
        filter_subject: &str,
    ) -> Result<consumer::PullConsumer, BusError> {
        debug!(
            consumer = name,
            filter = filter_subject,
            "getting/creating filtered durable JetStream consumer (deliver_policy=new)"
        );
        let stream = self
            .jetstream
            .get_stream(STREAM_NAME)
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        let consumer = stream
            .get_or_create_consumer(
                name,
                consumer::pull::Config {
                    durable_name: Some(name.to_string()),
                    filter_subject: filter_subject.to_string(),
                    ack_policy: consumer::AckPolicy::Explicit,
                    deliver_policy: consumer::DeliverPolicy::New,
                    ..Default::default()
                },
            )
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        Ok(consumer)
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

    /// Publish a fire-and-forget control message asking any running
    /// `fq run` daemon to hot-reload its agent definitions. Uses core
    /// NATS publish (not JetStream): the signal is ephemeral, so if no
    /// daemon is listening it is silently dropped rather than queued.
    pub async fn publish_control_reload(&self) -> Result<(), BusError> {
        debug!(
            subject = CONTROL_RELOAD_SUBJECT,
            "publishing control reload"
        );
        self.client
            .publish(CONTROL_RELOAD_SUBJECT, Bytes::new())
            .await
            .map_err(|err| BusError::Publish(err.to_string()))?;
        // Flush so the message actually leaves the client before a
        // short-lived CLI process exits.
        self.client
            .flush()
            .await
            .map_err(|err| BusError::Publish(err.to_string()))?;
        Ok(())
    }

    /// Subscribe to the daemon control-reload subject. Each item is a
    /// raw NATS message (the body is unused today); the daemon reacts
    /// to the arrival of the message, not its contents.
    pub async fn subscribe_control_reload(&self) -> Result<async_nats::Subscriber, BusError> {
        debug!(
            subject = CONTROL_RELOAD_SUBJECT,
            "subscribing to control reload"
        );
        let sub = self.client.subscribe(CONTROL_RELOAD_SUBJECT).await?;
        Ok(sub)
    }

    /// Request an operator-initiated clean stop of a running daemon
    /// (`fq down`, issue #63). Publishes on the core-NATS control-down
    /// subject with a body of [`DOWN_MODE_DRAIN`] or [`DOWN_MODE_NOW`] and
    /// flushes. A running `fq run` daemon tears down cleanly, deregisters
    /// its worker, and exits; in drain mode it first suspends in-flight
    /// invocations at a step boundary. No daemon listening is a silent
    /// no-op.
    pub async fn publish_control_down(&self, now: bool) -> Result<(), BusError> {
        let body = if now { DOWN_MODE_NOW } else { DOWN_MODE_DRAIN };
        debug!(
            subject = CONTROL_DOWN_SUBJECT,
            mode = body,
            "publishing control down"
        );
        self.client
            .publish(CONTROL_DOWN_SUBJECT, Bytes::from_static(body.as_bytes()))
            .await
            .map_err(|err| BusError::Publish(err.to_string()))?;
        // Flush so the message leaves the client before the short-lived
        // CLI process exits.
        self.client
            .flush()
            .await
            .map_err(|err| BusError::Publish(err.to_string()))?;
        Ok(())
    }

    /// Subscribe to the daemon control-down subject. Unlike reload,
    /// the daemon reads the message *body* to pick the stop mode (see
    /// [`down_mode_now_from_body`]).
    pub async fn subscribe_control_down(&self) -> Result<async_nats::Subscriber, BusError> {
        debug!(
            subject = CONTROL_DOWN_SUBJECT,
            "subscribing to control down"
        );
        let sub = self.client.subscribe(CONTROL_DOWN_SUBJECT).await?;
        Ok(sub)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentId;

    #[test]
    fn url_credentials_parses_token_user_pass_and_bare_forms() {
        assert_eq!(
            url_credentials("nats://fq-dev-token@127.0.0.1:4222"),
            Some(("fq-dev-token".to_string(), None)),
            "bare userinfo is a token"
        );
        assert_eq!(
            url_credentials("nats://fq:secret@localhost:4222"),
            Some(("fq".to_string(), Some("secret".to_string()))),
            "user:pass form"
        );
        assert_eq!(url_credentials("nats://127.0.0.1:4222"), None);
        assert_eq!(url_credentials("not a url"), None);
    }
    use crate::events::{
        ConfigSnapshot, EventPayload, SandboxSnapshot, TriggerSource, TriggeredPayload,
    };
    use serde_json::json;
    use uuid::Uuid;

    fn aid(s: &str) -> AgentId {
        AgentId::new(s).expect("test agent id must be valid")
    }

    fn sample_event(agent_id: &str) -> Event {
        Event::new(
            aid(agent_id),
            Uuid::now_v7(),
            EventPayload::Triggered(TriggeredPayload {
                trigger_source: TriggerSource::Manual,
                trigger_subject: None,
                trigger_payload: json!({"input": "hello"}),
                config_snapshot: ConfigSnapshot {
                    name: agent_id.to_string(),
                    model: "claude-haiku".to_string(),
                    system_prompt: "Test.".to_string(),
                    tools: vec![],
                    sandbox: SandboxSnapshot {
                        fs_read: vec![],
                        fs_write: vec![],
                        network: vec![],
                        env: vec![],
                        exec_cwd: vec![],
                    },
                    budget: None,
                    ..Default::default()
                },
            }),
        )
    }

    /// Round-trips a publish through a private `nats-server` this test spawns
    /// (#233) — no shared broker, no skip.
    #[tokio::test]
    async fn publish_and_subscribe_round_trip() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

        let bus = EventBus::connect(&url).await.expect("connect to NATS");
        let agent_id = format!("bus-test-{}", Uuid::now_v7().simple());
        let event = sample_event(&agent_id);
        let expected_id = event.envelope.event_id;

        let mut subscriber = bus
            .subscribe(format!("fq.agent.{agent_id}.>"))
            .await
            .expect("subscribe");

        // Give the subscription a moment to register before publishing.
        tokio::time::sleep(Duration::from_millis(50)).await;
        bus.publish(&event).await.expect("publish");

        let received = tokio::time::timeout(Duration::from_secs(2), subscriber.next())
            .await
            .expect("timeout waiting for event")
            .expect("stream closed")
            .expect("deserialise");

        assert_eq!(received.envelope.event_id, expected_id);
        assert_eq!(received.envelope.agent_id.as_str(), agent_id);
    }

    #[test]
    fn down_mode_now_from_body_maps_markers() {
        assert!(down_mode_now_from_body(DOWN_MODE_NOW.as_bytes()));
        assert!(!down_mode_now_from_body(DOWN_MODE_DRAIN.as_bytes()));
        // Unknown / empty body defaults to the safe drain path.
        assert!(!down_mode_now_from_body(b""));
        assert!(!down_mode_now_from_body(b"garbage"));
    }

    /// The pre-flight guard (issue #4) rejects a payload larger than
    /// the server's advertised `max_payload` with a clear, attributable
    /// error, and never reaches NATS. Exercised against the pure seam
    /// so it needs no live server.
    #[test]
    fn payload_guard_rejects_oversized_and_accepts_within_limit() {
        // Strictly over the limit -> rejected with size and limit.
        match check_payload_size(1_048_577, 1_048_576) {
            Err(BusError::PayloadTooLarge { size, limit }) => {
                assert_eq!(size, 1_048_577);
                assert_eq!(limit, 1_048_576);
            }
            other => panic!("expected PayloadTooLarge, got {other:?}"),
        }
        // Exactly at the limit and below are accepted (NATS accepts a
        // body equal to max_payload; only strictly-greater is a
        // violation).
        assert!(check_payload_size(1_048_576, 1_048_576).is_ok());
        assert!(check_payload_size(0, 1_048_576).is_ok());
    }

    /// End-to-end at the serialisation boundary: a real oversized
    /// event (a system prompt padded past a small limit) serialises
    /// to more bytes than the limit, and the guard rejects it cleanly
    /// with the actual serialised size — no NATS round-trip.
    #[test]
    fn oversized_event_is_rejected_by_the_guard() {
        let limit = 1_024usize;
        let mut event = sample_event("guard-test");
        if let EventPayload::Triggered(ref mut p) = event.payload {
            p.config_snapshot.system_prompt = "x".repeat(4_096);
        } else {
            panic!("sample_event should be a Triggered payload");
        }
        let payload = serde_json::to_vec(&event).expect("serialise event");
        assert!(
            payload.len() > limit,
            "test event must exceed the limit to be meaningful"
        );
        match check_payload_size(payload.len(), limit) {
            Err(BusError::PayloadTooLarge { size, limit: l }) => {
                assert_eq!(size, payload.len());
                assert_eq!(l, limit);
            }
            other => panic!("expected PayloadTooLarge, got {other:?}"),
        }
    }

    /// Annotations live on the wire — the barrier (envelope-refactor
    /// plan step 4) is at the consumer-context boundary, not at the
    /// bus. A producer can attach annotations to a published event
    /// and a subscriber that deserialises the same event sees them
    /// intact; only `Event::for_consumer_context` strips them when
    /// building a downstream agent's prompt input.
    #[tokio::test]
    async fn annotations_preserved_through_publish_round_trip() {
        use crate::events::annotation_keys;
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();
        let bus = EventBus::connect(&url).await.expect("connect to NATS");
        let agent_id = format!("bus-anno-{}", Uuid::now_v7().simple());
        let event = sample_event(&agent_id)
            .annotate(annotation_keys::NOTES, json!("hi"))
            .annotate(annotation_keys::CONFIDENCE, json!(0.8));

        let mut subscriber = bus
            .subscribe(format!("fq.agent.{agent_id}.>"))
            .await
            .expect("subscribe");
        tokio::time::sleep(Duration::from_millis(50)).await;
        bus.publish(&event).await.expect("publish");

        let received = tokio::time::timeout(Duration::from_secs(2), subscriber.next())
            .await
            .expect("timeout waiting for event")
            .expect("stream closed")
            .expect("deserialise");

        assert_eq!(received.annotations.0.len(), 2);
        assert_eq!(
            received.annotations.0.get(annotation_keys::NOTES),
            Some(&json!("hi"))
        );
        assert_eq!(
            received.annotations.0.get(annotation_keys::CONFIDENCE),
            Some(&json!(0.8))
        );
    }
}
