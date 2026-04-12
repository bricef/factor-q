//! Agent executor.
//!
//! Takes a validated [`Agent`] and a trigger, runs the agent's LLM
//! loop, dispatches tool calls against the [`ToolRegistry`], and
//! emits the full event sequence to the event bus. Each LLM turn
//! produces one `llm.request` / `llm.response` / `cost` triple, and
//! any tool calls in a response turn into `tool.call` / `tool.result`
//! events before the next LLM call.
//!
//! The executor is generic over the [`LlmClient`] so it can be tested
//! with a [`FixtureClient`](crate::llm::fixture::FixtureClient)
//! without any real provider credentials.

use std::sync::Arc;
use std::time::Instant;

use fq_tools::{ToolContext, ToolError, ToolResult, ToolSandbox};
use serde_json::Value;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::agent::Agent;
use crate::bus::{BusError, EventBus};
use crate::events::{
    CompletedPayload, CostPayload, Event, EventPayload, FailedPayload, FailureKind, FailurePhase,
    InvocationTotals, LlmRequestPayload, LlmResponsePayload, Message, MessageRole,
    MessageToolCall, RequestParams, ToolCallPayload, ToolErrorKind, ToolResultPayload,
    TriggerSource, TriggeredPayload,
};
use crate::llm::{ChatRequest, ChatResponse, LlmClient, LlmError};
use crate::pricing::PricingTable;
use crate::tools::ToolRegistry;

/// Maximum number of LLM iterations per invocation. Guards against
/// runaway tool-call loops. Plenty of headroom for legitimate tool
/// sequences; failing at this limit produces a `Failed` event with
/// `error_kind = RuntimeError`.
const MAX_ITERATIONS: u32 = 20;

/// The agent executor.
pub struct AgentExecutor {
    bus: EventBus,
    pricing: Arc<PricingTable>,
    tools: Arc<ToolRegistry>,
}

impl AgentExecutor {
    pub fn new(bus: EventBus, pricing: Arc<PricingTable>, tools: Arc<ToolRegistry>) -> Self {
        Self {
            bus,
            pricing,
            tools,
        }
    }

    /// Run a single invocation of an agent.
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
        let mut totals = InvocationTotals::default();

        info!(
            agent_id = %agent_id,
            invocation_id = %invocation_id,
            "starting invocation"
        );

        let user_message = payload_to_user_message(&trigger_payload);

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

        // Materialise the agent's sandbox once for the whole run and
        // build the tool schema list to advertise to the LLM.
        let sandbox = agent.sandbox().to_tool_sandbox();
        let tool_schemas = self.tools.build_schemas(agent.tools());

        // Seed the conversation: system prompt + initial user message.
        let mut messages = vec![
            Message {
                role: MessageRole::System,
                content: Some(agent.system_prompt().to_string()),
                tool_calls: vec![],
                tool_call_id: None,
            },
            Message {
                role: MessageRole::User,
                content: Some(user_message),
                tool_calls: vec![],
                tool_call_id: None,
            },
        ];

        let mut final_response: Option<ChatResponse> = None;

        for iteration in 0..MAX_ITERATIONS {
            // Make one LLM turn.
            let turn = self
                .run_llm_turn(
                    agent,
                    llm,
                    &agent_id,
                    invocation_id,
                    &messages,
                    &tool_schemas,
                    &mut totals,
                    start,
                )
                .await;

            let response = match turn {
                Ok(r) => r,
                Err(err) => return Err(err),
            };

            // Budget check after every LLM call.
            if let Some(budget) = agent.budget() {
                if totals.total_cost > budget {
                    totals.total_duration_ms = start.elapsed().as_millis() as u64;
                    self.emit_failed(
                        &agent_id,
                        invocation_id,
                        FailureKind::BudgetExceeded,
                        format!(
                            "cost ${:.6} exceeded budget ${budget:.2}",
                            totals.total_cost
                        ),
                        FailurePhase::LlmResponse,
                        totals,
                    )
                    .await?;
                    return Ok(InvocationOutcome::BudgetExceeded {
                        invocation_id,
                        cost: totals.total_cost,
                    });
                }
            }

            if response.tool_calls.is_empty() {
                // No tool calls → the LLM is done.
                final_response = Some(response);
                break;
            }

            // Append the assistant's tool-calling message to history.
            messages.push(Message {
                role: MessageRole::Assistant,
                content: response.content.clone(),
                tool_calls: response.tool_calls.clone(),
                tool_call_id: None,
            });

            // Execute each tool call in order and feed results back.
            for tool_call in &response.tool_calls {
                let result = self
                    .execute_tool_call(agent, &sandbox, &agent_id, invocation_id, tool_call)
                    .await?;
                totals.total_tool_calls += 1;
                messages.push(Message {
                    role: MessageRole::Tool,
                    content: Some(result.output),
                    tool_calls: vec![],
                    tool_call_id: Some(tool_call.tool_call_id.clone()),
                });
            }

            // If the LLM said stop_reason == EndTurn despite emitting
            // tool calls (some providers do), respect that and exit.
            if matches!(
                response.stop_reason,
                crate::events::StopReason::EndTurn | crate::events::StopReason::StopSequence
            ) && response.tool_calls.is_empty()
            {
                final_response = Some(response);
                break;
            }

            // If this was the last allowed iteration, fail with a
            // clear message rather than silently dropping the work.
            if iteration + 1 >= MAX_ITERATIONS {
                totals.total_duration_ms = start.elapsed().as_millis() as u64;
                self.emit_failed(
                    &agent_id,
                    invocation_id,
                    FailureKind::RuntimeError,
                    format!("exceeded max iterations ({MAX_ITERATIONS})"),
                    FailurePhase::LlmResponse,
                    totals,
                )
                .await?;
                return Err(ExecutorError::MaxIterationsExceeded);
            }
        }

        let response = final_response.ok_or(ExecutorError::MaxIterationsExceeded)?;

        let duration_ms = start.elapsed().as_millis() as u64;
        totals.total_duration_ms = duration_ms;

        self.publish(Event::new(
            agent_id.clone(),
            invocation_id,
            EventPayload::Completed(CompletedPayload {
                result_summary: response.content.clone(),
                total_llm_calls: totals.total_llm_calls,
                total_tool_calls: totals.total_tool_calls,
                total_cost: totals.total_cost,
                total_duration_ms: duration_ms,
            }),
        ))
        .await?;

        info!(
            agent_id = %agent_id,
            invocation_id = %invocation_id,
            duration_ms,
            cost = totals.total_cost,
            "invocation completed"
        );

        Ok(InvocationOutcome::Completed {
            invocation_id,
            response,
            cost: totals.total_cost,
            duration_ms,
        })
    }

    /// Run a single LLM turn: publish `llm.request`, call the LLM,
    /// publish `llm.response` and `cost`, and return the response.
    #[allow(clippy::too_many_arguments)]
    async fn run_llm_turn(
        &self,
        agent: &Agent,
        llm: &dyn LlmClient,
        agent_id: &str,
        invocation_id: Uuid,
        messages: &[Message],
        tools: &[crate::events::ToolSchema],
        totals: &mut InvocationTotals,
        start: Instant,
    ) -> Result<ChatResponse, ExecutorError> {
        let call_id = Uuid::now_v7();
        let request = ChatRequest {
            model: agent.model().to_string(),
            messages: messages.to_vec(),
            tools: tools.to_vec(),
            params: RequestParams {
                temperature: None,
                max_tokens: Some(4096),
            },
        };

        self.publish(Event::new(
            agent_id.to_string(),
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

        let response = match llm.chat(request).await {
            Ok(r) => r,
            Err(err) => {
                totals.total_duration_ms = start.elapsed().as_millis() as u64;
                self.emit_failed(
                    agent_id,
                    invocation_id,
                    FailureKind::LlmError,
                    err.to_string(),
                    FailurePhase::LlmRequest,
                    *totals,
                )
                .await?;
                return Err(ExecutorError::Llm(err));
            }
        };

        totals.total_llm_calls += 1;

        self.publish(Event::new(
            agent_id.to_string(),
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

        // Cost calculation.
        let pricing = self.pricing.lookup(agent.model());
        if pricing.is_none() {
            warn!(
                model = agent.model(),
                "no pricing known for model; cost will be reported as $0"
            );
        }
        let (input_cost, output_cost, total_cost) = pricing
            .map(|p| p.calculate(response.usage.input_tokens, response.usage.output_tokens))
            .unwrap_or((0.0, 0.0, 0.0));
        totals.total_cost += total_cost;

        self.publish(Event::new(
            agent_id.to_string(),
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
                cumulative_invocation_cost: totals.total_cost,
                cumulative_agent_cost: totals.total_cost,
            }),
        ))
        .await?;

        Ok(response)
    }

    /// Dispatch a single tool call, emitting `tool.call` and
    /// `tool.result` events and returning the result for inclusion in
    /// the next LLM request. Tool errors (including sandbox
    /// violations) are reported as non-fatal `tool.result` events
    /// with `is_error: true` so the LLM can read them and adapt.
    async fn execute_tool_call(
        &self,
        agent: &Agent,
        sandbox: &ToolSandbox,
        agent_id: &str,
        invocation_id: Uuid,
        call: &MessageToolCall,
    ) -> Result<ToolResult, ExecutorError> {
        // Verify the tool is in the agent's allowed list. An LLM that
        // fabricates a tool name gets told no.
        if !agent.tools().iter().any(|name| name == &call.tool_name) {
            return self
                .emit_tool_error(
                    agent_id,
                    invocation_id,
                    call,
                    ToolErrorKind::PermissionDenied,
                    format!("tool '{}' is not available to this agent", call.tool_name),
                )
                .await;
        }

        // Publish tool.call before running.
        self.publish(Event::new(
            agent_id.to_string(),
            invocation_id,
            EventPayload::ToolCall(ToolCallPayload {
                tool_call_id: call.tool_call_id.clone(),
                tool_name: call.tool_name.clone(),
                parameters: call.parameters.clone(),
            }),
        ))
        .await?;

        let tool = match self.tools.get(&call.tool_name) {
            Some(t) => t,
            None => {
                return self
                    .emit_tool_error(
                        agent_id,
                        invocation_id,
                        call,
                        ToolErrorKind::ExecutionFailed,
                        format!("no implementation registered for tool '{}'", call.tool_name),
                    )
                    .await;
            }
        };

        let ctx = ToolContext::new(sandbox);
        let tool_start = Instant::now();
        let outcome = tool.execute(&ctx, call.parameters.clone()).await;
        let duration_ms = tool_start.elapsed().as_millis() as u64;

        match outcome {
            Ok(result) => {
                self.publish(Event::new(
                    agent_id.to_string(),
                    invocation_id,
                    EventPayload::ToolResult(ToolResultPayload {
                        tool_call_id: call.tool_call_id.clone(),
                        output: result.output.clone(),
                        is_error: result.is_error,
                        error_kind: None,
                        duration_ms,
                    }),
                ))
                .await?;
                Ok(result)
            }
            Err(err) => {
                let (kind, message) = classify_tool_error(&err);
                self.publish(Event::new(
                    agent_id.to_string(),
                    invocation_id,
                    EventPayload::ToolResult(ToolResultPayload {
                        tool_call_id: call.tool_call_id.clone(),
                        output: message.clone(),
                        is_error: true,
                        error_kind: Some(kind),
                        duration_ms,
                    }),
                ))
                .await?;
                // Tool errors are not fatal — the LLM receives the
                // error text in the next turn and decides what to do.
                Ok(ToolResult {
                    output: message,
                    is_error: true,
                })
            }
        }
    }

    /// Emit a synthetic tool.result event for failures that happen
    /// before the tool itself is called (unknown tool, not in
    /// agent's allowed list). Returns a ToolResult the loop will
    /// feed back to the LLM.
    async fn emit_tool_error(
        &self,
        agent_id: &str,
        invocation_id: Uuid,
        call: &MessageToolCall,
        kind: ToolErrorKind,
        message: String,
    ) -> Result<ToolResult, ExecutorError> {
        self.publish(Event::new(
            agent_id.to_string(),
            invocation_id,
            EventPayload::ToolResult(ToolResultPayload {
                tool_call_id: call.tool_call_id.clone(),
                output: message.clone(),
                is_error: true,
                error_kind: Some(kind),
                duration_ms: 0,
            }),
        ))
        .await?;
        Ok(ToolResult {
            output: message,
            is_error: true,
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

fn classify_tool_error(err: &ToolError) -> (ToolErrorKind, String) {
    match err {
        ToolError::PermissionDenied(msg) => (ToolErrorKind::SandboxViolation, msg.clone()),
        ToolError::NotFound(path) => (
            ToolErrorKind::ExecutionFailed,
            format!("path not found: {}", path.display()),
        ),
        ToolError::InvalidParameters(msg) => (ToolErrorKind::InvalidParameters, msg.clone()),
        ToolError::Io(msg) => (ToolErrorKind::ExecutionFailed, msg.clone()),
        ToolError::ExecutionFailed(msg) => (ToolErrorKind::ExecutionFailed, msg.clone()),
    }
}

/// Convert a trigger payload into the string content of a user message.
fn payload_to_user_message(payload: &Value) -> String {
    match payload {
        Value::Null => "(no input)".to_string(),
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
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

/// Errors returned from the executor.
#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    #[error("event bus error: {0}")]
    Bus(#[from] BusError),

    #[error("LLM error: {0}")]
    Llm(#[from] LlmError),

    #[error("max iterations exceeded")]
    MaxIterationsExceeded,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Sandbox;
    use crate::events::{EventPayload, StopReason, TokenUsage};
    use crate::llm::fixture::FixtureClient;
    use crate::pricing::ModelPricing;
    use futures::StreamExt;
    use serde_json::json;
    use std::collections::HashMap;
    use std::time::Duration;
    use tempfile::tempdir;

    fn unique_agent_id(prefix: &str) -> String {
        format!("{prefix}-{}", Uuid::now_v7().simple())
    }

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

    fn tool_call_response(
        tool_name: &str,
        call_id: &str,
        params: Value,
        tokens: (u32, u32),
    ) -> ChatResponse {
        ChatResponse {
            content: None,
            tool_calls: vec![MessageToolCall {
                tool_call_id: call_id.to_string(),
                tool_name: tool_name.to_string(),
                parameters: params,
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: tokens.0,
                output_tokens: tokens.1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        }
    }

    #[tokio::test]
    async fn emits_full_event_sequence_for_successful_run() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        let bus = EventBus::connect(&url).await.expect("connect to NATS");
        let executor = AgentExecutor::new(bus.clone(), test_pricing(), test_tools());
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

        let kinds: Vec<&str> = events.iter().map(event_kind).collect();
        assert_eq!(
            kinds,
            vec!["triggered", "llm_request", "llm_response", "cost", "completed"],
        );

        let first_invocation = events[0].invocation_id;
        assert!(events.iter().all(|e| e.invocation_id == first_invocation));
    }

    #[tokio::test]
    async fn runs_tool_call_loop_and_emits_tool_events() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        // Set up a sandbox directory with a file to read.
        let dir = tempdir().unwrap();
        let target = dir.path().join("hello.md");
        std::fs::write(&target, "# hello").unwrap();

        let agent_id = unique_agent_id("tool-loop");
        let agent = Agent::builder()
            .id(agent_id.clone())
            .model("claude-haiku")
            .system_prompt("Use tools when asked.")
            .tools(["file_read"])
            .sandbox(Sandbox::new().fs_read(dir.path().to_string_lossy().to_string()))
            .budget(1.0)
            .build()
            .unwrap();

        // The LLM calls file_read first, then emits a final message.
        let llm = FixtureClient::new();
        llm.push_response(tool_call_response(
            "file_read",
            "call_abc",
            json!({ "path": target.to_string_lossy() }),
            (100, 50),
        ));
        llm.push_response(canned_response("Got the file.", 150, 20));

        let bus = EventBus::connect(&url).await.expect("connect to NATS");
        let executor = AgentExecutor::new(bus.clone(), test_pricing(), test_tools());

        let mut subscriber = bus
            .subscribe(format!("fq.agent.{agent_id}.>"))
            .await
            .expect("subscribe");
        tokio::time::sleep(Duration::from_millis(50)).await;

        let outcome = executor
            .run(&agent, &llm, TriggerSource::Manual, None, json!({}))
            .await
            .expect("run completes");

        // Cost: 100/50 @ haiku = 0.0001 + 0.00025 = 0.00035
        //       150/20 @ haiku = 0.00015 + 0.0001 = 0.00025
        // total = 0.0006
        match outcome {
            InvocationOutcome::Completed { cost, .. } => {
                assert!((cost - 0.00060).abs() < 1e-9, "cost was {cost}");
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        // Expected event sequence:
        //  triggered
        //  llm_request (1)  llm_response (tool call)  cost
        //  tool_call  tool_result
        //  llm_request (2)  llm_response (end)        cost
        //  completed
        let mut events = Vec::new();
        for _ in 0..10 {
            let event = tokio::time::timeout(Duration::from_secs(2), subscriber.next())
                .await
                .expect("timeout waiting for event")
                .expect("stream closed")
                .expect("deserialise");
            events.push(event);
        }

        let kinds: Vec<&str> = events.iter().map(event_kind).collect();
        assert_eq!(
            kinds,
            vec![
                "triggered",
                "llm_request",
                "llm_response",
                "cost",
                "tool_call",
                "tool_result",
                "llm_request",
                "llm_response",
                "cost",
                "completed",
            ],
            "got {kinds:?}"
        );

        // The tool.result should contain the file content and not be
        // an error.
        let tool_result = events
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::ToolResult(p) => Some(p),
                _ => None,
            })
            .expect("missing tool.result");
        assert!(!tool_result.is_error);
        assert_eq!(tool_result.output, "# hello");

        // The second llm.request should have received the tool result
        // as a tool-role message.
        let second_req = events
            .iter()
            .filter_map(|e| match &e.payload {
                EventPayload::LlmRequest(p) => Some(p),
                _ => None,
            })
            .nth(1)
            .expect("missing second llm.request");
        let tool_role_count = second_req
            .messages
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Tool))
            .count();
        assert_eq!(tool_role_count, 1, "expected one tool-role message");
    }

    #[tokio::test]
    async fn tool_sandbox_violations_surface_to_the_llm() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        let allowed = tempdir().unwrap();
        let forbidden_dir = tempdir().unwrap();
        let target = forbidden_dir.path().join("secret.txt");
        std::fs::write(&target, "no").unwrap();

        let agent_id = unique_agent_id("sandbox-violator");
        let agent = Agent::builder()
            .id(agent_id.clone())
            .model("claude-haiku")
            .system_prompt("Try to read a file.")
            .tools(["file_read"])
            .sandbox(Sandbox::new().fs_read(allowed.path().to_string_lossy().to_string()))
            .budget(1.0)
            .build()
            .unwrap();

        let llm = FixtureClient::new();
        llm.push_response(tool_call_response(
            "file_read",
            "call_violate",
            json!({ "path": target.to_string_lossy() }),
            (50, 20),
        ));
        llm.push_response(canned_response("I could not read the file.", 80, 30));

        let bus = EventBus::connect(&url).await.expect("connect to NATS");
        let executor = AgentExecutor::new(bus.clone(), test_pricing(), test_tools());

        let mut subscriber = bus
            .subscribe(format!("fq.agent.{agent_id}.>"))
            .await
            .expect("subscribe");
        tokio::time::sleep(Duration::from_millis(50)).await;

        executor
            .run(&agent, &llm, TriggerSource::Manual, None, json!({}))
            .await
            .expect("run completes");

        // Collect events until we see tool.result.
        let mut seen_tool_result = None;
        for _ in 0..10 {
            let event = tokio::time::timeout(Duration::from_secs(2), subscriber.next())
                .await
                .expect("timeout waiting for event")
                .expect("stream closed")
                .expect("deserialise");
            if let EventPayload::ToolResult(p) = &event.payload {
                seen_tool_result = Some(p.clone());
                break;
            }
        }

        let result = seen_tool_result.expect("no tool.result received");
        assert!(result.is_error);
        assert!(matches!(
            result.error_kind,
            Some(ToolErrorKind::SandboxViolation)
        ));
    }

    #[tokio::test]
    async fn tool_not_in_agent_allowlist_is_denied() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        let agent_id = unique_agent_id("allowlist");
        // Agent only has file_read, but the LLM tries to use file_write.
        let agent = Agent::builder()
            .id(agent_id.clone())
            .model("claude-haiku")
            .system_prompt("You like to write.")
            .tools(["file_read"])
            .budget(1.0)
            .build()
            .unwrap();

        let llm = FixtureClient::new();
        llm.push_response(tool_call_response(
            "file_write",
            "call_deny",
            json!({ "path": "/tmp/x", "content": "x" }),
            (50, 20),
        ));
        llm.push_response(canned_response("Done anyway.", 80, 30));

        let bus = EventBus::connect(&url).await.expect("connect to NATS");
        let executor = AgentExecutor::new(bus.clone(), test_pricing(), test_tools());

        let mut subscriber = bus
            .subscribe(format!("fq.agent.{agent_id}.>"))
            .await
            .expect("subscribe");
        tokio::time::sleep(Duration::from_millis(50)).await;

        executor
            .run(&agent, &llm, TriggerSource::Manual, None, json!({}))
            .await
            .expect("run completes");

        let mut saw_denied = false;
        for _ in 0..10 {
            let event = tokio::time::timeout(Duration::from_secs(2), subscriber.next())
                .await
                .expect("timeout")
                .expect("stream closed")
                .expect("deserialise");
            if let EventPayload::ToolResult(p) = &event.payload {
                assert!(p.is_error);
                assert!(matches!(p.error_kind, Some(ToolErrorKind::PermissionDenied)));
                saw_denied = true;
                break;
            }
        }
        assert!(saw_denied, "expected a denied tool.result");
    }

    #[tokio::test]
    async fn emits_failed_event_when_budget_exceeded() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        let bus = EventBus::connect(&url).await.expect("connect to NATS");
        let executor = AgentExecutor::new(bus.clone(), test_pricing(), test_tools());

        let agent_id = unique_agent_id("overspender");
        let agent = Agent::builder()
            .id(agent_id.clone())
            .model("claude-haiku")
            .system_prompt("You spend a lot.")
            .budget(0.0001)
            .build()
            .unwrap();

        let llm = FixtureClient::new();
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

    fn event_kind(e: &Event) -> &'static str {
        match &e.payload {
            EventPayload::Triggered(_) => "triggered",
            EventPayload::LlmRequest(_) => "llm_request",
            EventPayload::LlmResponse(_) => "llm_response",
            EventPayload::ToolCall(_) => "tool_call",
            EventPayload::ToolResult(_) => "tool_result",
            EventPayload::Cost(_) => "cost",
            EventPayload::Completed(_) => "completed",
            EventPayload::Failed(_) => "failed",
            EventPayload::SystemStartup(_) => "system_startup",
            EventPayload::SystemShutdown(_) => "system_shutdown",
            EventPayload::SystemTaskFailed(_) => "system_task_failed",
        }
    }
}
