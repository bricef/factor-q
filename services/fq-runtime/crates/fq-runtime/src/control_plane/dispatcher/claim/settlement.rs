//! Answering a delivery: release its claim, then ack or NAK it, with the
//! delivery kept in progress from the moment settling starts until the
//! answer is published (issue 1057).
//!
//! A delivery is renewed while its run sets up, while it waits in a hold
//! or for a worker permit, and while its durable start is marked. Settling
//! is the stretch after that for a delivery whose run did not start: a run
//! that returned before its first WAL write, or a trigger refused before it
//! ran. The answer goes out only after the claim is released, a write
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
//! so the refusals at the front of `handle`, admission's, the agent cap's
//! and the pre-start settlement all do; the dead-letter path hands `settle`
//! its publish, which answers with an ack or, for a dead letter it could
//! not publish, [`Answer::HandOver`]. Six answers that keep the claim do
//! not:
//! - the ack after a mark that landed, and the ack after a mark refused on
//!   every attempt (`mark_durable_started`), both of which follow the
//!   mark's own renewals with no store write between, the claim being the
//!   run's record;
//! - the ack of a started run with no claim store to mark it on, which
//!   has no claim to keep and nothing to write;
//! - the ack after the record of a start the mark found no claim for
//!   (`unmarked_start`), which follows the record's own renewals likewise;
//! - the ack of a copy whose run has a start on record (`own_claim`),
//!   which follows a mark of its own that nothing renews;
//! - and the ack of a copy of a started run in `claim_delivery`, which
//!   follows only the claim's read.
//!
//! **Bounded.** The release is a single `DELETE`. The store bounds its
//! waits by itself: up to 30 seconds for a pooled connection and up to 5
//! for the write lock, sqlx's defaults, which `ControlPlaneStore::open`
//! does not change; a release that fails is logged and the answer goes out
//! all the same. [`RELEASE_BOUND`] caps the wait from outside as well, for
//! a store stuck in a way neither bounds, such as storage that stops
//! answering: settling then stops waiting and answers the delivery. It
//! stops the wait, not the release, which it has no way to call back: the
//! statement is already on its connection's thread, the bound being longer
//! than the wait for a connection, and lands when the store lets it. Landing late does no harm, since a release removes the
//! claim only while it still names the invocation giving it back (issue
//! 1061). The dead letter's publish waits for one of the client's
//! 5,000 publish permits, which only a client with that many publishes in
//! flight waits on at all, then up to 5 seconds to queue the message and 5
//! more for its acknowledgement. So settling holds a delivery in progress
//! for about 50 seconds at the very worst, against a trigger stream that
//! keeps its messages for a day.

use std::future::Future;
use std::time::Duration;

use tracing::{error, warn};

use super::super::{TriggerDispatcher, trigger_name};
use super::{Claimed, keeping_alive};
use crate::control_plane::{ControlPlaneStore, TriggerKey};

/// How long settling waits for the claim's release before answering the
/// delivery without it.
///
/// Giving up stops the wait, not the release: the `DELETE` is already on
/// its connection's thread, since the bound outlasts the 30-second wait for
/// a pooled connection (a release given up while it still waited for one
/// would never run), and it lands when the store lets it, after the
/// answer. Whenever it lands it does no harm: it removes the claim only
/// while the claim is still `claimed` and still names the invocation giving
/// it back, so it cannot delete a claim a copy has taken again since
/// (issue 1061).
///
/// So the bound's length is no longer what keeps a late release safe. It is
/// 40 seconds so that it stays above the store's own waits, sqlx's
/// 30-second acquire timeout and 5-second busy timeout (its defaults, which
/// `ControlPlaneStore::open` does not change): every release the store
/// answers, landed or refused, then reaches settling before the bound and
/// is logged as what it was, and settling gives up only on a store stuck
/// outside those waits, which its error line then means. A shorter bound
/// would buy no safety and would log as given up releases the store was
/// still about to answer.
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
            let release = self.release_unstarted_claim(msg, claim, trigger_id, answer.context());
            let released = tokio::time::timeout(release_bound, release);
            if released.await.is_err() {
                let stream_seq = claim.stream_seq;
                error!(stream_seq, invocation_id = %claim.invocation_id, subject = %msg.subject,
                    trigger_id = %trigger_name(trigger_id), context = answer.context(),
                    waited_ms = release_bound.as_millis() as u64,
                    "gave up waiting for the claim store to release a trigger's claim; answering \
                     the delivery without it. The release is not cancelled: it lands when the \
                     store lets it, removing the claim only if it still names this invocation, \
                     and until then the claim stays as it was");
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

    /// Give back `claim`, if it is still `claimed` and still names the
    /// invocation it was taken for. Every caller answers a delivery whose
    /// run has not started, holding the claim `claimed` under its own
    /// invocation; an ack after a landed mark keeps the claim and gives
    /// nothing back (issue 1055). So a release that removes nothing means
    /// something changed the claim underneath it: a second process on the
    /// same store, the dead-letter watch or a requeue marking it after
    /// finding a run in the worker's WAL, or a hand edit. (A copy taking the
    /// claim again cannot do it in one daemon: a re-take follows only a
    /// release that settling gave up waiting for, and that release lands
    /// unwatched.)
    ///
    /// That release is logged at warn with the claim as it stands now, read
    /// again on this path only, since what it holds is what tells those
    /// causes apart, and with the delivery's subject, trigger and answer
    /// (`context`), as the answer's own lines carry them.
    pub(in crate::control_plane::dispatcher) async fn release_unstarted_claim(
        &self,
        msg: &async_nats::jetstream::Message,
        claim: &Claimed,
        trigger_id: Option<uuid::Uuid>,
        context: &str,
    ) {
        let Some((store, _)) = self.claim_store.as_ref() else {
            return;
        };
        let invocation_id = claim.invocation_id.to_string();
        let key = self.trigger_key(claim.stream_seq);
        match store.release_trigger_claim(key, &invocation_id).await {
            Ok(true) => {}
            Ok(false) => {
                let now = ClaimNow::read(store, key).await;
                warn!(stream_seq = claim.stream_seq, %invocation_id, subject = %msg.subject,
                    trigger_id = %trigger_name(trigger_id), context,
                    claim_state = %now.state, claimant = %now.claimant,
                    claim_invocation_id = %now.invocation_id,
                    "released no trigger claim: none is still `claimed` under the invocation \
                     giving it back, so something changed the claim before its release; the \
                     claim as it stands now is on this line");
            }
            Err(err) => error!(error = %err, stream_seq = claim.stream_seq, %invocation_id,
                "failed to release unfinished trigger claim"),
        }
    }
}

/// A claim as it stands now, for the line about a release that found it
/// changed: its state, claimant and invocation, or that it is gone or could
/// not be read.
struct ClaimNow {
    state: String,
    claimant: String,
    invocation_id: String,
}

impl ClaimNow {
    async fn read(store: &ControlPlaneStore, key: TriggerKey<'_>) -> Self {
        let none = || "none".to_string();
        match store.trigger_claim(key).await {
            // The state as its column spells it, by `ClaimState`'s own
            // `Display`, so a state added to the claim is named there.
            Ok(Some(row)) => Self {
                state: row.state.to_string(),
                claimant: row.claimant,
                invocation_id: row.invocation_id.unwrap_or_else(none),
            },
            Ok(None) => Self {
                state: "gone".to_string(),
                claimant: none(),
                invocation_id: none(),
            },
            Err(err) => Self {
                state: format!("unreadable: {err}"),
                claimant: none(),
                invocation_id: none(),
            },
        }
    }
}
