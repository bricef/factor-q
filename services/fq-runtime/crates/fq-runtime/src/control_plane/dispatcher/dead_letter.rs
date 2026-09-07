//! The dispatcher's inline dead-letter path: the terminal event an
//! exhausted transient trigger gets before it is consumed. Moved out of
//! `dispatcher.rs` verbatim when the admission hold and the deferral
//! half of #278 arrived — the file was within a few dozen lines of its
//! budget, and this is the self-contained piece.

use tracing::error;

use super::TriggerDispatcher;
use crate::agent::AgentId;
use crate::bus::TRIGGER_MAX_DELIVER;
use crate::events::{Event, EventPayload, FailureKind, FailurePhase, InvocationTotals};
use crate::worker::ExecutorError;

impl TriggerDispatcher {
    /// Emit a terminal failure event before consuming an exhausted transient
    /// trigger. This is the dead-letter surface for the trigger consumer:
    /// the original trigger remains available in JetStream until its normal
    /// retention expiry, while the terminal event makes the exhaustion
    /// visible to the projection and operators (`fq doctor` counts the
    /// `trigger_exhausted` kind; the annotations carry what a requeue
    /// needs).
    ///
    /// This is the *fast path*: it fires only when the final delivery
    /// reaches a live dispatcher and fails there, and its ACK
    /// suppresses the server's MAX_DELIVERIES advisory (probed
    /// empirically, #169) — so the two emitters are mutually exclusive
    /// in every non-crash path. Exhaustion this dispatcher never
    /// observes (a crash during the final delivery; a pre-bound poison
    /// trigger at upgrade time) is surfaced by the advisory watch
    /// ([`crate::control_plane::advisory_watch`]) from the durable
    /// capture stream. The shared `trigger_stream_seq` annotation
    /// reconciles the two.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn dead_letter_exhausted(
        &self,
        agent_id: &AgentId,
        trigger_subject: &str,
        trigger_id: uuid::Uuid,
        trigger_payload: &serde_json::Value,
        stream_seq: u64,
        delivery_attempt: u32,
        err: &ExecutorError,
    ) {
        let event = Event::new(
            agent_id.clone(),
            uuid::Uuid::now_v7(),
            EventPayload::Failed(crate::events::FailedPayload {
                error_kind: FailureKind::TriggerExhausted,
                error_message: format!(
                    "trigger exhausted after {delivery_attempt} deliveries (limit {TRIGGER_MAX_DELIVER}): {err}"
                ),
                phase: FailurePhase::Setup,
                partial_totals: InvocationTotals::default(),
            }),
        )
        .annotate(
            crate::dead_letter::DEAD_LETTER_SUBJECT_KEY,
            serde_json::Value::String(trigger_subject.to_string()),
        )
        .annotate(
            crate::dead_letter::DEAD_LETTER_PAYLOAD_KEY,
            trigger_payload.clone(),
        )
        .annotate(
            crate::dead_letter::DEAD_LETTER_STREAM_SEQ_KEY,
            serde_json::json!(stream_seq),
        )
        // The name of the trigger that died, next to the position of
        // it. This path always has one: the dispatcher honoured or
        // assigned it before the invocation started.
        .annotate(
            crate::dead_letter::DEAD_LETTER_TRIGGER_ID_KEY,
            serde_json::Value::String(trigger_id.to_string()),
        )
        .annotate(
            crate::dead_letter::DEAD_LETTER_SOURCE_KEY,
            serde_json::Value::String("inline".to_string()),
        );
        if let Err(publish_err) = self.bus.publish(&event).await {
            error!(
                agent_id = %agent_id,
                delivery_attempt,
                error = %publish_err,
                "failed to publish exhausted trigger dead-letter event"
            );
        } else {
            error!(
                agent_id = %agent_id,
                delivery_attempt,
                "trigger retry limit exhausted; emitted terminal dead-letter event"
            );
        }
    }
}
