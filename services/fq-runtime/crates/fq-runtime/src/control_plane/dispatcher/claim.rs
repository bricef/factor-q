//! Durable arbitration for trigger redeliveries.

use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, error, warn};

use crate::bus::TRIGGER_STREAM_NAME;
use crate::control_plane::store::RunRecords;
use crate::control_plane::{ControlPlaneStore, ControlPlaneStoreError, TriggerClaim, TriggerKey};
use crate::worker::DurableStartReport;

use super::admission::{HOLD_KEEPALIVE, keep_alive};
use super::own_claim::{Handled, OwnClaim};
use super::{CONSUMER_NAME, TriggerDispatcher, trigger_name};

mod settlement;
mod unmarked_start;
pub(super) use settlement::Answer;

/// What the durable claim says about one delivery.
///
/// Named apart from [`super::admission::Admission`], which decides
/// something else entirely a few lines further down `handle` — whether
/// the agent's *model* is accepting work. Two sibling enums called
/// `Admission` in one function is a reading hazard, not a symmetry.
pub(super) enum ClaimVerdict {
    /// This worker owns the delivery: dispatch it. `handled` notes the copy
    /// as handling its claim until `mark_durable_started` releases it, once
    /// the start is marked or the mark gives up, or else until `handle`
    /// drops it (`own_claim`).
    Proceed {
        claim: Claimed,
        delivered: i64,
        handled: Handled,
    },
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

/// The pauses before each retry of a durable-start mark the claim store
/// refused (#967), within [`MARK_RETRY_BUDGET`]. A store that refuses at
/// once gets four attempts, the last 1.5 seconds after the first.
///
/// The schedule is sized against store faults that come back at once and
/// clear in a moment, such as an I/O error or a pooled connection that had
/// to be replaced: a short first pause, and longer ones for a fault that
/// takes a little longer. It is not what governs a store that blocks. One
/// attempt waits for a write lock another connection holds for as long as
/// the store's busy timeout, which `ControlPlaneStore::open` does not set,
/// so it is sqlx's default of five seconds; before that, with every pooled
/// connection taken, it waits for one for up to the pool's acquire
/// timeout, sqlx's default of thirty seconds. The budget is what keeps
/// those from multiplying.
///
/// Design Principle 8 names a retry interval as configuration, and this
/// is a constant anyway, on precedent: `HOLD_KEEPALIVE`, the trigger
/// retry ladder (`TRIGGER_RETRY_BACKOFF`) and the store's own connect
/// retry (`CONNECT_RETRY_FIRST_PAUSE` in `db/schema.rs`) are constants
/// too, each sized against the fault it waits out. The trigger's 30-second
/// ack window does not bound it: `mark_durable_started` keeps the delivery
/// alive while the attempts run.
pub(super) const MARK_RETRY_PAUSES: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_millis(400),
    Duration::from_millis(1000),
];

/// How long after the first attempt to mark a durable start began a
/// further attempt may still be offered: after a refusal, the next one is
/// offered only if it would begin, its pause included, less than this
/// after the first began. This is the "second or two in all" #967
/// decided.
///
/// A store that refuses at once keeps its four attempts. A store whose
/// first attempt blocks past this gets that one attempt, as it did before
/// there were retries, and the delivery falls through to the ack. The
/// worst case is an attempt begun just inside the budget that then
/// blocks: a little under two seconds plus one attempt's longest wait,
/// which is the busy timeout (about seven seconds in all) or, with every
/// pooled connection taken, the acquire timeout and then the busy timeout
/// (about thirty-seven). `handle` does not poll the invocation for that
/// long; one blocking attempt cost the same before there were retries.
pub(super) const MARK_RETRY_BUDGET: Duration = Duration::from_secs(2);

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
            // The ownership row's presence is the record; its status is only
            // reported, as stored, so one this build cannot read does not
            // stop the boot. Whether either record holds the run is
            // `RunRecords`'s one definition, shared with `fq doctor` and the
            // dispatcher's settling of its own claims.
            let records = RunRecords {
                wal: starts.run(invocation_id),
                owner_row_status: unsettled.owner_row_status.clone(),
            };
            if !records.start_on_record() {
                continue;
            }
            let run = records.where_on_record().unwrap_or_default();
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
    pub(super) fn trigger_key(&self, stream_seq: u64) -> TriggerKey<'static> {
        TriggerKey {
            stream: TRIGGER_STREAM_NAME,
            stream_epoch: self.bus.trigger_stream_epoch(),
            stream_seq,
        }
    }

    /// Record the delivery's durable start on its claim, then ack it.
    ///
    /// `handle` calls this once the invocation has made its first WAL
    /// write: while it is still running, or just after it returned when it
    /// fired the signal and returned in one poll (#966). Either way the
    /// delivery is acked here and the run is never stopped, because a run
    /// that started must not be redelivered.
    ///
    /// **A refused mark is offered again** after each of
    /// [`MARK_RETRY_PAUSES`], for as long as [`MARK_RETRY_BUDGET`] allows,
    /// so a claim store that refuses for a moment heals with nothing else
    /// happening. The run waits for the mark, since `handle` does not poll
    /// the invocation meanwhile: on a healthy day for one statement; when
    /// every attempt is refused at once, for 1.5 seconds of pauses; when an
    /// attempt blocks, for as long as it waits in the store, which the
    /// budget's own documentation bounds.
    ///
    /// `handle` keeps the delivery alive through the run's setup, up to
    /// this point, so its ack window is all but full when the first
    /// attempt starts; but one attempt can wait out the store's busy
    /// timeout or its pool. An in-progress ack every [`HOLD_KEEPALIVE`]
    /// therefore runs alongside the attempts, not only between them,
    /// taking over from the setup's. The first goes out a tick after the
    /// mark starts, so the healthy path sends none.
    ///
    /// **A mark that finds no claim naming the run** is not offered again,
    /// since a claim that is gone does not come back: the start is
    /// recorded by what the claim holds instead, and the delivery acked
    /// keeping it (`unmarked_start`, #1046).
    ///
    /// **A mark refused on every attempt** is answered with an ack that
    /// does not release the claim ([`Self::ack_keeping_claim`]), and the
    /// run continues, so the delivery is not redelivered. The claim is not
    /// released first, as [`Self::ack`] would: a copy that came back would
    /// then win a fresh claim and start the work again.
    ///
    /// What stays behind is a `claimed` row for a run that started, and
    /// only until this daemon next starts: [`TriggerClaims::recover`]
    /// records the start on it before the dispatcher consumes anything, as
    /// it does for every run with a start on record, and its documentation
    /// says why neither a redelivery nor a replay of the trigger can then
    /// start it again. Until then `fq doctor` lists the row as started and
    /// never marked, once a minute has passed.
    ///
    /// **Once it returns, the copy stops counting as handling its claim**
    /// ([`Handled::release`]): the delivery has been answered and the run's
    /// first WAL write is on record, though the run goes on.
    ///
    /// **An ack that is lost as well** is settled by the copy it brings
    /// back. An ack is published without waiting for the broker, so only
    /// an ack the client refuses to send is seen here, and
    /// `ack_keeping_claim` logs it; one lost on its way shows only when
    /// the delivery comes back. Either way the run carries on to its end.
    /// A copy that reaches this daemon before it next starts, while the run
    /// is still going or after it ended, meets its own `claimed` row with
    /// no copy handling it, finds the run's start on record in the worker's
    /// WAL or the control plane's ownership table, and acks the delivery
    /// and marks the claim, without running the trigger again (`own_claim`,
    /// #967). If the lost ack was the trigger's last delivery, no copy comes
    /// back; the dead-letter watch then finds the start on record, marks the
    /// claim, and writes no `trigger_exhausted` (`exhaustion`).
    pub(super) async fn mark_durable_started(
        &self,
        msg: &async_nats::jetstream::Message,
        claim: &Claimed,
        trigger_id: uuid::Uuid,
        handled: &mut Handled,
    ) {
        let Some((store, worker_id)) = self.claim_store.as_ref() else {
            self.ack(msg, claim, Some(trigger_id), "durably started")
                .await;
            handled.release();
            return;
        };
        let Claimed {
            stream_seq,
            invocation_id,
        } = *claim;
        let marked = keeping_alive(
            msg,
            Some(trigger_id),
            mark_with_retries(store, self.trigger_key(stream_seq), invocation_id),
        )
        .await;
        match marked {
            // The warn says the mark landed; a mark that found no claim
            // naming the run did not, and its refusals go on the line
            // `record_unmarked_start` logs instead.
            Ok(Landed {
                value: true,
                refused,
            }) => {
                if let Some(last) = refused.last() {
                    warn!(error = %last, refused = refused.len(), stream_seq, %invocation_id,
                        %trigger_id, subject = %msg.subject,
                        "marked trigger durably started after the claim store refused the \
                         attempts before it; the run carried on");
                }
                self.ack(msg, claim, Some(trigger_id), "durably started")
                    .await;
            }
            Ok(Landed {
                value: false,
                refused,
            }) => {
                self.record_unmarked_start(msg, store, worker_id, claim, trigger_id, refused.len())
                    .await;
            }
            Err(Refused {
                attempts,
                elapsed,
                last,
            }) => {
                error!(error = %last, attempts, elapsed_ms = elapsed.as_millis() as u64,
                    stream_seq, %invocation_id, %trigger_id, subject = %msg.subject,
                    "could not mark trigger durably started: the claim store refused every \
                     attempt. Acking the delivery anyway, keeping its claim, and letting the run \
                     continue, so it is not redelivered. The claim stays `claimed` for a run that \
                     started until this daemon next starts, when recovery records the start on \
                     it; until then `fq doctor` lists it as started and never marked");
                self.ack_keeping_claim(msg, Some(trigger_id), "durably started, mark failed")
                    .await;
            }
        }
        handled.release();
    }

    /// Give back `claim`, if it is still `claimed` and still names the
    /// invocation it was taken for. It removes nothing once the claim is
    /// marked started, which is every ack after a durable start, or once
    /// another copy has taken it again (issue 1061).
    pub(super) async fn release_unstarted_claim(&self, claim: &Claimed) {
        let Some((store, _)) = self.claim_store.as_ref() else {
            return;
        };
        let invocation_id = claim.invocation_id.to_string();
        match store
            .release_trigger_claim(self.trigger_key(claim.stream_seq), &invocation_id)
            .await
        {
            Ok(true) => {}
            Ok(false) => debug!(stream_seq = claim.stream_seq, %invocation_id,
                "released no trigger claim: none is still `claimed` under this invocation"),
            Err(err) => error!(error = %err, stream_seq = claim.stream_seq, %invocation_id,
                "failed to release unfinished trigger claim"),
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
        let named = invocation_id.to_string();
        // Noted before the id is written on any claim, so no copy can find
        // a row naming it while it is not noted (`own_claim`).
        let handled = self.handling.begin(&named);
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
                handled,
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
            handled,
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
            // This process's own claim, with no start on it: settled by
            // whether a copy is still handling it and what is on record
            // (`own_claim`, #967).
            TriggerClaim::Held {
                claimant,
                invocation_id: claimed_for,
                claimed_at,
            } if claimant == *worker_id => {
                let met = OwnClaim {
                    worker_id,
                    seq,
                    delivered,
                    now,
                    named: &named,
                    claimed_for,
                    claimed_at,
                };
                self.settle_own_claim(msg, store, met, proceed).await
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
    /// Release the delivery's claim and ack it, keeping the delivery in
    /// progress until the ack is published ([`settlement`], #1057).
    pub(super) async fn ack(
        &self,
        msg: &async_nats::jetstream::Message,
        claim: &Claimed,
        trigger_id: Option<uuid::Uuid>,
        context: &str,
    ) {
        let answer = Answer::Ack { context };
        self.settle(msg, claim, trigger_id, std::future::ready(answer))
            .await;
    }

    /// Ack the delivery, leaving its claim row as it is. [`Self::ack`]
    /// releases the claim first; this alone is for a run that started but
    /// whose claim could not be marked (`mark_durable_started`), or was
    /// marked by a copy that met its own claim (`own_claim`), or whose
    /// start was recorded on a claim the mark did not find
    /// (`unmarked_start`).
    pub(super) async fn ack_keeping_claim(
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
    /// retries in a tight loop. The claim is released first, and the
    /// delivery is kept in progress until the NAK is published
    /// ([`settlement`], #1057).
    pub(super) async fn nak(
        &self,
        msg: &async_nats::jetstream::Message,
        claim: &Claimed,
        trigger_id: uuid::Uuid,
        delay: std::time::Duration,
        context: &str,
    ) {
        let answer = Answer::Nak { delay, context };
        self.settle(msg, claim, Some(trigger_id), std::future::ready(answer))
            .await;
    }

    /// Publish a NAK, the claim already released.
    async fn send_nak(
        &self,
        msg: &async_nats::jetstream::Message,
        trigger_id: Option<uuid::Uuid>,
        delay: std::time::Duration,
        context: &str,
    ) {
        if let Err(err) = msg
            .ack_with(async_nats::jetstream::AckKind::Nak(Some(delay)))
            .await
        {
            error!(
                error = %err,
                context,
                subject = %msg.subject,
                trigger_id = %trigger_name(trigger_id),
                "failed to NAK trigger message"
            );
        } else {
            warn!(
                context,
                subject = %msg.subject,
                trigger_id = %trigger_name(trigger_id),
                retry_in_ms = delay.as_millis() as u64,
                "NAK'd trigger for redelivery"
            );
        }
    }
}

/// An attempt [`offer_with_retries`] offered landed.
pub(super) struct Landed<T> {
    /// What it returned.
    pub(super) value: T,
    /// The refusals before it, none on a healthy day.
    pub(super) refused: Vec<ControlPlaneStoreError>,
}

/// Every attempt [`offer_with_retries`] offered was refused.
pub(super) struct Refused {
    /// How many attempts were offered.
    pub(super) attempts: usize,
    /// How long they took, from the start of the first.
    pub(super) elapsed: Duration,
    /// The last refusal.
    pub(super) last: ControlPlaneStoreError,
}

/// Offer a durable start to the claim store's mark: [`offer_with_retries`]
/// with [`ControlPlaneStore::mark_trigger_started`]. Landed, it says
/// whether the mark found the claim naming the run. A mark that found no
/// such claim is not offered again: retrying would not bring the claim
/// back.
async fn mark_with_retries(
    store: &ControlPlaneStore,
    key: TriggerKey<'_>,
    invocation_id: uuid::Uuid,
) -> Result<Landed<bool>, Refused> {
    let named = invocation_id.to_string();
    offer_with_retries("mark", key, invocation_id, || {
        store.mark_trigger_started(key, &named)
    })
    .await
}

/// Offer a write of a run's durable start to the claim store, the mark or
/// the record of a start the mark did not find (`step` names which), and
/// again after each of [`MARK_RETRY_PAUSES`] while it refuses, as long as
/// the next attempt would begin within [`MARK_RETRY_BUDGET`] of the first.
/// `Ok` once an attempt lands, with the refusals before it; `Err` once the
/// store has refused every attempt offered. Each refusal is logged at
/// `debug`; the caller logs the outcome, once. The faults the schedule is
/// sized for, an I/O error or a pooled connection that had to be replaced,
/// meet either write alike.
pub(super) async fn offer_with_retries<T, F, W>(
    step: &'static str,
    key: TriggerKey<'_>,
    invocation_id: uuid::Uuid,
    mut write: F,
) -> Result<Landed<T>, Refused>
where
    F: FnMut() -> W,
    W: std::future::Future<Output = Result<T, ControlPlaneStoreError>>,
{
    let began = tokio::time::Instant::now();
    let mut refused = Vec::new();
    let mut pauses = MARK_RETRY_PAUSES.iter();
    loop {
        let err = match write().await {
            Ok(value) => return Ok(Landed { value, refused }),
            Err(err) => err,
        };
        let in_budget = |pause: &&Duration| began.elapsed() + **pause < MARK_RETRY_BUDGET;
        let Some(pause) = pauses.next().filter(in_budget) else {
            return Err(Refused {
                attempts: refused.len() + 1,
                elapsed: began.elapsed(),
                last: err,
            });
        };
        debug!(error = %err, step, attempt = refused.len() + 1, stream_seq = key.stream_seq,
            %invocation_id, retry_in_ms = pause.as_millis() as u64,
            "the claim store refused a write of a trigger's durable start; offering it again");
        refused.push(err);
        tokio::time::sleep(*pause).await;
    }
}

/// Drive `work` to its end while keeping `msg`'s delivery alive with an
/// in-progress ack every [`HOLD_KEEPALIVE`], the cadence every hold
/// keeps. The first goes out a tick after `work` starts, not at once as a
/// hold's does: work that ends inside the first tick, which is every mark
/// and every settlement on a healthy day, sends none. Where the run's
/// setup, a hold or the mark came before, it kept the delivery's window
/// all but full, so the wait costs nothing. The refusals at the front of
/// `handle` come after only the claim's own store calls, which nothing
/// renews, so their first renewal comes a tick after whatever share of the
/// window those calls used.
async fn keeping_alive<T>(
    msg: &async_nats::jetstream::Message,
    trigger_id: Option<uuid::Uuid>,
    work: impl std::future::Future<Output = T>,
) -> T {
    let mut work = std::pin::pin!(work);
    loop {
        tokio::select! {
            biased;
            done = &mut work => return done,
            () = tokio::time::sleep(HOLD_KEEPALIVE) => keep_alive(msg, trigger_id).await,
        }
    }
}

pub(super) type ClaimStore = (Arc<ControlPlaneStore>, String);
