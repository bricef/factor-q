//! MAX_DELIVERIES advisory watch (#169): the durable backstop for
//! trigger exhaustion the dispatcher never observes.
//!
//! The dispatcher's inline dead-letter path (#49) ACKs the final
//! delivery, and an ACK suppresses the server's MAX_DELIVERIES
//! advisory — probed empirically against a live broker (2026-07-14):
//!
//! - NAK on the final delivery → advisory fires
//! - ACK on the final delivery → **no advisory** (the inline path and
//!   this watch are mutually exclusive in every non-crash path)
//! - never acked (crashed dispatcher; ack_wait lapse) → advisory fires
//! - bound lowered below an existing message's delivery count (the
//!   consumer-upgrade scenario) → advisory fires on the next
//!   redelivery evaluation
//!
//! So exactly the exhaustion the inline path misses produces an
//! advisory — and it can fire while no dispatcher is alive to hear it
//! (with the retry backoff, up to ~2 minutes after a crash), which is
//! why advisories are captured server-side into a durable stream
//! ([`crate::bus::ADVISORY_STREAM_NAME`]) rather than subscribed live.
//!
//! Delivery semantics: at-least-once end to end, like every producer
//! on this bus (the archive sweeper republishes too). A watch crash
//! between emit and ack can duplicate one event; the shared
//! `trigger_stream_seq` annotation makes duplicates reconcilable, and
//! a best-effort last-event check suppresses the common overlap.

use futures::StreamExt;
use serde::Deserialize;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::agent::AgentId;
use crate::bus::{
    ADVISORY_STREAM_NAME, BusError, EventBus, STREAM_NAME, TRIGGER_MAX_DELIVER,
    TRIGGER_STREAM_NAME, agent_id_from_trigger_subject,
};
use crate::events::{
    DEAD_LETTER_PAYLOAD_KEY, DEAD_LETTER_SOURCE_KEY, DEAD_LETTER_STREAM_SEQ_KEY,
    DEAD_LETTER_SUBJECT_KEY, Event, EventPayload, FailedPayload, FailureKind, FailurePhase,
    InvocationTotals, subjects,
};

/// Name of the durable JetStream consumer the advisory watch creates.
pub const CONSUMER_NAME: &str = "fq-advisory-watch";

/// The fields this watch consumes from the server's
/// `io.nats.jetstream.advisory.v1.max_deliver` payload; unknown
/// fields are ignored.
#[derive(Debug, Deserialize)]
struct MaxDeliverAdvisory {
    stream: String,
    consumer: String,
    stream_seq: u64,
    deliveries: i64,
}

/// Errors that prevent the watch from starting or progressing.
#[derive(Debug, thiserror::Error)]
pub enum AdvisoryWatchError {
    #[error("bus error: {0}")]
    Bus(#[from] BusError),

    #[error("advisory stream error: {0}")]
    Stream(String),
}

/// What handling one advisory did — returned for logs and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvisoryOutcome {
    /// A dead-letter event was emitted.
    Emitted,
    /// The inline path already emitted for this stream sequence.
    AlreadySurfaced,
    /// Unparseable or foreign advisory; consumed without effect.
    Skipped,
}

/// The advisory watch. Owns the bus; spawn as a tokio task via
/// [`Self::run`], alongside the dispatcher whose blind spots it
/// covers.
pub struct AdvisoryWatch {
    bus: EventBus,
}

impl AdvisoryWatch {
    pub fn new(bus: EventBus) -> Self {
        Self { bus }
    }

    /// Run the watch loop until `shutdown` fires.
    pub async fn run(self, mut shutdown: oneshot::Receiver<()>) -> Result<(), AdvisoryWatchError> {
        info!(stream = ADVISORY_STREAM_NAME, "advisory watch starting");
        let consumer = self.bus.advisory_consumer(CONSUMER_NAME).await?;
        let mut messages = consumer
            .messages()
            .await
            .map_err(|err| AdvisoryWatchError::Stream(err.to_string()))?;

        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => {
                    info!("advisory watch received shutdown signal");
                    break;
                }
                msg = messages.next() => {
                    match msg {
                        Some(Ok(msg)) => match self.handle_advisory(&msg.payload).await {
                            Ok(outcome) => {
                                debug!(?outcome, "advisory handled");
                                if let Err(err) = msg.ack().await {
                                    error!(error = %err, "failed to ack advisory");
                                }
                            }
                            Err(err) => {
                                // Transient bus/stream trouble: NAK for a
                                // bounded retry (the advisory consumer
                                // carries the same delivery bound and
                                // backoff as the trigger consumer).
                                warn!(error = %err, "advisory handling failed; nak for retry");
                                if let Err(nak_err) = msg
                                    .ack_with(async_nats::jetstream::AckKind::Nak(None))
                                    .await
                                {
                                    error!(error = %nak_err, "failed to nak advisory");
                                }
                            }
                        },
                        Some(Err(err)) => {
                            warn!(error = %err, "error reading advisory message");
                        }
                        None => {
                            warn!("advisory stream ended unexpectedly");
                            break;
                        }
                    }
                }
            }
        }

        info!("advisory watch stopped");
        Ok(())
    }

    /// Handle one captured advisory payload. `pub(crate)` so the
    /// broker-backed tests drive it directly.
    pub(crate) async fn handle_advisory(
        &self,
        payload: &[u8],
    ) -> Result<AdvisoryOutcome, AdvisoryWatchError> {
        let advisory: MaxDeliverAdvisory = match serde_json::from_slice(payload) {
            Ok(a) => a,
            Err(err) => {
                warn!(error = %err, "unparseable advisory; consuming");
                return Ok(AdvisoryOutcome::Skipped);
            }
        };
        // The capture subject is scoped to the trigger stream; be
        // defensive about a widened filter anyway.
        if advisory.stream != TRIGGER_STREAM_NAME {
            debug!(stream = %advisory.stream, "advisory for a foreign stream; skipping");
            return Ok(AdvisoryOutcome::Skipped);
        }

        // The original trigger: subject → agent, payload → annotation.
        // Triggers age out after 24h — an older advisory still
        // surfaces, attributed to the system sentinel.
        let trigger = self
            .bus
            .jetstream()
            .get_stream(TRIGGER_STREAM_NAME)
            .await
            .map_err(|err| AdvisoryWatchError::Stream(err.to_string()))?
            .get_raw_message(advisory.stream_seq)
            .await
            .ok();

        let (agent_id, trigger_subject, trigger_payload) = match &trigger {
            Some(raw) => (
                agent_id_from_trigger_subject(&raw.subject).and_then(|id| AgentId::new(id).ok()),
                raw.subject.to_string(),
                serde_json::from_slice(&raw.payload).unwrap_or(serde_json::Value::Null),
            ),
            None => (None, String::new(), serde_json::Value::Null),
        };

        // Best-effort dedup vs the inline fast path. Its ACK suppresses
        // the advisory, so overlap requires an emit-then-crash-before-
        // ack sliver; if the agent's most recent failed event already
        // carries this stream sequence, skip.
        if let Some(agent) = &agent_id
            && self.already_surfaced(agent, advisory.stream_seq).await
        {
            info!(
                agent_id = %agent,
                stream_seq = advisory.stream_seq,
                "exhausted trigger already surfaced inline; skipping"
            );
            return Ok(AdvisoryOutcome::AlreadySurfaced);
        }

        let payload = EventPayload::Failed(FailedPayload {
            error_kind: FailureKind::TriggerExhausted,
            error_message: format!(
                "trigger exhausted after {} deliveries (limit {TRIGGER_MAX_DELIVER}) — \
                 surfaced from the MAX_DELIVERIES advisory (consumer {})",
                advisory.deliveries, advisory.consumer
            ),
            phase: FailurePhase::Setup,
            partial_totals: InvocationTotals::default(),
        });
        let event = match &agent_id {
            Some(agent) => Event::new(agent.clone(), Uuid::now_v7(), payload),
            // Trigger aged out of the stream: the agent is unknowable,
            // but the exhaustion still counts.
            None => Event::system(Uuid::now_v7(), payload),
        }
        .annotate(
            DEAD_LETTER_SUBJECT_KEY,
            serde_json::Value::String(trigger_subject),
        )
        .annotate(DEAD_LETTER_PAYLOAD_KEY, trigger_payload)
        .annotate(
            DEAD_LETTER_STREAM_SEQ_KEY,
            serde_json::json!(advisory.stream_seq),
        )
        .annotate(
            DEAD_LETTER_SOURCE_KEY,
            serde_json::Value::String("advisory".to_string()),
        );
        self.bus.publish(&event).await?;
        error!(
            agent_id = %event.envelope.agent_id,
            stream_seq = advisory.stream_seq,
            deliveries = advisory.deliveries,
            "trigger exhausted without a live dispatcher; emitted dead-letter event from advisory"
        );
        Ok(AdvisoryOutcome::Emitted)
    }

    /// Whether the agent's most recent `failed` event is a dead-letter
    /// for this stream sequence. Best-effort: any read failure means
    /// "not surfaced", erring toward emitting (at-least-once).
    async fn already_surfaced(&self, agent: &AgentId, stream_seq: u64) -> bool {
        let Ok(stream) = self.bus.jetstream().get_stream(STREAM_NAME).await else {
            return false;
        };
        let Ok(raw) = stream
            .get_last_raw_message_by_subject(&subjects::agent_failed(agent.as_str()))
            .await
        else {
            return false;
        };
        let Ok(event) = serde_json::from_slice::<Event>(&raw.payload) else {
            return false;
        };
        matches!(
            &event.payload,
            EventPayload::Failed(p) if matches!(p.error_kind, FailureKind::TriggerExhausted)
        ) && event.annotations.0.get(DEAD_LETTER_STREAM_SEQ_KEY)
            == Some(&serde_json::json!(stream_seq))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    /// Poll the advisory capture stream for the MAX_DELIVERIES
    /// advisory of one specific consumer (names are unique per test,
    /// so the subject is precise).
    async fn wait_for_captured_advisory(bus: &EventBus, consumer_name: &str) -> Vec<u8> {
        let subject = format!(
            "$JS.EVENT.ADVISORY.CONSUMER.MAX_DELIVERIES.{TRIGGER_STREAM_NAME}.{consumer_name}"
        );
        let stream = bus
            .jetstream()
            .get_stream(ADVISORY_STREAM_NAME)
            .await
            .expect("advisory stream");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(raw) = stream.get_last_raw_message_by_subject(&subject).await {
                return raw.payload.to_vec();
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "advisory for {consumer_name} never captured"
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// #169 end-to-end against a live broker: a trigger exhausted with
    /// no live dispatcher (never acked — the crash shape) produces a
    /// captured advisory; handling it emits the dead-letter event with
    /// the same kind and annotations as the inline path; handling the
    /// same advisory again (redelivery / inline overlap) is suppressed
    /// by the stream-sequence dedup.
    #[tokio::test]
    async fn exhausted_unacked_trigger_is_surfaced_from_the_advisory() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };
        use futures::StreamExt;
        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let suffix = Uuid::now_v7().simple().to_string();
        let agent_id_str = format!("advisory-e2e-{suffix}");
        let consumer_name = format!("advisory-e2e-{suffix}");
        let trigger_subject = crate::bus::trigger_subject(&agent_id_str);

        bus.publish_trigger(&agent_id_str, &json!({"input": "poison"}))
            .await
            .expect("publish trigger");

        // A scratch consumer shaped like the probe's crash case: tight
        // bound, short ack_wait, no backoff — exhaustion in ~2s. The
        // advisory fires for any consumer of the trigger stream.
        let stream = bus
            .jetstream()
            .get_stream(TRIGGER_STREAM_NAME)
            .await
            .expect("trigger stream");
        let consumer = stream
            .create_consumer(async_nats::jetstream::consumer::pull::Config {
                durable_name: Some(consumer_name.clone()),
                ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
                filter_subject: trigger_subject.clone(),
                max_deliver: 2,
                ack_wait: Duration::from_secs(1),
                ..Default::default()
            })
            .await
            .expect("scratch consumer");
        let mut messages = consumer.messages().await.expect("messages");
        for i in 0..2 {
            let _unacked = tokio::time::timeout(Duration::from_secs(5), messages.next())
                .await
                .unwrap_or_else(|_| panic!("delivery {i}"))
                .unwrap()
                .unwrap();
            // never acked — the dispatcher is "dead"
        }

        let advisory = wait_for_captured_advisory(&bus, &consumer_name).await;
        let watch = AdvisoryWatch::new(bus.clone());
        assert_eq!(
            watch.handle_advisory(&advisory).await.unwrap(),
            AdvisoryOutcome::Emitted
        );

        // The event is on the bus, kind + annotations intact.
        let events_stream = bus
            .jetstream()
            .get_stream(STREAM_NAME)
            .await
            .expect("event stream");
        let raw = events_stream
            .get_last_raw_message_by_subject(&subjects::agent_failed(&agent_id_str))
            .await
            .expect("dead-letter event");
        let event: Event = serde_json::from_slice(&raw.payload).expect("event parses");
        match &event.payload {
            EventPayload::Failed(p) => {
                assert!(matches!(p.error_kind, FailureKind::TriggerExhausted));
                assert!(
                    p.error_message.contains("MAX_DELIVERIES advisory"),
                    "got: {}",
                    p.error_message
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(
            event.annotations.0.get(DEAD_LETTER_SUBJECT_KEY),
            Some(&json!(trigger_subject))
        );
        assert_eq!(
            event.annotations.0.get(DEAD_LETTER_PAYLOAD_KEY),
            Some(&json!({"input": "poison"}))
        );
        assert!(
            event
                .annotations
                .0
                .get(DEAD_LETTER_STREAM_SEQ_KEY)
                .is_some_and(|seq| seq.as_u64().is_some())
        );
        assert_eq!(
            event.annotations.0.get(DEAD_LETTER_SOURCE_KEY),
            Some(&json!("advisory"))
        );

        // Redelivered / overlapping advisory: suppressed by the seq
        // dedup against the event just emitted.
        assert_eq!(
            watch.handle_advisory(&advisory).await.unwrap(),
            AdvisoryOutcome::AlreadySurfaced
        );
    }

    /// A trigger that aged out of the stream before its advisory was
    /// processed still counts — attributed to the system sentinel.
    #[tokio::test]
    async fn aged_out_trigger_still_surfaces_under_the_system_sentinel() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };
        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let watch = AdvisoryWatch::new(bus.clone());
        // No message exists at this sequence.
        let advisory = format!(
            r#"{{"type":"io.nats.jetstream.advisory.v1.max_deliver","stream":"{TRIGGER_STREAM_NAME}","consumer":"gone","stream_seq":18446744073709551,"deliveries":5}}"#
        );
        assert_eq!(
            watch.handle_advisory(advisory.as_bytes()).await.unwrap(),
            AdvisoryOutcome::Emitted
        );
        let events_stream = bus
            .jetstream()
            .get_stream(STREAM_NAME)
            .await
            .expect("event stream");
        let raw = events_stream
            .get_last_raw_message_by_subject(&subjects::agent_failed("system"))
            .await
            .expect("sentinel dead-letter event");
        let event: Event = serde_json::from_slice(&raw.payload).expect("event parses");
        assert!(matches!(
            &event.payload,
            EventPayload::Failed(p) if matches!(p.error_kind, FailureKind::TriggerExhausted)
        ));
        assert_eq!(
            event.annotations.0.get(DEAD_LETTER_STREAM_SEQ_KEY),
            Some(&json!(18446744073709551u64))
        );
    }

    /// The exact payload shape the server emits (captured from a live
    /// broker probe, 2026-07-14) parses into the fields we consume.
    #[test]
    fn parses_the_servers_advisory_payload() {
        let payload = r#"{"type":"io.nats.jetstream.advisory.v1.max_deliver","id":"g1ZC7klaOZ1E53SLlZN5hk","timestamp":"2026-07-14T16:18:43.605745113Z","stream":"fq-triggers","consumer":"fq-dispatcher","stream_seq":42,"deliveries":5}"#;
        let advisory: MaxDeliverAdvisory = serde_json::from_slice(payload.as_bytes()).unwrap();
        assert_eq!(advisory.stream, "fq-triggers");
        assert_eq!(advisory.consumer, "fq-dispatcher");
        assert_eq!(advisory.stream_seq, 42);
        assert_eq!(advisory.deliveries, 5);
    }
}
