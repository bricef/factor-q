//! Durable arbitration for trigger redeliveries.

use std::sync::Arc;

use tracing::{error, warn};

use crate::bus::{TRIGGER_MAX_DELIVER, TRIGGER_STREAM_NAME};
use crate::control_plane::{ControlPlaneStore, TriggerClaim, TriggerKey};
use crate::worker::{DurableStartReport, WalRun};

use super::{CONSUMER_NAME, TriggerDispatcher, trigger_name};

/// What the durable claim says about one delivery.
///
/// Named apart from [`super::admission::Admission`], which decides
/// something else entirely a few lines further down `handle` — whether
/// the agent's *model* is accepting work. Two sibling enums called
/// `Admission` in one function is a reading hazard, not a symmetry.
pub(super) enum ClaimVerdict {
    /// This worker owns the delivery: dispatch it.
    Proceed { claim: Claimed, delivered: i64 },
    /// Someone else owns it, or it has already run: leave it alone.
    Stop,
}

/// The claim a delivery that may proceed holds: where the message sits
/// on the trigger stream, and the id its invocation will run under.
///
/// The id is chosen before the claim row is written and is written with
/// it, so the row names the invocation from claim time rather than from
/// its durable start (#959). `handle` hands it to the worker through
/// [`crate::worker::DurableStart`], which is what makes it the id the
/// invocation's WAL rows carry.
#[derive(Clone, Copy)]
pub(super) struct Claimed {
    pub(super) stream_seq: u64,
    pub(super) invocation_id: uuid::Uuid,
}

/// Where the invocation is when `handle` marks its durable start, which
/// decides what a failed mark does (`mark_durable_started`).
#[derive(Clone, Copy)]
pub(super) enum Marking {
    /// Still running: `handle` saw the signal fire inside its loop. A
    /// failed mark stops the run, un-acked.
    WhileRunning,
    /// Already returned: the invocation fired the signal and returned in
    /// one poll, and `handle` found the signal fired only after its loop
    /// had ended on the result (#966). A failed mark is acked anyway,
    /// keeping the claim.
    AfterReturn,
}

/// The trigger-claim table, handed to a dispatcher as its arbiter. The
/// only way to get one is [`TriggerClaims::recover`], which first records
/// on its claim the durable start of every run that has one on record.
///
/// The adoption arm of `claim_delivery` reads a `claimed` row under
/// another claimant's id as "nothing durable has started for this
/// trigger". That is true only once every start on record is on its
/// claim. A process that died between an invocation's first WAL write
/// and its claim being marked left a `claimed` row whose invocation is in
/// flight (#959). A run whose mark failed can go on to finish under a
/// `claimed` row, its delivery acked with the claim kept (#967). Adopting
/// either row would start the trigger a second time, on a redelivery or on
/// a replay of the stream. The claim-installing constructors take this
/// value and nothing else, so a claim-arbitrating dispatcher cannot be
/// built before the recording has run.
pub struct TriggerClaims {
    store: Arc<ControlPlaneStore>,
}

impl TriggerClaims {
    /// Record on its claim the durable start of every run that has one on
    /// record while its claim still reads `claimed`, then hand `store` over
    /// as a claim arbiter.
    ///
    /// A run has a start on record when either of two records holds it:
    ///
    /// - **The worker's WAL**, as `starts` reports it: the run made its
    ///   first WAL write and its row is still there, in flight whatever its
    ///   category, ambiguous included, or finished and not yet archived.
    /// - **The control plane's ownership row** for the invocation the claim
    ///   names. The archive hand-off writes it before the worker deletes its
    ///   WAL row, so a run that has left the WAL is here; and nothing sweeps
    ///   it, so it outlives the archive row, which retention does sweep. A
    ///   report of ambiguity writes one too, for a run the WAL holds.
    ///
    /// **An operator's drop is on record the same way, whether or not the
    /// run started.** A drop writes the invocation's ownership row
    /// (`failed`), and it can name a run that never made its first WAL
    /// write: a live drop is accepted from the moment the runner takes the
    /// run, before its workspace, its MCP servers and its `triggered`
    /// publish, and a drop of a run whose process died resolves the run's
    /// agent from that `triggered` event. Such a claim is marked too, so a
    /// redelivery or a replay of its trigger does not run the work. That is
    /// the outcome the dispatcher gives the same drop on a live daemon: an
    /// operator halt before the first WAL write is consumed, not retried,
    /// because the drop is the run's outcome (`TriggerFate::Consume` in the
    /// dispatcher).
    ///
    /// These are the two records `fq doctor` reads to call a claim started
    /// and never marked (`Views::stranded_trigger_claims`), and the claims
    /// are read through the same query
    /// ([`ControlPlaneStore::unsettled_trigger_claims`]), so a claim the
    /// report would list that way is one this marks. A `claimed` row with
    /// neither record, or with no invocation id, is left exactly as it is:
    /// it may be a delivery that never started, and the adoption arm is
    /// right to take it.
    ///
    /// Each claim is matched to its run by the invocation id it has named
    /// since claim time and marked by its key
    /// ([`ControlPlaneStore::mark_unsettled_trigger_started`]), so the cost
    /// is that one query, a pass over the claim table joined to the
    /// ownership table by primary key, and one primary-key write per claim
    /// marked. A claim this marks is logged at `warn`, once, with the
    /// invocation id and where its run is on record: in flight or finished
    /// in the worker's WAL, or the status its ownership row holds.
    ///
    /// Run it at startup, over the report of the worker this dispatcher
    /// hands invocations to, before the dispatcher consumes anything. A test
    /// that builds a dispatcher directly does the same with its worker's
    /// WAL, empty or not: there is no other way to obtain the claims.
    pub async fn recover(
        store: Arc<ControlPlaneStore>,
        starts: &DurableStartReport,
    ) -> Result<Self, crate::control_plane::ControlPlaneStoreError> {
        for unsettled in store.unsettled_trigger_claims().await? {
            let claim = &unsettled.claim;
            let Some(invocation_id) = claim.invocation_id.as_deref() else {
                continue;
            };
            let run = match (starts.run(invocation_id), unsettled.owner_status) {
                (Some(WalRun::InFlight), _) => "in flight in the worker's WAL".to_string(),
                (Some(WalRun::Finished), _) => {
                    "finished, in the worker's WAL until its archive is acknowledged".to_string()
                }
                (None, Some(status)) => {
                    format!("on the control plane's record as {}", status.as_str())
                }
                (None, None) => continue,
            };
            if store
                .mark_unsettled_trigger_started(claim.key(), invocation_id)
                .await?
            {
                warn!(%invocation_id, stream_seq = claim.stream_seq, claimant = %claim.claimant,
                    run = %run,
                    "recorded a durable start missing from its trigger's claim: a start of the run \
                     this claim names, or an operator's drop of it, is on record, so a redelivery \
                     or a replay of the trigger will be dropped instead of running it again");
            }
        }
        Ok(Self { store })
    }
}

impl TriggerDispatcher {
    /// Install durable trigger claims, claiming as `worker_id`. For tests
    /// that build a dispatcher directly; the daemon uses
    /// [`Self::with_caps_and_claims`].
    ///
    /// **Precondition, enforced by the argument's type:** the claims come
    /// from [`TriggerClaims::recover`] over the worker's report of the runs
    /// its WAL holds, which marks the claim of every run with a start on
    /// record. Without that, a trigger whose run started under a process
    /// that did not mark its claim, in flight (#959) or finished (#967), is
    /// adopted and started again.
    pub fn with_trigger_claims(
        mut self,
        claims: TriggerClaims,
        worker_id: impl Into<String>,
    ) -> Self {
        self.claim_store = Some((claims.store, worker_id.into()));
        self
    }

    /// Share the daemon's admission — its per-agent counts (#718) and its
    /// worker permits, which startup recovery and `fq invocation resume`
    /// queue on too (#994) — and install durable trigger claims, claiming
    /// as `worker_id`. The admission's worker cap replaces the one
    /// [`Self::new`] was given; the daemon builds both from one setting.
    ///
    /// **Precondition, enforced by the argument's type:** the claims come
    /// from [`TriggerClaims::recover`] over the worker's report of the runs
    /// its WAL holds, which marks the claim of every run with a start on
    /// record, and so has run before this dispatcher consumes a trigger.
    /// Without that, a trigger whose run started under a process that did
    /// not mark its claim, in flight (#959) or finished (#967), is adopted
    /// and started again.
    pub fn with_caps_and_claims(
        mut self,
        admission: super::RunAdmission,
        claims: TriggerClaims,
        worker_id: impl Into<String>,
    ) -> Self {
        self.agent_caps = Arc::clone(admission.agents());
        self.workers = Arc::clone(admission.workers());
        self.claim_store = Some((claims.store, worker_id.into()));
        self
    }

    /// This delivery's stable identity: the trigger stream, the
    /// incarnation of it this bus connected to, and the sequence.
    fn trigger_key(&self, stream_seq: u64) -> TriggerKey<'static> {
        TriggerKey {
            stream: TRIGGER_STREAM_NAME,
            stream_epoch: self.bus.trigger_stream_epoch(),
            stream_seq,
        }
    }

    /// Record the delivery's durable start on its claim, then ack it.
    /// Returns whether the delivery was acked, which it is unless the mark
    /// fails while the invocation is still running. A failed mark leaves
    /// the claim `claimed` either way; `marking` decides the rest, and the
    /// failure is logged accordingly.
    ///
    /// [`Marking::WhileRunning`]: the delivery is neither acked nor NAKed,
    /// and `handle` stops driving the invocation and returns. Stopping is
    /// the safe answer to a failed mark. The invocation has made its first
    /// WAL write, so dropping it leaves its WAL row in flight: the next
    /// start's recovery marks this claim
    /// ([`TriggerClaims::recover`]) and resumes the run once, and the
    /// trigger's redelivery then meets a `durably_started` row and is
    /// dropped. Until that start, redeliveries meet this process's own
    /// `claimed` row and are dropped too, and if they run out first the
    /// trigger is dead-lettered as `trigger_exhausted`.
    ///
    /// Letting the invocation run on would end it at a terminal WAL row
    /// under a claim still `claimed`, with the delivery un-acked. Recovery
    /// marks only in-flight invocations, so after a restart inside the
    /// redelivery ladder the next process would adopt that claim and start
    /// the trigger a second time. Whether a failed mark should instead be
    /// retried, or the run continued with recovery also marking finished
    /// runs, is for #967 to decide.
    ///
    /// [`Marking::AfterReturn`]: the invocation fired the signal and
    /// returned in the same poll, so `handle` cannot stop it. The
    /// delivery is acked anyway, because a run that started must not be
    /// redelivered. What a redelivery would come to depends on how the run
    /// returned:
    ///
    /// - **Finished** (`Completed`, `BudgetExceeded`, a permanent
    ///   failure), its WAL row terminal: on this daemon the same-claimant
    ///   `Held` arm would drop each redelivery until the trigger was
    ///   dead-lettered as `trigger_exhausted` although it ran. After a
    ///   restart inside the retry ladder, recovery, which marks only
    ///   in-flight rows, would leave the claim to be adopted and the work
    ///   run a second time.
    /// - **`Deferred`**: the run continues. `conclude` queues its resume
    ///   and this process runs it on, under the unmarked claim, and a
    ///   redelivery would meet the same `Held` arm.
    /// - **A transient error or `Suspended`**, its WAL row in flight: a
    ///   restart's recovery marks the claim and resumes the run, but on a
    ///   live daemon the redeliveries would still end in a false dead
    ///   letter.
    ///
    /// The ack does not release the claim, as [`Self::ack`] would: with the
    /// claim released, a lost ack would let the redelivery win a fresh
    /// claim and start the work again, while a lost ack that kept the
    /// claim leaves what not acking would have. What stays behind is a
    /// `claimed` row for a run that started; see #967.
    pub(super) async fn mark_durable_started(
        &self,
        msg: &async_nats::jetstream::Message,
        claim: &Claimed,
        trigger_id: uuid::Uuid,
        marking: Marking,
    ) -> bool {
        let Claimed {
            stream_seq,
            invocation_id,
        } = *claim;
        if let Some((store, _)) = self.claim_store.as_ref()
            && let Err(err) = store
                .mark_trigger_started(self.trigger_key(stream_seq), &invocation_id.to_string())
                .await
        {
            match marking {
                Marking::WhileRunning => {
                    error!(error = %err, stream_seq, %invocation_id,
                        "failed to mark trigger durably started; stopping the run here, un-acked: \
                         its WAL row stays in flight and the next start resumes it once");
                    return false;
                }
                Marking::AfterReturn => {
                    error!(error = %err, stream_seq, %invocation_id,
                        "failed to mark trigger durably started after its invocation returned; \
                         acking it anyway and leaving its claim `claimed`: a run that started \
                         must not be redelivered (see #967)");
                    self.ack_keeping_claim(msg, Some(trigger_id), "durably started, mark failed")
                        .await;
                    return true;
                }
            }
        }
        self.ack(msg, Some(trigger_id), "durably started").await;
        true
    }

    pub(super) async fn release_unstarted_claim(&self, msg: &async_nats::jetstream::Message) {
        let (Some((store, _)), Ok(info)) = (self.claim_store.as_ref(), msg.info()) else {
            return;
        };
        if let Err(err) = store
            .release_trigger_claim(self.trigger_key(info.stream_sequence))
            .await
        {
            error!(error = %err, stream_seq = info.stream_sequence,
                "failed to release unfinished trigger claim");
        }
    }

    /// Claim the stable broker identity before drain, routing, or parsing,
    /// naming on the claim the invocation id a delivery that proceeds will
    /// run under.
    pub(super) async fn claim_delivery(
        &self,
        msg: &async_nats::jetstream::Message,
    ) -> ClaimVerdict {
        let invocation_id = uuid::Uuid::now_v7();
        let Some((store, worker_id)) = self.claim_store.as_ref() else {
            // Unit-level dispatcher tests that do not host a control plane keep
            // their old isolated setup; the daemon always installs the store.
            let info = msg.info().ok();
            return ClaimVerdict::Proceed {
                claim: Claimed {
                    stream_seq: info.as_ref().map_or(0, |i| i.stream_sequence),
                    invocation_id,
                },
                delivered: info.as_ref().map_or(1, |i| i.delivered),
            };
        };
        let info = match msg.info() {
            Ok(info) => info,
            Err(err) => {
                error!(error = %err, "cannot identify trigger delivery; leaving it unacked");
                return ClaimVerdict::Stop;
            }
        };
        let seq = info.stream_sequence;
        let delivered = info.delivered;
        let now = chrono::Utc::now().timestamp_millis();
        let named = invocation_id.to_string();
        let result = match store
            .claim_trigger(self.trigger_key(seq), worker_id, &named, now)
            .await
        {
            Ok(result) => result,
            Err(err) => {
                error!(error = %err, stream_seq = seq, "cannot claim trigger; leaving it unacked");
                return ClaimVerdict::Stop;
            }
        };
        let proceed = ClaimVerdict::Proceed {
            claim: Claimed {
                stream_seq: seq,
                invocation_id,
            },
            delivered,
        };
        match result {
            TriggerClaim::Won => proceed,
            TriggerClaim::Started {
                invocation_id: started,
            } => {
                // Refused before `trigger::delivered` names the trigger and
                // before anything runs. The id this copy chose lost the insert
                // to the existing row, so it was never written anywhere.
                warn!(stream_seq = seq, delivered, started_invocation_id = %started,
                    duplicate_invocation_id = "none (refused before start)",
                    "dropping duplicate trigger delivery after durable start");
                self.bus
                    .consumer_ledger()
                    .note_duplicate_drop(CONSUMER_NAME);
                if let Err(err) = msg.ack().await {
                    error!(error = %err, stream_seq = seq, "failed to ack duplicate trigger");
                }
                ClaimVerdict::Stop
            }
            TriggerClaim::Held {
                claimant,
                invocation_id: claimed_for,
                claimed_at,
            } if claimant == *worker_id => {
                // This process's own claim, with no start on it. The copy
                // that took it owns the delivery: an ack or NAK from this
                // one would resolve that delivery underneath it, so this one
                // is dropped unanswered. It is logged and counted, apart
                // from the `Started` drops above (#968), because a refused
                // copy writes nothing anywhere else.
                //
                // **Nothing on the row says whether the first copy will
                // settle it,** so the line says what the row shows and does
                // not guess. A copy lands here in two kinds of case:
                //
                // - The first copy is alive. A hold keeps its delivery alive
                //   with an in-progress ack every `HOLD_KEEPALIVE`, so it is
                //   redelivered only if those acks stop landing for a whole
                //   ack window; from its worker permit to its first WAL
                //   write (workspace, MCP servers, the first step) nothing
                //   keeps it alive, so a start slower than the window is
                //   redelivered too. The first copy still starts once, and
                //   its durable start acks the message.
                // - This process will never settle the claim (#967): the
                //   invocation task panicked before its durable start, a
                //   release failed before a NAK, or `mark_trigger_started`
                //   failed, which stops a running invocation un-acked and
                //   leaves its WAL row for the next start to resume
                //   (`mark_durable_started`). Every redelivery then lands
                //   here until `max_deliver` is reached and the trigger is
                //   dead-lettered as `trigger_exhausted`: false where the run
                //   started, and after a panic nothing ran.
                //
                // A process that exits holding a claim (the drain check, the
                // pause hold's `Interrupted`, a failed `requeue_held` publish,
                // a drain that stops the invocation before its first WAL
                // write, #974) leaves a row the next process meets under
                // another worker id, and the arm below adopts it.
                //
                // The subject names the agent, which is the only handle on
                // a claim whose invocation never wrote a WAL row besides the
                // sequence. On the last delivery the line says so: no copy
                // follows, which is true whatever the claim is waiting on.
                let last = if delivered >= TRIGGER_MAX_DELIVER {
                    "; this was its last delivery, so no copy follows, and unless the delivery \
                     is acked before its window lapses the trigger is dead-lettered as \
                     `trigger_exhausted`"
                } else {
                    ""
                };
                warn!(stream_seq = seq, delivered, subject = %msg.subject, claimant = %claimant,
                    claim_age_ms = now.saturating_sub(claimed_at),
                    claim_invocation_id = %claimed_for.as_deref().unwrap_or("none"),
                    "dropping a redelivered trigger whose claim this daemon already holds with no \
                     start recorded, and leaving it unacked: the first copy may still be waiting \
                     or starting, or this daemon may never settle the claim (see #967); the claim \
                     does not say which{last}");
                self.bus
                    .consumer_ledger()
                    .note_held_duplicate_drop(CONSUMER_NAME);
                ClaimVerdict::Stop
            }
            TriggerClaim::Held {
                claimant,
                invocation_id: claimed_for,
                claimed_at,
            } => {
                // One worker is deployed today. A different id therefore names
                // a dead previous process; ADR-0032 adds shared liveness + CAS.
                //
                // That process may have started the run and not marked this
                // row: it died after the run's first WAL write (#959), or its
                // mark failed and the run went on to finish (#967). Startup
                // recovery has already answered for both: it marks the claim
                // of every run with a start on record, in the worker's WAL or
                // in the control plane's ownership table, by the id the row
                // has named since claim time, and this dispatcher holds
                // claims only as what that marking returned
                // (`TriggerClaims::recover`). A row still `claimed` here
                // therefore names an invocation with no start on record, and
                // this start is, as far as any record shows, its trigger's
                // first.
                //
                // The adopted claim runs under this copy's own id, written over
                // the dead process's by the same compare-and-set. Reusing that
                // one is unsafe where its invocation reached a terminal WAL row
                // without its claim being marked: the worker store does not
                // update a terminal row and the archive keeps the first record
                // of an id, so a second run under it would have its WAL writes
                // or its archive refused without a word.
                match store
                    .take_over_trigger_claim(
                        self.trigger_key(seq),
                        &claimant,
                        worker_id,
                        &named,
                        now,
                    )
                    .await
                {
                    Ok(true) => proceed,
                    // The row changed between the read above and this
                    // compare-and-set, and is no longer `claimed` by that
                    // claimant. What changed it is not on the row, so the
                    // line names the claim as it was read and no cause.
                    // Not counted: one daemon cannot reach this, and it is
                    // not a duplicate of anything this daemon holds.
                    Ok(false) => {
                        warn!(stream_seq = seq, delivered, subject = %msg.subject,
                            claimant = %claimant,
                            claim_age_ms = now.saturating_sub(claimed_at),
                            claim_invocation_id = %claimed_for.as_deref().unwrap_or("none"),
                            "dropping a redelivered trigger, and leaving it unacked: the claim it \
                             met changed before this daemon could adopt it, and is no longer \
                             `claimed` by that claimant");
                        ClaimVerdict::Stop
                    }
                    Err(err) => {
                        error!(error = %err, stream_seq = seq, "cannot adopt trigger claim");
                        ClaimVerdict::Stop
                    }
                }
            }
        }
    }
}

impl TriggerDispatcher {
    pub(super) async fn ack(
        &self,
        msg: &async_nats::jetstream::Message,
        trigger_id: Option<uuid::Uuid>,
        context: &str,
    ) {
        self.release_unstarted_claim(msg).await;
        self.ack_keeping_claim(msg, trigger_id, context).await;
    }

    /// Ack the delivery, leaving its claim row as it is. [`Self::ack`]
    /// releases the claim first; this alone is for a run that started but
    /// whose claim could not be marked (`mark_durable_started`).
    async fn ack_keeping_claim(
        &self,
        msg: &async_nats::jetstream::Message,
        trigger_id: Option<uuid::Uuid>,
        context: &str,
    ) {
        if let Err(err) = msg.ack().await {
            error!(
                error = %err,
                context,
                subject = %msg.subject,
                trigger_id = %trigger_name(trigger_id),
                "failed to ack trigger message"
            );
        }
    }

    /// NAK a trigger so JetStream redelivers it — used when an
    /// invocation fails *before its first WAL write* with a transient
    /// error, so the otherwise-lost run is retried. The delay escalates
    /// with the delivery attempt ([`trigger_retry_backoff`]); a bare
    /// `Nak(None)` would redeliver immediately and burn the bounded
    /// retries in a tight loop.
    pub(super) async fn nak(
        &self,
        msg: &async_nats::jetstream::Message,
        trigger_id: uuid::Uuid,
        delay: std::time::Duration,
        context: &str,
    ) {
        self.release_unstarted_claim(msg).await;
        if let Err(err) = msg
            .ack_with(async_nats::jetstream::AckKind::Nak(Some(delay)))
            .await
        {
            error!(
                error = %err,
                context,
                subject = %msg.subject,
                trigger_id = %trigger_id,
                "failed to NAK trigger message"
            );
        } else {
            warn!(
                context,
                subject = %msg.subject,
                trigger_id = %trigger_id,
                retry_in_ms = delay.as_millis() as u64,
                "NAK'd trigger for redelivery"
            );
        }
    }
}

pub(super) type ClaimStore = (Arc<ControlPlaneStore>, String);
