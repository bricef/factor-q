//! A run's durable start whose mark found no claim naming the run (issue
//! 1046).
//!
//! The claim a delivery writes names the run it starts, from claim time,
//! and a copy that holds it is the only one that can change it while it
//! handles it (`own_claim`). So at the run's first WAL write the mark
//! normally finds that claim and records the start on it. It can find
//! something else only if a writer outside this copy's handling changed
//! the row: a second process adopting it, or someone editing the store by
//! hand. The daemon does neither itself. Before issue 1061 a release an
//! earlier copy gave up on could delete it too, landing after this copy
//! took the claim again; a release now removes only its own invocation's
//! claim.
//!
//! **The run has started, whatever the claim says**, and the claim is what
//! refuses a copy of its trigger that comes back, from a lost ack or a
//! replay of the stream. So the start is recorded rather than dropped
//! ([`ControlPlaneStore::record_trigger_start`]): a claim that was gone is
//! written; another invocation's claim, not started, has this start
//! recorded in its place, since left `claimed` it could be released from
//! under a run that started; another run's start already on the claim is
//! left, being all a copy needs to be refused. Retrying the mark would not
//! do any of that: a missing row does not come back. The record itself is
//! offered again on the mark's schedule while the store refuses it: the
//! faults that schedule is sized for meet it as they meet the mark, and a
//! refused record, unlike a refused mark, is repaired by nothing later,
//! since no claim is left naming the run for recovery to mark.
//!
//! **The delivery is then acked keeping the claim**, not with
//! [`TriggerDispatcher::ack`]. After a record that landed the claim is
//! started, which no release touches; but a claim still `claimed` and
//! naming this run, which only a store that refused or skipped the writes
//! leaves, is the one claim a release of this run would remove, and it is
//! the run's only record until recovery marks it. It is acked even when the
//! store refuses the record, as it is when the store refuses the mark: the
//! run started, and a delivery left to come back would meet no claim and
//! start the trigger again now.
//!
//! **Each case is one `error` line**, naming the run, the trigger, the
//! sequence and the other invocation where there is one, since something
//! changed a claim it should not have, with the attempts at the mark and
//! at the record the store refused first. A claim found naming the run
//! after all on the record's read is a `warn`: the start is marked, but
//! something changed the claim between the two reads. There is no count:
//! the daemon brings none of these about itself, and the line is what says
//! one happened.

use tracing::{error, warn};

use super::super::TriggerDispatcher;
use super::{Claimed, Landed, Refused, keeping_alive, offer_with_retries};
use crate::control_plane::ControlPlaneStore;
use crate::control_plane::store::RecordedStart;

impl TriggerDispatcher {
    /// Record the durable start of `claim`'s run, whose mark found no claim
    /// naming it after `refused_marks` refusals, then ack the delivery
    /// keeping whatever claim there is. The record is offered on the mark's
    /// own schedule, and the delivery is kept alive while it is.
    pub(super) async fn record_unmarked_start(
        &self,
        msg: &async_nats::jetstream::Message,
        store: &ControlPlaneStore,
        worker_id: &str,
        claim: &Claimed,
        trigger_id: uuid::Uuid,
        refused_marks: usize,
    ) {
        let Claimed {
            stream_seq,
            invocation_id,
        } = *claim;
        let key = self.trigger_key(stream_seq);
        let named = invocation_id.to_string();
        let now = chrono::Utc::now().timestamp_millis();
        let recorded = keeping_alive(
            msg,
            Some(trigger_id),
            offer_with_retries("record", key, invocation_id, || {
                store.record_trigger_start(key, worker_id, &named, now)
            }),
        )
        .await;
        let subject = msg.subject.as_str();
        let (recorded, refused_records) = match recorded {
            Ok(Landed { value, refused }) => (Ok(value), refused.len()),
            Err(refused) => (Err(refused), 0),
        };
        match recorded {
            Ok(RecordedStart::Marked) => {
                warn!(stream_seq, %invocation_id, %trigger_id, subject, refused_marks,
                    refused_records,
                    "the claim of a trigger whose run started named the run on a second look, \
                     after its mark had found it naming another or gone; marked the start on it");
            }
            Ok(RecordedStart::Written) => {
                error!(stream_seq, %invocation_id, %trigger_id, subject, refused_marks,
                    refused_records,
                    "the claim of a trigger whose run started was gone when its start was \
                     marked; wrote one recording the start, so a copy of the trigger that comes \
                     back is refused rather than run again");
            }
            Ok(RecordedStart::WrittenOver { claimed_for }) => {
                error!(stream_seq, %invocation_id, %trigger_id, subject, refused_marks,
                    refused_records,
                    claim_invocation_id = %claimed_for.as_deref().unwrap_or("none"),
                    "the claim of a trigger whose run started named another invocation, not \
                     started, when its start was marked; recorded this run's start on it in that \
                     invocation's place, so the claim cannot be released from under a run that \
                     started");
            }
            Ok(RecordedStart::AlreadyStarted {
                invocation_id: started,
            }) => {
                error!(stream_seq, %invocation_id, %trigger_id, subject, refused_marks,
                    refused_records,
                    started_invocation_id = %started.as_deref().unwrap_or("none"),
                    "the claim of a trigger whose run started already records another run's \
                     start: the trigger has run twice. Left the claim naming that run, which \
                     refuses any further copy; this run is on no claim");
            }
            Err(Refused {
                attempts,
                elapsed,
                last,
            }) => {
                error!(error = %last, attempts, elapsed_ms = elapsed.as_millis() as u64,
                    stream_seq, %invocation_id, %trigger_id, subject, refused_marks,
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
