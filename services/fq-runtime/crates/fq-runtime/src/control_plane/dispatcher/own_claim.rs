//! A copy of a trigger that meets a claim this daemon holds itself, settled
//! by what is on record (issue 967).
//!
//! A `claimed` row under this daemon's own claimant id was written by this
//! process: the id is minted once per daemon start, and a row another
//! process left is adopted in `claim_delivery` instead. A copy that meets
//! one is in one of three cases, and this module tells them apart.
//!
//! 1. **A copy of the trigger is still handling the claim**: waiting in a
//!    hold or for a worker permit, setting up, or marking its start. That
//!    copy owns the delivery, and an ack or a NAK from this one would
//!    settle it underneath, so this copy is dropped unanswered, logged and
//!    counted (`held_duplicate_dropped`). The holds and the setup keep the
//!    first copy's delivery alive, so a copy comes only when those
//!    in-progress acks stopped landing for a whole ack window.
//! 2. **No copy is handling it, and the run has a start on record**, in the
//!    worker's WAL or on the control plane's ownership table, by the one
//!    definition ([`RunRecords`]) startup recovery and `fq doctor` read
//!    too. The run's mark was refused past its retries and the ack sent in
//!    its place was lost; or its task died in the very poll that made its
//!    first WAL write; or an operator dropped it. The copy is acked and the
//!    claim marked `durably_started`, as the next start's recovery would
//!    mark it, and the trigger is not run again. It is counted with the
//!    copies of runs that started (`duplicate_dropped`).
//! 3. **No copy is handling it, and nothing is on record**: the copy that
//!    took it ended before its run's first WAL write and left the delivery
//!    unsettled, because its task died, or because the claim's release
//!    before its NAK failed. The copy takes the claim again under its own
//!    invocation id, by a compare-and-set on the abandoned one, and runs
//!    the trigger, once. Logged at `warn`, once, and counted
//!    (`abandoned_claim_retaken`). A claim a drain or a shutdown left for
//!    the next binary is this case too, if a copy reaches this process
//!    before it exits; that copy then stops at `handle`'s own drain check,
//!    after the re-take, and leaves the claim to the next binary again.
//!
//! **Being handled.** Every copy notes the invocation id it will run under
//! in [`Handling`] before it writes that id on a claim (a fresh claim, an
//! adoption, a re-take), and the note is taken out when `handle` returns
//! or unwinds from a panic. A row naming a noted id is case 1. A row naming
//! an id that is not noted was written by a copy whose `handle` has ended;
//! that copy polled its run, so a run that had not made its first WAL write
//! by then never will.
//!
//! **Reading the records.** The WAL first, through the worker
//! ([`crate::worker::Worker::wal_run`]), then the ownership row, joined to
//! the claim as recovery reads it: the archive hand-off writes the
//! ownership row before it deletes the WAL row, so a run archived between
//! the two reads is still found. A record that cannot be read is never
//! taken for "nothing on record": the worker cannot say, or the claim store
//! errs, and the copy is dropped, as every copy that met its own claim was
//! before this, for a later copy to ask again.
//!
//! **The trap.** A run whose task panicked in the poll that made its first
//! WAL write has no copy handling it, and `handle` never saw it start, yet
//! the WAL holds it in flight. That is case 2, not case 3: taking the claim
//! again would start the trigger over a run that started. The run stays in
//! flight in the WAL, with nothing driving it, until this daemon next starts
//! and its recovery resumes it. `fq invocation resume` does not reach it
//! sooner: it takes only a run recovery found ambiguous.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};

use tracing::{error, warn};

use crate::bus::TRIGGER_MAX_DELIVER;
use crate::control_plane::store::RunRecords;
use crate::control_plane::{ControlPlaneStore, TriggerKey};
use crate::worker::WalRun;

use super::claim::ClaimVerdict;
use super::{CONSUMER_NAME, TriggerDispatcher};

/// The invocation ids this dispatcher's copies of triggers are handling:
/// noted before a copy writes its id on a claim, and taken out when its
/// `handle` returns or unwinds.
#[derive(Debug, Clone, Default)]
pub(super) struct Handling(Arc<Mutex<HashSet<String>>>);

impl Handling {
    /// Note that a copy is handling `invocation_id`, until the returned
    /// note drops.
    pub(super) fn begin(&self, invocation_id: &str) -> Handled {
        self.lock().insert(invocation_id.to_string());
        Handled {
            handling: self.clone(),
            invocation_id: invocation_id.to_string(),
        }
    }

    fn handles(&self, invocation_id: &str) -> bool {
        self.lock().contains(invocation_id)
    }

    fn lock(&self) -> MutexGuard<'_, HashSet<String>> {
        // Poisoned only by a panic while it is held, and nothing that can
        // panic runs under it: the set is still the set.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A copy's note in [`Handling`]. `handle` holds it to its end, and
/// dropping it, a panic's unwinding included, takes the note out.
pub(super) struct Handled {
    handling: Handling,
    invocation_id: String,
}

impl Drop for Handled {
    fn drop(&mut self) {
        self.handling.lock().remove(&self.invocation_id);
    }
}

/// Why a copy is dropped when the claim changed between its reads and its
/// write: another copy, which met the same abandoned claim, got there
/// first, and settles the delivery or runs the trigger itself.
const RACED: &str = "another copy settled or took the claim first";

/// What a copy found when its claim met this daemon's own `claimed` row.
pub(super) struct OwnClaim<'a> {
    /// This daemon's claimant id.
    pub(super) worker_id: &'a str,
    pub(super) seq: u64,
    pub(super) delivered: i64,
    pub(super) now: i64,
    /// The invocation id this copy runs under if it takes the claim again.
    pub(super) named: &'a str,
    /// The invocation id the row names.
    pub(super) claimed_for: Option<String>,
    pub(super) claimed_at: i64,
}

impl TriggerDispatcher {
    /// Settle a copy that met a claim this daemon holds with no start on
    /// it, by the three cases the module sets out. `proceed` is what this
    /// copy carries on with if it takes the claim again.
    pub(super) async fn settle_own_claim(
        &self,
        msg: &async_nats::jetstream::Message,
        store: &ControlPlaneStore,
        met: OwnClaim<'_>,
        proceed: ClaimVerdict,
    ) -> ClaimVerdict {
        // Every row this process writes names an id (#959), so a row naming
        // none is not one this build wrote, and is left as it is.
        let Some(abandoned) = met.claimed_for.as_deref() else {
            self.drop_held(msg, &met, None, "the claim names no invocation to look up");
            return ClaimVerdict::Stop;
        };
        if self.handling.handles(abandoned) {
            self.drop_held(msg, &met, None, "a copy of it is still handling the claim");
            return ClaimVerdict::Stop;
        }
        let key = self.trigger_key(met.seq);
        let records = match self.records_of(store, key, &met, abandoned).await {
            Ok(Some(records)) => records,
            Ok(None) => {
                self.drop_held(msg, &met, None, RACED);
                return ClaimVerdict::Stop;
            }
            Err(err) => {
                let why = "whether its run has a start on record could not be read, and a \
                           later copy asks again";
                self.drop_held(msg, &met, Some(&err), why);
                return ClaimVerdict::Stop;
            }
        };
        if records.start_on_record() {
            self.settle_started(msg, store, key, &met, abandoned, &records)
                .await;
            return ClaimVerdict::Stop;
        }
        match store
            .retake_trigger_claim(key, met.worker_id, abandoned, met.named, met.now)
            .await
        {
            Ok(true) => {
                warn!(stream_seq = met.seq, delivered = met.delivered, subject = %msg.subject,
                    claim_age_ms = met.now.saturating_sub(met.claimed_at),
                    abandoned_invocation_id = %abandoned, invocation_id = %met.named,
                    "taking again a trigger claim this daemon holds that no copy is handling and \
                     whose run has no start on record, and running the trigger under a new \
                     invocation id: the copy that took it ended before its run's first WAL write \
                     and left the delivery unsettled (its task died, the claim's release before its \
                     NAK failed, or a drain or a shutdown left it for the next binary)");
                self.bus
                    .consumer_ledger()
                    .note_abandoned_claim_retaken(CONSUMER_NAME);
                proceed
            }
            Ok(false) => {
                self.drop_held(msg, &met, None, RACED);
                ClaimVerdict::Stop
            }
            Err(err) => {
                let why = "it could not be taken again, and a later copy asks again";
                self.drop_held(msg, &met, Some(&err.to_string()), why);
                ClaimVerdict::Stop
            }
        }
    }

    /// Where the run the abandoned claim names is on record: the WAL first,
    /// then the ownership row, read with the claim. `Ok(None)` when the
    /// claim no longer reads `claimed` by this daemon for that run.
    async fn records_of(
        &self,
        store: &ControlPlaneStore,
        key: TriggerKey<'_>,
        met: &OwnClaim<'_>,
        abandoned: &str,
    ) -> Result<Option<RunRecords>, String> {
        let wal = self
            .worker
            .wal_run(abandoned)
            .await
            .map_err(|err| err.to_string())?;
        let Some(row) = store
            .unsettled_trigger_claim(key)
            .await
            .map_err(|err| err.to_string())?
        else {
            return Ok(None);
        };
        if row.claim.claimant != met.worker_id
            || row.claim.invocation_id.as_deref() != Some(abandoned)
        {
            return Ok(None);
        }
        Ok(Some(RunRecords {
            wal,
            owner_row_status: row.owner_row_status,
        }))
    }

    /// Case 2: ack the copy and mark the claim. The ack keeps the claim
    /// whether or not the mark lands, as `mark_durable_started`'s does: the
    /// start is on record, so the trigger's work is done, and a released
    /// claim would let a later copy start it again.
    async fn settle_started(
        &self,
        msg: &async_nats::jetstream::Message,
        store: &ControlPlaneStore,
        key: TriggerKey<'_>,
        met: &OwnClaim<'_>,
        abandoned: &str,
        records: &RunRecords,
    ) {
        let run = records.where_on_record().unwrap_or_default();
        let undriven = if records.wal == Some(WalRun::InFlight) {
            "; if no task is driving it, which is so when its task died after its first WAL \
             write, it stays in flight until this daemon next starts and its recovery resumes \
             it"
        } else {
            ""
        };
        match store.mark_unsettled_trigger_started(key, abandoned).await {
            Ok(marked) => {
                warn!(stream_seq = met.seq, delivered = met.delivered, subject = %msg.subject,
                    claim_age_ms = met.now.saturating_sub(met.claimed_at),
                    claim_invocation_id = %abandoned, run = %run, marked,
                    "acking a redelivered trigger whose claim this daemon holds with no start \
                     recorded, and marking the claim: no copy is handling it and its run has a \
                     start on record, so the trigger is not run again{undriven}");
            }
            Err(err) => {
                error!(error = %err, stream_seq = met.seq, delivered = met.delivered,
                    subject = %msg.subject, claim_invocation_id = %abandoned, run = %run,
                    "could not mark the claim of a redelivered trigger whose run has a start on \
                     record. Acking the delivery anyway and keeping its claim, so the trigger is \
                     not run again; the next start's recovery marks it{undriven}");
            }
        }
        self.bus
            .consumer_ledger()
            .note_duplicate_drop(CONSUMER_NAME);
        self.ack_keeping_claim(msg, None, "a start on record, its claim this daemon's own")
            .await;
    }

    /// Drop the copy unanswered, as one of a claim in hand, saying why.
    /// The subject names the agent, which is the only handle on a claim
    /// whose invocation never wrote a WAL row besides the sequence. On the
    /// last delivery the line says so: no copy follows, which is true
    /// whatever the claim is waiting on.
    fn drop_held(
        &self,
        msg: &async_nats::jetstream::Message,
        met: &OwnClaim<'_>,
        error: Option<&str>,
        why: &str,
    ) {
        let last = if met.delivered >= TRIGGER_MAX_DELIVER {
            "; this was its last delivery, so no copy follows, and unless the delivery is acked \
             before its window lapses the trigger is dead-lettered as `trigger_exhausted`"
        } else {
            ""
        };
        warn!(stream_seq = met.seq, delivered = met.delivered, subject = %msg.subject,
            claimant = %met.worker_id, claim_age_ms = met.now.saturating_sub(met.claimed_at),
            claim_invocation_id = %met.claimed_for.as_deref().unwrap_or("none"),
            error = %error.unwrap_or("none"),
            "dropping a redelivered trigger whose claim this daemon already holds with no start \
             recorded, and leaving it unacked: {why}{last}");
        self.bus
            .consumer_ledger()
            .note_held_duplicate_drop(CONSUMER_NAME);
    }
}
