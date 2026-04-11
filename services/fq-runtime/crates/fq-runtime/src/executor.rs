//! Agent executor.
//!
//! Takes a validated [`Agent`] and a trigger, runs one pass through the
//! LLM, and emits the full event sequence to the event bus. Phase 1 slice:
//! no tool calls, one LLM round trip, then completion.
//!
//! The executor is generic over the [`LlmClient`] so it can be tested with
//! a [`FixtureClient`](crate::llm::fixture::FixtureClient) without needing
//! any real provider credentials.

use std::time::Instant;

use serde_json::Value;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::agent::Agent;
use crate::bus::{BusError, EventBus};
use crate::events::{
    CompletedPayload, CostPayload, Event, EventPayload, FailedPayload, FailureKind, FailurePhase,
    InvocationTotals, LlmRequestPayload, LlmResponsePayload, Message, MessageRole, RequestParams,
    TriggerSource, TriggeredPayload,
};
use crate::llm::{ChatRequest, ChatResponse, LlmClient, LlmError};
use crate::pricing::ModelPricing;

/// The agent executor. Cheap to construct, takes shared references to the
/// dependencies it uses. Clone is fine — both fields are already `Arc` or
/// `Clone`.
pub struct AgentExecutor {
    bus: EventBus,
}

impl AgentExecutor {
    pub fn new(bus: EventBus) -> Self {
        Self { bus }
    }

    /// Run a single invocation of an agent.
    ///
    /// Emits `triggered`, `llm.request`, `llm.response`, `cost`, and
    /// `completed` (or `failed`) events. Returns when the invocation is
    /// complete — success or failure.
    pub async fn run(
        &self,
        agent: &Agent,
        llm: &dyn LlmClient,
        trigger_source: TriggerSource,
        trigger_subject: Option<String>,
        trigger_payload: Value,
    ) -> Result<InvocationOutcome, ExecutorError> {
        let invocation_id = Uuid::now_v7();
        let start = Instant::now();
        let agent_id = agent.id().as_str().to_string();
        info!(
            agent_id = %agent_id,
            invocation_id = %invocation_id,
            "starting invocation"
        );

        // Emit Triggered
        self.publish(Event::new(
            agent_id.clone(),
            invocation_id,
            EventPayload::Triggered(TriggeredPayload {
                trigger_source,
                trigger_subject,
                trigger_payload,
                config_snapshot: agent.to_snapshot(),
            }),
        ))
        .await?;

        // For phase 1 we do a single LLM call with the system prompt and no
        // additional user context beyond the trigger. Later slices will
        // build a richer message sequence and support tool-call loops.
        let messages = vec![Message {
            role: MessageRole::System,
            content: Some(agent.system_prompt().to_string()),
            tool_calls: vec![],
            tool_call_id: None,
        }];

        let call_id = Uuid::now_v7();
        let request = ChatRequest {
            model: agent.model().to_string(),
            messages,
            tools: vec![],
            params: RequestParams {
                temperature: None,
                max_tokens: Some(4096),
            },
        };

        // Emit LlmRequest
        self.publish(Event::new(
            agent_id.clone(),
            invocation_id,
            EventPayload::LlmRequest(LlmRequestPayload {
                call_id,
                model: request.model.clone(),
                messages: request.messages.clone(),
                tools_available: request.tools.clone(),
                request_params: request.params.clone(),
            }),
        ))
        .await?;

        // Call LLM
        let response = match llm.chat(request.clone()).await {
            Ok(r) => r,
            Err(err) => {
                let totals = InvocationTotals {
                    total_llm_calls: 1,
                    total_tool_calls: 0,
                    total_cost: 0.0,
                    total_duration_ms: start.elapsed().as_millis() as u64,
                };
                self.emit_failed(
                    &agent_id,
                    invocation_id,
                    FailureKind::LlmError,
                    err.to_string(),
                    FailurePhase::LlmRequest,
                    totals,
                )
                .await?;
                return Err(ExecutorError::Llm(err));
            }
        };

        // Emit LlmResponse
        self.publish(Event::new(
            agent_id.clone(),
            invocation_id,
            EventPayload::LlmResponse(LlmResponsePayload {
                call_id,
                content: response.content.clone(),
                tool_calls: response.tool_calls.clone(),
                stop_reason: response.stop_reason,
                usage: response.usage,
            }),
        ))
        .await?;

        // Cost calculation. Unknown models get zero cost — a warning in the
        // log lets operators know they need to add pricing.
        let pricing = ModelPricing::lookup(agent.model());
        if pricing.is_none() {
            warn!(
                model = agent.model(),
                "no pricing known for model; cost will be reported as $0"
            );
        }
        let (input_cost, output_cost, total_cost) = pricing
            .as_ref()
            .map(|p| p.calculate(response.usage.input_tokens, response.usage.output_tokens))
            .unwrap_or((0.0, 0.0, 0.0));

        self.publish(Event::new(
            agent_id.clone(),
            invocation_id,
            EventPayload::Cost(CostPayload {
                call_id,
                model: agent.model().to_string(),
                input_tokens: response.usage.input_tokens,
                output_tokens: response.usage.output_tokens,
                cache_read_tokens: response.usage.cache_read_tokens,
                cache_write_tokens: response.usage.cache_write_tokens,
                input_cost,
                output_cost,
                total_cost,
                cumulative_invocation_cost: total_cost,
                cumulative_agent_cost: total_cost,
            }),
        ))
        .await?;

        // Budget check — if we blew the ceiling, report failure instead of
        // completion. For phase 1 this only runs after the fact; later we
        // will also check before each call.
        if let Some(budget) = agent.budget() {
            if total_cost > budget {
                let totals = InvocationTotals {
                    total_llm_calls: 1,
                    total_tool_calls: 0,
                    total_cost,
                    total_duration_ms: start.elapsed().as_millis() as u64,
                };
                self.emit_failed(
                    &agent_id,
                    invocation_id,
                    FailureKind::BudgetExceeded,
                    format!("cost ${total_cost:.6} exceeded budget ${budget:.2}"),
                    FailurePhase::LlmResponse,
                    totals,
                )
                .await?;
                return Ok(InvocationOutcome::BudgetExceeded {
                    invocation_id,
                    cost: total_cost,
                });
            }
        }

        // Completed
        let duration_ms = start.elapsed().as_millis() as u64;
        self.publish(Event::new(
            agent_id.clone(),
            invocation_id,
            EventPayload::Completed(CompletedPayload {
                result_summary: response.content.clone(),
                total_llm_calls: 1,
                total_tool_calls: 0,
                total_cost,
                total_duration_ms: duration_ms,
            }),
        ))
        .await?;

        info!(
            agent_id = %agent_id,
            invocation_id = %invocation_id,
            duration_ms,
            cost = total_cost,
            "invocation completed"
        );

        Ok(InvocationOutcome::Completed {
            invocation_id,
            response,
            cost: total_cost,
            duration_ms,
        })
    }

    async fn publish(&self, event: Event) -> Result<(), ExecutorError> {
        debug!(event_type = ?event.payload, "publishing event");
        self.bus.publish(&event).await.map_err(ExecutorError::Bus)
    }

    async fn emit_failed(
        &self,
        agent_id: &str,
        invocation_id: Uuid,
        error_kind: FailureKind,
        error_message: String,
        phase: FailurePhase,
        partial_totals: InvocationTotals,
    ) -> Result<(), ExecutorError> {
        warn!(
            agent_id = %agent_id,
            invocation_id = %invocation_id,
            error_kind = ?error_kind,
            "invocation failed"
        );
        self.publish(Event::new(
            agent_id.to_string(),
            invocation_id,
            EventPayload::Failed(FailedPayload {
                error_kind,
                error_message,
                phase,
                partial_totals,
            }),
        ))
        .await
    }
}

/// Outcome of a successful call to [`AgentExecutor::run`].
#[derive(Debug)]
pub enum InvocationOutcome {
    Completed {
        invocation_id: Uuid,
        response: ChatResponse,
        cost: f64,
        duration_ms: u64,
    },
    BudgetExceeded {
        invocation_id: Uuid,
        cost: f64,
    },
}

/// Errors returned from the executor. Note that most failure modes are
/// also emitted as `failed` events before this error is returned — the
/// error is for the caller, the event is for observers.
#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    #[error("event bus error: {0}")]
    Bus(#[from] BusError),

    #[error("LLM error: {0}")]
    Llm(#[from] LlmError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{EventPayload, StopReason, TokenUsage};
    use crate::llm::fixture::FixtureClient;
    use futures::StreamExt;
    use serde_json::json;
    use std::time::Duration;

    fn unique_agent_id(prefix: &str) -> String {
        format!("{prefix}-{}", Uuid::now_v7().simple())
    }

    fn sample_agent() -> Agent {
        Agent::builder()
            .id(unique_agent_id("exec-test"))
            .model("claude-haiku")
            .system_prompt("You are a test agent.")
            .budget(1.0)
            .build()
            .unwrap()
    }

    fn canned_response(text: &str, input_tokens: u32, output_tokens: u32) -> ChatResponse {
        ChatResponse {
            content: Some(text.to_string()),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens,
                output_tokens,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        }
    }

    /// Gated on FQ_NATS_URL so CI without NATS still passes.
    #[tokio::test]
    async fn emits_full_event_sequence_for_successful_run() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        let bus = EventBus::connect(&url).await.expect("connect to NATS");
        let executor = AgentExecutor::new(bus.clone());
        let agent = sample_agent();

        let llm = FixtureClient::new();
        llm.push_response(canned_response("Hello from the test agent.", 100, 200));

        let mut subscriber = bus
            .subscribe(format!("fq.agent.{}.>", agent.id().as_str()))
            .await
            .expect("subscribe");

        tokio::time::sleep(Duration::from_millis(50)).await;

        let outcome = executor
            .run(
                &agent,
                &llm,
                TriggerSource::Manual,
                None,
                json!({"input": "hi"}),
            )
            .await
            .expect("run completes");

        match outcome {
            InvocationOutcome::Completed { cost, .. } => {
                // 100 input @ $1/M + 200 output @ $5/M = $0.0011
                assert!((cost - 0.0011).abs() < 1e-9, "cost was {cost}");
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        let mut events = Vec::new();
        for _ in 0..5 {
            let event = tokio::time::timeout(Duration::from_secs(2), subscriber.next())
                .await
                .expect("timeout waiting for event")
                .expect("stream closed")
                .expect("deserialise");
            events.push(event);
        }

        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match &e.payload {
                EventPayload::Triggered(_) => "triggered",
                EventPayload::LlmRequest(_) => "llm_request",
                EventPayload::LlmResponse(_) => "llm_response",
                EventPayload::Cost(_) => "cost",
                EventPayload::Completed(_) => "completed",
                EventPayload::Failed(_) => "failed",
                EventPayload::ToolCall(_) => "tool_call",
                EventPayload::ToolResult(_) => "tool_result",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["triggered", "llm_request", "llm_response", "cost", "completed"],
            "event sequence was {kinds:?}"
        );

        let first_invocation = events[0].invocation_id;
        assert!(events.iter().all(|e| e.invocation_id == first_invocation));

        match &events[0].payload {
            EventPayload::Triggered(p) => {
                assert!(p.config_snapshot.name.starts_with("exec-test-"));
                assert_eq!(p.config_snapshot.model, "claude-haiku");
                assert_eq!(p.config_snapshot.budget, Some(1.0));
            }
            _ => unreachable!(),
        }
    }

    #[tokio::test]
    async fn emits_failed_event_when_budget_exceeded() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        let bus = EventBus::connect(&url).await.expect("connect to NATS");
        let executor = AgentExecutor::new(bus.clone());

        let agent_id = unique_agent_id("overspender");
        let agent = Agent::builder()
            .id(agent_id.clone())
            .model("claude-haiku")
            .system_prompt("You spend a lot.")
            .budget(0.0001)
            .build()
            .unwrap();

        let llm = FixtureClient::new();
        // 1M input tokens at $1/M = $1.00, far over the $0.0001 budget.
        llm.push_response(canned_response("expensive", 1_000_000, 0));

        let mut subscriber = bus
            .subscribe(format!("fq.agent.{agent_id}.>"))
            .await
            .expect("subscribe");
        tokio::time::sleep(Duration::from_millis(50)).await;

        let outcome = executor
            .run(&agent, &llm, TriggerSource::Manual, None, json!({}))
            .await
            .expect("run completes");
        assert!(matches!(outcome, InvocationOutcome::BudgetExceeded { .. }));

        let mut saw_failed = false;
        for _ in 0..6 {
            let event = tokio::time::timeout(Duration::from_secs(2), subscriber.next())
                .await
                .expect("timeout waiting for event")
                .expect("stream closed")
                .expect("deserialise");
            if let EventPayload::Failed(p) = &event.payload {
                assert!(matches!(p.error_kind, FailureKind::BudgetExceeded));
                saw_failed = true;
                break;
            }
        }
        assert!(saw_failed, "did not see Failed event");
    }
}
