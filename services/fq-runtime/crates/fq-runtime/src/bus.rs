//! NATS-backed event bus.
//!
//! Provides a typed interface for publishing and subscribing to factor-q
//! events. All events flow through a single JetStream stream, using
//! subject-based filtering for consumption.
//!
//! See `docs/design/committed/event-schema.md` for the event schema and subject
//! hierarchy.

use async_nats::jetstream::{self, consumer, stream};
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
    async fn publish(&self, event: &Event) -> Result<(), BusError>;
}

#[async_trait::async_trait]
impl EventSink for EventBus {
    async fn publish(&self, event: &Event) -> Result<(), BusError> {
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

/// Control subject a running `fq run` daemon subscribes to for a
/// hot-reload of agent definitions. Published by `fq reload`
/// (fire-and-forget, core NATS — deliberately NOT one of the
/// JetStream stream subjects, so a reload signal is ephemeral: if
/// no daemon is listening it is simply a no-op, never a queued
/// backlog). A reload affects the NEXT trigger only; in-flight
/// invocations keep the config they snapshotted at trigger time
/// (ADR-0020 refresh-between-invocations precedent).
pub const CONTROL_RELOAD_SUBJECT: &str = "fq.control.reload";

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
        Ok(bus)
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

    /// Publish an event to the bus.
    ///
    /// The event's subject is derived from its payload type via
    /// [`Event::subject`]. Publishing awaits the JetStream ack, confirming
    /// the event was durably stored.
    pub async fn publish(&self, event: &Event) -> Result<(), BusError> {
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

        self.jetstream
            .publish(subject, Bytes::from(payload))
            .await?
            .await?;
        Ok(())
    }

    /// Publish a trigger for a given agent. The JSON-encoded
    /// payload becomes the message body. The delivery is ack'd by
    /// JetStream once it's durably accepted, so this returns only
    /// after the trigger is persisted.
    pub async fn publish_trigger(
        &self,
        agent_id: &str,
        payload: &serde_json::Value,
    ) -> Result<(), BusError> {
        let subject = trigger_subject(agent_id);
        let body = serde_json::to_vec(payload)?;
        debug!(subject = %subject, "publishing trigger");
        self.jetstream
            .publish(subject, Bytes::from(body))
            .await?
            .await?;
        Ok(())
    }

    /// Create (or open) a durable JetStream pull consumer on the
    /// trigger stream, filtered to all trigger subjects.
    pub async fn trigger_consumer(&self, name: &str) -> Result<consumer::PullConsumer, BusError> {
        self.trigger_consumer_with_filter(name, ALL_TRIGGERS_SUBJECT)
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
    ) -> Result<consumer::PullConsumer, BusError> {
        debug!(
            consumer = name,
            filter = filter_subject,
            "getting/creating durable trigger consumer"
        );
        let stream = self
            .jetstream
            .get_stream(TRIGGER_STREAM_NAME)
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        let consumer = stream
            .get_or_create_consumer(
                name,
                consumer::pull::Config {
                    durable_name: Some(name.to_string()),
                    ack_policy: consumer::AckPolicy::Explicit,
                    filter_subject: filter_subject.to_string(),
                    ..Default::default()
                },
            )
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
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

    /// Integration test that requires a running NATS server on the default URL.
    ///
    /// Run with `just infra-up` before invoking:
    ///
    /// ```sh
    /// just fq-test
    /// ```
    ///
    /// Skipped unless the `FQ_NATS_URL` environment variable is set.
    #[tokio::test]
    async fn publish_and_subscribe_round_trip() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

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
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };
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
