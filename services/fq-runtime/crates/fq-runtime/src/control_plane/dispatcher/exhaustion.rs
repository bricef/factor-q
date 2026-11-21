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
//! carry: the watch takes this dispatcher's, and for an advisory older than
//! it consults neither the claims nor the message at its sequence, which in
//! this incarnation is another trigger, and dead-letters it naming no
//! trigger (`advisory_watch`). The claim row then names the run:
//!
//! - **No row** ([`Exhausted::NoClaim`]): nothing claims the delivery, so
//!   every copy that claimed it gave the claim back before any start, or
//!   none ever did. Nothing is on record. The watch takes it as it takes
//!   [`Exhausted::NothingOnRecord`]. `dead_letter.requeue`, which asks the
//!   same question about a dead letter that may be months old, tells the
//!   two apart: a row it finds absent may also be one the retention sweep
//!   deleted after its run started (issue 1059).
//! - **A row marked `durably_started`:** a start on record.
//! - **A row marked `consumed`** ([`ConsumedClaim::Checked`]): dealt with,
//!   and no run started, so no dead letter is due (issue 1055). It is not
//!   taken on trust: a start on record for the run it names is a start,
//!   and the claim is marked so (for a claim that records an operator's
//!   drop of the run, only a start in the WAL, since the drop itself writes
//!   the ownership row, whatever the claim's reason). When those
//!   records cannot be read the verdict is held
//!   ([`ConsumedClaim::Unchecked`]), and past the watch's limit no dead
//!   letter is written for it: none is due either way.
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
//! **Held verdicts.** Three cases are neither, and the watch holds its
//! verdict on them, NAKing the advisory to decide again when it comes
//! back:
//!
//! - **The records cannot be read** (the claim store errs, or the worker
//!   cannot say what its WAL holds). A dead letter written now could be
//!   false, and an operator who requeued it would run the work twice;
//!   holding it costs a later verdict, and shows meanwhile as the watch's
//!   consumer redelivering, and in the end as stuck, in `fq status` and
//!   `fq doctor`. A run already read in the WAL is a start even when the
//!   second read of the claim, joined to the ownership row, then errs.
//! - **A copy is still handling the claim.** Nothing is on record yet,
//!   and the run may still start: a dead letter now would be false if it
//!   does, and none at all would lose the trigger if the copy's task then
//!   died. The note ends when the copy's start is marked or its `handle`
//!   returns, and the verdict is taken then. A copy's delivery is renewed
//!   until its start is marked or, if it does not start, until the
//!   delivery is answered, so a copy is still handling a claim when its
//!   deliveries run out only if its renewals stopped before either: a
//!   drain or shutdown let it go while it waited or set up, its renewals
//!   did not land, or its setup blocked its thread. A setup that waits is
//!   kept alive, and its deliveries do not run out.
//! - **The claim changed between the two reads** of it: marked started,
//!   given back, or taken again under another id. It is read once more,
//!   and a start found is a verdict: the claim marked started (a run that
//!   left the WAL is settled by its ownership row and marked so), or a run
//!   the check already read in the WAL. Only with neither is the verdict
//!   held, naming the run the claim names now: after a re-take, the one
//!   that may yet start.
//!
//! A start found is never held: positive evidence wins wherever the check
//! has it.
//!
//! A held verdict does not wait for ever. Once the advisory is
//! `advisory_watch::HELD_VERDICT_LIMIT` old (measured from when the broker
//! emitted it) and the watch has looked at it
//! `advisory_watch::HELD_VERDICT_MIN_LOOKS` times, the watch takes a last
//! look ([`ExhaustionCheck::last_look`]): the verdict again, and the WAL
//! read once for the run the claim names, trusting only a start found,
//! which marks the claim as any start does. If that still decides
//! nothing, it writes the dead letter, marked as one
//! whose start could not be checked and naming the invocation the claim
//! names ([`Exhausted::Undecided`] carries it), so the trigger is never
//! lost silently when its advisory ages out of its stream. The reason on
//! such a dead letter is one of five: the claim could not be read; a copy
//! of the trigger is still handling its claim (and the WAL holds no run of
//! it); the worker could not say whether its WAL holds the run; the claim
//! could not be read with its ownership row; or the claim changed while
//! the run's records were read (and neither a mark nor a run was found).

use std::sync::Arc;

use tracing::error;

use crate::bus::TRIGGER_STREAM_NAME;
use crate::control_plane::store::{ClaimState, ConsumedWhy, RunRecords};
use crate::control_plane::{ControlPlaneStore, ControlPlaneStoreError, TriggerKey};
use crate::worker::{WalRun, Worker};

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
/// on record. Said on the watch's line, and in `dead_letter.requeue`'s
/// refusal, so an operator reading it knows whether a redelivered or
/// replayed copy will meet a marked claim now or only after this daemon's
/// next start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimMark {
    /// It was marked started already, by the copy that recorded the start.
    Already,
    /// This check marked it: it was `claimed` for the run.
    Now,
    /// It was not `claimed` for the run when the mark came (marked by its
    /// copy, given back, or taken again), and the mark left it as it was.
    Changed,
    /// The mark failed, for the reason given; startup recovery marks it.
    Failed(String),
    /// The claim said consumed, and the mark failed, for the reason given.
    /// Startup recovery leaves a consumed claim, so it still says consumed
    /// until a later look marks it.
    ConsumedNotMarked(String),
    /// The claim could not be read, for the reason given, so whether it
    /// needed marking was not known and it was left as it was. It said
    /// `claimed` when last read, and if it still does, startup recovery
    /// marks it.
    NotRead(String),
    /// As [`Self::NotRead`], for a claim that said consumed when last read:
    /// startup recovery leaves a consumed claim.
    ConsumedNotRead(String),
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
            Self::ConsumedNotMarked(err) => write!(
                f,
                "not marked: the mark failed ({err}); the claim still says consumed"
            ),
            Self::NotRead(err) => write!(
                f,
                "not marked: the claim could not be read ({err}); if it still says claimed, this \
                 daemon's next start marks it"
            ),
            Self::ConsumedNotRead(err) => write!(
                f,
                "not marked: the claim could not be read ({err}); if it still says consumed, \
                 startup recovery leaves it so"
            ),
        }
    }
}

/// The verdict on one exhausted trigger: the watch's, before it writes a
/// dead letter, and `dead_letter.requeue`'s, before it re-runs one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exhausted {
    /// No claim names the delivery: every copy that claimed it gave the
    /// claim back before any start, or none ever did, or the retention
    /// sweep has deleted it. Nothing is on record: the watch dead-letters
    /// it.
    NoClaim,
    /// A claim names the delivery and nothing of the trigger's run is on
    /// record: dead-letter it.
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
    /// The claim says the trigger was consumed without its run starting,
    /// and no start found says otherwise (issue 1055).
    Consumed(ConsumedClaim),
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

/// The verdict on an exhausted trigger whose claim says it was consumed
/// without its run starting, when no start found says otherwise (issue
/// 1055): the two verdicts on a consumed claim that are not a start, which
/// each reader answers in a function of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsumedClaim {
    /// No start is on record for the run the claim names: dealt with, so no
    /// dead letter is due from the watch, and the dispatcher's own dead
    /// letter, the one it writes as it consumes a claim as dead-lettered,
    /// can be requeued.
    Checked {
        /// The invocation that consumed it.
        invocation_id: Option<String>,
        /// How it was consumed.
        why: ConsumedWhy,
        /// Whether the claim records an operator's drop of the run, whose
        /// ownership row was then not read as a start.
        operator_dropped: bool,
    },
    /// The records that could contradict the claim could not be read:
    /// decided again later, like [`Exhausted::Undecided`], but past the
    /// watch's limit no dead letter is written for it, since none is due
    /// either way: a run on record started, and one not on record was dealt
    /// with.
    Unchecked {
        /// The invocation the claim names.
        invocation_id: String,
        /// How the claim says the trigger was consumed.
        why: ConsumedWhy,
        /// Whether the claim records an operator's drop of the run.
        operator_dropped: bool,
        /// Which record could not be read, and why.
        unread: String,
    },
}

impl ExhaustionCheck {
    /// The claim table's key for the trigger at `stream_seq`.
    fn key(&self, stream_seq: u64) -> TriggerKey<'static> {
        TriggerKey {
            stream: TRIGGER_STREAM_NAME,
            stream_epoch: self.stream_epoch,
            stream_seq,
        }
    }

    /// When the incarnation of the trigger stream whose claims this check
    /// reads was created, in nanoseconds since the Unix epoch
    /// ([`crate::bus::EventBus::trigger_stream_epoch`]). A record written
    /// before it names a sequence of an earlier stream, and this
    /// incarnation's claim at that sequence is another trigger's.
    pub fn stream_epoch(&self) -> i64 {
        self.stream_epoch
    }

    /// The verdict on the trigger at `stream_seq` of the stream this
    /// dispatcher consumes.
    pub(crate) async fn verdict(&self, stream_seq: u64) -> Exhausted {
        let key = self.key(stream_seq);
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
            return Exhausted::NoClaim;
        };
        // A claim marked started is a start on record, whatever else the
        // row holds.
        match row.state {
            ClaimState::DurablyStarted => {
                return Exhausted::Started {
                    invocation_id: row
                        .invocation_id
                        .unwrap_or_else(|| "none named on the claim".to_string()),
                    on_record: "on its claim, marked durably started".to_string(),
                    mark: ClaimMark::Already,
                };
            }
            ClaimState::Consumed { why } => {
                let dropped = row.operator_dropped_at.is_some();
                return self
                    .settle_consumed(key, row.invocation_id, why, dropped)
                    .await;
            }
            ClaimState::Claimed => {}
        }
        // A claim written before claims named their run cannot be looked
        // up, and is dead-lettered as it always was.
        let Some(invocation_id) = row.invocation_id else {
            return Exhausted::NothingOnRecord;
        };
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
            // The WAL holds the run: a start on record, whatever this read
            // could not say.
            Err(_) if wal.is_some() => {
                return self
                    .started(key, invocation_id, RunRecords::in_the_wal(wal))
                    .await;
            }
            Err(err) => {
                return Exhausted::Undecided {
                    why: format!(
                        "the trigger's claim could not be read with its ownership row: {err}"
                    ),
                    invocation_id: Some(invocation_id),
                };
            }
        };
        // Marked, given back or taken again between the two reads.
        let Some(unsettled) =
            unsettled.filter(|u| u.claim.invocation_id.as_deref() == Some(invocation_id.as_str()))
        else {
            return self.after_the_claim_changed(key, invocation_id, wal).await;
        };
        let records = RunRecords {
            wal,
            owner_row_status: unsettled.owner_row_status,
        };
        if !records.start_on_record() {
            return Exhausted::NothingOnRecord;
        }
        self.started(key, invocation_id, records).await
    }

    /// The verdict on a run with a start on record, the claim marked if it
    /// is still `claimed` for the run. The mark is a compare-and-set on the
    /// state and the id, so it leaves a claim marked, given back or taken
    /// again alone, and it is safe while a copy still handles the claim:
    /// that copy's own mark matches a claim naming its run in either state,
    /// so it finds the row this mark left started for it. A failed mark
    /// does not change the verdict: the trigger is not dead-lettered either
    /// way, and the watch's line says so.
    async fn started(
        &self,
        key: TriggerKey<'_>,
        invocation_id: String,
        records: RunRecords,
    ) -> Exhausted {
        let on_record = records.where_on_record().unwrap_or_default();
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

    /// The claim stopped being `claimed` for `invocation_id` between the
    /// two reads: a copy marked it started (it settled the claim by the
    /// record, the ownership row included once the run left the WAL, or the
    /// first copy's own mark landed), gave it back, or took it again under
    /// another id. It is read once more. Marked started is a start on
    /// record, and so is a run already read in the WAL: held instead, either
    /// would become, past the watch's limit, a dead letter marked unchecked
    /// for a run that started. Consumed since is settled as a consumed claim
    /// is, unless the run read in the WAL is not the one the claim names
    /// now: that run started, and a start read is never dropped for another
    /// run's answer. With neither, the verdict is held, naming the run the
    /// claim names now: after a re-take, that is the run that may yet
    /// start, and the one an operator has to look at.
    async fn after_the_claim_changed(
        &self,
        key: TriggerKey<'_>,
        invocation_id: String,
        wal: Option<WalRun>,
    ) -> Exhausted {
        let now = self.store.trigger_claim(key).await;
        if let Ok(Some(row)) = &now {
            // Consumed since, the claim is settled as consumed, unless the
            // run read in the WAL is not the one it names: that run started,
            // and a claim consumed under another says nothing of it.
            let settle =
                wal.is_none() || row.invocation_id.as_deref() == Some(invocation_id.as_str());
            match &row.state {
                ClaimState::DurablyStarted => {
                    return Exhausted::Started {
                        invocation_id: row.invocation_id.clone().unwrap_or(invocation_id),
                        on_record: "on its claim, marked durably started".to_string(),
                        mark: ClaimMark::Already,
                    };
                }
                ClaimState::Consumed { why } if settle => {
                    let named = row.invocation_id.clone();
                    let dropped = row.operator_dropped_at.is_some();
                    return self.settle_consumed(key, named, why.clone(), dropped).await;
                }
                ClaimState::Consumed { .. } | ClaimState::Claimed => {}
            }
        }
        if wal.is_some() {
            return self
                .started(key, invocation_id, RunRecords::in_the_wal(wal))
                .await;
        }
        let invocation_id = match now {
            Ok(Some(row)) => row.invocation_id.unwrap_or(invocation_id),
            Ok(None) | Err(_) => invocation_id,
        };
        Exhausted::Undecided {
            why: "the trigger's claim changed while its run's records were read".to_string(),
            invocation_id: Some(invocation_id),
        }
    }

    /// The watch's last look, past its limit, before it writes a dead
    /// letter it could not check: the verdict taken again and, if it still
    /// cannot be taken, the worker's WAL read once for the run the claim
    /// names. Only a start found counts. An empty read is never taken for
    /// "not started", for the reason `verdict` asks about a copy
    /// still handling the claim before it reads the records: that copy can
    /// make its first WAL write just after the read. A start found here
    /// marks the claim as any other does.
    ///
    /// `dead_letter.requeue` takes this look too: it acts on its answer at
    /// once, as the watch does past its limit, with no later look to wait
    /// for.
    pub async fn last_look(&self, stream_seq: u64) -> Exhausted {
        match self.verdict(stream_seq).await {
            Exhausted::Undecided {
                why,
                invocation_id: Some(invocation_id),
            } => match self.worker.wal_run(&invocation_id).await {
                Ok(Some(wal)) => {
                    let key = self.key(stream_seq);
                    let unread = ClaimMark::NotRead;
                    self.started_in_the_wal(key, invocation_id, wal, unread)
                        .await
                }
                Ok(None) | Err(_) => Exhausted::Undecided {
                    why,
                    invocation_id: Some(invocation_id),
                },
            },
            // A consumed claim's run, looked for once more in the WAL.
            Exhausted::Consumed(ConsumedClaim::Unchecked {
                invocation_id,
                why,
                operator_dropped,
                unread,
            }) => match self.worker.wal_run(&invocation_id).await {
                Ok(Some(wal)) => {
                    let key = self.key(stream_seq);
                    let unread = ClaimMark::ConsumedNotRead;
                    self.started_in_the_wal(key, invocation_id, wal, unread)
                        .await
                }
                Ok(None) | Err(_) => Exhausted::Consumed(ConsumedClaim::Unchecked {
                    invocation_id,
                    why,
                    operator_dropped,
                    unread,
                }),
            },
            verdict => verdict,
        }
    }

    /// Where the run `invocation_id` has a start on record, looked up by
    /// its own id rather than through a claim, or `None` when no record
    /// shows one. `dead_letter.requeue` asks it about the run a dead
    /// letter's `start_unchecked` mark names when no claim answers (issue
    /// 1059). The worker's WAL is read first and then the ownership row,
    /// as every reader of [`RunRecords`] reads them, so an archive landing
    /// between the two reads cannot hide the run. As in the last look, only
    /// a start found counts: a read that fails says nothing. The ownership
    /// row counts by its presence, so one whose status this build cannot
    /// read is a start all the same. Nothing is marked: no claim was read.
    pub async fn start_of_run(&self, invocation_id: &str) -> Option<String> {
        let wal = self.worker.wal_run(invocation_id).await.ok().flatten();
        let owner_row_status = match self.store.get_invocation_owner(invocation_id).await {
            Ok(row) => row.map(|row| row.status.as_str().to_string()),
            // Only a row it fetched and could not parse makes this read
            // `Malformed`: the row is there.
            Err(ControlPlaneStoreError::Malformed(_)) => {
                Some("a status this build cannot read".to_string())
            }
            Err(_) => None,
        };
        RunRecords {
            wal,
            owner_row_status,
        }
        .where_on_record()
    }

    /// A `consumed` claim, not taken on trust (issue 1055): it says no run
    /// started, and a start on record for the run it names is the truth.
    /// The run is looked up as a `claimed` row's is, the WAL first and then
    /// the ownership row, and a read that fails holds the verdict, since a
    /// requeue that took "consumed" on trust could run the work twice. A
    /// start found is [`Exhausted::Started`], logged at error, with the
    /// claim marked started for the run. On a claim that records an
    /// operator's drop of the run (`operator_dropped`) the ownership row is
    /// the drop's and is not read: only the WAL can show a start.
    async fn settle_consumed(
        &self,
        key: TriggerKey<'_>,
        invocation_id: Option<String>,
        why: ConsumedWhy,
        operator_dropped: bool,
    ) -> Exhausted {
        let Some(invocation_id) = invocation_id else {
            return Exhausted::Consumed(ConsumedClaim::Checked {
                invocation_id,
                why,
                operator_dropped,
            });
        };
        let held = |record: &str, err: String| {
            Exhausted::Consumed(ConsumedClaim::Unchecked {
                invocation_id: invocation_id.clone(),
                why: why.clone(),
                operator_dropped,
                unread: format!("the consumed trigger's {record} could not be read: {err}"),
            })
        };
        let wal = match self.worker.wal_run(&invocation_id).await {
            Ok(wal) => wal,
            Err(err) => return held("run in the worker's WAL", err.to_string()),
        };
        // An operator's drop of a run that never made its first WAL write
        // writes the run's ownership row, so on a claim that records the
        // drop that row is the drop's, not a start, whatever the claim's
        // reason: only a start in the WAL says otherwise.
        let owner_row_status = match self.store.get_invocation_owner(&invocation_id).await {
            _ if operator_dropped => None,
            Ok(row) => row.map(|row| row.status.as_str().to_string()),
            Err(ControlPlaneStoreError::Malformed(_)) => {
                Some("a status this build cannot read".into())
            }
            // The WAL holds the run: a start, whatever this read could not say.
            Err(_) if wal.is_some() => None,
            Err(err) => return held("ownership row", err.to_string()),
        };
        let records = RunRecords {
            wal,
            owner_row_status,
        };
        let Some(on_record) = records.where_on_record() else {
            let invocation_id = Some(invocation_id);
            return Exhausted::Consumed(ConsumedClaim::Checked {
                invocation_id,
                why,
                operator_dropped,
            });
        };
        self.consumed_started(key, invocation_id, why, on_record)
            .await
    }

    /// A start on record for the run a `consumed` claim names: a start,
    /// with the claim marked started for the run and an error line, since
    /// the claim said otherwise. [`Self::settle_consumed`] comes here, and
    /// so does the last look when it finds the run in the WAL.
    async fn consumed_started(
        &self,
        key: TriggerKey<'_>,
        invocation_id: String,
        why: ConsumedWhy,
        on_record: String,
    ) -> Exhausted {
        let mark = match self
            .store
            .mark_consumed_trigger_started(key, &invocation_id)
            .await
        {
            Ok(true) => ClaimMark::Now,
            Ok(false) => ClaimMark::Changed,
            Err(err) => ClaimMark::ConsumedNotMarked(err.to_string()),
        };
        error!(stream_seq = key.stream_seq, %invocation_id, consumed_why = %why, %on_record,
            %mark, "the claim of an exhausted trigger said it was consumed without a run, and \
            its run has a start on record; taken as started, so it is neither dead-lettered \
            nor requeued");
        Exhausted::Started {
            invocation_id,
            on_record,
            mark,
        }
    }

    /// The last look's start found in the WAL, the claim marked as it
    /// stands now: a `consumed` claim naming the run is corrected as
    /// [`Self::settle_consumed`] corrects it, and any other is marked as
    /// [`Self::started`] marks a `claimed` one. A claim that cannot be read
    /// is left, with the mark `unread` makes, which says what becomes of a
    /// claim in the state the verdict last read.
    async fn started_in_the_wal(
        &self,
        key: TriggerKey<'_>,
        invocation_id: String,
        wal: WalRun,
        unread: fn(String) -> ClaimMark,
    ) -> Exhausted {
        let records = RunRecords::in_the_wal(Some(wal));
        let row = match self.store.trigger_claim(key).await {
            Ok(row) => row,
            // Which mark the claim needs cannot be told: the start is the
            // verdict all the same, and the claim is left for a later look.
            Err(err) => {
                let on_record = records.where_on_record().unwrap_or_default();
                let mark = unread(err.to_string());
                return Exhausted::Started {
                    invocation_id,
                    on_record,
                    mark,
                };
            }
        };
        if let Some(row) = row
            && let ClaimState::Consumed { why } = row.state
            && row.invocation_id.as_deref() == Some(invocation_id.as_str())
        {
            let on_record = records.where_on_record().unwrap_or_default();
            return self
                .consumed_started(key, invocation_id, why, on_record)
                .await;
        }
        self.started(key, invocation_id, records).await
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
