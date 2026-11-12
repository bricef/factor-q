//! What becomes of a trigger whose invocation returned before its
//! durable start, the moment `handle` acts on `trigger_fate`.
//!
//! Split from `dispatcher.rs` to keep that file and `handle` inside their
//! size budgets when the drain case was added (#974).

use tracing::info;

use super::{TriggerDispatcher, TriggerFate, trigger_fate};
use crate::agent::AgentId;
use crate::worker::{ExecutorError, InvocationOutcome};

impl TriggerDispatcher {
    /// Settle the delivery of an invocation that returned before the
    /// durable-start signal fired, so the run left nothing durable
    /// behind. Its fate comes from the result:
    ///
    /// - **Transient** error (bus/store unavailable, a transient LLM
    ///   error): the run could succeed on a retry, so NAK, and JetStream
    ///   redelivers the trigger, recovering the otherwise-lost run. On the
    ///   last delivery it is dead-lettered instead.
    /// - **Permanent** error (a terminal `failed` outcome, a poison
    ///   payload) or a clean `Ok` return: nothing to retry, so ACK. The
    ///   `Failed` event already recorded why, and redelivering a poison
    ///   trigger would loop under the consumer's unbounded redelivery. An
    ///   operator halt during the invocation's setup is in this group:
    ///   the drop is its outcome.
    /// - A **drain** during the invocation's setup (#974): neither. The
    ///   trigger is the only record of the work, so the delivery is left
    ///   un-acked, the way `handle`'s own drain check leaves one. The
    ///   claim stays `claimed` under this process's id and names the
    ///   invocation that never wrote a WAL row, so the next binary's
    ///   startup recovery has nothing to mark, and when the broker
    ///   redelivers the trigger, that binary adopts the claim under its
    ///   new id and starts the trigger once (`claim_delivery`).
    pub(super) async fn settle_unstarted(
        &self,
        msg: &async_nats::jetstream::Message,
        agent_id: &AgentId,
        trigger_id: uuid::Uuid,
        trigger_payload: &serde_json::Value,
        delivery_attempt: u32,
        result: &Result<InvocationOutcome, ExecutorError>,
    ) {
        match (trigger_fate(result, delivery_attempt), result) {
            (TriggerFate::DeadLetter, Err(err)) => {
                self.dead_letter_exhausted(
                    agent_id,
                    msg.subject.as_str(),
                    trigger_id,
                    trigger_payload,
                    msg.info().map(|info| info.stream_sequence).unwrap_or(0),
                    delivery_attempt,
                    err,
                )
                .await;
                self.ack(msg, Some(trigger_id), "transient retry limit exhausted")
                    .await;
            }
            (TriggerFate::Retry(delay), _) => {
                self.nak(
                    msg,
                    trigger_id,
                    delay,
                    "transient failure before first WAL write",
                )
                .await;
            }
            (TriggerFate::Leave, _) => info!(
                subject = %msg.subject,
                trigger_id = %trigger_id,
                "draining — invocation stopped before its first WAL write; \
                 leaving trigger queued for the next binary"
            ),
            _ => {
                self.ack(
                    msg,
                    Some(trigger_id),
                    "invocation returned before durable start (permanent)",
                )
                .await;
            }
        }
    }
}
