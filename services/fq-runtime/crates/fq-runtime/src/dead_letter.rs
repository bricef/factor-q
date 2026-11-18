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
//!
//! The two shapes themselves are now `fq_ops::dead_letter` and are
//! re-exported here, so a caller reaches them by the same path as
//! before. What is left in this module is the *recognition*: the
//! annotation keys the emitters write, and [`from_event`], the lens
//! that reads one back. Both need an `Event`, which is runtime
//! machinery — the data can be deserialised by a client that has never
//! seen the log, and producing it cannot.

use crate::events::{Event, EventPayload, FailureKind};

pub use fq_ops::dead_letter::{DeadLetter, DeadLetterState, StartUnchecked};

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
/// exists nowhere else.
///
/// **This is `dead_letter.requeue`'s idempotency key**, which is why
/// its absence is a refusal rather than a degraded requeue: a dead
/// letter with no name here can be re-run, but only as new work
/// (`trigger.publish`), because there would be nothing for a second
/// requeue to be refused on.
pub const DEAD_LETTER_TRIGGER_ID_KEY: &str = "trigger_id";

/// The `start_unchecked` annotation as written. One this build cannot read
/// still says the start was not checked: dropped, it would show the dead
/// letter as checked, and a requeue of it could run the work twice.
fn start_unchecked(annotation: &serde_json::Value) -> StartUnchecked {
    serde_json::from_value(annotation.clone()).unwrap_or_else(|err| StartUnchecked {
        reason: format!("its reason could not be read ({err})"),
        invocation_id: None,
    })
}

/// Set by the advisory watch on a dead letter it wrote without being able
/// to check whether the trigger's run started (issue 967): its verdict
/// could not be taken by the time the advisory was an hour old and looked
/// at three times, and a last look found no start. The value is a
/// [`StartUnchecked`], the reason and the invocation the claim names, the
/// run to check before a requeue; [`from_event`] lifts it into
/// [`DeadLetter::start_unchecked`] so every reader of the dead letter sees
/// it.
pub const DEAD_LETTER_START_UNCHECKED_KEY: &str = "start_unchecked";

/// Set by the advisory watch on a dead letter it wrote for an advisory from
/// an earlier incarnation of the trigger stream (issue 1054): why the dead
/// letter names no trigger. The advisory's sequence is a position on the
/// stream before this one, and the message this stream holds at the same
/// sequence is another trigger, so the watch reads nothing at it and
/// writes the dead letter under the system sentinel with no subject,
/// payload or trigger id. The value is the reason, as text, with when the
/// advisory was emitted and when the stream was created; [`from_event`]
/// lifts it into [`DeadLetter::unattributed`], so a listing can say why a
/// dead letter names nothing and that its sequence is not this stream's.
///
/// Also set on one the watch wrote naming no trigger because the trigger's
/// message could not be read, for a reason other than its absence, by its
/// limit (issue 1066): the reason then gives what the last read answered.
pub const DEAD_LETTER_UNATTRIBUTED_KEY: &str = "unattributed";

/// Recognise a dead letter in one recorded event, or decline.
///
/// This is the whole of the atom's definition: a `Failed` event whose
/// `error_kind` is `TriggerExhausted`, with the emitters' annotations
/// lifted into domain fields. Every read — Get, List and Stream alike —
/// is this predicate applied to a different span of the log, so there
/// is one place the shape can drift.
///
/// A missing annotation is rendered as an empty value rather than
/// refused: the annotations are how the emitters describe the trigger,
/// and an exhaustion the log records with less detail is still an
/// exhaustion the operator needs to see.
///
/// A free function rather than `DeadLetter::from_event`, because an
/// inherent impl cannot follow a type across a crate boundary and the
/// type is a shape the client shares. The keys it reads stay here with
/// it: they are how the emitters write a dead letter, and nothing that
/// only *reads* one needs to know them.
pub fn from_event(event: &Event) -> Option<DeadLetter> {
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
        start_unchecked: annotation(DEAD_LETTER_START_UNCHECKED_KEY).map(start_unchecked),
        // Present in any form, it still says the dead letter names nothing.
        unattributed: annotation(DEAD_LETTER_UNATTRIBUTED_KEY)
            .map(|why| why.as_str().map_or_else(|| why.to_string(), str::to_string)),
    })
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
                detail: None,
            }),
        )
    }

    #[test]
    fn only_trigger_exhaustion_is_a_dead_letter() {
        assert!(
            from_event(&failed(FailureKind::TriggerExhausted)).is_some(),
            "an exhausted trigger is the atom"
        );
        assert!(
            from_event(&failed(FailureKind::RuntimeError)).is_none(),
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
        assert!(from_event(&completed).is_none());
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
        let dead = from_event(&event).expect("a dead letter");
        assert_eq!(dead.trigger_subject, "fq.trigger.researcher");
        assert_eq!(dead.trigger_stream_seq, Some(11));
        assert_eq!(dead.source, "inline");
        assert_eq!(dead.trigger_payload, serde_json::json!({"n": 1}));
        assert_eq!(dead.agent_id, "researcher");
        assert_eq!(dead.start_unchecked, None);
        assert_eq!(dead.unattributed, None);
    }

    /// A dead letter the watch wrote for an advisory from an earlier
    /// incarnation of the trigger stream says why it names no trigger,
    /// wherever it is read; a reason in a form this build does not expect
    /// still says it.
    #[test]
    fn an_unattributed_dead_letter_says_why() {
        let why = "its advisory came from an earlier incarnation of the trigger stream";
        let event = failed(FailureKind::TriggerExhausted)
            .annotate(DEAD_LETTER_UNATTRIBUTED_KEY, serde_json::json!(why));
        assert_eq!(
            from_event(&event).expect("a dead letter").unattributed,
            Some(why.to_string())
        );
        let garbled = failed(FailureKind::TriggerExhausted)
            .annotate(DEAD_LETTER_UNATTRIBUTED_KEY, serde_json::json!({"n": 1}));
        assert_eq!(
            from_event(&garbled).expect("a dead letter").unattributed,
            Some(r#"{"n":1}"#.to_string())
        );
    }

    /// One whose annotation cannot be read still says its start was not
    /// checked, rather than reading as a checked dead letter.
    #[test]
    fn an_unreadable_unchecked_annotation_still_marks_the_dead_letter_unchecked() {
        let event = failed(FailureKind::TriggerExhausted).annotate(
            DEAD_LETTER_START_UNCHECKED_KEY,
            serde_json::json!("garbled"),
        );
        let unchecked = from_event(&event)
            .expect("a dead letter")
            .start_unchecked
            .expect("still unchecked");
        assert!(
            unchecked.reason.starts_with("its reason could not be read"),
            "{unchecked:?}"
        );
        assert_eq!(unchecked.invocation_id, None);
    }

    /// A dead letter the watch wrote without checking the trigger's start
    /// says so, with the reason and the invocation to look at, wherever it
    /// is read.
    #[test]
    fn an_unchecked_start_is_lifted_onto_the_dead_letter() {
        let event = failed(FailureKind::TriggerExhausted).annotate(
            DEAD_LETTER_START_UNCHECKED_KEY,
            serde_json::json!({
                "reason": "a copy of the trigger is still handling its claim",
                "invocation_id": "019a0000-0000-7000-8000-000000000001",
            }),
        );
        assert_eq!(
            from_event(&event).expect("a dead letter").start_unchecked,
            Some(StartUnchecked {
                reason: "a copy of the trigger is still handling its claim".to_string(),
                invocation_id: Some("019a0000-0000-7000-8000-000000000001".to_string()),
            })
        );
    }

    /// An exhaustion the emitter could not fully describe is still one
    /// the operator must see — the advisory path loses the payload
    /// (and with it the trigger sequence) when the trigger has aged
    /// out, and a listing that dropped those rows would hide exactly
    /// the failures that took longest to surface.
    #[test]
    fn a_dead_letter_with_no_annotations_is_still_a_dead_letter() {
        let dead = from_event(&failed(FailureKind::TriggerExhausted)).expect("a dead letter");
        assert_eq!(dead.trigger_subject, "");
        assert_eq!(dead.trigger_stream_seq, None);
        assert_eq!(dead.source, "");
        assert_eq!(dead.trigger_payload, serde_json::Value::Null);
    }
}
