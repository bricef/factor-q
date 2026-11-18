//! The dispatcher's inline dead-letter path: the terminal event an
//! exhausted transient trigger gets before it is consumed, and the
//! hand-over to the dead-letter watch when that event cannot be published
//! (issue 1062). Moved out of `dispatcher.rs` verbatim when the admission
//! hold and the deferral half of #278 arrived — the file was within a few
//! dozen lines of its budget, and this is the self-contained piece.

use async_nats::jetstream::AckKind;
use tracing::error;

use super::{TriggerDispatcher, trigger_name};
use crate::agent::AgentId;
use crate::bus::TRIGGER_MAX_DELIVER;
use crate::events::{Event, EventPayload, FailureKind, FailurePhase, InvocationTotals};
use crate::worker::ExecutorError;

impl TriggerDispatcher {
    /// Emit a terminal failure event before consuming an exhausted transient
    /// trigger, and say whether it was published. This is the dead-letter
    /// surface for the trigger consumer:
    /// the original trigger remains available in JetStream until its normal
    /// retention expiry, while the terminal event makes the exhaustion
    /// visible to the projection and operators (`fq doctor` counts the
    /// `trigger_exhausted` kind; the annotations carry what a requeue
    /// needs).
    ///
    /// This is the *fast path*: it fires only when the final delivery
    /// reaches a live dispatcher and fails there. A published dead letter
    /// is acked, and the ack suppresses the server's MAX_DELIVERIES
    /// advisory (probed empirically, #169). One that could not be
    /// published is not acked (issue 1062): the delivery is handed over
    /// ([`Self::hand_over`]), the broker raises the advisory, and the
    /// advisory watch ([`crate::control_plane::advisory_watch`]) writes the
    /// dead letter from the durable capture stream, as it does for
    /// exhaustion this dispatcher never observes (a crash during the final
    /// delivery; a pre-bound poison trigger at upgrade time). The two
    /// emitters overlap where a publish that failed here had in fact
    /// reached the stream, and, as before issue 1062, where a published
    /// dead letter's ack never reached the broker. The watch skips a
    /// trigger whose dead letter is the agent's most recent `failed` event,
    /// but writes a second one when another failure of the agent came in
    /// between, when it cannot read the events stream at that moment, or
    /// when a publish from here lands only after it looked; the shared
    /// `trigger_stream_seq` annotation reconciles the two (de-duplication
    /// is issue 1066).
    ///
    /// A publish that fails is logged here, once, at error, with what the
    /// watch's dead letter will not carry: the error that ended the last
    /// delivery (`last_error`), beside the trigger's sequence, which
    /// matches the line to that dead letter. The line's trigger id matches
    /// it too only when the publisher named the trigger: for one published
    /// without a name, the id is the one the dispatcher gave this
    /// delivery, and the watch, which does not invent one, writes a dead
    /// letter with no trigger id, which a requeue refuses (issue 1080).
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
    ) -> bool {
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
                detail: None,
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
            error!(agent_id = %agent_id, subject = trigger_subject, stream_seq, %trigger_id,
                delivery_attempt, error = %publish_err, last_error = %err,
                "could not publish the dead letter of a trigger whose last delivery failed \
                 before its first WAL write; NAKing the delivery with no delay instead of acking \
                 it, so the broker reports the trigger exhausted at once and the dead-letter \
                 watch writes the dead letter. That dead letter does not carry the error that \
                 ended the delivery: `last_error` here is the record of it");
            return false;
        }
        error!(
            agent_id = %agent_id,
            delivery_attempt,
            "trigger retry limit exhausted; emitted terminal dead-letter event"
        );
        true
    }

    /// Hand a trigger's last delivery to the dead-letter watch after its
    /// dead letter could not be published (issue 1062): NAK it with no
    /// delay, its claim already released by the settlement.
    ///
    /// **Why a bare NAK.** No delivery follows the last, so the broker
    /// answers a NAK with no delay by putting the message back to be
    /// delivered, finding its deliveries spent when the consumer next pulls,
    /// and raising its MAX_DELIVERIES advisory, which the advisory stream
    /// captures. While the consume loop runs it has a pull waiting, bar the
    /// instants between one pull and the next and the half second it pauses
    /// after a failed one, so that is at once
    /// (`tests/dispatcher_log_lines.rs` pins it through the real loop). Once
    /// the loop has ended, on a drain or a shutdown, it pulls no more: the
    /// NAK finds no pull waiting, or only the loop's last one until that
    /// expires, and the advisory comes at the next start's first pull, not
    /// lost (`unpublished_dead_letter` pins the broker's side). A NAK
    /// carrying a delay, even of zero, would not do: on the
    /// trigger consumer, whose `backoff` is the retry ladder, the broker
    /// dates such a delivery back by the first ack window and then waits
    /// for the ladder's last entry, so the advisory would come eleven and a
    /// half minutes later. Probed on the pinned broker, and pinned by the
    /// tests in `durable_start_signal_tests::unpublished_dead_letter`. It
    /// relies on the consumer's `max_deliver` being
    /// [`TRIGGER_MAX_DELIVER`], which `EventBus::durable` repairs on open:
    /// with a higher bound, a bare NAK would redeliver at once.
    ///
    /// **A NAK the client cannot send** leaves the delivery unanswered, and
    /// the broker raises the same advisory once the delivery's ack window
    /// lapses, the ladder's last entry after its last renewal.
    pub(super) async fn hand_over(
        &self,
        msg: &async_nats::jetstream::Message,
        trigger_id: Option<uuid::Uuid>,
        context: &str,
    ) {
        if let Err(err) = msg.ack_with(AckKind::Nak(None)).await {
            error!(error = %err, context, subject = %msg.subject,
                trigger_id = %trigger_name(trigger_id),
                "failed to NAK the last delivery of a trigger whose dead letter was not \
                 published; the broker reports the trigger exhausted when the delivery's ack \
                 window lapses instead, and the dead-letter watch writes the dead letter then");
        }
    }
}
