//! The DeadLetter atom: a trigger that exhausted its delivery budget,
//! as an immutable, event-log-backed fact
//! (`docs/design/committed/operator-surface-domain-model.md`).
//!
//! A dead letter is not a projection fold and has no store of its own.
//! It is one recorded `agent.failed` event — the terminal one the
//! dispatcher (inline) or the advisory watcher publishes when a trigger
//! runs out of deliveries — read back through the lens below. The event
//! is the source of truth deliberately: the projection stores no
//! annotations, and the original trigger ages out of the trigger stream
//! long before the event ages out of the log.
//!
//! Split out of `control_plane::operator` (where it lived as that
//! module's DTO) because it is a domain value the operator surface
//! serves, not an operator *action* — and because a client naming
//! `control_plane::operator::` is exactly what the Phase-4 migration
//! gate counts.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::events::{Event, EventPayload, FailureKind};

/// Annotation keys shared by the two dead-letter emitters (#49/#169):
/// the dispatcher's inline path and the advisory watch. `trigger_*`
/// carries what a requeue needs; `trigger_stream_seq` is the dedup /
/// reconciliation key; `dead_letter_source` says which path emitted
/// (`"inline"` | `"advisory"`).
///
/// They live here, with the atom that reads them, rather than in
/// `events.rs`: this is the dead letter's own vocabulary, not the event
/// envelope's.
pub const DEAD_LETTER_SUBJECT_KEY: &str = "trigger_subject";
pub const DEAD_LETTER_PAYLOAD_KEY: &str = "trigger_payload";
pub const DEAD_LETTER_STREAM_SEQ_KEY: &str = "trigger_stream_seq";
pub const DEAD_LETTER_SOURCE_KEY: &str = "dead_letter_source";

/// The identity of the trigger that dead-lettered
/// ([`crate::trigger`]) — the name of the thing, alongside
/// `trigger_stream_seq`'s position of it.
///
/// Best-effort like its siblings, and absent for the same kind of
/// reason: the advisory path records it only when the original trigger
/// is still on the stream *and* carried the header, because the one
/// thing worse than an unnamed dead letter is one named with an id that
/// exists nowhere else. A requeue that wants to be idempotent keys on
/// this when it is there.
pub const DEAD_LETTER_TRIGGER_ID_KEY: &str = "trigger_id";

/// One dead-lettered trigger, as its terminal event records it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DeadLetter {
    pub event_id: String,
    /// When the exhaustion was recorded, RFC3339.
    ///
    /// Declared to the surface as a string rather than a reflected
    /// schema: that is exactly what it serialises as, and reflecting
    /// `chrono::DateTime` would need schemars' chrono integration —
    /// a wider change than this atom (the Event atom made the same
    /// call for its payload tree).
    #[schemars(with = "String")]
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub agent_id: String,
    pub trigger_subject: String,
    /// The original trigger's sequence on the **trigger** stream — the
    /// key that reconciles the inline and advisory emitters, and the
    /// selector `dead_letter.requeue` takes.
    ///
    /// Absent when the advisory arrived after the trigger had aged
    /// out, which is why it is not this atom's identity: an identity
    /// that can be missing is not one. The atom is addressed by its
    /// **event-log** sequence — see [`DeadLetterState`].
    pub trigger_stream_seq: Option<u64>,
    /// Which emitter surfaced it: `"inline"` | `"advisory"`.
    pub source: String,
    pub trigger_payload: serde_json::Value,
    pub error_message: String,
}

impl DeadLetter {
    /// Recognise a dead letter in one recorded event, or decline.
    ///
    /// This is the whole of the atom's definition: a `Failed` event
    /// whose `error_kind` is `TriggerExhausted`, with the emitters'
    /// annotations lifted into domain fields. Every read — Get, List
    /// and Stream alike — is this predicate applied to a different
    /// span of the log, so there is one place the shape can drift.
    ///
    /// A missing annotation is rendered as an empty value rather than
    /// refused: the annotations are how the emitters describe the
    /// trigger, and an exhaustion the log records with less detail is
    /// still an exhaustion the operator needs to see.
    pub fn from_event(event: &Event) -> Option<Self> {
        let EventPayload::Failed(failed) = &event.payload else {
            return None;
        };
        if !matches!(failed.error_kind, FailureKind::TriggerExhausted) {
            return None;
        }
        let annotation = |key: &str| event.annotations.0.get(key);
        let string = |key: &str| {
            annotation(key)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        Some(DeadLetter {
            event_id: event.envelope.event_id.to_string(),
            timestamp: event.envelope.timestamp,
            agent_id: event.envelope.agent_id.as_str().to_string(),
            trigger_subject: string(DEAD_LETTER_SUBJECT_KEY),
            trigger_stream_seq: annotation(DEAD_LETTER_STREAM_SEQ_KEY).and_then(|v| v.as_u64()),
            source: string(DEAD_LETTER_SOURCE_KEY),
            trigger_payload: annotation(DEAD_LETTER_PAYLOAD_KEY)
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            error_message: failed.error_message.clone(),
        })
    }
}

/// A dead letter addressed by its event-log sequence — the universal
/// cursor (P5): the same number that cursors `dead_letter.stream` and
/// feeds `min_seq` gates.
///
/// It does not ride in a command receipt's `AtomRef`, which names an
/// atom by identity — and this domain has no identity to give it, so
/// no command mints a DeadLetter reference today. That is the gap
/// #464 tracks; addressing a dead letter positionally is why it is a
/// gap rather than a design.
///
/// The identity is the log sequence and not the trigger sequence for
/// three reasons, in ascending order of force: the trigger sequence is
/// a coordinate on a *different* stream, it is not unique across
/// agents, and it can be absent altogether. The Event and Turn atoms
/// are keyed the same way, so a sequence read out of any of the three
/// streams means the same thing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DeadLetterState {
    pub seq: u64,
    /// The fact itself. Nested rather than flattened so the identity
    /// is visibly the atom's and not one of the trigger's own fields.
    pub dead_letter: DeadLetter,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentId;
    use crate::events::{FailedPayload, FailurePhase, InvocationTotals};

    fn failed(kind: FailureKind) -> Event {
        Event::new(
            AgentId::new("researcher").unwrap(),
            uuid::Uuid::now_v7(),
            EventPayload::Failed(FailedPayload {
                error_kind: kind,
                error_message: "trigger exhausted after 5 deliveries (limit 5)".into(),
                phase: FailurePhase::Setup,
                partial_totals: InvocationTotals::default(),
            }),
        )
    }

    #[test]
    fn only_trigger_exhaustion_is_a_dead_letter() {
        assert!(
            DeadLetter::from_event(&failed(FailureKind::TriggerExhausted)).is_some(),
            "an exhausted trigger is the atom"
        );
        assert!(
            DeadLetter::from_event(&failed(FailureKind::RuntimeError)).is_none(),
            "an ordinary failure shares the subject and is not the atom"
        );
        let completed = Event::new(
            AgentId::new("researcher").unwrap(),
            uuid::Uuid::now_v7(),
            EventPayload::Completed(crate::events::CompletedPayload {
                task_status: Default::default(),
                result_summary: None,
                total_llm_calls: 0,
                total_tool_calls: 0,
                total_cost: 0.0,
                total_duration_ms: 0,
            }),
        );
        assert!(DeadLetter::from_event(&completed).is_none());
    }

    #[test]
    fn the_emitters_annotations_become_domain_fields() {
        let event = failed(FailureKind::TriggerExhausted)
            .annotate(
                DEAD_LETTER_SUBJECT_KEY,
                serde_json::json!("fq.trigger.researcher"),
            )
            .annotate(DEAD_LETTER_PAYLOAD_KEY, serde_json::json!({"n": 1}))
            .annotate(DEAD_LETTER_STREAM_SEQ_KEY, serde_json::json!(11))
            .annotate(DEAD_LETTER_SOURCE_KEY, serde_json::json!("inline"));
        let dead = DeadLetter::from_event(&event).expect("a dead letter");
        assert_eq!(dead.trigger_subject, "fq.trigger.researcher");
        assert_eq!(dead.trigger_stream_seq, Some(11));
        assert_eq!(dead.source, "inline");
        assert_eq!(dead.trigger_payload, serde_json::json!({"n": 1}));
        assert_eq!(dead.agent_id, "researcher");
    }

    /// An exhaustion the emitter could not fully describe is still one
    /// the operator must see — the advisory path loses the payload
    /// (and with it the trigger sequence) when the trigger has aged
    /// out, and a listing that dropped those rows would hide exactly
    /// the failures that took longest to surface.
    #[test]
    fn a_dead_letter_with_no_annotations_is_still_a_dead_letter() {
        let dead =
            DeadLetter::from_event(&failed(FailureKind::TriggerExhausted)).expect("a dead letter");
        assert_eq!(dead.trigger_subject, "");
        assert_eq!(dead.trigger_stream_seq, None);
        assert_eq!(dead.source, "");
        assert_eq!(dead.trigger_payload, serde_json::Value::Null);
    }
}
