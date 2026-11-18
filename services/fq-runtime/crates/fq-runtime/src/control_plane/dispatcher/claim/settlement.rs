//! Answering a delivery: release its claim, then ack or NAK it, with the
//! delivery kept in progress from the moment settling starts until the
//! answer is published (issue 1057).
//!
//! A delivery is renewed while its run sets up, while it waits in a hold
//! or for a worker permit, and while its durable start is marked. Settling
//! is the stretch after that: a run that returned before its first WAL
//! write, a trigger refused before it ran, or a run whose start was just
//! marked. The answer goes out only after the claim is released, a write
//! on the claim store, and on a trigger's last delivery only after its
//! dead letter's publish, whose outcome decides the answer (issue 1062).
//! If that work outlasted what was left of the ack window, the broker
//! redelivered, and a copy that arrived as the release landed took the
//! freed claim and handled the trigger again. So the delivery is renewed
//! through all of it, at the cadence every hold keeps
//! ([`super::keeping_alive`]), and the renewals end with the answer.
//!
//! **Which answers go through here.** Every answer that releases a claim:
//! `ack` and `nak` are [`TriggerDispatcher::settle`] with an answer ready,
//! so the refusals at the front of `handle`, admission's, the agent cap's,
//! the pre-start settlement and the ack after a durable start all do; the
//! dead-letter path hands `settle` its publish, which answers with an ack
//! or, for a dead letter it could not publish, [`Answer::HandOver`]. Four
//! answers that keep the claim do not: the ack after a mark refused on every
//! attempt (`mark_durable_started`), which follows the mark's own
//! renewals with no store write between; the ack after the record of a
//! start the mark found no claim for (`unmarked_start`), which follows the
//! record's own renewals likewise; the ack of a copy whose run has a
//! start on record (`own_claim`), which follows a mark of its own that
//! nothing renews; and the ack of a copy of a started run in
//! `claim_delivery`, which follows only the claim's read.
//!
//! **Bounded.** The release is a single `DELETE`. The store bounds its
//! waits by itself: up to 30 seconds for a pooled connection and up to 5
//! for the write lock, sqlx's defaults, which `ControlPlaneStore::open`
//! does not change; a release that fails is logged and the answer goes out
//! all the same. [`RELEASE_BOUND`] caps the wait from outside as well, for
//! a store stuck in a way neither bounds, such as storage that stops
//! answering: settling then stops waiting and answers the delivery. It
//! stops the wait, not the release, which it has no way to call back: the
//! statement is already on its connection's thread, and lands when the
//! store lets it. The dead letter's publish waits for one of the client's
//! 5,000 publish permits, which only a client with that many publishes in
//! flight waits on at all, then up to 5 seconds to queue the message and 5
//! more for its acknowledgement. So settling holds a delivery in progress
//! for about 50 seconds at the very worst, against a trigger stream that
//! keeps its messages for a day.

use std::future::Future;
use std::time::Duration;

use tracing::error;

use super::super::{TriggerDispatcher, trigger_name};
use super::{Claimed, keeping_alive};

/// How long settling waits for the claim's release before answering the
/// delivery without it.
///
/// Giving up stops the wait, not the release: the `DELETE` is already on
/// its connection's thread and lands when the store lets it, after the
/// answer. That is safe only because the bound stays above the store's own
/// waits, sqlx's 30-second acquire timeout and 5-second busy timeout (its
/// defaults, which `ControlPlaneStore::open` does not change). A release
/// still unfinished at this bound has got past both, so it holds the write
/// lock, and nothing can claim the row before it lands, or it is stuck
/// outside SQLite's lock waits altogether; and the copy a NAK brings back
/// comes at least 30 seconds later. Below those waits, a release given up
/// while it still waited for the lock could land after a copy had taken
/// the claim again, and delete that copy's claim, since a release deletes
/// the `claimed` row by its sequence whatever invocation it names (issue
/// 1061 is to scope it to its invocation).
pub(in crate::control_plane::dispatcher) const RELEASE_BOUND: Duration = Duration::from_secs(40);

/// How a delivery is answered once its claim is released.
#[derive(Clone, Copy)]
pub(in crate::control_plane::dispatcher) enum Answer<'a> {
    /// Consumed: an ack.
    Ack { context: &'a str },
    /// To be redelivered after `delay`: a NAK.
    Nak { delay: Duration, context: &'a str },
    /// A trigger's last delivery whose dead letter could not be published,
    /// handed to the dead-letter watch (issue 1062): a NAK with no delay,
    /// which the broker answers at once with its max-deliveries advisory
    /// ([`TriggerDispatcher::hand_over`]).
    HandOver { context: &'a str },
}

impl Answer<'_> {
    /// What the delivery is being answered for, as its log lines say.
    fn context(&self) -> &str {
        match self {
            Answer::Ack { context }
            | Answer::Nak { context, .. }
            | Answer::HandOver { context } => context,
        }
    }
}

impl TriggerDispatcher {
    /// Settle a delivery: work out its `answer`, release its claim, publish
    /// the answer, keeping the delivery in progress throughout, from now
    /// until the answer is published. The answer is a future so that work
    /// it waits on is kept alive too: on a trigger's last delivery, the
    /// dead letter's publish, which decides it.
    pub(in crate::control_plane::dispatcher) async fn settle<'a>(
        &self,
        msg: &async_nats::jetstream::Message,
        claim: &Claimed,
        trigger_id: Option<uuid::Uuid>,
        answer: impl Future<Output = Answer<'a>>,
    ) {
        self.settle_within(msg, claim, trigger_id, answer, RELEASE_BOUND)
            .await;
    }

    /// [`Self::settle`], waiting for the release for at most
    /// `release_bound`.
    pub(in crate::control_plane::dispatcher) async fn settle_within<'a>(
        &self,
        msg: &async_nats::jetstream::Message,
        claim: &Claimed,
        trigger_id: Option<uuid::Uuid>,
        answer: impl Future<Output = Answer<'a>>,
        release_bound: Duration,
    ) {
        keeping_alive(msg, trigger_id, async {
            let answer = answer.await;
            let released = tokio::time::timeout(release_bound, self.release_unstarted_claim(claim));
            if released.await.is_err() {
                let stream_seq = claim.stream_seq;
                error!(stream_seq, subject = %msg.subject, trigger_id = %trigger_name(trigger_id),
                    context = answer.context(), waited_ms = release_bound.as_millis() as u64,
                    "gave up waiting for the claim store to release a trigger's claim; answering \
                     the delivery without it. The release is not cancelled: it lands when the \
                     store lets it, and until then the claim stays as it was");
            }
            match answer {
                Answer::Ack { context } => self.ack_keeping_claim(msg, trigger_id, context).await,
                Answer::Nak { delay, context } => {
                    self.send_nak(msg, trigger_id, delay, context).await;
                }
                Answer::HandOver { context } => self.hand_over(msg, trigger_id, context).await,
            }
        })
        .await;
    }
}
