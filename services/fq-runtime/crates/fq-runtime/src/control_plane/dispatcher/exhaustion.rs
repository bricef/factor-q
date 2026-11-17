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
//!   verdict waits for the copy to end, or for the watch's limit (below).
//!   Then the records, by the one
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
//!   returns, and the verdict is taken then. A copy is still handling a
//!   claim when its deliveries run out only if its keepalive stopped before
//!   its start was marked: a drain or shutdown let it go, its keepalives
//!   did not land, or its setup blocked its thread. A setup that waits is
//!   kept alive, and its deliveries do not run out.
//!
//! A held verdict does not wait for ever. Past the watch's limit
//! (`advisory_watch::HELD_VERDICT_LIMIT`, measured from when the broker
//! emitted the advisory) the watch writes the dead letter anyway, marked
//! as one whose start could not be checked and naming the invocation the
//! claim names ([`Exhausted::Undecided`] carries it), so the trigger is
//! never lost silently when its advisory ages out of its stream.

use std::sync::Arc;

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

/// What became of the claim of an exhausted trigger whose run has a start
/// on record. Said on the watch's line, so an operator reading it knows
/// whether a redelivered or replayed copy will meet a marked claim now or
/// only after this daemon's next start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClaimMark {
    /// It was marked started already, by the copy that recorded the start.
    Already,
    /// This check marked it: it was `claimed`.
    Now,
    /// It changed between the read and the mark (marked by its copy, or
    /// given back), and the mark left it as it was.
    Changed,
    /// The mark failed, for the reason given; startup recovery marks it.
    Failed(String),
}

impl std::fmt::Display for ClaimMark {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Already => f.write_str("already marked started"),
            Self::Now => f.write_str("marked started now"),
            Self::Changed => f.write_str("not marked: the claim changed before the mark"),
            Self::Failed(err) => write!(
                f,
                "not marked: the mark failed ({err}); this daemon's next start marks it"
            ),
        }
    }
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
        /// What became of the claim, for the log line.
        mark: ClaimMark,
    },
    /// Not decided yet: decide again later.
    Undecided {
        /// Why the verdict cannot be taken now.
        why: String,
        /// The invocation the claim names, when the claim was read: where
        /// to look if the verdict is never taken and the trigger is
        /// dead-lettered unchecked.
        invocation_id: Option<String>,
    },
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
                return Exhausted::Undecided {
                    why: format!("the trigger's claim could not be read: {err}"),
                    invocation_id: None,
                };
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
                mark: ClaimMark::Already,
            };
        }
        if self.handling.handles(&invocation_id) {
            return Exhausted::Undecided {
                why: "a copy of the trigger is still handling its claim".to_string(),
                invocation_id: Some(invocation_id),
            };
        }
        self.settle_by_the_records(key, invocation_id).await
    }

    /// A `claimed` row no copy is handling: its run's records, the WAL first,
    /// and the claim marked if they hold a start.
    async fn settle_by_the_records(&self, key: TriggerKey<'_>, invocation_id: String) -> Exhausted {
        let wal = match self.worker.wal_run(&invocation_id).await {
            Ok(wal) => wal,
            Err(err) => {
                return Exhausted::Undecided {
                    why: format!("the worker could not say whether its WAL holds the run: {err}"),
                    invocation_id: Some(invocation_id),
                };
            }
        };
        let unsettled = match self.store.unsettled_trigger_claim(key).await {
            Ok(unsettled) => unsettled,
            Err(err) => {
                return Exhausted::Undecided {
                    why: format!(
                        "the trigger's claim could not be read with its ownership row: {err}"
                    ),
                    invocation_id: Some(invocation_id),
                };
            }
        };
        // Marked or given back between the two reads: the next read decides.
        let Some(unsettled) =
            unsettled.filter(|u| u.claim.invocation_id.as_deref() == Some(invocation_id.as_str()))
        else {
            return Exhausted::Undecided {
                why: "the trigger's claim changed while its run's records were read".to_string(),
                invocation_id: Some(invocation_id),
            };
        };
        let records = RunRecords {
            wal,
            owner_row_status: unsettled.owner_row_status,
        };
        if !records.start_on_record() {
            return Exhausted::NothingOnRecord;
        }
        let on_record = records.where_on_record().unwrap_or_default();
        // A failed mark does not change the verdict: the trigger is not
        // dead-lettered either way, and the watch's line says so.
        let mark = match self
            .store
            .mark_unsettled_trigger_started(key, &invocation_id)
            .await
        {
            Ok(true) => ClaimMark::Now,
            Ok(false) => ClaimMark::Changed,
            Err(err) => ClaimMark::Failed(err.to_string()),
        };
        Exhausted::Started {
            invocation_id,
            on_record,
            mark,
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
