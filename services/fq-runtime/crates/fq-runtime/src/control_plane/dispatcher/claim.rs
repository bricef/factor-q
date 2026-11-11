//! Durable arbitration for trigger redeliveries.

use std::sync::Arc;

use tracing::{error, warn};

use crate::bus::TRIGGER_STREAM_NAME;
use crate::control_plane::{ControlPlaneStore, TriggerClaim, TriggerKey};

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

impl TriggerDispatcher {
    /// Install claims for focused tests that construct the dispatcher directly.
    pub fn with_trigger_claims(
        mut self,
        store: Arc<ControlPlaneStore>,
        worker_id: impl Into<String>,
    ) -> Self {
        self.claim_store = Some((store, worker_id.into()));
        self
    }

    /// Share daemon concurrency state and install durable trigger claims.
    pub fn with_caps_and_claims(
        mut self,
        caps: Arc<crate::control_plane::agent_cap::AgentConcurrency>,
        store: Arc<ControlPlaneStore>,
        worker_id: impl Into<String>,
    ) -> Self {
        self.agent_caps = caps;
        self.claim_store = Some((store, worker_id.into()));
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

    pub(super) async fn mark_durable_started(
        &self,
        msg: &async_nats::jetstream::Message,
        claim: &Claimed,
        trigger_id: uuid::Uuid,
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
            error!(error = %err, stream_seq, %invocation_id,
                "failed to mark trigger durably started; leaving delivery unacked");
            return false;
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
            TriggerClaim::Held { claimant } if claimant == *worker_id => {
                // The original task still owns this message. An ack or NAK from
                // the duplicate would resolve its delivery underneath that task.
                //
                // **This arm assumes the first copy is still alive, and in one
                // case it is not.** Most paths that leave a `claimed`-by-self
                // row with the message unacked are process-terminal — the drain
                // check, the pause hold's `Interrupted`, a failed `requeue_held`
                // publish — so the next delivery arrives at a new worker id and
                // the arm below adopts it. But a store error or a panic between
                // the claim and the durable start leaves this *live* process
                // holding a row for a run that will never start: every
                // redelivery then lands here and is dropped, until `max_deliver`
                // is reached and the trigger is dead-lettered as
                // `trigger_exhausted` — a verdict that would be false, since
                // nothing ever ran. Adopting a claim older than some multiple of
                // the longest legitimate hold is the follow-up that closes it;
                // ADR-0032's liveness-and-CAS protocol is where it belongs.
                ClaimVerdict::Stop
            }
            TriggerClaim::Held { claimant } => {
                // One worker is deployed today. A different id therefore names
                // a dead previous process; ADR-0032 adds shared liveness + CAS.
                //
                // That process may have reached its invocation's first WAL
                // write and died before marking this row. Startup recovery has
                // already answered for that case: it marks the claim of every
                // invocation it finds in flight, by the id the row has named
                // since claim time, before the daemon builds a dispatcher
                // (`fq-daemon`'s `recovery::classify_in_flight`, #959). A row
                // still `claimed` here therefore names an invocation with no
                // in-flight WAL row, and this start is its trigger's first.
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
                    Ok(false) => ClaimVerdict::Stop,
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
