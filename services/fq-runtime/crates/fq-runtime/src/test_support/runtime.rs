//! In-process test harness that boots the full `fq run`
//! runtime against live NATS and [`MockAnthropicServer`].
//!
//! Use it from NATS-gated tests to write acceptance
//! scenarios without re-building the per-test wiring inline.
//! Each [`TestRuntime`] instance gets a unique agent id,
//! worker id, and durable consumer name so multiple tests
//! can run in parallel against a shared NATS without
//! stepping on each other.
//!
//! See `docs/plans/active/2026-05-22-acceptance-harness.md`.
//!
//! # Example
//!
//! ```no_run
//! # use fq_runtime::test_support::runtime::TestRuntime;
//! # use fq_runtime::test_support::mock_anthropic::MockResponse;
//! # tokio_test::block_on(async {
//! let rt = TestRuntime::start().await.expect("harness");
//! rt.push_llm_response(MockResponse::text("done", 12, 4));
//! // ... drive a scenario via rt's accessors and helpers ...
//! rt.shutdown().await;
//! # });
//! ```

use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::agent::AgentId;
use crate::bus::EventBus;
use crate::control_plane::CoordinationConsumer;
use crate::control_plane::projection::ProjectionStore;
use crate::control_plane::store::{ControlPlaneStore, OwnerStatus};
use crate::events::Event;
use crate::llm::GenAiClient;
use crate::test_support::mock_anthropic::{MockAnthropicServer, MockResponse};
use crate::worker::{ArchiveAckConsumer, WorkerId, WorkerStore};

/// Skip the test if `FQ_NATS_URL` isn't set. Returns the URL
/// on success; prints a `skipping:` line and returns `None`
/// otherwise. Test code calls `let Some(url) = ... else { return; };`.
pub fn require_nats() -> Option<String> {
    match std::env::var("FQ_NATS_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("skipping: FQ_NATS_URL not set");
            None
        }
    }
}

/// Builder for [`TestRuntime`]. Defaults give you the
/// "happy path" harness (coordination consumer + ack
/// consumer + sane thresholds).
pub struct TestRuntimeBuilder {
    with_coordination: bool,
    stale_threshold_ms: i64,
    sweep_interval_ms: u64,
}

impl Default for TestRuntimeBuilder {
    fn default() -> Self {
        Self {
            with_coordination: true,
            stale_threshold_ms: 30_000,
            sweep_interval_ms: 10_000,
        }
    }
}

impl TestRuntimeBuilder {
    /// If `false`, skip spawning the `CoordinationConsumer`.
    /// Used by scenarios that exercise the worker-side retry
    /// path with no consumer present.
    pub fn with_coordination(mut self, on: bool) -> Self {
        self.with_coordination = on;
        self
    }

    /// Override the stale-worker threshold (used by the
    /// coordination consumer's stale-sweep). Default 30s.
    pub fn stale_threshold_ms(mut self, ms: i64) -> Self {
        self.stale_threshold_ms = ms;
        self
    }

    /// Override the stale-sweep cadence (default 10s).
    pub fn sweep_interval_ms(mut self, ms: u64) -> Self {
        self.sweep_interval_ms = ms;
        self
    }

    pub async fn start(self) -> Result<TestRuntime, String> {
        let Some(url) = require_nats() else {
            return Err("FQ_NATS_URL not set".to_string());
        };

        // genai's auth resolver demands the env var even
        // though the mock ignores the bearer.
        // Safety: tests share a process, but this value is
        // benign (no real API call).
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-mock-not-real") };

        let mock = MockAnthropicServer::start().await;

        let bus = EventBus::connect(&url)
            .await
            .map_err(|e| format!("EventBus::connect: {e}"))?;

        let agent_id = AgentId::new(format!("e2e-agent-{}", Uuid::now_v7().simple()))
            .map_err(|e| format!("agent id: {e}"))?;
        let worker_id = WorkerId::new(format!("e2e-worker-{}", Uuid::now_v7().simple()))
            .map_err(|e| format!("worker id: {e}"))?;

        // Control-plane store and projection store share a
        // SQLite file in production (`show_status` opens both
        // on the same path); mirror that here so the harness
        // reflects the real layout.
        let cp_dir = tempfile::tempdir().map_err(|e| format!("cp tempdir: {e}"))?;
        let cp_path = cp_dir.path().join("cp.db");
        let cp_store = Arc::new(
            ControlPlaneStore::open(&cp_path)
                .await
                .map_err(|e| format!("ControlPlaneStore::open: {e}"))?,
        );
        let proj_store = Arc::new(
            ProjectionStore::open(&cp_path)
                .await
                .map_err(|e| format!("ProjectionStore::open: {e}"))?,
        );

        let worker_dir = tempfile::tempdir().map_err(|e| format!("worker tempdir: {e}"))?;
        let worker_store = Arc::new(
            WorkerStore::open(&worker_dir.path().join("worker.db"))
                .await
                .map_err(|e| format!("WorkerStore::open: {e}"))?,
        );

        // Spawn the optional CoordinationConsumer.
        let mut cp_shutdown_tx: Option<oneshot::Sender<()>> = None;
        let mut cp_handle: Option<JoinHandle<()>> = None;
        if self.with_coordination {
            let (tx, rx) = oneshot::channel();
            let consumer_name = format!("fq-coordination-e2e-{}", Uuid::now_v7().simple());
            let consumer = CoordinationConsumer::new(bus.clone(), cp_store.clone())
                .with_test_consumer_name(consumer_name)
                .with_stale_threshold_ms(self.stale_threshold_ms)
                .with_sweep_interval_ms(self.sweep_interval_ms);
            cp_shutdown_tx = Some(tx);
            cp_handle = Some(tokio::spawn(async move {
                let _ = consumer.run(rx).await;
            }));
        }

        // ArchiveAckConsumer.
        let (ack_tx, ack_rx) = oneshot::channel();
        let ack_consumer =
            ArchiveAckConsumer::new(bus.clone(), worker_id.clone(), worker_store.clone());
        let ack_handle = tokio::spawn(async move {
            let _ = ack_consumer.run(ack_rx).await;
        });

        // Let any spawned consumers register before the test
        // starts publishing.
        tokio::time::sleep(Duration::from_millis(200)).await;

        Ok(TestRuntime {
            bus,
            cp_store,
            proj_store,
            worker_store,
            mock,
            agent_id,
            worker_id,
            cp_shutdown_tx,
            cp_handle,
            ack_shutdown_tx: Some(ack_tx),
            ack_handle: Some(ack_handle),
            _cp_dir: cp_dir,
            _worker_dir: worker_dir,
        })
    }
}

/// In-process acceptance-test harness. See module docs.
pub struct TestRuntime {
    bus: EventBus,
    cp_store: Arc<ControlPlaneStore>,
    proj_store: Arc<ProjectionStore>,
    worker_store: Arc<WorkerStore>,
    mock: MockAnthropicServer,
    agent_id: AgentId,
    worker_id: WorkerId,
    cp_shutdown_tx: Option<oneshot::Sender<()>>,
    cp_handle: Option<JoinHandle<()>>,
    ack_shutdown_tx: Option<oneshot::Sender<()>>,
    ack_handle: Option<JoinHandle<()>>,
    _cp_dir: TempDir,
    _worker_dir: TempDir,
}

impl TestRuntime {
    /// Boot the harness with default options.
    pub async fn start() -> Result<Self, String> {
        TestRuntimeBuilder::default().start().await
    }

    /// Builder entry point for variant configurations.
    pub fn builder() -> TestRuntimeBuilder {
        TestRuntimeBuilder::default()
    }

    /// The live NATS event bus the harness is using.
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    /// The control-plane store (read/write). Tests assert on
    /// archive rows and coordination ownership via this.
    pub fn cp_store(&self) -> &Arc<ControlPlaneStore> {
        &self.cp_store
    }

    /// The projection store (read/write). Tests seed events
    /// here (via [`Self::seed_projection_event`]) so the
    /// `agent_id_for_invocation` lookup in
    /// `control_plane::operator::drop_invocation` resolves.
    pub fn proj_store(&self) -> &Arc<ProjectionStore> {
        &self.proj_store
    }

    /// The worker store (read/write). Tests assert on
    /// `invocation_state` rows here.
    pub fn worker_store(&self) -> &Arc<WorkerStore> {
        &self.worker_store
    }

    /// Seed an event into the projection store so subsequent
    /// agent-id lookups (e.g. by
    /// `control_plane::operator::drop_invocation`) succeed.
    pub async fn seed_projection_event(&self, event: &Event) -> Result<(), String> {
        self.proj_store
            .insert_event(event)
            .await
            .map_err(|e| format!("insert_event: {e}"))
    }

    /// The mock Anthropic server. Push canned responses with
    /// `mock.push_response(MockResponse::text(...))`.
    pub fn mock(&self) -> &MockAnthropicServer {
        &self.mock
    }

    /// Unique agent id for this test run.
    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    /// Unique worker id for this test run.
    pub fn worker_id(&self) -> &WorkerId {
        &self.worker_id
    }

    /// Convenience: build a `GenAiClient` pointed at the
    /// harness's mock.
    pub fn llm_client(&self) -> GenAiClient {
        GenAiClient::with_base_url(self.mock.base_url())
    }

    /// Convenience: push a response to the mock's queue.
    pub fn push_llm_response(&self, r: MockResponse) {
        self.mock.push_response(r);
    }

    /// Poll the control-plane store for an `invocation_archive`
    /// row matching `invocation_id` until it appears or the
    /// timeout elapses. Returns `Ok(())` on success, an `Err`
    /// describing what was missing on timeout.
    pub async fn wait_for_archive(
        &self,
        invocation_id: Uuid,
        timeout: Duration,
    ) -> Result<(), String> {
        let inv_str = invocation_id.to_string();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match self.cp_store.get_archive(&inv_str).await {
                Ok(Some(_)) => return Ok(()),
                Ok(None) => {}
                Err(e) => return Err(format!("get_archive error: {e}")),
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "archive row not found for {inv_str} within {:?}",
                    timeout
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Poll the worker store for an `invocation_state` row
    /// matching `invocation_id` to be removed (cleaned up
    /// after the archive ack).
    pub async fn wait_for_local_cleanup(
        &self,
        invocation_id: Uuid,
        timeout: Duration,
    ) -> Result<(), String> {
        let inv_str = invocation_id.to_string();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match self.worker_store.get_invocation_state(&inv_str).await {
                Ok(None) => return Ok(()),
                Ok(Some(_)) => {}
                Err(e) => return Err(format!("get_invocation_state error: {e}")),
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "invocation_state row still present for {inv_str} after {:?}",
                    timeout
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Poll the coordination store for an owner row to reach
    /// a specific status.
    pub async fn wait_for_owner_status(
        &self,
        invocation_id: Uuid,
        expected: OwnerStatus,
        timeout: Duration,
    ) -> Result<(), String> {
        let inv_str = invocation_id.to_string();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match self.cp_store.get_invocation_owner(&inv_str).await {
                Ok(Some(row)) if row.status == expected => return Ok(()),
                Ok(_) => {}
                Err(e) => return Err(format!("get_invocation_owner error: {e}")),
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "owner status for {inv_str} did not reach {expected:?} within {:?}",
                    timeout
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Stop every spawned component and wait for their tasks
    /// to drain. Safe to call multiple times (subsequent calls
    /// are no-ops).
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.cp_shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(tx) = self.ack_shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.cp_handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), h).await;
        }
        if let Some(h) = self.ack_handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), h).await;
        }
        self.mock.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn harness_starts_and_shuts_down_cleanly() {
        let Some(_) = require_nats() else {
            return;
        };
        let rt = TestRuntime::start().await.expect("harness");
        assert!(!rt.agent_id().as_str().is_empty());
        assert!(!rt.worker_id().as_str().is_empty());
        assert!(rt.mock().base_url().starts_with("http://"));
        rt.shutdown().await;
    }

    #[tokio::test]
    async fn harness_without_coordination_starts_and_shuts_down() {
        let Some(_) = require_nats() else {
            return;
        };
        let rt = TestRuntime::builder()
            .with_coordination(false)
            .start()
            .await
            .expect("harness");
        rt.shutdown().await;
    }

    #[tokio::test]
    async fn drop_ambiguous_terminates_invocation_end_to_end() {
        // Step 9's deferred acceptance test, end-to-end via
        // the harness: seed an ambiguous owner row + a
        // projection event for the agent lookup; call
        // operator::drop_invocation; assert the coordination
        // consumer flips the owner to Failed, writes the
        // archive row, and the audit chain shows the
        // operator_recovered event with our reason.
        let Some(_) = require_nats() else {
            return;
        };

        use crate::control_plane::operator;
        use crate::events::{EventPayload, InvocationAmbiguousPayload};
        use futures::StreamExt;

        let rt = TestRuntime::start().await.expect("harness");

        let invocation_id = Uuid::now_v7();
        let inv_str = invocation_id.to_string();

        // Subscribe to the operator_recovered subject BEFORE
        // we publish so we don't miss the event.
        let mut audit_sub = rt
            .bus()
            .subscribe(format!(
                "fq.agent.{}.invocation.operator_recovered",
                rt.agent_id().as_str()
            ))
            .await
            .expect("subscribe");
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Seed an Ambiguous owner row (as if the worker had
        // emitted invocation.ambiguous on restart).
        rt.cp_store()
            .upsert_invocation_ownership(
                &inv_str,
                rt.worker_id().as_str(),
                1_000,
                OwnerStatus::Ambiguous,
            )
            .await
            .expect("seed owner");

        // Seed a projection event so operator::drop_invocation's
        // agent_id lookup resolves. The InvocationAmbiguous
        // event is the natural seed — it's what the worker
        // would have published.
        let ambiguous_event = Event::new(
            rt.agent_id().clone(),
            invocation_id,
            EventPayload::InvocationAmbiguous(InvocationAmbiguousPayload {
                stuck_entity: "tool_dispatch".to_string(),
                stuck_call_id: "tc-1".to_string(),
                note: "seeded for test".to_string(),
            }),
        );
        rt.seed_projection_event(&ambiguous_event)
            .await
            .expect("seed projection");

        // Execute the operator action.
        let result = operator::drop_invocation(
            rt.bus(),
            rt.proj_store(),
            &inv_str,
            Some("e2e drop scenario"),
        )
        .await
        .expect("drop_invocation");
        assert_eq!(result.agent_id, rt.agent_id().as_str());
        assert_eq!(result.reason.as_deref(), Some("e2e drop scenario"));

        // Owner row reaches Failed (CP handler must process
        // the event off the JetStream consumer).
        rt.wait_for_owner_status(invocation_id, OwnerStatus::Failed, Duration::from_secs(5))
            .await
            .expect("owner flipped to Failed");

        // Archive row exists with our final_phase.
        rt.wait_for_archive(invocation_id, Duration::from_secs(2))
            .await
            .expect("archive row");
        let archive = rt.cp_store().get_archive(&inv_str).await.unwrap().unwrap();
        assert_eq!(archive.final_phase, "failed");
        assert_eq!(archive.agent_id, rt.agent_id().as_str());

        // Audit chain has exactly one operator_recovered with
        // the reason carried through.
        let audit_event = tokio::time::timeout(Duration::from_secs(2), audit_sub.next())
            .await
            .expect("audit timeout")
            .expect("audit stream closed")
            .expect("audit deserialise");
        assert_eq!(audit_event.envelope.invocation_id, invocation_id);
        match &audit_event.payload {
            EventPayload::InvocationOperatorRecovered(p) => {
                assert_eq!(p.action, "drop");
                assert_eq!(p.final_phase, "failed");
                assert_eq!(p.reason.as_deref(), Some("e2e drop scenario"));
            }
            other => panic!("expected InvocationOperatorRecovered, got {other:?}"),
        }

        rt.shutdown().await;
    }

    #[tokio::test]
    async fn stale_worker_marked_stale_within_threshold() {
        // Step 7's deferred acceptance test, end-to-end:
        // a registered worker that stops heartbeating gets
        // flipped to Stale by the coordination consumer's
        // sweep within the configured window.
        let Some(_) = require_nats() else {
            return;
        };

        use crate::control_plane::store::WorkerStatus;
        use chrono::Utc;

        // Short threshold + short sweep cadence so the test
        // wraps quickly.
        let rt = TestRuntime::builder()
            .stale_threshold_ms(500)
            .sweep_interval_ms(100)
            .start()
            .await
            .expect("harness");

        // Register a separate worker (not the harness's own
        // worker_id; we don't want the self-worker-skip rule
        // to interfere).
        let stale_candidate = format!("stale-test-{}", Uuid::now_v7().simple());
        rt.cp_store()
            .register_worker(&stale_candidate, "test-host", Utc::now().timestamp_millis())
            .await
            .expect("register worker");

        // Wait for: threshold (500ms) + sweep cadence (100ms)
        // + slack. 1.5s is plenty.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let worker = rt
                .cp_store()
                .get_worker(&stale_candidate)
                .await
                .expect("get_worker")
                .expect("worker row exists");
            if worker.status == WorkerStatus::Stale {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!(
                    "worker {stale_candidate} not stale after {:?}; status={:?}, last_heartbeat={}",
                    Duration::from_secs(2),
                    worker.status,
                    worker.last_heartbeat
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        rt.shutdown().await;
    }
}
