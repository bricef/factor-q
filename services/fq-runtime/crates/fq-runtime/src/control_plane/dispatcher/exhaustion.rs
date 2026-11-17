//! What the dead-letter watch asks before it calls a trigger exhausted
//! (issue 967).
//!
//! The broker reports a trigger exhausted when its last delivery's answer
//! never reached it. That happens to a trigger whose run started too: the
//! last delivery started the run and its task died, or its mark landed or
//! gave up and its ack was lost, or it was a copy of a run started earlier
//! and its ack was lost. After the last delivery no copy comes back for
//! the dispatcher's own-claim arm to settle, so the watch decides by the
//! records instead, and a `trigger_exhausted` dead letter is written only
//! when nothing of the trigger's run is on record.
//!
//! **From the advisory to the records.** The broker's MAX_DELIVERIES
//! advisory names the stream and the message's sequence. The claim table
//! keys on those and the stream's incarnation, which the advisory does not
//! carry: the watch takes this dispatcher's, and does not consult the
//! claims at all for an advisory older than it (`advisory_watch`). The
//! claim row then names the run:
//!
//! - **No row:** nothing claims the delivery, so every copy that claimed
//!   it gave the claim back before any start, or none ever did. Nothing is
//!   on record.
//! - **A row marked `durably_started`:** a start on record.
//! - **A `claimed` row:** first, whether a copy of the trigger is still
//!   handling it ([`Handling`]): if so, the run may still start, and the
//!   verdict waits for the copy to end. Then the records, by the one
//!   definition recovery, `fq doctor` and the own-claim arm use
//!   ([`RunRecords`]): the worker's WAL first, through
//!   [`Worker::wal_run`], then the ownership row, so a run archived between
//!   the two reads is still found. A start on record marks the claim, as
//!   recovery would at the next start.
//!
//! The note is asked about before the records are read, for the reason it
//! is in `own_claim`: read the other way round, the WAL could be found
//! empty just before a copy makes its first WAL write and dies, and a run
//! that started would be dead-lettered.
//!
//! **Held verdicts.** Two cases are neither, and the watch holds its
//! verdict on them, NAKing the advisory to decide again when it comes
//! back:
//!
//! - **The records cannot be read** (the claim store errs, or the worker
//!   cannot say what its WAL holds). A dead letter written now could be
//!   false, and an operator who requeued it would run the work twice;
//!   holding it costs a later verdict, and shows meanwhile as the watch's
//!   consumer redelivering, and in the end as stuck, in `fq status` and
//!   `fq doctor`.
//! - **A copy is still handling the claim.** Nothing is on record yet,
//!   and the run may still start: a dead letter now would be false if it
//!   does, and none at all would lose the trigger if the copy's task then
//!   died. The note ends when the copy's start is marked or its `handle`
//!   returns, and the verdict is taken then.

use std::sync::Arc;

use tracing::warn;

use crate::bus::TRIGGER_STREAM_NAME;
use crate::control_plane::store::RunRecords;
use crate::control_plane::{ControlPlaneStore, TriggerKey};
use crate::worker::Worker;

use super::TriggerDispatcher;
use super::own_claim::Handling;

/// What the dead-letter watch consults about a trigger whose deliveries
/// ran out: the dispatcher's claim store, the worker that answers for its
/// WAL, and the copies of triggers the dispatcher is handling.
#[derive(Clone)]
pub struct ExhaustionCheck {
    store: Arc<ControlPlaneStore>,
    worker: Arc<dyn Worker>,
    handling: Handling,
    stream_epoch: i64,
}

/// The watch's verdict on one exhausted trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Exhausted {
    /// Nothing of the trigger's run is on record: dead-letter it.
    NothingOnRecord,
    /// The trigger's run has a start on record: not dead-lettered.
    Started {
        /// The invocation the claim names.
        invocation_id: String,
        /// Where the start is on record, for the log line.
        on_record: String,
        /// Whether this check marked the claim, which was `claimed`.
        marked: bool,
    },
    /// Not decided yet, for the reason given: decide again later.
    Undecided(String),
}

impl ExhaustionCheck {
    /// The verdict on the trigger at `stream_seq` of the stream this
    /// dispatcher consumes.
    pub(crate) async fn verdict(&self, stream_seq: u64) -> Exhausted {
        let key = TriggerKey {
            stream: TRIGGER_STREAM_NAME,
            stream_epoch: self.stream_epoch,
            stream_seq,
        };
        let row = match self.store.trigger_claim(key).await {
            Ok(row) => row,
            Err(err) => {
                return Exhausted::Undecided(format!(
                    "the trigger's claim could not be read: {err}"
                ));
            }
        };
        let Some(row) = row else {
            return Exhausted::NothingOnRecord;
        };
        // A claim written before claims named their run cannot be looked
        // up, and is dead-lettered as it always was.
        let Some(invocation_id) = row.invocation_id else {
            return Exhausted::NothingOnRecord;
        };
        if row.durably_started {
            return Exhausted::Started {
                invocation_id,
                on_record: "on its claim, marked durably started".to_string(),
                marked: false,
            };
        }
        if self.handling.handles(&invocation_id) {
            return Exhausted::Undecided(format!(
                "a copy of the trigger is still handling its claim, for invocation {invocation_id}"
            ));
        }
        self.settle_by_the_records(key, invocation_id).await
    }

    /// A `claimed` row no copy is handling: its run's records, the WAL first,
    /// and the claim marked if they hold a start.
    async fn settle_by_the_records(&self, key: TriggerKey<'_>, invocation_id: String) -> Exhausted {
        let wal = match self.worker.wal_run(&invocation_id).await {
            Ok(wal) => wal,
            Err(err) => {
                return Exhausted::Undecided(format!(
                    "the worker could not say whether its WAL holds invocation {invocation_id}: {err}"
                ));
            }
        };
        let unsettled = match self.store.unsettled_trigger_claim(key).await {
            Ok(unsettled) => unsettled,
            Err(err) => {
                return Exhausted::Undecided(format!(
                    "the trigger's claim could not be read with its ownership row: {err}"
                ));
            }
        };
        // Marked or given back between the two reads: the next read decides.
        let Some(unsettled) =
            unsettled.filter(|u| u.claim.invocation_id.as_deref() == Some(invocation_id.as_str()))
        else {
            return Exhausted::Undecided(
                "the trigger's claim changed while its run's records were read".to_string(),
            );
        };
        let records = RunRecords {
            wal,
            owner_row_status: unsettled.owner_row_status,
        };
        if !records.start_on_record() {
            return Exhausted::NothingOnRecord;
        }
        let on_record = records.where_on_record().unwrap_or_default();
        let marked = match self
            .store
            .mark_unsettled_trigger_started(key, &invocation_id)
            .await
        {
            Ok(marked) => marked,
            Err(err) => {
                warn!(error = %err, stream_seq = key.stream_seq, %invocation_id,
                    "could not mark the claim of an exhausted trigger whose run has a start on \
                     record; it is not dead-lettered either way, and the next start's recovery \
                     marks the claim");
                false
            }
        };
        Exhausted::Started {
            invocation_id,
            on_record,
            marked,
        }
    }
}

impl TriggerDispatcher {
    /// What the dead-letter watch consults before it calls one of this
    /// dispatcher's triggers exhausted, or `None` when the dispatcher
    /// holds no claim store, and so no records to consult.
    pub fn exhaustion_check(&self) -> Option<ExhaustionCheck> {
        let (store, _) = self.claim_store.as_ref()?;
        Some(ExhaustionCheck {
            store: Arc::clone(store),
            worker: Arc::clone(&self.worker),
            handling: self.handling.clone(),
            stream_epoch: self.bus.trigger_stream_epoch(),
        })
    }
}
