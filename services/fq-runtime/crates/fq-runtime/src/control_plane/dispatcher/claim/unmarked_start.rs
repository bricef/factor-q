//! A run's durable start whose mark found no claim naming the run (issue
//! 1046).
//!
//! The claim a delivery writes names the run it starts, from claim time,
//! and a copy that holds it is the only one that can change it while it
//! handles it (`own_claim`). So at the run's first WAL write the mark
//! normally finds that claim and records the start on it. It can find
//! something else only if a writer outside this copy's handling changed
//! the row: a release an earlier copy gave up on that lands after this
//! copy took the claim again (issue 1061), a second process adopting it,
//! or someone editing the store by hand. None of those is reachable in one
//! daemon today, by the bound on a release and the one-daemon assumption.
//!
//! **The run has started, whatever the claim says**, and the claim is what
//! refuses a copy of its trigger that comes back, from a lost ack or a
//! replay of the stream. So the start is recorded rather than dropped
//! ([`ControlPlaneStore::record_trigger_start`]): a claim that was gone is
//! written; another invocation's claim, not started, has this start
//! recorded in its place, since left `claimed` it could be released from
//! under a run that started; another run's start already on the claim is
//! left, being all a copy needs to be refused. Retrying the mark would not
//! do any of that: a missing row does not come back.
//!
//! **The delivery is then acked keeping the claim**, never with
//! [`TriggerDispatcher::ack`], whose release deletes a `claimed` row
//! whatever invocation it names. It is acked even when the store refuses
//! the record, as it is when the store refuses the mark: the run started,
//! and a delivery left to come back would meet no claim and start the
//! trigger again now.
//!
//! **Each case is one `error` line**, naming the run, the trigger, the
//! sequence and the other invocation where there is one, since something
//! changed a claim it should not have. There is no count: nothing in one
//! daemon reaches it, and the line is what says it happened.

use tracing::error;

use super::super::TriggerDispatcher;
use super::{Claimed, keeping_alive};
use crate::control_plane::ControlPlaneStore;
use crate::control_plane::store::RecordedStart;

impl TriggerDispatcher {
    /// Record the durable start of `claim`'s run, whose mark found no claim
    /// naming it, then ack the delivery keeping whatever claim there is.
    /// The delivery is kept alive while the store records it.
    pub(super) async fn record_unmarked_start(
        &self,
        msg: &async_nats::jetstream::Message,
        store: &ControlPlaneStore,
        worker_id: &str,
        claim: &Claimed,
        trigger_id: uuid::Uuid,
    ) {
        let Claimed {
            stream_seq,
            invocation_id,
        } = *claim;
        let named = invocation_id.to_string();
        let now = chrono::Utc::now().timestamp_millis();
        let recorded = keeping_alive(
            msg,
            Some(trigger_id),
            store.record_trigger_start(self.trigger_key(stream_seq), worker_id, &named, now),
        )
        .await;
        let subject = msg.subject.as_str();
        match recorded {
            // Marked for the run between the mark and this read.
            Ok(RecordedStart::Marked) => {}
            Ok(RecordedStart::Written) => {
                error!(stream_seq, %invocation_id, %trigger_id, subject,
                    "the claim of a trigger whose run started was gone when its start was \
                     marked; wrote one recording the start, so a copy of the trigger that comes \
                     back is refused rather than run again");
            }
            Ok(RecordedStart::WrittenOver { claimed_for }) => {
                error!(stream_seq, %invocation_id, %trigger_id, subject,
                    claim_invocation_id = %claimed_for.as_deref().unwrap_or("none"),
                    "the claim of a trigger whose run started named another invocation, not \
                     started, when its start was marked; recorded this run's start on it in that \
                     invocation's place, so the claim cannot be released from under a run that \
                     started");
            }
            Ok(RecordedStart::AlreadyStarted {
                invocation_id: started,
            }) => {
                error!(stream_seq, %invocation_id, %trigger_id, subject,
                    started_invocation_id = %started.as_deref().unwrap_or("none"),
                    "the claim of a trigger whose run started already records another run's \
                     start: the trigger has run twice. Left the claim naming that run, which \
                     refuses any further copy; this run is on no claim");
            }
            Err(err) => {
                error!(error = %err, stream_seq, %invocation_id, %trigger_id, subject,
                    "could not record the durable start of a trigger whose claim does not name \
                     its run: acking the delivery anyway and letting the run continue, so it is \
                     not redelivered now. No claim records this run's start, so a replay of the \
                     trigger, or a copy a lost ack brings back, would start it again");
            }
        }
        self.ack_keeping_claim(msg, Some(trigger_id), "durably started, its start recorded")
            .await;
    }
}
