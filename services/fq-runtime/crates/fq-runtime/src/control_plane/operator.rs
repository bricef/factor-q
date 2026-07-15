//! Operator-issued actions against control-plane state.
//!
//! These are the verbs the operator CLI (`fq invocation
//! drop` and friends) calls into. Lifted here so the test
//! harness can drive the same code path the CLI uses, with
//! no duplication.

use futures::StreamExt;
use uuid::Uuid;

use crate::agent::AgentId;
use crate::bus::{BusError, EventBus, STREAM_NAME, TRIGGER_STREAM_NAME};
use crate::control_plane::projection::ProjectionStore;
use crate::control_plane::projection::store::StoreError;
use crate::control_plane::store::{ControlPlaneStore, ControlPlaneStoreError};
use crate::events::{
    DEAD_LETTER_PAYLOAD_KEY, DEAD_LETTER_SOURCE_KEY, DEAD_LETTER_STREAM_SEQ_KEY,
    DEAD_LETTER_SUBJECT_KEY, Event, EventPayload, FailureKind, InvocationOperatorRecoveredPayload,
    subjects,
};

/// Outcome of a successful [`drop_invocation`].
#[derive(Debug, Clone)]
pub struct DropResult {
    pub invocation_id: String,
    pub agent_id: String,
    pub event_id: String,
    pub reason: Option<String>,
}

/// Failure modes for [`drop_invocation`].
#[derive(Debug, thiserror::Error)]
pub enum DropError {
    #[error("invocation {0} not found: no projection event and no coordination owner row")]
    UnknownInvocation(String),
    #[error("invalid agent id from projection: {0}")]
    InvalidAgentId(String),
    #[error("invalid invocation id `{id}`: {source}")]
    InvalidInvocationId {
        id: String,
        #[source]
        source: uuid::Error,
    },
    #[error("projection store error: {0}")]
    Store(#[from] StoreError),
    #[error("control-plane store error: {0}")]
    ControlPlane(#[from] ControlPlaneStoreError),
    #[error("event bus error: {0}")]
    Bus(#[from] BusError),
}

/// Operator-issued drop. Looks up the agent for the given
/// invocation from the projection, builds an
/// `invocation.operator_recovered` event with
/// `action="drop"` and `final_phase="failed"`, and publishes
/// it. The control-plane's coordination consumer is
/// responsible for writing the archive row and flipping the
/// owner status.
pub async fn drop_invocation(
    bus: &EventBus,
    proj_store: &ProjectionStore,
    control_store: &ControlPlaneStore,
    invocation_id: &str,
    reason: Option<&str>,
) -> Result<DropResult, DropError> {
    // Older/synthetic recovery rows may have no projection event and therefore
    // no agent. Clear those rows directly; normal rows retain the existing
    // event-driven terminal/archive transition.
    let agent_id_str = match proj_store.agent_id_for_invocation(invocation_id).await? {
        Some(agent_id) => agent_id,
        None => {
            // No projection event names an agent — this is either an
            // agent-less recovery row or an id that never existed.
            // `delete_invocation_owner` tells them apart by whether it
            // actually removed a row: a truly-unknown id must still error
            // rather than emit a phantom operator-recovered event
            // (ADR-0026 — the event log is the system of record).
            if !control_store.delete_invocation_owner(invocation_id).await? {
                return Err(DropError::UnknownInvocation(invocation_id.to_string()));
            }
            "operator".to_string()
        }
    };
    let agent_id =
        AgentId::new(agent_id_str.clone()).map_err(|e| DropError::InvalidAgentId(e.to_string()))?;
    let inv_uuid = Uuid::parse_str(invocation_id).map_err(|e| DropError::InvalidInvocationId {
        id: invocation_id.to_string(),
        source: e,
    })?;

    let event = Event::new(
        agent_id,
        inv_uuid,
        EventPayload::InvocationOperatorRecovered(InvocationOperatorRecoveredPayload {
            action: "drop".to_string(),
            final_phase: "failed".to_string(),
            reason: reason.map(|s| s.to_string()),
        }),
    );
    let event_id = event.envelope.event_id.to_string();
    bus.publish(&event).await?;

    Ok(DropResult {
        invocation_id: invocation_id.to_string(),
        agent_id: agent_id_str,
        event_id,
        reason: reason.map(|s| s.to_string()),
    })
}

// ---------------------------------------------------------------
// Dead-lettered triggers: list and requeue (#49 / #169).
// ---------------------------------------------------------------

/// One dead-lettered trigger, reconstructed from its terminal event
/// on the bus. The event — not the projection — is the source of
/// truth here: the projection stores no annotations, and the original
/// trigger ages out of its stream long before the event does.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeadLetter {
    pub event_id: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub agent_id: String,
    pub trigger_subject: String,
    /// The original trigger's sequence on the trigger stream — the
    /// key that reconciles the inline and advisory emitters, and the
    /// selector for [`requeue_dead_letter`].
    pub trigger_stream_seq: Option<u64>,
    /// Which emitter surfaced it: `"inline"` | `"advisory"`.
    pub source: String,
    pub trigger_payload: serde_json::Value,
    pub error_message: String,
}

/// Failure modes for the dead-letter verbs.
#[derive(Debug, thiserror::Error)]
pub enum DeadLetterError {
    #[error("no dead-lettered triggers found for agent `{0}`")]
    NoDeadLetters(String),
    #[error(
        "no dead letter for agent `{agent}` with trigger sequence {seq} — \
         `fq dead-letters list` shows the known sequences"
    )]
    SeqNotFound { agent: String, seq: u64 },
    #[error(
        "the trigger behind this dead letter is not recoverable: the event carries no \
         payload (the trigger had already aged out when the advisory was processed) and \
         sequence {seq:?} is no longer in the trigger stream"
    )]
    PayloadUnavailable { seq: Option<u64> },
    #[error("event bus error: {0}")]
    Bus(#[from] BusError),
    #[error("stream error: {0}")]
    Stream(String),
}

/// List dead-lettered triggers, newest first, by scanning the event
/// stream's `failed` subjects with an ephemeral ordered consumer
/// (ack-less; leaves no durable state behind).
pub async fn list_dead_letters(
    bus: &EventBus,
    agent_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<DeadLetter>, DeadLetterError> {
    let filter_subject = match agent_filter {
        Some(agent) => subjects::agent_failed(agent),
        None => subjects::ALL_AGENTS_FAILED.to_string(),
    };
    let stream = bus
        .jetstream()
        .get_stream(STREAM_NAME)
        .await
        .map_err(|err| DeadLetterError::Stream(err.to_string()))?;
    let mut consumer = stream
        .create_consumer(async_nats::jetstream::consumer::pull::OrderedConfig {
            filter_subject,
            ..Default::default()
        })
        .await
        .map_err(|err| DeadLetterError::Stream(err.to_string()))?;
    let pending = consumer
        .info()
        .await
        .map_err(|err| DeadLetterError::Stream(err.to_string()))?
        .num_pending;
    if pending == 0 {
        return Ok(Vec::new());
    }

    let mut out: Vec<DeadLetter> = Vec::new();
    let mut messages = consumer
        .messages()
        .await
        .map_err(|err| DeadLetterError::Stream(err.to_string()))?
        .take(pending as usize);
    while let Some(msg) = messages.next().await {
        let Ok(msg) = msg else { continue };
        let Ok(event) = serde_json::from_slice::<Event>(&msg.payload) else {
            continue;
        };
        let EventPayload::Failed(failed) = &event.payload else {
            continue;
        };
        if !matches!(failed.error_kind, FailureKind::TriggerExhausted) {
            continue;
        }
        let get_str = |key: &str| {
            event
                .annotations
                .0
                .get(key)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        out.push(DeadLetter {
            event_id: event.envelope.event_id.to_string(),
            timestamp: event.envelope.timestamp,
            agent_id: event.envelope.agent_id.as_str().to_string(),
            trigger_subject: get_str(DEAD_LETTER_SUBJECT_KEY),
            trigger_stream_seq: event
                .annotations
                .0
                .get(DEAD_LETTER_STREAM_SEQ_KEY)
                .and_then(|v| v.as_u64()),
            source: get_str(DEAD_LETTER_SOURCE_KEY),
            trigger_payload: event
                .annotations
                .0
                .get(DEAD_LETTER_PAYLOAD_KEY)
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            error_message: failed.error_message.clone(),
        });
    }
    // Stream order is oldest-first; the operator wants newest first.
    out.reverse();
    out.truncate(limit);
    Ok(out)
}

/// Outcome of a successful [`requeue_dead_letter`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct RequeueResult {
    pub agent_id: String,
    pub trigger_payload: serde_json::Value,
    /// The fresh trigger's sequence on the trigger stream.
    pub new_trigger_seq: u64,
    /// The dead-letter event the trigger was reconstructed from.
    pub source_event_id: String,
}

/// Re-publish a dead-lettered trigger as a fresh trigger (new
/// sequence, fresh delivery budget). Selects the agent's most recent
/// dead letter, or the one whose original trigger sequence matches
/// `trigger_seq`. **Not idempotent**: requeueing twice triggers the
/// agent twice — the fresh trigger is a new message with no memory of
/// this one.
pub async fn requeue_dead_letter(
    bus: &EventBus,
    agent_id: &str,
    trigger_seq: Option<u64>,
) -> Result<RequeueResult, DeadLetterError> {
    let dead = list_dead_letters(bus, Some(agent_id), usize::MAX).await?;
    let dead_letter = match trigger_seq {
        Some(seq) => dead
            .iter()
            .find(|d| d.trigger_stream_seq == Some(seq))
            .ok_or_else(|| DeadLetterError::SeqNotFound {
                agent: agent_id.to_string(),
                seq,
            })?,
        None => dead
            .first()
            .ok_or_else(|| DeadLetterError::NoDeadLetters(agent_id.to_string()))?,
    };

    // The event's annotations normally carry the payload verbatim. An
    // empty subject means the advisory path could not resolve the
    // trigger when it processed the advisory (aged out — or a
    // transient stream error at that moment). One direct look at the
    // trigger stream distinguishes the two before giving up.
    let payload = if dead_letter.trigger_subject.is_empty() {
        match dead_letter.trigger_stream_seq {
            Some(seq) => {
                let raw = bus
                    .jetstream()
                    .get_stream(TRIGGER_STREAM_NAME)
                    .await
                    .map_err(|err| DeadLetterError::Stream(err.to_string()))?
                    .get_raw_message(seq)
                    .await
                    .map_err(|_| DeadLetterError::PayloadUnavailable { seq: Some(seq) })?;
                serde_json::from_slice(&raw.payload).unwrap_or(serde_json::Value::Null)
            }
            None => return Err(DeadLetterError::PayloadUnavailable { seq: None }),
        }
    } else {
        dead_letter.trigger_payload.clone()
    };

    let new_trigger_seq = bus.publish_trigger(agent_id, &payload).await?;
    Ok(RequeueResult {
        agent_id: agent_id.to_string(),
        trigger_payload: payload,
        new_trigger_seq,
        source_event_id: dead_letter.event_id.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A dead-letter event exactly as both emitters shape it (#165's
    /// broker tests pin the emitters to this contract).
    fn dead_letter_event(
        agent: &AgentId,
        seq: u64,
        source: &str,
        payload: serde_json::Value,
    ) -> Event {
        Event::new(
            agent.clone(),
            Uuid::now_v7(),
            EventPayload::Failed(crate::events::FailedPayload {
                error_kind: FailureKind::TriggerExhausted,
                error_message: format!("trigger exhausted after 5 deliveries (limit 5) [{source}]"),
                phase: crate::events::FailurePhase::Setup,
                partial_totals: crate::events::InvocationTotals::default(),
            }),
        )
        .annotate(
            DEAD_LETTER_SUBJECT_KEY,
            json!(crate::bus::trigger_subject(agent.as_str())),
        )
        .annotate(DEAD_LETTER_PAYLOAD_KEY, payload)
        .annotate(DEAD_LETTER_STREAM_SEQ_KEY, json!(seq))
        .annotate(DEAD_LETTER_SOURCE_KEY, json!(source))
    }

    fn unique_agent(prefix: &str) -> AgentId {
        AgentId::new(format!(
            "{prefix}-{}",
            Uuid::now_v7().simple().to_string().get(20..32).unwrap()
        ))
        .unwrap()
    }

    /// list: finds only dead-letter failed events, newest first,
    /// scoped by agent; requeue: republishes the payload as a fresh
    /// trigger (returned seq resolves to the same payload on the
    /// trigger stream), selects by --trigger-seq, and errors cleanly
    /// on unknown agents and sequences.
    #[tokio::test]
    async fn dead_letters_list_and_requeue_round_trip() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };
        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let agent = unique_agent("dl-op");
        let other = unique_agent("dl-op-other");

        // Two dead letters for `agent` (older seq 11, newer seq 12),
        // one ordinary failure (must be excluded), one for `other`.
        bus.publish(&dead_letter_event(&agent, 11, "inline", json!({"n": 1})))
            .await
            .unwrap();
        bus.publish(&dead_letter_event(&agent, 12, "advisory", json!({"n": 2})))
            .await
            .unwrap();
        bus.publish(&Event::new(
            agent.clone(),
            Uuid::now_v7(),
            EventPayload::Failed(crate::events::FailedPayload {
                error_kind: FailureKind::RuntimeError,
                error_message: "ordinary failure".to_string(),
                phase: crate::events::FailurePhase::Setup,
                partial_totals: crate::events::InvocationTotals::default(),
            }),
        ))
        .await
        .unwrap();
        bus.publish(&dead_letter_event(&other, 13, "inline", json!({"n": 3})))
            .await
            .unwrap();

        // List for `agent`: exactly its two dead letters, newest first.
        let dead = list_dead_letters(&bus, Some(agent.as_str()), 50)
            .await
            .unwrap();
        assert_eq!(dead.len(), 2, "{dead:?}");
        assert_eq!(dead[0].trigger_stream_seq, Some(12));
        assert_eq!(dead[0].source, "advisory");
        assert_eq!(dead[1].trigger_stream_seq, Some(11));
        assert_eq!(dead[0].trigger_payload, json!({"n": 2}));
        assert_eq!(dead[0].agent_id, agent.as_str());

        // The limit applies after newest-first ordering.
        let top = list_dead_letters(&bus, Some(agent.as_str()), 1)
            .await
            .unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].trigger_stream_seq, Some(12));

        // Requeue default: the newest. The returned seq must resolve
        // to the republished payload on the trigger stream.
        let requeued = requeue_dead_letter(&bus, agent.as_str(), None)
            .await
            .unwrap();
        assert_eq!(requeued.trigger_payload, json!({"n": 2}));
        let raw = bus
            .jetstream()
            .get_stream(TRIGGER_STREAM_NAME)
            .await
            .unwrap()
            .get_raw_message(requeued.new_trigger_seq)
            .await
            .expect("fresh trigger on the stream");
        assert_eq!(
            raw.subject.as_str(),
            crate::bus::trigger_subject(agent.as_str())
        );
        let republished: serde_json::Value = serde_json::from_slice(&raw.payload).unwrap();
        assert_eq!(republished, json!({"n": 2}));

        // Requeue by original trigger seq: the older one.
        let requeued = requeue_dead_letter(&bus, agent.as_str(), Some(11))
            .await
            .unwrap();
        assert_eq!(requeued.trigger_payload, json!({"n": 1}));

        // Error modes.
        let missing = unique_agent("dl-op-none");
        assert!(matches!(
            requeue_dead_letter(&bus, missing.as_str(), None).await,
            Err(DeadLetterError::NoDeadLetters(_))
        ));
        assert!(matches!(
            requeue_dead_letter(&bus, agent.as_str(), Some(9999)).await,
            Err(DeadLetterError::SeqNotFound { seq: 9999, .. })
        ));
    }

    /// The advisory aged-out shape (empty subject, null payload, seq
    /// no longer on the trigger stream) fails with the explanatory
    /// error rather than requeueing a null trigger.
    #[tokio::test]
    async fn requeue_of_an_unrecoverable_dead_letter_refuses() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };
        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let agent = unique_agent("dl-aged");
        let event = Event::new(
            agent.clone(),
            Uuid::now_v7(),
            EventPayload::Failed(crate::events::FailedPayload {
                error_kind: FailureKind::TriggerExhausted,
                error_message: "aged out".to_string(),
                phase: crate::events::FailurePhase::Setup,
                partial_totals: crate::events::InvocationTotals::default(),
            }),
        )
        .annotate(DEAD_LETTER_SUBJECT_KEY, json!(""))
        .annotate(DEAD_LETTER_PAYLOAD_KEY, serde_json::Value::Null)
        .annotate(DEAD_LETTER_STREAM_SEQ_KEY, json!(18_446_744_073_709_u64))
        .annotate(DEAD_LETTER_SOURCE_KEY, json!("advisory"));
        bus.publish(&event).await.unwrap();

        assert!(matches!(
            requeue_dead_letter(&bus, agent.as_str(), None).await,
            Err(DeadLetterError::PayloadUnavailable { .. })
        ));
    }
}
