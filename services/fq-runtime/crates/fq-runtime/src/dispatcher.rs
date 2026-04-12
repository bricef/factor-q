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
//! - At-least-once: a crash mid-execution can lead to redelivery.
//!   The executor is not transactional; a redelivered trigger will
//!   run the agent a second time. Idempotency is the caller's
//!   concern for phase 1.
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
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

use crate::agent::{AgentId, AgentRegistry};
use crate::bus::{agent_id_from_trigger_subject, BusError, EventBus};
use crate::events::TriggerSource;
use crate::executor::{AgentExecutor, ExecutorError};
use crate::llm::LlmClient;

/// Name of the durable JetStream consumer the dispatcher creates.
pub const CONSUMER_NAME: &str = "fq-dispatcher";

/// NATS-triggered dispatcher. Owns references to the pieces of the
/// runtime it needs — call [`TriggerDispatcher::run`] to drive it.
pub struct TriggerDispatcher {
    bus: EventBus,
    registry: Arc<AgentRegistry>,
    executor: Arc<AgentExecutor>,
    llm: Arc<dyn LlmClient>,
}

impl TriggerDispatcher {
    pub fn new(
        bus: EventBus,
        registry: Arc<AgentRegistry>,
        executor: Arc<AgentExecutor>,
        llm: Arc<dyn LlmClient>,
    ) -> Self {
        Self {
            bus,
            registry,
            executor,
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
        let loaded = match self.registry.get_loaded(&agent_id) {
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

        let outcome = self
            .executor
            .run(
                &loaded.agent,
                self.llm.as_ref(),
                TriggerSource::Subject,
                Some(msg.subject.to_string()),
                payload,
            )
            .await;

        match outcome {
            Ok(_) => {
                self.ack(msg, "completed").await;
            }
            Err(err) => {
                // The executor has already emitted a Failed event
                // on the event stream. Ack the trigger so we don't
                // loop on what is almost certainly a permanent
                // problem.
                warn!(
                    agent_id = %agent_id,
                    error = %err,
                    "executor returned an error for NATS-triggered run"
                );
                self.log_executor_error(&err);
                self.ack(msg, "executor error").await;
            }
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

    fn log_executor_error(&self, err: &ExecutorError) {
        match err {
            ExecutorError::Llm(e) => error!(error = %e, "llm error during dispatch"),
            ExecutorError::Bus(e) => error!(error = %e, "bus error during dispatch"),
            ExecutorError::MaxIterationsExceeded => {
                error!("agent exceeded max iterations during dispatch")
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
    use crate::events::{EventPayload, StopReason, TokenUsage};
    use crate::llm::fixture::FixtureClient;
    use crate::llm::ChatResponse;
    use crate::pricing::{ModelPricing, PricingTable};
    use crate::tools::ToolRegistry;
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
        registry: Arc<AgentRegistry>,
        executor: Arc<AgentExecutor>,
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
                self.executor.clone(),
                self.llm.clone(),
            );

            loop {
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
        use crate::projection::store::{EventFilter, ProjectionStore};

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
        let registry = Arc::new(registry);

        // Fake LLM: always returns the canned response.
        let llm = Arc::new({
            let c = FixtureClient::new();
            // Push enough responses for a few possible retries.
            for _ in 0..5 {
                c.push_response(canned_response());
            }
            c
        });

        let executor = Arc::new(AgentExecutor::new(bus.clone(), test_pricing(), test_tools()));

        // Projection store, so we can verify events landed.
        let store = Arc::new(
            ProjectionStore::open(&dir.path().join("events.db"))
                .await
                .unwrap(),
        );

        // Spawn a projection consumer so events are materialised.
        let proj_consumer =
            crate::projection::ProjectionConsumer::new(bus.clone(), store.clone());
        let (proj_tx, proj_rx) = oneshot::channel();
        let proj_handle = tokio::spawn(async move { proj_consumer.run(proj_rx).await });

        // Spawn the dispatcher with a filter scoped to just this
        // test's agent id, so parallel tests do not compete for
        // each other's messages on the work-queue stream.
        let dispatcher = TestDispatcher {
            bus: bus.clone(),
            registry: registry.clone(),
            executor: executor.clone(),
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
}
