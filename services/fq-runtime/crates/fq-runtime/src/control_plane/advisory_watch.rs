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
//! (the final delivery's ack window is the retry ladder's last entry,
//! [`crate::bus::TRIGGER_RETRY_BACKOFF`], so up to twelve minutes after
//! a crash), which is why advisories are captured server-side into a
//! durable stream ([`crate::bus::ADVISORY_STREAM_NAME`]) rather than
//! subscribed live.
//!
//! Delivery semantics: at-least-once end to end, like every producer
//! on this bus (the archive sweeper republishes too). A watch crash
//! between emit and ack can duplicate one event; the shared
//! `trigger_stream_seq` annotation makes duplicates reconcilable, and
//! a best-effort last-event check suppresses the common overlap.

use std::time::Duration;

use futures::StreamExt;
use serde::Deserialize;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::agent::AgentId;
use crate::bus::{
    ADVISORY_STREAM_NAME, BusError, ConsumerRedeliveryPolicy, EventBus, RedeliveryLog, STREAM_NAME,
    TRIGGER_MAX_DELIVER, TRIGGER_STREAM_NAME,
};
use crate::control_plane::dispatcher::{Exhausted, ExhaustionCheck};
use crate::dead_letter::{
    DEAD_LETTER_PAYLOAD_KEY, DEAD_LETTER_SOURCE_KEY, DEAD_LETTER_START_UNCHECKED_KEY,
    DEAD_LETTER_STREAM_SEQ_KEY, DEAD_LETTER_SUBJECT_KEY, DEAD_LETTER_TRIGGER_ID_KEY,
    StartUnchecked,
};
use crate::events::{
    Event, EventPayload, FailedPayload, FailureKind, FailurePhase, InvocationTotals, subjects,
};
use crate::trigger::{agent_id_from_subject, trigger_id_in};

/// Name of the durable JetStream consumer the advisory watch creates.
pub const CONSUMER_NAME: &str = "fq-advisory-watch";

/// How long the watch holds its verdict on an exhausted trigger before it
/// writes the dead letter anyway, marked as one whose start could not be
/// checked ([`StartUnchecked`]), measured from when the broker emitted the
/// advisory. Issue 967: a held verdict must end on purpose.
///
/// One hour. The dispatcher's whole retry ladder is about half an hour from
/// a trigger's first delivery to the end of its fifth delivery's window, so
/// a verdict still held an hour after the deliveries ran out, a copy still
/// handling the claim or records still unreadable, is not going to resolve
/// by itself, and waiting longer only keeps from the operator a trigger
/// that may never run. The bound above it is the advisory stream's own: it
/// keeps an advisory for 24 hours ([`crate::bus::DEFAULT_TRIGGER_MAX_AGE`]),
/// and a held advisory that ages out is gone with no dead letter and no
/// record, which an hour ends far inside. The trigger's own message is kept
/// for the same 24 hours from its publish, which came before the advisory
/// by at least the ladder, so a dead letter written at the limit still
/// carries the trigger's subject, payload and id.
///
/// Measured from the advisory's own timestamp, so a restart does not reset
/// it, against this daemon's clock: a daemon clock ahead of the broker's by
/// some amount ends the hold that much early, one behind ends it that much
/// late, and an hour absorbs any ordinary difference. An advisory with no
/// timestamp the watch can read is not held at all: its age cannot be told,
/// so it could never be said to have run out, and it is dead-lettered
/// unchecked at once rather than held until it ages out of its stream.
pub const HELD_VERDICT_LIMIT: Duration = Duration::from_secs(60 * 60);

/// The fields this watch consumes from the server's
/// `io.nats.jetstream.advisory.v1.max_deliver` payload; unknown
/// fields are ignored.
#[derive(Debug, Deserialize)]
struct MaxDeliverAdvisory {
    stream: String,
    consumer: String,
    stream_seq: u64,
    deliveries: i64,
    /// When the server emitted it, RFC 3339.
    #[serde(default)]
    timestamp: Option<String>,
}

impl MaxDeliverAdvisory {
    /// Whether the advisory was emitted for the incarnation of the trigger
    /// stream created at `stream_epoch` (nanoseconds) or a later one, not
    /// for one before it. An advisory without a timestamp the watch can
    /// read is taken as this incarnation's: the server always stamps it.
    fn of_incarnation(&self, stream_epoch: i64) -> bool {
        let emitted = self
            .timestamp
            .as_deref()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .and_then(|at| at.timestamp_nanos_opt());
        emitted.is_none_or(|nanos| nanos >= stream_epoch)
    }

    /// Whether the advisory was emitted less than `limit` before `now`.
    /// One with no timestamp the watch can read is not: its age cannot be
    /// told. One stamped after `now` (a broker clock ahead of this
    /// daemon's) is.
    fn emitted_within(&self, limit: Duration, now: chrono::DateTime<chrono::Utc>) -> bool {
        self.timestamp
            .as_deref()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .is_some_and(|at| {
                (now - at.with_timezone(&chrono::Utc))
                    .to_std()
                    .unwrap_or_default()
                    < limit
            })
    }
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvisoryOutcome {
    /// A dead-letter event was emitted.
    Emitted,
    /// The inline path already emitted for this stream sequence.
    AlreadySurfaced,
    /// Unparseable or foreign advisory; consumed without effect.
    Skipped,
    /// The trigger's run has a start on record: no dead letter, and the
    /// claim is marked (issue 967).
    RunStarted,
    /// The verdict is held, for the reason given: the advisory is NAKed
    /// and decided again when it comes back.
    Held { why: String },
    /// A dead-letter event was emitted without the trigger's start
    /// checked: its verdict was held past [`HELD_VERDICT_LIMIT`], and the
    /// event says so ([`StartUnchecked`]).
    Unchecked,
}

/// The advisory watch. Owns the bus; spawn as a tokio task via
/// [`Self::run`], alongside the dispatcher whose blind spots it
/// covers.
pub struct AdvisoryWatch {
    bus: EventBus,
    /// What the dispatcher has on record for a trigger, consulted before
    /// one is called exhausted. `None` dead-letters every exhausted
    /// trigger without looking.
    records: Option<ExhaustionCheck>,
    /// How long a held verdict is held ([`HELD_VERDICT_LIMIT`]).
    held_verdict_limit: Duration,
}

impl AdvisoryWatch {
    pub fn new(bus: EventBus) -> Self {
        Self {
            bus,
            records: None,
            held_verdict_limit: HELD_VERDICT_LIMIT,
        }
    }

    /// Hold verdicts for `limit` instead of [`HELD_VERDICT_LIMIT`], so a
    /// test can run an advisory past the limit without waiting an hour.
    #[cfg(test)]
    pub(crate) fn with_held_verdict_limit(mut self, limit: Duration) -> Self {
        self.held_verdict_limit = limit;
        self
    }

    /// Consult the dispatcher's records before calling a trigger
    /// exhausted ([`TriggerDispatcher::exhaustion_check`]).
    ///
    /// [`TriggerDispatcher::exhaustion_check`]: crate::control_plane::dispatcher::TriggerDispatcher::exhaustion_check
    pub fn with_records(mut self, records: Option<ExhaustionCheck>) -> Self {
        self.records = records;
        self
    }

    /// Run the watch loop until `shutdown` fires.
    pub async fn run(self, mut shutdown: oneshot::Receiver<()>) -> Result<(), AdvisoryWatchError> {
        info!(stream = ADVISORY_STREAM_NAME, "advisory watch starting");
        let policy = self.bus.redelivery_policy();
        let mut redelivery_log = RedeliveryLog::new(policy);
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
                            Ok(AdvisoryOutcome::Held { why }) => {
                                nak_for_retry(&msg, policy, &mut redelivery_log, &why, true).await;
                            }
                            Ok(outcome) => {
                                debug!(?outcome, "advisory handled");
                                if let Err(err) = msg.ack().await {
                                    error!(error = %err, "failed to ack advisory");
                                }
                            }
                            Err(err) => {
                                let err = err.to_string();
                                nak_for_retry(&msg, policy, &mut redelivery_log, &err, false).await;
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

        // The identity is read, never assigned. This watch did not
        // dispatch the trigger, so it has no standing to name it: if
        // the publisher stamped a header the id is recorded, and if the
        // trigger arrived unnamed (or has aged out of the stream) the
        // dead letter says so by carrying no name, rather than
        // inventing one that matches nothing anywhere else.
        let (agent_id, trigger_subject, trigger_payload, trigger_id) = match &trigger {
            Some(raw) => (
                agent_id_from_subject(&raw.subject).and_then(|id| AgentId::new(id).ok()),
                raw.subject.to_string(),
                serde_json::from_slice(&raw.payload).unwrap_or(serde_json::Value::Null),
                trigger_id_in(Some(&raw.headers)),
            ),
            None => (None, String::new(), serde_json::Value::Null, None),
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

        // Issue 967: a trigger whose run has a start on record is not
        // dead-lettered, and a verdict that cannot be taken yet is held.
        // The claims key on the stream's incarnation, which the advisory
        // does not carry: one emitted before the stream was last created
        // names a message of the old stream, and today's claim at that
        // sequence says nothing about it, so the claims are not consulted.
        let this_incarnation = advisory.of_incarnation(self.bus.trigger_stream_epoch());
        if !this_incarnation {
            info!(stream_seq = advisory.stream_seq, timestamp = ?advisory.timestamp,
                "an advisory from an earlier incarnation of the trigger stream; its trigger's \
                 claim cannot be found, so it is dead-lettered without consulting the claims");
        }
        let mut unchecked = None;
        if let Some(records) = self.records.as_ref().filter(|_| this_incarnation) {
            match records.verdict(advisory.stream_seq).await {
                Exhausted::NothingOnRecord => {}
                Exhausted::Started {
                    invocation_id,
                    on_record,
                    marked,
                } => {
                    warn!(subject = %trigger_subject, stream_seq = advisory.stream_seq,
                        deliveries = advisory.deliveries, %invocation_id, run = %on_record, marked,
                        "not dead-lettering a trigger whose deliveries ran out: its run has a start \
                         on record, so a `trigger_exhausted` dead letter would be false and \
                         requeuing it would run the work twice; its claim is marked started");
                    self.bus
                        .consumer_ledger()
                        .note_started_not_dead_lettered(CONSUMER_NAME);
                    return Ok(AdvisoryOutcome::RunStarted);
                }
                Exhausted::Undecided { why, invocation_id } => {
                    if advisory.emitted_within(self.held_verdict_limit, chrono::Utc::now()) {
                        return Ok(AdvisoryOutcome::Held { why });
                    }
                    unchecked = Some(StartUnchecked {
                        reason: why,
                        invocation_id,
                    });
                }
            }
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
            detail: None,
        });
        let event = match &agent_id {
            Some(agent) => Event::new(agent.clone(), Uuid::now_v7(), payload),
            // Trigger aged out of the stream: the agent is unknowable,
            // but the exhaustion still counts.
            None => Event::system(Uuid::now_v7(), payload),
        }
        .annotate(
            DEAD_LETTER_SUBJECT_KEY,
            serde_json::Value::String(trigger_subject.clone()),
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
        let event = match trigger_id {
            Some(id) => event.annotate(
                DEAD_LETTER_TRIGGER_ID_KEY,
                serde_json::Value::String(id.to_string()),
            ),
            None => event,
        };
        let event = match &unchecked {
            Some(unchecked) => event.annotate(
                DEAD_LETTER_START_UNCHECKED_KEY,
                serde_json::to_value(unchecked).unwrap_or_default(),
            ),
            None => event,
        };
        self.bus.publish(&event).await?;
        if let Some(unchecked) = unchecked {
            error!(agent_id = %event.envelope.agent_id, stream_seq = advisory.stream_seq,
                subject = %trigger_subject, reason = %unchecked.reason,
                invocation_id = %unchecked.invocation_id.as_deref().unwrap_or("none"),
                limit_secs = self.held_verdict_limit.as_secs(),
                "dead-lettered an exhausted trigger without checking whether its run started: \
                 its verdict was held past the watch's limit, so the dead letter is marked as \
                 unchecked and may be false; look at the invocation before requeueing it");
            return Ok(AdvisoryOutcome::Unchecked);
        }
        error!(
            agent_id = %event.envelope.agent_id,
            stream_seq = advisory.stream_seq,
            deliveries = advisory.deliveries,
            "trigger exhausted with nothing of its run on record, or none consulted; emitted a \
             dead-letter event from its MAX_DELIVERIES advisory"
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

/// NAK an advisory with the bus's escalating delay, logging why at most
/// once per escalation step. The advisory consumer redelivers without
/// limit (an advisory dropped to a delivery bound is an exhausted trigger
/// with no record of it), so the delay is what keeps a persistent fault,
/// or a verdict held for long, from becoming a hot loop (#549). `held`
/// says the verdict was held rather than the handling failed.
async fn nak_for_retry(
    msg: &async_nats::jetstream::Message,
    policy: ConsumerRedeliveryPolicy,
    redelivery_log: &mut RedeliveryLog,
    why: &str,
    held: bool,
) {
    let delivered = msg
        .info()
        .ok()
        .and_then(|info| u64::try_from(info.delivered).ok())
        .unwrap_or(1);
    let delay = policy.nak_delay(delivered);
    if redelivery_log.admit(delivered, std::time::Instant::now()) {
        if held {
            warn!(reason = %why, delivered, retry_in_ms = delay.as_millis() as u64,
                "holding the dead-letter verdict on an exhausted trigger; nak to decide again");
        } else {
            warn!(
                error = %why,
                delivered,
                retry_in_ms = delay.as_millis() as u64,
                "advisory handling failed; nak for retry"
            );
        }
    }
    if let Err(nak_err) = msg
        .ack_with(async_nats::jetstream::AckKind::Nak(Some(delay)))
        .await
    {
        error!(error = %nak_err, "failed to nak advisory");
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
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();
        use futures::StreamExt;
        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let suffix = Uuid::now_v7().simple().to_string();
        let agent_id_str = format!("advisory-e2e-{suffix}");
        let consumer_name = format!("advisory-e2e-{suffix}");
        let trigger_subject = crate::events::subjects::trigger(&agent_id_str);

        let published = bus
            .publish_trigger(
                &AgentId::new(&agent_id_str).unwrap(),
                &json!({"input": "poison"}),
            )
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
        // Read off the still-present trigger's header, not invented
        // here: this watch never dispatched the trigger, so the only
        // name it can record is the one already on the message.
        assert_eq!(
            event.annotations.0.get(DEAD_LETTER_TRIGGER_ID_KEY),
            Some(&json!(published.id.to_string())),
            "the dead letter names the trigger the publisher named"
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
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();
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
        assert_eq!(
            event.annotations.0.get(DEAD_LETTER_TRIGGER_ID_KEY),
            None,
            "with the trigger gone there is no name to record, and this \
             watch does not invent one"
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
        assert_eq!(
            advisory.timestamp.as_deref(),
            Some("2026-07-14T16:18:43.605745113Z")
        );
    }

    /// The incarnation an advisory belongs to is read off its timestamp:
    /// one emitted before the stream's creation is an earlier
    /// incarnation's, one at or after it this one's, and one with no
    /// readable timestamp is taken as this one's.
    #[test]
    fn an_advisory_belongs_to_the_incarnation_its_timestamp_falls_in() {
        let at = |timestamp: Option<&str>| MaxDeliverAdvisory {
            stream: TRIGGER_STREAM_NAME.to_string(),
            consumer: "fq-dispatcher".to_string(),
            stream_seq: 1,
            deliveries: 5,
            timestamp: timestamp.map(str::to_string),
        };
        let created = chrono::DateTime::parse_from_rfc3339("2026-07-14T16:00:00Z")
            .unwrap()
            .timestamp_nanos_opt()
            .unwrap();
        assert!(at(Some("2026-07-14T16:18:43Z")).of_incarnation(created));
        assert!(at(Some("2026-07-14T16:00:00Z")).of_incarnation(created));
        assert!(!at(Some("2026-07-14T15:59:59Z")).of_incarnation(created));
        assert!(at(None).of_incarnation(created));
        assert!(at(Some("not a time")).of_incarnation(created));
    }
}
