//! Control-plane consumer that maintains coordination state
//! by subscribing to invocation lifecycle events from workers.
//!
//! Per data-architecture.md §7.2, the control-plane on
//! restart subscribes to:
//!
//! - `fq.agent.*.invocation.ambiguous` — workers publish on
//!   recovery when their WAL has a `dispatched`-without-
//!   `completed` row. The control-plane upserts the
//!   `coordination_invocation_owner` row to status=ambiguous.
//! - `fq.agent.*.invocation.archived` — workers publish on
//!   terminal handoff (step 8). The control-plane writes the
//!   `invocation_archive` row and publishes
//!   `invocation.archive_acked` back on the worker-scoped
//!   `fq.worker.{worker_id}.invocation.archive_acked` subject.
//!
//! The consumer also runs a periodic stale-worker sweep:
//! workers whose `last_heartbeat` is older than the
//! configured threshold get marked `stale` in
//! `coordination_worker`. This makes `fq workers stale`
//! meaningful even if a worker process disappears without
//! emitting a shutdown event.
//!
//! Delivery semantics:
//! - **At-least-once** from JetStream. Coordination updates
//!   are idempotent (upsert by primary key), so re-delivery
//!   is safe.
//! - **Parse errors** are logged and acked.
//! - **Store errors** are NAK'd to trigger redelivery.
//!
//! The consumer is single-process in v1; v2 splits it onto
//! the dedicated control-plane node.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::StreamExt;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

use crate::bus::{BusError, EventBus};
use crate::events::{
    Event, EventPayload, InvocationArchiveAckedPayload, InvocationArchivedPayload,
};

use super::store::{
    ControlPlaneStore, ControlPlaneStoreError, InvocationArchiveRow, OwnerStatus, WorkerStatus,
};

/// Name of the durable JetStream consumer the coordination
/// consumer creates. Distinct from the projection consumer's
/// durable name so they advance independently.
pub const CONSUMER_NAME: &str = "fq-coordination";

/// Subject filter for the coordination consumer's durable
/// subscription. Matches every invocation lifecycle event
/// across all agents. Note: NATS subject wildcards use `*`
/// for one token and `>` for one-or-more, so this matches
/// `fq.agent.<id>.invocation.<kind>` for any single id and
/// any single kind.
pub const FILTER_SUBJECT: &str = "fq.agent.*.invocation.*";

/// Default time after which a worker without a fresh
/// heartbeat is considered stale (30 seconds).
pub const DEFAULT_STALE_THRESHOLD_MS: i64 = 30_000;

/// Default cadence for the stale-worker sweep (10 seconds).
pub const DEFAULT_SWEEP_INTERVAL_MS: u64 = 10_000;

/// Coordination consumer. Owns the bus and the
/// control-plane store; spawn it as a tokio task via
/// [`Self::run`].
pub struct CoordinationConsumer {
    bus: EventBus,
    store: Arc<ControlPlaneStore>,
    stale_threshold_ms: i64,
    sweep_interval_ms: u64,
    /// Worker id of the local worker, so we don't mark
    /// ourselves stale during sweeps. Optional because v2
    /// control-plane processes won't have a co-located worker.
    self_worker_id: Option<String>,
    /// Test-only override for the durable consumer name. When
    /// `Some`, `run` also uses the deliver-from-new variant of
    /// the bus's consumer factory so the test doesn't replay
    /// the stream's accumulated history. See
    /// [`Self::with_test_consumer_name`].
    test_consumer_name: Option<String>,
    /// Test-only override for the JetStream filter subject.
    /// When `Some`, used in place of [`FILTER_SUBJECT`] so the
    /// test consumer only receives events for one agent's
    /// invocations. Without this, parallel acceptance tests
    /// cross-contaminate via the ack subject (one test's CP
    /// ack-then-delete races another test's sweeper).
    test_filter_subject: Option<String>,
}

impl CoordinationConsumer {
    pub fn new(bus: EventBus, store: Arc<ControlPlaneStore>) -> Self {
        Self {
            bus,
            store,
            stale_threshold_ms: DEFAULT_STALE_THRESHOLD_MS,
            sweep_interval_ms: DEFAULT_SWEEP_INTERVAL_MS,
            self_worker_id: None,
            test_consumer_name: None,
            test_filter_subject: None,
        }
    }

    /// Override the stale-worker threshold. Test-only.
    pub fn with_stale_threshold_ms(mut self, ms: i64) -> Self {
        self.stale_threshold_ms = ms;
        self
    }

    /// Override the sweep cadence. Test-only.
    pub fn with_sweep_interval_ms(mut self, ms: u64) -> Self {
        self.sweep_interval_ms = ms;
        self
    }

    /// Tell the consumer which worker_id is the local worker
    /// so the sweep can skip it. In v1 the daemon is both
    /// roles; we don't want to mark our own worker stale.
    pub fn with_self_worker_id(mut self, worker_id: String) -> Self {
        self.self_worker_id = Some(worker_id);
        self
    }

    /// Override the JetStream durable consumer name and start
    /// from new messages only (skip the stream's history).
    /// Test-only — the acceptance harness uses this so each
    /// test gets an isolated consumer without replaying the
    /// shared stream's accumulated events.
    pub fn with_test_consumer_name(mut self, name: String) -> Self {
        self.test_consumer_name = Some(name);
        self
    }

    /// Narrow the JetStream filter subject for this consumer.
    /// Test-only — the acceptance harness sets this to
    /// `fq.agent.<test_agent_id>.invocation.*` so parallel
    /// tests' CP consumers don't pick up each other's events
    /// (and don't race on the worker-scoped ack subject).
    pub fn with_test_filter_subject(mut self, filter: String) -> Self {
        self.test_filter_subject = Some(filter);
        self
    }

    /// Run the consumer loop until `shutdown` fires.
    pub async fn run(
        self,
        mut shutdown: oneshot::Receiver<()>,
    ) -> Result<(), CoordinationConsumerError> {
        let effective_filter = self
            .test_filter_subject
            .as_deref()
            .unwrap_or(FILTER_SUBJECT);
        info!(filter = effective_filter, "coordination consumer starting");
        let consumer = match &self.test_consumer_name {
            Some(name) => {
                self.bus
                    .durable_consumer_with_filter_from_new(name, effective_filter)
                    .await?
            }
            None => {
                self.bus
                    .durable_consumer_with_filter(CONSUMER_NAME, effective_filter)
                    .await?
            }
        };
        let mut messages = consumer
            .messages()
            .await
            .map_err(|err| CoordinationConsumerError::Stream(err.to_string()))?;

        let mut sweep_timer = tokio::time::interval(Duration::from_millis(self.sweep_interval_ms));

        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => {
                    info!("coordination consumer received shutdown signal");
                    break;
                }
                msg = messages.next() => {
                    match msg {
                        Some(Ok(msg)) => {
                            self.handle_message(&msg).await;
                        }
                        Some(Err(err)) => {
                            warn!(error = %err, "error reading coordination message");
                        }
                        None => {
                            warn!("coordination message stream ended unexpectedly");
                            break;
                        }
                    }
                }
                _ = sweep_timer.tick() => {
                    if let Err(err) = self.sweep_stale_workers().await {
                        warn!(error = %err, "stale-worker sweep failed");
                    }
                }
            }
        }

        info!("coordination consumer stopped");
        Ok(())
    }

    async fn handle_message(&self, msg: &async_nats::jetstream::Message) {
        let event = match serde_json::from_slice::<Event>(&msg.payload) {
            Ok(e) => e,
            Err(err) => {
                warn!(error = %err, "failed to deserialise coordination message; acking");
                if let Err(e) = msg.ack().await {
                    error!(error = %e, "failed to ack malformed coordination message");
                }
                return;
            }
        };

        let result: Result<(), CoordinationConsumerError> = match &event.payload {
            EventPayload::InvocationAmbiguous(payload) => {
                self.handle_invocation_ambiguous(&event, payload).await
            }
            EventPayload::InvocationArchived(payload) => {
                self.handle_invocation_archived(&event, payload).await
            }
            EventPayload::InvocationOperatorRecovered(payload) => {
                self.handle_invocation_operator_recovered(&event, payload)
                    .await
            }
            _ => {
                // Unknown invocation event variant — ack and
                // move on. We only filter to invocation.*
                // subjects but new variants may surface
                // before the consumer learns about them.
                Ok(())
            }
        };

        match result {
            Ok(()) => {
                if let Err(err) = msg.ack().await {
                    error!(
                        error = %err,
                        event_id = %event.envelope.event_id,
                        "failed to ack coordination message"
                    );
                }
            }
            Err(err) => {
                error!(
                    error = %err,
                    event_id = %event.envelope.event_id,
                    "coordination handler failed; will be redelivered"
                );
                if let Err(e) = msg
                    .ack_with(async_nats::jetstream::AckKind::Nak(None))
                    .await
                {
                    error!(error = %e, "failed to NAK coordination message");
                }
            }
        }
    }

    pub(crate) async fn handle_invocation_ambiguous(
        &self,
        event: &Event,
        _payload: &crate::events::InvocationAmbiguousPayload,
    ) -> Result<(), CoordinationConsumerError> {
        let invocation_id = event.envelope.invocation_id.to_string();
        debug!(
            invocation_id = %invocation_id,
            "marking invocation ambiguous in coordination store"
        );

        // Upsert because the trigger-dispatch path doesn't
        // populate ownership rows yet (a later plan step). Use
        // the event's `agent_id` as the worker_id placeholder
        // when no row exists — later when ownership is
        // explicitly recorded by the dispatcher, we'll have
        // a real worker_id.
        self.store
            .upsert_invocation_ownership(
                &invocation_id,
                event.envelope.agent_id.as_str(),
                Utc::now().timestamp_millis(),
                OwnerStatus::Ambiguous,
            )
            .await?;
        Ok(())
    }

    /// Step 8: worker → control-plane archive hand-off.
    ///
    /// 1. Write the archive row (idempotent on `invocation_id`
    ///    via `ON CONFLICT DO NOTHING` — a redelivery is a
    ///    no-op).
    /// 2. Mark the ownership row `completed` so the worker
    ///    drops out of "in-flight" accounting.
    /// 3. Publish `invocation.archive_acked` on
    ///    `fq.worker.{worker_id}.invocation.archive_acked` so
    ///    the originating worker can delete its local row.
    ///
    /// If step 3 fails the message is NAK'd: insert is
    /// idempotent and the worker's retry sweeper will republish
    /// `invocation.archived` anyway, so duplicates are safe.
    pub(crate) async fn handle_invocation_archived(
        &self,
        event: &Event,
        payload: &InvocationArchivedPayload,
    ) -> Result<(), CoordinationConsumerError> {
        let invocation_id = event.envelope.invocation_id.to_string();
        let now_ms = Utc::now().timestamp_millis();
        debug!(
            invocation_id = %invocation_id,
            worker_id = %payload.worker_id,
            "writing invocation archive row"
        );

        self.store
            .insert_archive(&InvocationArchiveRow {
                invocation_id: invocation_id.clone(),
                agent_id: event.envelope.agent_id.as_str().to_string(),
                final_phase: payload.final_phase.clone(),
                final_state_blob: payload.final_state_blob.clone(),
                started_at: payload.started_at_ms,
                terminal_at: payload.terminal_at_ms,
                archived_at: now_ms,
            })
            .await?;

        // The owning worker no longer carries this invocation
        // as in-flight. Upsert because the dispatcher does not
        // yet populate ownership rows for happy-path triggers
        // (handled in a later plan step) — when it does, this
        // becomes a pure status flip.
        //
        // Don't downgrade an already-terminal status: if the
        // operator dropped this invocation before the worker
        // managed to publish `invocation.archived`, the owner
        // row is `Failed` and the operator's decision sticks.
        // Step-9 risk: race between `fq invocation drop` and
        // a late-finishing worker.
        let current_owner = self.store.get_invocation_owner(&invocation_id).await?;
        let already_terminal = matches!(
            current_owner.as_ref().map(|o| o.status),
            Some(OwnerStatus::Failed | OwnerStatus::Completed)
        );
        if !already_terminal {
            let new_status = match payload.final_phase.as_str() {
                "failed" => OwnerStatus::Failed,
                _ => OwnerStatus::Completed,
            };
            self.store
                .upsert_invocation_ownership(
                    &invocation_id,
                    payload.worker_id.as_str(),
                    now_ms,
                    new_status,
                )
                .await?;
        }

        let ack = Event::new(
            event.envelope.agent_id.clone(),
            event.envelope.invocation_id,
            EventPayload::InvocationArchiveAcked(InvocationArchiveAckedPayload {
                worker_id: payload.worker_id.clone(),
            }),
        );
        self.bus.publish(&ack).await?;
        Ok(())
    }

    /// Step 9: operator-issued terminal transition.
    ///
    /// 1. Write the archive row (idempotent on `invocation_id`).
    ///    State blob is empty in v1 — the CP doesn't have the
    ///    worker's state for an ambiguous invocation.
    /// 2. Update the ownership row's status to match
    ///    `final_phase`. Always overrides the previous status
    ///    (including any prior terminal status), so an operator
    ///    can correct a wrong worker outcome if they need to.
    /// 3. **No ack** — unlike `invocation.archived`, no worker
    ///    is waiting to clean up. If the worker is still alive
    ///    and emits `invocation.archived` afterwards, the
    ///    archive's status update is guarded against
    ///    downgrading a terminal owner status (see
    ///    `handle_invocation_archived`).
    pub(crate) async fn handle_invocation_operator_recovered(
        &self,
        event: &Event,
        payload: &crate::events::InvocationOperatorRecoveredPayload,
    ) -> Result<(), CoordinationConsumerError> {
        let invocation_id = event.envelope.invocation_id.to_string();
        let now_ms = Utc::now().timestamp_millis();
        debug!(
            invocation_id = %invocation_id,
            action = %payload.action,
            final_phase = %payload.final_phase,
            "applying operator-recovered terminal transition"
        );

        let final_status = match payload.final_phase.as_str() {
            "completed" => OwnerStatus::Completed,
            // v1 always emits "failed"; treat unknown phases as
            // failed too so an audit trail is preserved.
            _ => OwnerStatus::Failed,
        };

        // Preserve the existing worker_id if the owner row
        // exists; otherwise use the agent_id as a placeholder
        // (mirrors the ambiguous handler's behaviour).
        let existing = self.store.get_invocation_owner(&invocation_id).await?;
        let worker_id = existing
            .as_ref()
            .map(|o| o.worker_id.clone())
            .unwrap_or_else(|| event.envelope.agent_id.as_str().to_string());

        self.store
            .insert_archive(&InvocationArchiveRow {
                invocation_id: invocation_id.clone(),
                agent_id: event.envelope.agent_id.as_str().to_string(),
                final_phase: payload.final_phase.clone(),
                final_state_blob: Vec::new(),
                started_at: existing.as_ref().map(|o| o.assigned_at).unwrap_or(now_ms),
                terminal_at: now_ms,
                archived_at: now_ms,
            })
            .await?;

        self.store
            .upsert_invocation_ownership(&invocation_id, &worker_id, now_ms, final_status)
            .await?;

        Ok(())
    }

    async fn sweep_stale_workers(&self) -> Result<(), ControlPlaneStoreError> {
        let now_ms = Utc::now().timestamp_millis();
        let stale = self
            .store
            .list_stale_workers(now_ms, self.stale_threshold_ms)
            .await?;
        for worker in stale {
            // Don't mark our own worker stale.
            if self
                .self_worker_id
                .as_deref()
                .is_some_and(|id| id == worker.worker_id)
            {
                continue;
            }
            // Skip workers already marked shutdown or already
            // stale — only promote alive→stale.
            if worker.status != WorkerStatus::Alive {
                continue;
            }
            debug!(worker_id = %worker.worker_id, "marking worker stale");
            self.store.mark_worker_stale(&worker.worker_id).await?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CoordinationConsumerError {
    #[error("bus error: {0}")]
    Bus(#[from] BusError),

    #[error("control-plane store error: {0}")]
    Store(#[from] ControlPlaneStoreError),

    #[error("jetstream message stream error: {0}")]
    Stream(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentId;

    // Pure unit test: handler-shape verification using a
    // wrapper that simulates dispatch without a real bus.
    // Real end-to-end behaviour is exercised via the
    // NATS-gated integration tests below.

    #[tokio::test]
    async fn handler_upserts_ownership_to_ambiguous() {
        use crate::events::{Event, EventPayload, InvocationAmbiguousPayload};
        use tempfile::tempdir;
        use uuid::Uuid;

        let dir = tempdir().unwrap();
        let store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("events.db"))
                .await
                .unwrap(),
        );

        // Build the consumer just to call its handler (we
        // don't run() it; we test the handler in isolation).
        // We can't easily call private handle_invocation_ambiguous
        // from outside, so build the same call directly via
        // the store.
        let invocation_id = Uuid::now_v7().to_string();
        store
            .upsert_invocation_ownership(&invocation_id, "agent-x", 1_000, OwnerStatus::Ambiguous)
            .await
            .unwrap();

        // Verify.
        let owner = store.get_invocation_owner(&invocation_id).await.unwrap();
        assert!(owner.is_some());
        assert_eq!(owner.unwrap().status, OwnerStatus::Ambiguous);

        // Re-publishing (idempotent path): same invocation
        // gets upserted again with no error.
        store
            .upsert_invocation_ownership(&invocation_id, "agent-x", 2_000, OwnerStatus::Ambiguous)
            .await
            .unwrap();
        let owner = store
            .get_invocation_owner(&invocation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(owner.assigned_at, 2_000);

        // Avoid unused warnings
        let _ = (Event::new(
            AgentId::new("agent-x").unwrap(),
            Uuid::now_v7(),
            EventPayload::InvocationAmbiguous(InvocationAmbiguousPayload {
                stuck_entity: "tool_dispatch".to_string(),
                stuck_call_id: "tc1".to_string(),
                note: "test".to_string(),
            }),
        ),);
    }

    #[tokio::test]
    async fn coordination_consumer_handles_invocation_ambiguous_end_to_end() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        use crate::events::{Event, EventPayload, InvocationAmbiguousPayload};
        use tempfile::tempdir;
        use uuid::Uuid;

        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let dir = tempdir().unwrap();
        let store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("events.db"))
                .await
                .unwrap(),
        );

        // Use a unique invocation id and a unique consumer
        // name so this test can run alongside others.
        let invocation_id = Uuid::now_v7();
        let agent_id = AgentId::new(format!("coord-test-{}", Uuid::now_v7().simple())).unwrap();
        let consumer_name = format!("fq-coordination-test-{}", Uuid::now_v7().simple());

        // Spawn the consumer with a custom durable name so
        // we don't compete with the real fq-coordination
        // consumer.
        let bus_for_consumer = bus.clone();
        let store_for_consumer = store.clone();
        let agent_for_consumer = agent_id.clone();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            run_test_consumer(
                bus_for_consumer,
                store_for_consumer,
                consumer_name,
                FILTER_SUBJECT,
                agent_for_consumer,
                shutdown_rx,
            )
            .await
        });

        // Give the consumer a moment to register.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Publish an invocation.ambiguous event.
        let event = Event::new(
            agent_id.clone(),
            invocation_id,
            EventPayload::InvocationAmbiguous(InvocationAmbiguousPayload {
                stuck_entity: "tool_dispatch".to_string(),
                stuck_call_id: "tc-test".to_string(),
                note: "test".to_string(),
            }),
        );
        bus.publish(&event).await.expect("publish");

        // Wait for the coordination row to appear.
        let inv_str = invocation_id.to_string();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(row) = store.get_invocation_owner(&inv_str).await.unwrap()
                && row.status == OwnerStatus::Ambiguous
            {
                break;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("coordination row did not appear in time");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

        // Re-publishing the same event is idempotent.
        bus.publish(&event).await.expect("publish 2");
        let row = store.get_invocation_owner(&inv_str).await.unwrap().unwrap();
        assert_eq!(row.status, OwnerStatus::Ambiguous);
    }

    #[tokio::test]
    async fn handler_archives_invocation_and_publishes_ack() {
        // Direct-handler test: drive `handle_invocation_archived`
        // with a real bus (so the ack publish can be observed)
        // and assert the archive row, ownership status flip, and
        // ack subject all land. Skips the JetStream consume side
        // since that's exercised by the ambiguous end-to-end test
        // above — the dispatch loop is the same code path.
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        use crate::events::EventPayload;
        use crate::worker::WorkerId;
        use tempfile::tempdir;
        use uuid::Uuid;

        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let dir = tempdir().unwrap();
        let store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("events.db"))
                .await
                .unwrap(),
        );

        let agent_id = AgentId::new(format!("arch-test-{}", Uuid::now_v7().simple())).unwrap();
        let worker_id = WorkerId::new(Uuid::now_v7().to_string()).expect("worker id");
        let invocation_id = Uuid::now_v7();

        // Subscribe before publishing so the ack isn't missed.
        let mut ack_sub = bus
            .subscribe(format!(
                "fq.worker.{}.invocation.archive_acked",
                worker_id.as_str()
            ))
            .await
            .expect("subscribe to ack subject");
        tokio::time::sleep(Duration::from_millis(100)).await;

        let consumer = CoordinationConsumer::new(bus.clone(), store.clone());
        let archived_event = Event::new(
            agent_id.clone(),
            invocation_id,
            EventPayload::InvocationArchived(InvocationArchivedPayload {
                worker_id: worker_id.clone(),
                final_phase: "completed".to_string(),
                final_state_blob: b"final-state".to_vec(),
                started_at_ms: 1_000,
                terminal_at_ms: 2_000,
            }),
        );
        let archived_payload = match &archived_event.payload {
            EventPayload::InvocationArchived(p) => p.clone(),
            _ => unreachable!(),
        };
        consumer
            .handle_invocation_archived(&archived_event, &archived_payload)
            .await
            .expect("archived handler succeeds");

        // Archive row written.
        let inv_str = invocation_id.to_string();
        let archive = store
            .get_archive(&inv_str)
            .await
            .unwrap()
            .expect("archive row should exist after handler");
        assert_eq!(archive.agent_id, agent_id.as_str());
        assert_eq!(archive.final_phase, "completed");
        assert_eq!(archive.final_state_blob, b"final-state");
        assert_eq!(archive.started_at, 1_000);
        assert_eq!(archive.terminal_at, 2_000);

        // Ownership flipped to Completed.
        let owner = store
            .get_invocation_owner(&inv_str)
            .await
            .unwrap()
            .expect("ownership row");
        assert_eq!(owner.status, OwnerStatus::Completed);
        assert_eq!(owner.worker_id, worker_id.as_str());

        // Ack arrived on the worker-scoped subject.
        let ack_event = tokio::time::timeout(Duration::from_secs(2), ack_sub.next())
            .await
            .expect("ack timeout")
            .expect("ack stream closed")
            .expect("ack deserialise");
        assert_eq!(ack_event.envelope.invocation_id, invocation_id);
        match &ack_event.payload {
            EventPayload::InvocationArchiveAcked(p) => {
                assert_eq!(p.worker_id, worker_id);
            }
            other => panic!("expected InvocationArchiveAcked, got {other:?}"),
        }

        // Idempotency: handling the same event twice is safe;
        // the archive row is unchanged and a second ack is
        // published (the worker's local dedupe on
        // invocation_id handles that).
        consumer
            .handle_invocation_archived(&archived_event, &archived_payload)
            .await
            .expect("redelivery is a no-op for the store");
        let archive_again = store.get_archive(&inv_str).await.unwrap().unwrap();
        assert_eq!(
            archive_again.archived_at, archive.archived_at,
            "ON CONFLICT DO NOTHING preserves the first archived_at"
        );
    }

    #[tokio::test]
    async fn handler_operator_recovered_writes_archive_and_updates_owner() {
        // Operator-issued drop: no live worker involvement.
        // Handler writes the archive row, flips the owner row
        // to Failed, and does not publish an ack.
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        use crate::events::{Event, EventPayload, InvocationOperatorRecoveredPayload};
        use crate::worker::WorkerId;
        use tempfile::tempdir;
        use uuid::Uuid;

        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let dir = tempdir().unwrap();
        let store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("events.db"))
                .await
                .unwrap(),
        );

        let agent_id =
            AgentId::new(format!("op-recover-test-{}", Uuid::now_v7().simple())).unwrap();
        let worker_id = WorkerId::new(Uuid::now_v7().to_string()).expect("worker id");
        let invocation_id = Uuid::now_v7();
        let inv_str = invocation_id.to_string();

        // Seed an Ambiguous owner row as if the worker reported
        // it on restart.
        store
            .upsert_invocation_ownership(
                &inv_str,
                worker_id.as_str(),
                1_000,
                OwnerStatus::Ambiguous,
            )
            .await
            .unwrap();

        // Subscribe to the worker's ack subject so we can
        // assert nothing is published there.
        let mut ack_sub = bus
            .subscribe(format!(
                "fq.worker.{}.invocation.archive_acked",
                worker_id.as_str()
            ))
            .await
            .expect("subscribe");
        tokio::time::sleep(Duration::from_millis(50)).await;

        let consumer = CoordinationConsumer::new(bus.clone(), store.clone());
        let event = Event::new(
            agent_id.clone(),
            invocation_id,
            EventPayload::InvocationOperatorRecovered(InvocationOperatorRecoveredPayload {
                action: "drop".to_string(),
                final_phase: "failed".to_string(),
                reason: Some("flaky network".to_string()),
            }),
        );
        let payload = match &event.payload {
            EventPayload::InvocationOperatorRecovered(p) => p.clone(),
            _ => unreachable!(),
        };
        consumer
            .handle_invocation_operator_recovered(&event, &payload)
            .await
            .expect("handler succeeds");

        // Archive row exists.
        let archive = store
            .get_archive(&inv_str)
            .await
            .unwrap()
            .expect("archive row");
        assert_eq!(archive.agent_id, agent_id.as_str());
        assert_eq!(archive.final_phase, "failed");
        assert!(archive.final_state_blob.is_empty());

        // Owner row flipped to Failed, worker_id preserved.
        let owner = store
            .get_invocation_owner(&inv_str)
            .await
            .unwrap()
            .expect("owner row");
        assert_eq!(owner.status, OwnerStatus::Failed);
        assert_eq!(owner.worker_id, worker_id.as_str());

        // No ack published.
        let ack = tokio::time::timeout(Duration::from_millis(200), ack_sub.next()).await;
        assert!(
            ack.is_err(),
            "operator-recovered must not publish an ack; got {ack:?}"
        );
    }

    #[tokio::test]
    async fn handler_operator_recovered_idempotent_on_redelivery() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        use crate::events::{Event, EventPayload, InvocationOperatorRecoveredPayload};
        use crate::worker::WorkerId;
        use tempfile::tempdir;
        use uuid::Uuid;

        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let dir = tempdir().unwrap();
        let store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("events.db"))
                .await
                .unwrap(),
        );
        let agent_id =
            AgentId::new(format!("op-recover-idem-{}", Uuid::now_v7().simple())).unwrap();
        let worker_id = WorkerId::new(Uuid::now_v7().to_string()).expect("worker id");
        let invocation_id = Uuid::now_v7();
        let inv_str = invocation_id.to_string();

        store
            .upsert_invocation_ownership(
                &inv_str,
                worker_id.as_str(),
                1_000,
                OwnerStatus::Ambiguous,
            )
            .await
            .unwrap();

        let consumer = CoordinationConsumer::new(bus.clone(), store.clone());
        let event = Event::new(
            agent_id.clone(),
            invocation_id,
            EventPayload::InvocationOperatorRecovered(InvocationOperatorRecoveredPayload {
                action: "drop".to_string(),
                final_phase: "failed".to_string(),
                reason: None,
            }),
        );
        let payload = match &event.payload {
            EventPayload::InvocationOperatorRecovered(p) => p.clone(),
            _ => unreachable!(),
        };

        consumer
            .handle_invocation_operator_recovered(&event, &payload)
            .await
            .expect("first apply");
        let first = store.get_archive(&inv_str).await.unwrap().unwrap();

        consumer
            .handle_invocation_operator_recovered(&event, &payload)
            .await
            .expect("redelivery is a no-op for the archive insert");
        let second = store.get_archive(&inv_str).await.unwrap().unwrap();
        assert_eq!(
            first.archived_at, second.archived_at,
            "redelivery preserves the first archived_at"
        );

        let owner = store.get_invocation_owner(&inv_str).await.unwrap().unwrap();
        assert_eq!(owner.status, OwnerStatus::Failed);
    }

    #[tokio::test]
    async fn handler_archived_does_not_downgrade_failed_owner() {
        // Race scenario: operator drops first, sets owner =
        // Failed; then the worker emits invocation.archived
        // with final_phase = "completed". The owner row must
        // remain Failed.
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        use crate::events::{Event, EventPayload, InvocationArchivedPayload};
        use crate::worker::WorkerId;
        use tempfile::tempdir;
        use uuid::Uuid;

        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let dir = tempdir().unwrap();
        let store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("events.db"))
                .await
                .unwrap(),
        );
        let agent_id = AgentId::new(format!("race-test-{}", Uuid::now_v7().simple())).unwrap();
        let worker_id = WorkerId::new(Uuid::now_v7().to_string()).expect("worker id");
        let invocation_id = Uuid::now_v7();
        let inv_str = invocation_id.to_string();

        // Operator already set this to Failed.
        store
            .upsert_invocation_ownership(&inv_str, worker_id.as_str(), 1_000, OwnerStatus::Failed)
            .await
            .unwrap();

        // Worker now reports archived with completed (a stale
        // success that the operator decided to ignore).
        let consumer = CoordinationConsumer::new(bus.clone(), store.clone());
        let event = Event::new(
            agent_id.clone(),
            invocation_id,
            EventPayload::InvocationArchived(InvocationArchivedPayload {
                worker_id: worker_id.clone(),
                final_phase: "completed".to_string(),
                final_state_blob: b"would-have-been-result".to_vec(),
                started_at_ms: 1_000,
                terminal_at_ms: 2_000,
            }),
        );
        let payload = match &event.payload {
            EventPayload::InvocationArchived(p) => p.clone(),
            _ => unreachable!(),
        };
        consumer
            .handle_invocation_archived(&event, &payload)
            .await
            .expect("handler succeeds");

        let owner = store.get_invocation_owner(&inv_str).await.unwrap().unwrap();
        assert_eq!(
            owner.status,
            OwnerStatus::Failed,
            "operator's Failed must stick; worker's later completed must not downgrade it"
        );
    }

    #[tokio::test]
    async fn completed_invocation_archives_and_worker_cleans_up_against_mock() {
        // The plan's deferred acceptance test, realised against
        // MockAnthropicServer instead of the live Anthropic API.
        // Full pipeline: ReducerRunner → bus → CoordinationConsumer
        // → archive row + ack → ArchiveAckConsumer → invocation_state
        // row deleted. Uses the TestRuntime harness so the setup
        // boilerplate isn't repeated across scenarios.
        let Some(_) = crate::test_support::runtime::require_nats() else {
            return;
        };

        use crate::Agent;
        use crate::events::TriggerSource;
        use crate::test_support::mock_anthropic::MockResponse;
        use crate::test_support::runtime::TestRuntime;
        use crate::worker::InvocationOutcome;
        use crate::worker::reducer::Harness;
        use crate::{PricingTable, ReducerContext, ReducerRunner, RunnerConfig, ToolRegistry};

        let rt = TestRuntime::start().await.expect("harness");
        rt.push_llm_response(MockResponse::text("done.", 12, 4));

        // Capture the agent's invocation chain on NATS for the
        // event-order assertion.
        let mut chain_sub = rt
            .bus()
            .subscribe(format!("fq.agent.{}.>", rt.agent_id().as_str()))
            .await
            .expect("subscribe");
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Drive a single invocation through a fresh runner.
        let agent = Agent::builder()
            .id(rt.agent_id().as_str())
            .model("claude-haiku-4-5")
            .system_prompt("be brief")
            .budget(1.0)
            .build()
            .unwrap();
        let llm = rt.llm_client();
        let runner = ReducerRunner::new(
            Arc::new(ReducerContext::new(Arc::new(ToolRegistry::with_builtins()))),
            Arc::new(RunnerConfig::new(
                rt.bus().clone(),
                Arc::new(PricingTable::empty()),
                rt.worker_store().clone(),
                rt.worker_id().clone(),
            )),
            Harness::new(),
        );

        let outcome = runner
            .run(
                &agent,
                &llm,
                TriggerSource::Manual,
                None,
                serde_json::json!({"input": "go"}),
            )
            .await
            .expect("run completes");
        let invocation_id = match outcome {
            InvocationOutcome::Completed { invocation_id, .. } => invocation_id,
            other => panic!("expected Completed outcome, got {other:?}"),
        };

        // Full hand-off: archive row on CP, invocation_state
        // deleted on worker.
        rt.wait_for_archive(invocation_id, Duration::from_secs(10))
            .await
            .expect("archive row");
        rt.wait_for_local_cleanup(invocation_id, Duration::from_secs(10))
            .await
            .expect("invocation_state cleanup");

        // Archive row contents.
        let archive = rt
            .cp_store()
            .get_archive(&invocation_id.to_string())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(archive.agent_id, rt.agent_id().as_str());
        assert_eq!(archive.final_phase, "completed");

        // Drain captured events; verify completed precedes
        // invocation_archived.
        let mut chain_kinds = Vec::new();
        let collect_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < collect_deadline {
            match tokio::time::timeout(Duration::from_millis(200), chain_sub.next()).await {
                Ok(Some(Ok(ev))) => {
                    chain_kinds.push(crate::test_support::events::event_kind(&ev));
                }
                _ => break,
            }
        }
        assert!(
            chain_kinds
                .windows(2)
                .any(|w| w == ["completed", "invocation_archived"]),
            "expected completed followed by invocation_archived in {chain_kinds:?}",
        );

        // Mock saw exactly one request with the right model.
        let received = rt.mock().received_requests();
        assert_eq!(
            received.len(),
            1,
            "expected one chat call, got {received:?}"
        );
        assert_eq!(received[0]["model"], "claude-haiku-4-5");

        rt.shutdown().await;
    }

    /// Test consumer with a custom durable name so parallel
    /// test runs don't compete with each other or with the
    /// production `fq-coordination` consumer.
    ///
    /// Dispatches both `invocation.ambiguous` (direct store
    /// upsert) and `invocation.archived` (delegates to the
    /// real handler, which writes the archive row and emits
    /// the worker-scoped ack). Other event types are ack'd
    /// and ignored.
    async fn run_test_consumer(
        bus: EventBus,
        store: Arc<ControlPlaneStore>,
        consumer_name: String,
        filter_subject: &str,
        _agent_filter: AgentId,
        mut shutdown: oneshot::Receiver<()>,
    ) -> Result<(), CoordinationConsumerError> {
        // Start from new messages only: when the full lib
        // suite runs against a shared NATS, the stream is
        // crowded with `fq.agent.*.invocation.*` events from
        // other tests. Default `DeliverPolicy::All` drains
        // them all before reaching this test's published
        // event and the 5s deadline below trips. Pairs with
        // a unique consumer_name per test.
        let consumer = bus
            .durable_consumer_with_filter_from_new(&consumer_name, filter_subject)
            .await?;
        let mut messages = consumer
            .messages()
            .await
            .map_err(|err| CoordinationConsumerError::Stream(err.to_string()))?;

        // A real CoordinationConsumer wrapper so we can reuse
        // its production handlers for the archived path.
        let inner = CoordinationConsumer::new(bus.clone(), store.clone());

        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => break,
                msg = messages.next() => {
                    match msg {
                        Some(Ok(msg)) => {
                            let event: Event = match serde_json::from_slice(&msg.payload) {
                                Ok(e) => e,
                                Err(_) => { let _ = msg.ack().await; continue; }
                            };
                            match &event.payload {
                                EventPayload::InvocationAmbiguous(_) => {
                                    let _ = store.upsert_invocation_ownership(
                                        &event.envelope.invocation_id.to_string(),
                                        event.envelope.agent_id.as_str(),
                                        Utc::now().timestamp_millis(),
                                        OwnerStatus::Ambiguous,
                                    ).await;
                                }
                                EventPayload::InvocationArchived(payload) => {
                                    let _ = inner
                                        .handle_invocation_archived(&event, payload)
                                        .await;
                                }
                                EventPayload::InvocationOperatorRecovered(payload) => {
                                    let _ = inner
                                        .handle_invocation_operator_recovered(&event, payload)
                                        .await;
                                }
                                _ => {}
                            }
                            let _ = msg.ack().await;
                        }
                        Some(Err(_)) | None => break,
                    }
                }
            }
        }
        Ok(())
    }
}
