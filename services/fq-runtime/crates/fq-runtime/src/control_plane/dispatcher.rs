//! NATS-triggered agent dispatcher.
//!
//! Sits on the `fq.trigger.>` JetStream stream and dispatches each
//! incoming message to the appropriate agent via the executor.
//! Runs as a long-lived tokio task inside `fq run`, alongside the
//! projection consumer.
//!
//! # Delivery semantics
//!
//! - Work-queue: each trigger is delivered to exactly one consumer
//!   and deleted after ack. There is no replay of already-processed
//!   triggers on restart.
//! - Durable consumer: if the dispatcher crashes or is restarted,
//!   JetStream remembers its position and redelivers any unacked
//!   triggers after the ack deadline.
//! - Ack-after-durable-start: the trigger is acked once the
//!   invocation has *durably started* — signalled through the
//!   [`Worker`] seam after its first WAL write — not at dispatch and
//!   not at completion (issue #41). From the first WAL write on,
//!   in-flight durability is the reducer WAL's job
//!   (`recovery::scan_in_flight`), so a crash resumes the invocation
//!   exactly once from the WAL with no redelivered duplicate. This
//!   closes both failure windows: a crash *before* the first WAL write
//!   leaves the trigger unacked, so JetStream redelivers it and the
//!   otherwise-missed run recovers; and the ack still fires at the
//!   first WAL write (seconds in), *well before* completion, so a
//!   long-running invocation is not redelivered and re-run past the
//!   30s ack-wait — the redelivery storm the M0 dogfood loop found
//!   (2026-07-06), where an invocation longer than the ack-wait was
//!   re-run, stays fixed.
//!
//! # Error handling
//!
//! Most errors are **acked, not NAK'd**: unknown agent ids, invalid
//! JSON payloads, and executor errors are all permanent problems
//! that retrying would not fix. We log and move on. The only
//! situations that intentionally propagate are the bus/consumer
//! itself failing (a bigger problem) and receive-side protocol
//! errors.
//!
//! An executor error already produces a `Failed` event on the
//! event stream, so downstream consumers (the projection, tailers)
//! see the failure even though the trigger is acked.

use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::{RwLock, oneshot};
use tracing::{debug, error, info, warn};

use crate::agent::{AgentId, AgentRegistry};
use crate::bus::{BusError, EventBus, agent_id_from_trigger_subject};
use crate::events::TriggerSource;
use crate::llm::LlmClient;
use crate::worker::{DrainState, DurableStart, ExecutorError, Worker};

/// Name of the durable JetStream consumer the dispatcher creates.
pub const CONSUMER_NAME: &str = "fq-dispatcher";

/// Shared, hot-swappable agent registry — the manual equivalent of
/// `ArcSwap` (which isn't a dependency in this tree). The nested
/// `Arc`s are not a bug; each layer has a distinct job:
///
/// - **outer `Arc`** — shares the one `RwLock` across the tasks that
///   hold it (the dispatcher and the reload listener); `tokio::spawn`'d
///   tasks need owned `'static` handles.
/// - **`RwLock`** — lets `fq reload` swap the registry while the
///   dispatcher reads it.
/// - **inner `Arc<AgentRegistry>`** — lets a reader snapshot the
///   current registry with an O(1) refcount bump and drop the lock
///   immediately (see `read().await.clone()` at the read site), rather
///   than holding the lock across a whole (unbounded) invocation or
///   deep-cloning the registry on every trigger.
///
/// The dispatcher reads through this handle on every trigger, so a
/// hot-reload atomically swaps the inner `Arc` for a freshly-loaded one
/// and the *next* trigger picks it up. In-flight invocations already
/// hold their own `Agent` clone (snapshotted at trigger time), so a
/// swap never disturbs them — matching the ADR-0020
/// refresh-between-invocations precedent.
pub type SharedRegistry = Arc<RwLock<Arc<AgentRegistry>>>;

/// Wrap an owned registry in a fresh [`SharedRegistry`] handle.
pub fn shared_registry(registry: AgentRegistry) -> SharedRegistry {
    Arc::new(RwLock::new(Arc::new(registry)))
}

/// NATS-triggered dispatcher. Owns references to the pieces of the
/// runtime it needs — call [`TriggerDispatcher::run`] to drive it.
///
/// The dispatcher lives on the control-plane side of the role
/// boundary; it talks to workers exclusively through the
/// [`Worker`] trait. v1 hands it an in-process worker
/// (`Arc::new(ReducerRunner::new(...))`); v2 will hand it a
/// remote-worker adapter that proxies over NATS.
pub struct TriggerDispatcher {
    bus: EventBus,
    registry: SharedRegistry,
    worker: Arc<dyn Worker>,
    llm: Arc<dyn LlmClient>,
}

impl TriggerDispatcher {
    pub fn new(
        bus: EventBus,
        registry: SharedRegistry,
        worker: Arc<dyn Worker>,
        llm: Arc<dyn LlmClient>,
    ) -> Self {
        Self {
            bus,
            registry,
            worker,
            llm,
        }
    }

    /// Run the dispatcher loop until `shutdown` fires.
    pub async fn run(self, mut shutdown: oneshot::Receiver<()>) -> Result<(), DispatcherError> {
        info!("trigger dispatcher starting");
        let consumer = self.bus.trigger_consumer(CONSUMER_NAME).await?;
        let mut messages = consumer
            .messages()
            .await
            .map_err(|err| DispatcherError::Stream(err.to_string()))?;

        loop {
            // ADR-0027 graceful drain: once the worker is draining, stop
            // pulling new triggers. In-flight invocations suspend at their
            // next step boundary via the shared drain signal (the reducer
            // polls it); un-pulled triggers stay queued on the durable
            // work-queue consumer for the next binary. Final teardown
            // still arrives through `shutdown`.
            if self.worker.drain_status() == DrainState::Draining {
                info!("trigger dispatcher draining — no longer consuming new triggers");
                break;
            }

            tokio::select! {
                biased;
                _ = &mut shutdown => {
                    info!("trigger dispatcher received shutdown signal");
                    break;
                }
                msg = messages.next() => {
                    match msg {
                        Some(Ok(msg)) => {
                            self.handle(&msg).await;
                        }
                        Some(Err(err)) => {
                            warn!(error = %err, "error reading next JetStream trigger");
                        }
                        None => {
                            warn!("trigger stream ended unexpectedly");
                            break;
                        }
                    }
                }
            }
        }

        info!("trigger dispatcher stopped");
        Ok(())
    }

    async fn handle(&self, msg: &async_nats::jetstream::Message) {
        // A drain requested after this trigger was pulled but before it
        // was dispatched: leave it un-acked so it redelivers to the next
        // binary rather than starting an invocation that would only
        // suspend at step 0. The common case — a drain between
        // invocations — is caught by the consume loop's top-of-loop
        // check; this closes the pulled-just-as-drain-lands race.
        if self.worker.drain_status() == DrainState::Draining {
            debug!(
                subject = %msg.subject,
                "draining — leaving trigger queued for the next binary"
            );
            return;
        }

        // Parse the agent id out of the subject. Invalid format →
        // ack and drop (redelivery won't help).
        let agent_id_str = match agent_id_from_trigger_subject(&msg.subject) {
            Some(id) => id.to_string(),
            None => {
                warn!(
                    subject = %msg.subject,
                    "trigger with unexpected subject format, dropping"
                );
                self.ack(msg, "bad subject").await;
                return;
            }
        };

        // Validate and look up the agent.
        let agent_id = match AgentId::new(&agent_id_str) {
            Ok(id) => id,
            Err(err) => {
                warn!(
                    agent_id = %agent_id_str,
                    error = %err,
                    "trigger for invalid agent id, dropping"
                );
                self.ack(msg, "invalid agent id").await;
                return;
            }
        };
        // Read the registry through the swappable handle. Cloning the
        // inner Arc under a short read lock gives this invocation a
        // stable snapshot for its whole lifetime: a concurrent reload
        // that swaps in a new Arc does not disturb an in-flight run
        // (ADR-0020 refresh-between-invocations).
        let registry = self.registry.read().await.clone();
        let loaded = match registry.get_loaded(&agent_id) {
            Some(loaded) => loaded,
            None => {
                warn!(
                    agent_id = %agent_id,
                    "trigger for unknown agent, dropping"
                );
                self.ack(msg, "unknown agent").await;
                return;
            }
        };

        // Parse the payload as JSON. Empty body becomes null.
        let payload: serde_json::Value = if msg.payload.is_empty() {
            serde_json::Value::Null
        } else {
            match serde_json::from_slice(&msg.payload) {
                Ok(v) => v,
                Err(err) => {
                    warn!(
                        agent_id = %agent_id,
                        error = %err,
                        "trigger payload is not valid JSON, dropping"
                    );
                    self.ack(msg, "invalid payload").await;
                    return;
                }
            }
        };

        debug!(
            agent_id = %agent_id,
            subject = %msg.subject,
            "dispatching trigger"
        );

        // Ack the trigger once the invocation has *durably started* —
        // signalled through the Worker seam after its first WAL write —
        // rather than at dispatch (issue #41). The trigger's job ends
        // once the run is recoverable: from the first WAL write on, the
        // reducer's three-state WAL owns in-flight durability and crash
        // recovery (`recovery::scan_in_flight` → `categorise` → resume),
        // so a crash resumes the invocation exactly once from the WAL
        // with no redelivered duplicate.
        //
        // Two failure windows this closes / preserves:
        //
        // - **Before durable start** — a crash here leaves the trigger
        //   unacked, so JetStream redelivers it after the ack-wait,
        //   recovering the otherwise-missed run. (This is the gap the
        //   old ack-on-dispatch left: a crash between dispatch and the
        //   first WAL write was a missed, re-triggerable run.)
        //
        // - **After durable start, before completion** — the ack fires
        //   at the first WAL write (seconds in), *well before* the
        //   invocation completes, so a long-running invocation is not
        //   redelivered and re-run past the 30s ack-wait. This preserves
        //   the redelivery-storm fix (M0 dogfood loop, 2026-07-06):
        //   holding the ack until completion re-ran invocations longer
        //   than the ack-wait — one trigger produced N invocations.
        //
        // The signal is fired at most once; if the invocation returns
        // before firing it (a permanent error before any WAL write, or a
        // worker that never signals), we ack on return — retrying a
        // permanent error would not help, and the run already happened.
        let (durable_start, mut durably_started) = DurableStart::channel();
        let mut invocation = std::pin::pin!(self.worker.run_invocation(
            &loaded.agent,
            self.llm.as_ref(),
            TriggerSource::Subject,
            Some(msg.subject.to_string()),
            payload,
            durable_start,
        ));

        let mut acked = false;
        let result = loop {
            tokio::select! {
                biased;
                signal = &mut durably_started, if !acked => {
                    // `Ok` = fired (first WAL write landed); `Err` = the
                    // sender was dropped without firing (the invocation
                    // returned before its first WAL write). Either way,
                    // stop waiting on this branch; the ack below (on
                    // return) covers the drop case.
                    if signal.is_ok() {
                        self.ack(msg, "durably started").await;
                        acked = true;
                    }
                }
                outcome = &mut invocation => break outcome,
            }
        };

        // The invocation returned before the durable-start signal fired,
        // so the run left nothing durable behind. Decide the trigger's
        // fate from the result:
        //
        // - **Transient** error (bus/store unavailable, a transient LLM
        //   error) — the run could succeed on a retry, so NAK: JetStream
        //   redelivers the trigger, recovering the otherwise-lost run.
        // - **Permanent** error (a terminal `failed` outcome / poison
        //   payload) or a clean `Ok` return — nothing to retry, so ACK.
        //   The `Failed` event already recorded why, and redelivering a
        //   poison trigger would loop under the consumer's unbounded
        //   redelivery.
        //
        // Redelivery is unbounded today; bounding it (`max_deliver`) and
        // dead-lettering exhausted triggers is tracked in #49.
        if !acked {
            match &result {
                Err(err) if err.is_transient() => {
                    self.nak(msg, "transient failure before first WAL write")
                        .await;
                }
                _ => {
                    self.ack(msg, "invocation returned before durable start (permanent)")
                        .await;
                }
            }
        }

        if let Err(err) = result {
            // The executor already emitted a Failed event; the trigger is
            // acked and the WAL owns recovery, so there is nothing to
            // redeliver.
            warn!(
                agent_id = %agent_id,
                error = %err,
                "executor returned an error for NATS-triggered run"
            );
            self.log_executor_error(&err);
        }
    }

    async fn ack(&self, msg: &async_nats::jetstream::Message, context: &str) {
        if let Err(err) = msg.ack().await {
            error!(
                error = %err,
                context,
                "failed to ack trigger message"
            );
        }
    }

    /// NAK a trigger so JetStream redelivers it after the ack-wait — used
    /// when an invocation fails *before its first WAL write* with a
    /// transient error, so the otherwise-lost run is retried. Redelivery
    /// is unbounded today; bounding it + dead-lettering is tracked in #49.
    async fn nak(&self, msg: &async_nats::jetstream::Message, context: &str) {
        if let Err(err) = msg
            .ack_with(async_nats::jetstream::AckKind::Nak(None))
            .await
        {
            error!(
                error = %err,
                context,
                "failed to NAK trigger message"
            );
        } else {
            warn!(context, "NAK'd trigger for redelivery");
        }
    }

    fn log_executor_error(&self, err: &ExecutorError) {
        match err {
            ExecutorError::Llm(e) => error!(error = %e, "llm error during dispatch"),
            ExecutorError::Bus(e) => error!(error = %e, "bus error during dispatch"),
            ExecutorError::WorkerStore(msg) => {
                error!(error = %msg, "worker store error during dispatch")
            }
            ExecutorError::Workspace(e) => {
                error!(error = %e, "workspace error during dispatch")
            }
            ExecutorError::InvocationFailed { kind, message } => {
                error!(kind = ?kind, error = %message, "invocation failed during dispatch")
            }
        }
    }
}

/// Errors that prevent the dispatcher from starting or progressing.
#[derive(Debug, thiserror::Error)]
pub enum DispatcherError {
    #[error("bus error: {0}")]
    Bus(#[from] BusError),

    #[error("trigger stream error: {0}")]
    Stream(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, Sandbox};
    use crate::events::{StopReason, TokenUsage};
    use crate::llm::ChatResponse;
    use crate::llm::fixture::FixtureClient;
    use crate::pricing::{ModelPricing, PricingTable};
    use crate::tools::ToolRegistry;
    use crate::worker::{
        Harness, ReducerContext, ReducerRunner, RunnerConfig, WorkerId, WorkerStore,
    };
    use serde_json::json;
    use std::collections::HashMap;
    use std::time::Duration;
    use uuid::Uuid;

    fn test_pricing() -> Arc<PricingTable> {
        let mut entries = HashMap::new();
        entries.insert(
            "claude-haiku".to_string(),
            ModelPricing {
                input_per_million: 1.0,
                output_per_million: 5.0,
                cache_read_per_million: None,
                cache_write_per_million: None,
            },
        );
        Arc::new(PricingTable::from_map(entries))
    }

    fn test_tools() -> Arc<ToolRegistry> {
        Arc::new(ToolRegistry::with_builtins())
    }

    fn sample_agent(name: &str) -> Agent {
        Agent::builder()
            .id(name)
            .model("claude-haiku")
            .system_prompt("You are a test agent.")
            .sandbox(Sandbox::new())
            .budget(1.0)
            .build()
            .unwrap()
    }

    fn canned_response() -> ChatResponse {
        ChatResponse {
            content: Some("Hello from the test agent.".to_string()),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        }
    }

    fn unique_agent_id(prefix: &str) -> String {
        format!("{prefix}-{}", Uuid::now_v7().simple())
    }

    fn unique_consumer_name() -> String {
        format!("fq-dispatcher-test-{}", Uuid::now_v7().simple())
    }

    /// A variant of TriggerDispatcher that uses a custom consumer
    /// name AND a narrow filter subject so parallel test runs do
    /// not compete for each other's messages on the work-queue
    /// trigger stream.
    struct TestDispatcher {
        bus: EventBus,
        registry: SharedRegistry,
        worker: Arc<dyn Worker>,
        llm: Arc<dyn LlmClient>,
        consumer_name: String,
        filter_subject: String,
    }

    impl TestDispatcher {
        async fn run(self, mut shutdown: oneshot::Receiver<()>) -> Result<(), DispatcherError> {
            let consumer = self
                .bus
                .trigger_consumer_with_filter(&self.consumer_name, &self.filter_subject)
                .await?;
            let mut messages = consumer
                .messages()
                .await
                .map_err(|err| DispatcherError::Stream(err.to_string()))?;

            let dispatcher = TriggerDispatcher::new(
                self.bus.clone(),
                self.registry.clone(),
                self.worker.clone(),
                self.llm.clone(),
            );

            loop {
                // Mirror `TriggerDispatcher::run`: stop consuming on drain.
                if self.worker.drain_status() == DrainState::Draining {
                    break;
                }
                tokio::select! {
                    biased;
                    _ = &mut shutdown => break,
                    msg = messages.next() => {
                        match msg {
                            Some(Ok(msg)) => dispatcher.handle(&msg).await,
                            Some(Err(_)) | None => break,
                        }
                    }
                }
            }
            Ok(())
        }
    }

    #[test]
    fn agent_id_from_trigger_subject_happy_path() {
        assert_eq!(
            agent_id_from_trigger_subject("fq.trigger.researcher"),
            Some("researcher")
        );
        assert_eq!(
            agent_id_from_trigger_subject("fq.trigger.some-agent-id-123"),
            Some("some-agent-id-123")
        );
    }

    #[test]
    fn agent_id_from_trigger_subject_rejects_unexpected_prefix() {
        assert!(agent_id_from_trigger_subject("fq.agent.researcher.triggered").is_none());
        assert!(agent_id_from_trigger_subject("bad.prefix.name").is_none());
        assert!(agent_id_from_trigger_subject("fq.trigger.").is_none());
    }

    /// End-to-end: publish a trigger to NATS, run the dispatcher,
    /// verify that the agent's events appear in the event stream.
    #[tokio::test]
    async fn dispatcher_executes_published_trigger() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };
        use crate::bus::EventBus;
        use crate::control_plane::projection::store::{EventFilter, ProjectionStore};

        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let agent_id_str = unique_agent_id("dispatch-test");

        // Build an in-memory registry with a single agent.
        let mut registry = AgentRegistry::new();
        // We need the registry's load_file API, but there's no
        // direct insert; write a tempfile and load it.
        let dir = tempfile::tempdir().unwrap();
        let agent_path = dir.path().join(format!("{agent_id_str}.md"));
        std::fs::write(
            &agent_path,
            format!(
                r#"---
name: {agent_id_str}
model: claude-haiku
budget: 1.0
---

You are a test agent."#
            ),
        )
        .unwrap();
        registry.load_file(&agent_path);
        assert!(
            registry.errors().is_empty(),
            "registry errors: {:?}",
            registry.errors()
        );
        let registry = shared_registry(registry);

        // Fake LLM: always returns the canned response.
        let llm = Arc::new({
            let c = FixtureClient::new();
            // Push enough responses for a few possible retries.
            for _ in 0..5 {
                c.push_response(canned_response());
            }
            c
        });

        let worker_store = Arc::new(
            WorkerStore::open(&dir.path().join("worker.db"))
                .await
                .unwrap(),
        );
        let worker_id = WorkerId::new(format!("dispatcher-test-{}", Uuid::now_v7().simple()))
            .expect("worker id");
        let worker: Arc<dyn Worker> = Arc::new(ReducerRunner::new(
            Arc::new(ReducerContext::builder().tools(test_tools()).build()),
            Arc::new(
                RunnerConfig::builder()
                    .bus(bus.clone())
                    .pricing(test_pricing())
                    .store(worker_store)
                    .worker_id(worker_id)
                    .build(),
            ),
            Harness::new(),
        ));

        // Projection store, so we can verify events landed.
        let store = Arc::new(
            ProjectionStore::open(&dir.path().join("events.db"))
                .await
                .unwrap(),
        );

        // Spawn a projection consumer so events are materialised.
        let proj_consumer =
            crate::control_plane::projection::ProjectionConsumer::new(bus.clone(), store.clone());
        let (proj_tx, proj_rx) = oneshot::channel();
        let proj_handle = tokio::spawn(async move { proj_consumer.run(proj_rx).await });

        // Spawn the dispatcher with a filter scoped to just this
        // test's agent id, so parallel tests do not compete for
        // each other's messages on the work-queue stream.
        let dispatcher = TestDispatcher {
            bus: bus.clone(),
            registry: registry.clone(),
            worker: worker.clone(),
            llm: llm.clone(),
            consumer_name: unique_consumer_name(),
            filter_subject: crate::bus::trigger_subject(&agent_id_str),
        };
        let (disp_tx, disp_rx) = oneshot::channel();
        let disp_handle = tokio::spawn(async move { dispatcher.run(disp_rx).await });

        // Give consumers a moment to register before publishing.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Publish a trigger.
        bus.publish_trigger(&agent_id_str, &json!({"input": "hi"}))
            .await
            .expect("publish trigger");

        // Wait for events to land in the projection.
        let agent_id = AgentId::new(&agent_id_str).unwrap();
        let _ = agent_id; // (used by filter below via &agent_id_str)
        let filter = EventFilter {
            agent: Some(&agent_id_str),
            ..Default::default()
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let rows = store.query_events(&filter, 100).await.unwrap();
            let has_triggered = rows.iter().any(|r| r.event_type == "triggered");
            let has_completed = rows.iter().any(|r| r.event_type == "completed");
            if has_triggered && has_completed {
                break;
            }
            if tokio::time::Instant::now() > deadline {
                panic!(
                    "timed out waiting for dispatched events; got {} rows",
                    rows.len()
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // Shut down.
        let _ = disp_tx.send(());
        let _ = proj_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(2), disp_handle).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), proj_handle).await;

        // The llm should have seen at least one call.
        assert!(
            !llm.requests().is_empty(),
            "fixture client received no requests"
        );
    }

    // Silence unused warnings for the helper fn when no NATS.
    #[allow(dead_code)]
    fn _suppress_unused() {
        let _ = sample_agent("x");
    }

    /// Regression (M0 dogfood loop, 2026-07-06): the trigger is acked as
    /// soon as the invocation is *dispatched*, not when it *completes* —
    /// so an invocation longer than the consumer's 30s ack-wait is not
    /// redelivered and re-run. A worker that blocks the invocation
    /// in-flight lets us assert the trigger has already been acked
    /// (`num_ack_pending` → 0) without waiting real seconds. With the old
    /// ack-on-completion behaviour this times out (the message stays
    /// unacked while the invocation runs).
    #[tokio::test]
    async fn trigger_is_acked_before_the_invocation_finishes() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::Notify;

        struct BlockingWorker {
            started: Arc<AtomicUsize>,
            release: Arc<Notify>,
        }
        #[async_trait::async_trait]
        impl Worker for BlockingWorker {
            async fn run_invocation(
                &self,
                _agent: &Agent,
                _llm: &dyn crate::llm::LlmClient,
                _source: TriggerSource,
                _subject: Option<String>,
                _payload: serde_json::Value,
                mut durable_start: crate::worker::DurableStart,
            ) -> Result<crate::worker::InvocationOutcome, ExecutorError> {
                self.started.fetch_add(1, Ordering::SeqCst);
                // Simulate the first WAL write landing: this is what lets
                // the dispatcher ack while the invocation is still
                // in-flight (issue #41).
                durable_start.fire();
                self.release.notified().await;
                Ok(crate::worker::InvocationOutcome::Completed {
                    invocation_id: Uuid::now_v7(),
                    response: canned_response(),
                    cost: 0.0,
                    duration_ms: 0,
                })
            }

            async fn request_drain(&self, _req: crate::worker::DrainRequest) {}

            fn drain_status(&self) -> crate::worker::DrainState {
                crate::worker::DrainState::Running
            }
        }

        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let agent_id_str = unique_agent_id("ack-before-run");

        let dir = tempfile::tempdir().unwrap();
        let agent_path = dir.path().join(format!("{agent_id_str}.md"));
        std::fs::write(
            &agent_path,
            format!(
                "---\nname: {agent_id_str}\nmodel: claude-haiku\nbudget: 1.0\n---\n\nTest agent."
            ),
        )
        .unwrap();
        let mut registry = AgentRegistry::new();
        registry.load_file(&agent_path);
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());

        let started = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let worker: Arc<dyn Worker> = Arc::new(BlockingWorker {
            started: started.clone(),
            release: release.clone(),
        });
        let dispatcher = Arc::new(TriggerDispatcher::new(
            bus.clone(),
            shared_registry(registry),
            worker,
            Arc::new(FixtureClient::new()) as Arc<dyn crate::llm::LlmClient>,
        ));

        let mut consumer = bus
            .trigger_consumer_with_filter(
                &unique_consumer_name(),
                &crate::bus::trigger_subject(&agent_id_str),
            )
            .await
            .expect("consumer");

        bus.publish_trigger(&agent_id_str, &json!({"input": "hi"}))
            .await
            .expect("publish");
        let msg = {
            let mut stream = consumer.messages().await.expect("messages");
            tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .expect("a message within 5s")
                .expect("stream open")
                .expect("message ok")
        };

        let d = dispatcher.clone();
        let handle = tokio::spawn(async move { d.handle(&msg).await });

        // Wait until the invocation has actually entered (and blocked).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while started.load(Ordering::SeqCst) == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "invocation never started"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            started.load(Ordering::SeqCst),
            1,
            "invocation ran more than once"
        );

        // The invocation is blocked in-flight; the trigger must already
        // be acked (num_ack_pending drops to 0). Poll to absorb ack
        // propagation; a stuck 1 is the redelivery-storm regression.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let pending = consumer.info().await.expect("info").num_ack_pending;
            if pending == 0 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "trigger not acked while the invocation is still in-flight \
                 (num_ack_pending={pending}) — the redelivery-storm regression"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        release.notify_one();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    /// Issue #41: the trigger is acked only *after* the invocation's
    /// first durable (WAL) write, signalled through the `Worker` seam.
    /// A worker that blocks in-flight *without* firing the durable-start
    /// signal models a crash in the ack->first-WAL-write window: the
    /// trigger must stay unacked (`num_ack_pending` stuck at 1) so
    /// JetStream can redeliver it. With the old ack-on-dispatch
    /// behaviour this would drop to 0 immediately — the missed-run gap.
    #[tokio::test]
    async fn trigger_is_not_acked_before_the_first_wal_write() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::Notify;

        // Enters the invocation and blocks, but never fires
        // durable_start — i.e. it dies before its first WAL write.
        struct NeverSignalsWorker {
            started: Arc<AtomicUsize>,
            release: Arc<Notify>,
        }
        #[async_trait::async_trait]
        impl Worker for NeverSignalsWorker {
            async fn run_invocation(
                &self,
                _agent: &Agent,
                _llm: &dyn crate::llm::LlmClient,
                _source: TriggerSource,
                _subject: Option<String>,
                _payload: serde_json::Value,
                _durable_start: crate::worker::DurableStart,
            ) -> Result<crate::worker::InvocationOutcome, ExecutorError> {
                self.started.fetch_add(1, Ordering::SeqCst);
                // Never fire the signal; just block. Dropping
                // `_durable_start` here (on return) would signal nothing.
                self.release.notified().await;
                Ok(crate::worker::InvocationOutcome::Completed {
                    invocation_id: Uuid::now_v7(),
                    response: canned_response(),
                    cost: 0.0,
                    duration_ms: 0,
                })
            }

            async fn request_drain(&self, _req: crate::worker::DrainRequest) {}

            fn drain_status(&self) -> crate::worker::DrainState {
                crate::worker::DrainState::Running
            }
        }

        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let agent_id_str = unique_agent_id("no-ack-before-wal");

        let dir = tempfile::tempdir().unwrap();
        let agent_path = dir.path().join(format!("{agent_id_str}.md"));
        std::fs::write(
            &agent_path,
            format!(
                "---\nname: {agent_id_str}\nmodel: claude-haiku\nbudget: 1.0\n---\n\nTest agent."
            ),
        )
        .unwrap();
        let mut registry = AgentRegistry::new();
        registry.load_file(&agent_path);
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());

        let started = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let worker: Arc<dyn Worker> = Arc::new(NeverSignalsWorker {
            started: started.clone(),
            release: release.clone(),
        });
        let dispatcher = Arc::new(TriggerDispatcher::new(
            bus.clone(),
            shared_registry(registry),
            worker,
            Arc::new(FixtureClient::new()) as Arc<dyn crate::llm::LlmClient>,
        ));

        let mut consumer = bus
            .trigger_consumer_with_filter(
                &unique_consumer_name(),
                &crate::bus::trigger_subject(&agent_id_str),
            )
            .await
            .expect("consumer");

        bus.publish_trigger(&agent_id_str, &json!({"input": "hi"}))
            .await
            .expect("publish");
        let msg = {
            let mut stream = consumer.messages().await.expect("messages");
            tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .expect("a message within 5s")
                .expect("stream open")
                .expect("message ok")
        };

        let d = dispatcher.clone();
        let handle = tokio::spawn(async move { d.handle(&msg).await });

        // Wait until the invocation is in-flight (and blocked).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while started.load(Ordering::SeqCst) == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "invocation never started"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // The invocation is blocked before any WAL write and never fired
        // the durable-start signal: the trigger must remain unacked. Poll
        // for a stretch to absorb ack propagation; it must stay at 1.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(750);
        while tokio::time::Instant::now() < deadline {
            let pending = consumer.info().await.expect("info").num_ack_pending;
            assert_eq!(
                pending, 1,
                "trigger must stay unacked before the first WAL write \
                 (redeliverable) — got num_ack_pending={pending}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // Let it finish so the spawned task and consumer tear down.
        release.notify_one();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    /// A worker that returns an error *before* firing `durable_start` —
    /// models a failure before the first WAL write. `transient` picks
    /// whether that error is retryable, exercising the dispatcher's
    /// ACK (permanent) / NAK (transient) split (#41 / #46).
    struct FailsBeforeWalWorker {
        transient: bool,
    }

    #[async_trait::async_trait]
    impl Worker for FailsBeforeWalWorker {
        async fn run_invocation(
            &self,
            _agent: &Agent,
            _llm: &dyn crate::llm::LlmClient,
            _source: TriggerSource,
            _subject: Option<String>,
            _payload: serde_json::Value,
            _durable_start: crate::worker::DurableStart,
        ) -> Result<crate::worker::InvocationOutcome, ExecutorError> {
            // Return without firing durable_start (i.e. before any WAL
            // write). A transient error should be NAK'd for redelivery, a
            // permanent one ACK'd.
            if self.transient {
                Err(ExecutorError::WorkerStore(
                    "simulated transient store outage".to_string(),
                ))
            } else {
                Err(ExecutorError::InvocationFailed {
                    kind: crate::events::FailureKind::RuntimeError,
                    message: "permanent poison payload".to_string(),
                })
            }
        }

        async fn request_drain(&self, _req: crate::worker::DrainRequest) {}

        fn drain_status(&self) -> crate::worker::DrainState {
            crate::worker::DrainState::Running
        }
    }

    /// Dispatch a trigger whose invocation fails before the first WAL
    /// write, and report whether the trigger was **redelivered** (NAK)
    /// rather than consumed (ACK). Requires a live broker.
    async fn dispatched_pre_wal_failure_is_redelivered(transient: bool) -> bool {
        use std::sync::Arc;
        let url = std::env::var("FQ_NATS_URL").expect("FQ_NATS_URL");
        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let label = if transient {
            "pre-wal-transient"
        } else {
            "pre-wal-permanent"
        };
        let agent_id_str = unique_agent_id(label);

        let dir = tempfile::tempdir().unwrap();
        let agent_path = dir.path().join(format!("{agent_id_str}.md"));
        std::fs::write(
            &agent_path,
            format!(
                "---\nname: {agent_id_str}\nmodel: claude-haiku\nbudget: 1.0\n---\n\nTest agent."
            ),
        )
        .unwrap();
        let mut registry = AgentRegistry::new();
        registry.load_file(&agent_path);
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());

        let worker: Arc<dyn Worker> = Arc::new(FailsBeforeWalWorker { transient });
        let dispatcher = Arc::new(TriggerDispatcher::new(
            bus.clone(),
            shared_registry(registry),
            worker,
            Arc::new(FixtureClient::new()) as Arc<dyn crate::llm::LlmClient>,
        ));

        let consumer = bus
            .trigger_consumer_with_filter(
                &unique_consumer_name(),
                &crate::bus::trigger_subject(&agent_id_str),
            )
            .await
            .expect("consumer");

        bus.publish_trigger(&agent_id_str, &json!({"input": "hi"}))
            .await
            .expect("publish");
        let mut stream = consumer.messages().await.expect("messages");
        let msg = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("a message within 5s")
            .expect("stream open")
            .expect("message ok");

        // The worker returns its error before any WAL write; `handle`
        // classifies it and ACKs (permanent) or NAKs (transient).
        dispatcher.handle(&msg).await;

        // A NAK redelivers the trigger; an ACK consumes it. Re-poll the
        // same stream: a redelivered message means it was NAK'd.
        match tokio::time::timeout(Duration::from_secs(2), stream.next()).await {
            Ok(Some(Ok(redelivered))) => {
                // Ack the redelivery so it doesn't churn after the test.
                let _ = redelivered.ack().await;
                true
            }
            _ => false,
        }
    }

    /// #41 / #46: a *transient* failure before the first WAL write NAKs
    /// the trigger so JetStream redelivers it — the otherwise-lost run is
    /// retried, not dropped.
    #[tokio::test]
    async fn transient_failure_before_first_wal_write_naks_for_redelivery() {
        if std::env::var("FQ_NATS_URL").is_err() {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        }
        assert!(
            dispatched_pre_wal_failure_is_redelivered(true).await,
            "a transient pre-WAL failure must NAK (redeliver) the trigger"
        );
    }

    /// #41 / #46: a *permanent* failure before the first WAL write ACKs
    /// the trigger — retrying a poison run would loop under the unbounded
    /// consumer, and the Failed event already recorded why.
    #[tokio::test]
    async fn permanent_failure_before_first_wal_write_acks() {
        if std::env::var("FQ_NATS_URL").is_err() {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        }
        assert!(
            !dispatched_pre_wal_failure_is_redelivered(false).await,
            "a permanent pre-WAL failure must ACK (consume) the trigger"
        );
    }

    /// ADR-0027: once the worker is draining, the dispatcher stops
    /// consuming — it exits its consume loop on its own (no shutdown
    /// signal needed) and never dispatches an already-available trigger.
    /// The trigger is left un-acked on the durable work-queue consumer,
    /// so JetStream retains it for the next binary.
    #[tokio::test]
    async fn a_draining_dispatcher_stops_consuming_triggers() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };
        use crate::bus::EventBus;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        // Counts dispatches; reports drain state from a flag we control.
        struct CountingWorker {
            calls: Arc<AtomicUsize>,
            draining: Arc<AtomicBool>,
        }
        #[async_trait::async_trait]
        impl Worker for CountingWorker {
            async fn run_invocation(
                &self,
                _agent: &Agent,
                _llm: &dyn crate::llm::LlmClient,
                _source: TriggerSource,
                _subject: Option<String>,
                _payload: serde_json::Value,
                _durable_start: crate::worker::DurableStart,
            ) -> Result<crate::worker::InvocationOutcome, ExecutorError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(crate::worker::InvocationOutcome::Completed {
                    invocation_id: Uuid::now_v7(),
                    response: canned_response(),
                    cost: 0.0,
                    duration_ms: 0,
                })
            }
            async fn request_drain(&self, _req: crate::worker::DrainRequest) {
                self.draining.store(true, Ordering::SeqCst);
            }
            fn drain_status(&self) -> crate::worker::DrainState {
                if self.draining.load(Ordering::SeqCst) {
                    crate::worker::DrainState::Draining
                } else {
                    crate::worker::DrainState::Running
                }
            }
        }

        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let agent_id_str = unique_agent_id("drain-stop");

        let mut registry = AgentRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let agent_path = dir.path().join(format!("{agent_id_str}.md"));
        std::fs::write(
            &agent_path,
            format!(
                "---\nname: {agent_id_str}\nmodel: claude-haiku\nbudget: 1.0\n---\n\nTest agent."
            ),
        )
        .unwrap();
        registry.load_file(&agent_path);
        let registry = shared_registry(registry);

        let llm: Arc<dyn LlmClient> = Arc::new({
            let c = FixtureClient::new();
            for _ in 0..5 {
                c.push_response(canned_response());
            }
            c
        });

        // A trigger is waiting on the stream before the dispatcher runs.
        let calls = Arc::new(AtomicUsize::new(0));
        let worker: Arc<dyn Worker> = Arc::new(CountingWorker {
            calls: calls.clone(),
            draining: Arc::new(AtomicBool::new(true)), // draining from the start
        });
        let dispatcher = TestDispatcher {
            bus: bus.clone(),
            registry,
            worker,
            llm,
            consumer_name: unique_consumer_name(),
            filter_subject: crate::bus::trigger_subject(&agent_id_str),
        };
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(async move { dispatcher.run(shutdown_rx).await });

        bus.publish_trigger(&agent_id_str, &json!({ "input": "hi" }))
            .await
            .expect("publish trigger");

        // The draining dispatcher must exit on its own — without us ever
        // sending `shutdown` — and without dispatching the waiting trigger.
        let joined = tokio::time::timeout(Duration::from_secs(3), handle).await;
        assert!(
            joined.is_ok(),
            "a draining dispatcher must exit on its own (top-of-loop drain break), \
             not block waiting for triggers"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a draining dispatcher must not dispatch an available trigger"
        );
        drop(shutdown_tx);
    }
}
