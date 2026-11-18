//! What becomes of the delivery of an invocation that returned before its
//! durable start: the moment `handle` acts on `trigger_fate`.
//!
//! Moved out of `handle` when the drain case was added (#974). That was a
//! choice, not forced by a size budget: before the move `handle` was 233
//! of its 250 lines and `dispatcher.rs` 742 of its 800, so the new arm
//! would have fitted. Settling is one step with a policy of its own to
//! document, and `handle`, where every admission change lands, is shorter
//! for calling it.

use tracing::info;

use super::claim::Answer;
use super::{TriggerDispatcher, TriggerFate, trigger_fate};
use crate::agent::AgentId;
use crate::worker::{ExecutorError, InvocationOutcome};

impl TriggerDispatcher {
    /// Settle the delivery of an invocation that returned before the
    /// durable-start signal fired, so the run left no WAL row behind. Its
    /// fate comes from the result:
    ///
    /// - **Transient** error (bus/store unavailable, a transient LLM
    ///   error): the run could succeed on a retry, so NAK, and JetStream
    ///   redelivers the trigger, recovering the otherwise-lost run. On the
    ///   last delivery it is dead-lettered instead, and acked once its dead
    ///   letter is published. A dead letter that cannot be published leaves
    ///   the delivery unacked (issue 1062): the claim is released and the
    ///   delivery NAKed with no delay, the broker raises its max-deliveries
    ///   advisory at once, and the dead-letter watch, finding nothing on
    ///   record for the trigger, writes the dead letter, retrying until the
    ///   events stream takes it. Acked, the trigger would be consumed with
    ///   no dead letter from either.
    /// - **Permanent** error (a terminal `failed` outcome, a poison
    ///   payload) or a clean `Ok` return: nothing to retry, so ACK. The
    ///   `Failed` event already recorded why, and redelivering a poison
    ///   trigger would loop under the consumer's unbounded redelivery. An
    ///   operator halt during the invocation's setup is in this group: the
    ///   operator dropped the invocation, and the drop is its outcome.
    /// - A **drain** during the invocation's setup (#974): neither. The
    ///   trigger is the only record of the work, so the delivery is left
    ///   un-acked, the way `handle`'s own drain check leaves one. The
    ///   claim stays `claimed` under this process's id and names the
    ///   invocation that never wrote a WAL row, so the next binary's
    ///   startup recovery has nothing to mark, and when the broker
    ///   redelivers the trigger, that binary adopts the claim under its
    ///   new id and starts the trigger once (`claim_delivery`). Adoption
    ///   writes the new invocation's id over the old one, so the log line
    ///   here is what links the abandoned invocation to the delivery.
    pub(super) async fn settle_before_durable_start(
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
                // The dead letter is published inside the settlement, so
                // the delivery is kept alive through it as well as through
                // the release and the answer (#1057), and its publish
                // decides that answer (issue 1062).
                let answer = async {
                    let published = self
                        .dead_letter_exhausted(
                            agent_id,
                            msg.subject.as_str(),
                            trigger_id,
                            trigger_payload,
                            msg.info().map(|info| info.stream_sequence).unwrap_or(0),
                            delivery_attempt,
                            err,
                        )
                        .await;
                    if published {
                        Answer::Ack {
                            context: "transient retry limit exhausted",
                        }
                    } else {
                        Answer::HandOver {
                            context: "transient retry limit exhausted, dead letter not published",
                        }
                    }
                };
                self.settle(msg, Some(trigger_id), answer).await;
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
            (TriggerFate::Leave, _) => {
                // `trigger_fate` leaves only an `Unstarted` drain.
                let abandoned = match result {
                    Ok(InvocationOutcome::Unstarted { invocation_id, .. }) => {
                        invocation_id.to_string()
                    }
                    _ => "-".to_string(),
                };
                info!(
                    subject = %msg.subject,
                    trigger_id = %trigger_id,
                    invocation_id = %abandoned,
                    stream_seq = msg.info().map(|info| info.stream_sequence).unwrap_or(0),
                    delivery_attempt,
                    "draining — invocation stopped before its first WAL write; leaving the \
                     trigger queued for the next binary, which reruns it under a new invocation id"
                );
            }
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
