//! Operator-issued actions against control-plane state.
//!
//! These are the verbs the operator CLI (`fq invocation
//! drop` and friends) calls into. Lifted here so the test
//! harness can drive the same code path the CLI uses, with
//! no duplication.

use futures::StreamExt;
use uuid::Uuid;

use crate::agent::AgentId;
use crate::bus::{BusError, EventBus, STREAM_NAME};
use crate::control_plane::projection::ProjectionStore;
use crate::control_plane::projection::store::StoreError;
use crate::control_plane::store::{ControlPlaneStore, ControlPlaneStoreError};
use crate::events::{Event, EventPayload, InvocationOperatorRecoveredPayload, subjects};

/// Outcome of a successful [`drop_invocation`].
#[derive(Debug, Clone)]
pub struct DropResult {
    pub invocation_id: String,
    pub agent_id: String,
    pub event_id: String,
    pub reason: Option<String>,
    /// The drop event's sequence on the event stream — the receipt
    /// coordinate a gated read waits on for read-your-writes (D4).
    pub event_seq: u64,
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

/// Operator-issued drop. Resolves the agent the invocation belongs to,
/// builds an `invocation.operator_recovered` event with `action="drop"`
/// and `final_phase="failed"`, and publishes it. The control-plane's
/// coordination consumer is responsible for writing the archive row and
/// flipping the owner status.
///
/// `driving_agent` is the agent a **live** runner on this daemon is
/// driving the invocation for, straight from that runner (#107). It
/// takes precedence over every durable source because it is the only
/// one with zero lag: the projection is folded by an asynchronous
/// consumer and nothing on the dispatch path writes an owner row, so an
/// invocation that started moments ago is live in the runner and absent
/// from both stores. Passing it makes the safety invariant structural —
/// **a drop of a running invocation can never resolve to
/// `UnknownInvocation`** — which matters because the caller has, by
/// then, already armed the halt that stops the work. `None` means "no
/// runner here is driving it", and resolution falls back to the durable
/// record.
pub async fn drop_invocation(
    bus: &EventBus,
    proj_store: &ProjectionStore,
    control_store: &ControlPlaneStore,
    invocation_id: &str,
    reason: Option<&str>,
    driving_agent: Option<&AgentId>,
) -> Result<DropResult, DropError> {
    // Resolution, freshest source first. A live runner's answer needs no
    // corroboration and cannot be stale; only when no runner here is
    // driving the invocation does the durable record get a say. Older or
    // synthetic recovery rows may have no projection event and therefore
    // no agent — those rows are cleared directly below; normal rows
    // retain the existing event-driven terminal/archive transition.
    let resolved_agent = match driving_agent {
        Some(agent) => Some(agent.as_str().to_string()),
        None => proj_store.agent_id_for_invocation(invocation_id).await?,
    };
    let agent_id_str = match resolved_agent {
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
            AgentId::operator().into_inner()
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
    let event_seq = bus.publish(&event).await?;

    Ok(DropResult {
        invocation_id: invocation_id.to_string(),
        agent_id: agent_id_str,
        event_id,
        reason: reason.map(|s| s.to_string()),
        event_seq,
    })
}

// ---------------------------------------------------------------
// Dead-lettered triggers: list and requeue (#49 / #169).
// ---------------------------------------------------------------

/// One dead-lettered trigger, reconstructed from its terminal event
/// on the bus. The event — not the projection — is the source of
/// truth here: the projection stores no annotations, and the original
/// trigger ages out of its stream long before the event does.
///
/// The type itself now lives in [`crate::dead_letter`], which is also
/// where the operator surface's DeadLetter atom serves it from; this
/// re-export keeps `requeue`'s established path working.
pub use crate::dead_letter::DeadLetter;

/// Failure modes for the dead-letter listing.
#[derive(Debug, thiserror::Error)]
pub enum DeadLetterError {
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
        // One definition of "is a dead letter", shared with the atom
        // the operator surface serves — so the two readings of the
        // same log cannot drift.
        if let Some(dead) = DeadLetter::from_event(&event) {
            out.push(dead);
        }
    }
    // Stream order is oldest-first; the operator wants newest first.
    out.reverse();
    out.truncate(limit);
    Ok(out)
}

// `requeue_dead_letter` used to live here, and its departure took the
// last non-report legacy call point with it (plan Phase 4, verb 8).
//
// It selected a dead letter, reconstructed the payload from the
// annotations — falling back to a direct read of the trigger stream —
// and republished it as a fresh, unrecorded trigger. **The stream
// fallback did not survive the move, and not because something
// replaced it.** It existed for a dead letter whose `trigger_subject`
// was empty, which is exactly the advisory path failing to read the
// original off the stream; and that is the same branch, in the same
// `match`, that records no `trigger_id`. Keyed on the identity, a dead
// letter like that is now refused before a payload is needed at all —
// so the fallback's only case became unreachable, and the payload has
// one source: the dead letter's own record of the trigger it lost.
//
// The command lives in `fq-cli`'s `dead_letter_requeue.rs`, where the
// edge can hold it to a guarantee a client never could: the requeue
// records what it re-ran, so a second one is refused rather than run.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dead_letter::{
        DEAD_LETTER_PAYLOAD_KEY, DEAD_LETTER_SOURCE_KEY, DEAD_LETTER_STREAM_SEQ_KEY,
        DEAD_LETTER_SUBJECT_KEY,
    };
    use crate::events::FailureKind;
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
            json!(crate::events::subjects::trigger(agent.as_str())),
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

    /// The listing finds only dead-letter failed events, newest first,
    /// scoped by agent, and honours its limit at the newest end.
    ///
    /// Requeue used to be asserted here too, against the same fixture.
    /// It is a command on the edge now, and its guarantee is a fact
    /// about a store this module cannot see — so the assertions moved
    /// whole to `fq-cli`'s `edge_dead_letter_requeue.rs`, where a
    /// daemon owns both the log and the record.
    #[tokio::test]
    async fn dead_letters_list_round_trip() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();
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

        // Another agent's listing sees only its own.
        let theirs = list_dead_letters(&bus, Some(other.as_str()), 50)
            .await
            .unwrap();
        assert_eq!(theirs.len(), 1);
        assert_eq!(theirs[0].trigger_stream_seq, Some(13));

        // An agent with nothing gets an empty listing, not an error:
        // "nothing fell on the floor" is an answer.
        let missing = unique_agent("dl-op-none");
        assert!(
            list_dead_letters(&bus, Some(missing.as_str()), 50)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
