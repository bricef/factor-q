//! Durable arbitration for trigger redeliveries.

use std::sync::Arc;

use tracing::{error, warn};

use crate::bus::TRIGGER_STREAM_NAME;
use crate::control_plane::{ControlPlaneStore, TriggerClaim};

use super::{CONSUMER_NAME, TriggerDispatcher, trigger_name};

pub(super) enum Admission {
    Proceed { stream_seq: u64, delivered: i64 },
    Stop,
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

    pub(super) async fn mark_durable_started(
        &self,
        msg: &async_nats::jetstream::Message,
        stream_seq: u64,
        trigger_id: uuid::Uuid,
        invocation_id: uuid::Uuid,
    ) -> bool {
        if let Some((store, _)) = self.claim_store.as_ref()
            && let Err(err) = store
                .mark_trigger_started(TRIGGER_STREAM_NAME, stream_seq, &invocation_id.to_string())
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
            .release_trigger_claim(TRIGGER_STREAM_NAME, info.stream_sequence)
            .await
        {
            error!(error = %err, stream_seq = info.stream_sequence,
                "failed to release unfinished trigger claim");
        }
    }

    /// Claim the stable broker identity before drain, routing, or parsing.
    pub(super) async fn claim_delivery(&self, msg: &async_nats::jetstream::Message) -> Admission {
        let Some((store, worker_id)) = self.claim_store.as_ref() else {
            // Unit-level dispatcher tests that do not host a control plane keep
            // their old isolated setup; the daemon always installs the store.
            let info = msg.info().ok();
            return Admission::Proceed {
                stream_seq: info.as_ref().map_or(0, |i| i.stream_sequence),
                delivered: info.as_ref().map_or(1, |i| i.delivered),
            };
        };
        let info = match msg.info() {
            Ok(info) => info,
            Err(err) => {
                error!(error = %err, "cannot identify trigger delivery; leaving it unacked");
                return Admission::Stop;
            }
        };
        let seq = info.stream_sequence;
        let delivered = info.delivered;
        let now = chrono::Utc::now().timestamp_millis();
        let result = match store
            .claim_trigger(TRIGGER_STREAM_NAME, seq, worker_id, now)
            .await
        {
            Ok(result) => result,
            Err(err) => {
                error!(error = %err, stream_seq = seq, "cannot claim trigger; leaving it unacked");
                return Admission::Stop;
            }
        };
        match result {
            TriggerClaim::Won => Admission::Proceed {
                stream_seq: seq,
                delivered,
            },
            TriggerClaim::Started { invocation_id } => {
                // This delivery has no invocation id: refusing it before
                // `trigger::delivered` is precisely what prevents one being minted.
                warn!(stream_seq = seq, delivered, started_invocation_id = %invocation_id,
                    duplicate_invocation_id = "none (refused before minting)",
                    "dropping duplicate trigger delivery after durable start");
                self.bus
                    .consumer_ledger()
                    .note_duplicate_drop(CONSUMER_NAME);
                if let Err(err) = msg.ack().await {
                    error!(error = %err, stream_seq = seq, "failed to ack duplicate trigger");
                }
                Admission::Stop
            }
            TriggerClaim::Held { claimant } if claimant == *worker_id => {
                // The original task still owns this message. An ack or NAK from
                // the duplicate would resolve its delivery underneath that task.
                Admission::Stop
            }
            TriggerClaim::Held { claimant } => {
                // One worker is deployed today. A different id therefore names
                // a dead previous process; ADR-0032 adds shared liveness + CAS.
                match store
                    .take_over_trigger_claim(TRIGGER_STREAM_NAME, seq, &claimant, worker_id, now)
                    .await
                {
                    Ok(true) => Admission::Proceed {
                        stream_seq: seq,
                        delivered,
                    },
                    Ok(false) => Admission::Stop,
                    Err(err) => {
                        error!(error = %err, stream_seq = seq, "cannot adopt trigger claim");
                        Admission::Stop
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
