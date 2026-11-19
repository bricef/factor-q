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

use super::claim::{Answer, Claimed};
use super::{TriggerDispatcher, TriggerFate, trigger_fate};
use crate::agent::AgentId;
use crate::control_plane::store::ConsumedWhy;
use crate::worker::{BoundaryStop, ExecutorError, InvocationOutcome};

impl TriggerDispatcher {
    /// Settle the delivery of an invocation that returned before the
    /// durable-start signal fired, so the run left no WAL row behind. Its
    /// fate comes from the result:
    ///
    /// - **Transient** error (bus/store unavailable, a transient LLM
    ///   error): the run could succeed on a retry, so NAK, and JetStream
    ///   redelivers the trigger, recovering the otherwise-lost run. On the
    ///   last delivery it is dead-lettered instead: once its dead letter is
    ///   published, its claim is recorded consumed as dead-lettered and the
    ///   delivery acked (issue 1055). A dead letter that cannot be published
    ///   leaves the delivery unacked (issue 1062): the claim is released and
    ///   the delivery NAKed with no delay, the broker raises its
    ///   max-deliveries advisory at once, and the dead-letter watch, finding
    ///   nothing on record for the trigger, writes the dead letter, retrying
    ///   until the events stream takes it. Acked, the trigger would be
    ///   consumed with no dead letter from either.
    /// - **Permanent** error (a terminal `failed` outcome, a poison
    ///   payload) or a clean `Ok` return: nothing to retry, so the claim is
    ///   recorded consumed, with why, and the delivery ACKed (issue 1055):
    ///   a copy that a lost ack brings back is then dropped rather than run
    ///   to fail again. The `Failed` event already recorded why, and
    ///   redelivering a poison trigger would loop under the consumer's
    ///   unbounded redelivery. An operator halt before the invocation's
    ///   first WAL write is in this group: the operator dropped the
    ///   invocation, and the drop is its outcome.
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
    ///
    /// Which delivery of the trigger this is comes off `msg`, as the
    /// number `handle` hands the run does (`claim_delivery`).
    pub(super) async fn settle_before_durable_start(
        &self,
        msg: &async_nats::jetstream::Message,
        claim: &Claimed,
        agent_id: &AgentId,
        trigger_id: uuid::Uuid,
        trigger_payload: &serde_json::Value,
        result: &Result<InvocationOutcome, ExecutorError>,
    ) {
        let delivery_attempt = msg
            .info()
            .map_or(1, |info| u32::try_from(info.delivered).unwrap_or(1));
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
                        Answer::Consume {
                            why: ConsumedWhy::DeadLettered,
                            context: "transient retry limit exhausted",
                        }
                    } else {
                        Answer::HandOver {
                            context: "transient retry limit exhausted, dead letter not published",
                        }
                    }
                };
                self.settle(msg, claim, Some(trigger_id), answer).await;
            }
            (TriggerFate::Retry(delay), _) => {
                self.nak(
                    msg,
                    claim,
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
                self.consume(
                    msg,
                    claim,
                    Some(trigger_id),
                    unstarted_why(result),
                    "invocation returned before durable start (permanent)",
                )
                .await;
            }
        }
    }
}

/// Why a run that returned before its durable start, and is not retried,
/// consumed its trigger: an operator's halt in setup, a failure for good,
/// or a return without one (issue 1055). An operator's drop that races
/// either of the last two leaves the reason as it is: the drop is recorded
/// on the claim beside it (`consume_trigger_claim`, when the drop's
/// ownership row is there first).
fn unstarted_why(result: &Result<InvocationOutcome, ExecutorError>) -> ConsumedWhy {
    match result {
        Ok(InvocationOutcome::Unstarted {
            stopped_by: BoundaryStop::Halt,
            ..
        }) => ConsumedWhy::HaltedInSetup,
        Err(_) => ConsumedWhy::FailedForGood,
        Ok(_) => ConsumedWhy::ReturnedUnstarted,
    }
}
