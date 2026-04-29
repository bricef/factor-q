//! Host-side loop driver for the reducer harness.
//!
//! Drives any [`Reducer`] impl through a complete agent
//! invocation, executing the requested [`NextAction`]s against
//! the existing runtime infrastructure (LLM client, tool
//! registry, event bus, pricing table) and feeding the results
//! back to the reducer.
//!
//! The runner emits the same canonical event sequence as
//! [`crate::AgentExecutor`] (`triggered` → `llm.request` →
//! `llm.response` → `cost` → optional `tool.call` /
//! `tool.result` → ... → `completed` / `failed`) so projection
//! consumers and downstream observers cannot tell which path
//! produced an invocation.
//!
//! This is the host side of the reducer/host boundary. The
//! reducer decides what to do next; the runner makes it happen.

use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use fq_tools::builtin::SELF_INSPECT_TOOL_NAME;
use fq_tools::{ToolContext, ToolError, ToolSandbox};
use serde_json::Value;
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::types::{
    AgentConfig, CapabilityResult, EmittedEvent, HarnessError, LogEntry, LogLevel, ModelRequest,
    ModelResponse, NextAction, Reducer, StepInput, ToolCallRequest, ToolCallResult, TriggerPayload,
    TriggerSourceKind,
};
use crate::agent::Agent;
use crate::bus::EventBus;
use crate::events::{
    self, CompletedPayload, CostPayload, Event, EventPayload, FailedPayload, FailureKind,
    FailurePhase, InvocationTotals, LlmRequestPayload, LlmResponsePayload, ToolCallPayload,
    ToolErrorKind, ToolResultPayload, TriggerSource, TriggeredPayload,
};
use crate::executor::{ExecutorError, InvocationOutcome};
use crate::llm::{ChatRequest, ChatResponse, LlmClient};
use crate::pricing::PricingTable;
use crate::tools::ToolRegistry;

/// Soft cap on the number of `step()` calls per invocation.
/// Independent of the reducer's own `max_iterations` so a buggy
/// reducer (e.g. one that perpetually returns CallModel without
/// progress) cannot wedge the host indefinitely.
const HOST_STEP_BUDGET: u32 = 1_000;

/// Drive an agent invocation through a [`Reducer`]. Composes
/// the same runtime pieces as the legacy [`crate::AgentExecutor`].
pub struct ReducerRunner {
    bus: EventBus,
    pricing: Arc<PricingTable>,
    tools: Arc<ToolRegistry>,
}

impl ReducerRunner {
    pub fn new(bus: EventBus, pricing: Arc<PricingTable>, tools: Arc<ToolRegistry>) -> Self {
        Self {
            bus,
            pricing,
            tools,
        }
    }

    /// Run a single invocation of `agent` through the supplied
    /// reducer. Behavioural twin of [`crate::AgentExecutor::run`].
    pub async fn run<R: Reducer + Send + Sync>(
        &self,
        reducer: &R,
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
            "starting reducer invocation"
        );

        let sandbox = agent.sandbox().to_tool_sandbox();
        let tool_schemas = self.tools.build_schemas(agent.tools());

        let agent_config = AgentConfig {
            agent_id: agent_id.clone(),
            model: agent.model().to_string(),
            system_prompt: agent.system_prompt().to_string(),
            tools_available: tool_schemas.clone(),
            allowed_tool_names: agent.tools().to_vec(),
            max_iterations: 0, // 0 = use harness default
        };

        let trigger = TriggerPayload {
            source: match trigger_source {
                TriggerSource::Manual => TriggerSourceKind::Manual,
                TriggerSource::Subject => TriggerSourceKind::Subject,
                TriggerSource::Schedule => TriggerSourceKind::Schedule,
            },
            subject: trigger_subject.clone(),
            payload: trigger_payload.clone(),
        };

        // Emit `triggered` once, mirroring the legacy executor.
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

        let mut state: Vec<u8> = Vec::new();
        let mut last_result: Option<CapabilityResult> = None;

        for step_index in 0..HOST_STEP_BUDGET {
            let input = StepInput {
                config: agent_config.clone(),
                trigger: trigger.clone(),
                state,
                last_result,
                now_ms: now_ms(),
                random_seed: rand_u64(),
                step_index,
            };

            let output = match reducer.step(input) {
                Ok(o) => o,
                Err(err) => {
                    totals.total_duration_ms = start.elapsed().as_millis() as u64;
                    self.emit_failed(
                        &agent_id,
                        invocation_id,
                        FailureKind::RuntimeError,
                        format!("reducer step failed: {err}"),
                        FailurePhase::LlmResponse,
                        totals,
                    )
                    .await?;
                    return Err(ExecutorError::MaxIterationsExceeded);
                }
            };

            self.write_logs(&agent_id, invocation_id, &output.logs);
            self.emit_semantic_events(&output.events);
            state = output.state;

            match output.next_action {
                NextAction::Complete(text) => {
                    let duration_ms = start.elapsed().as_millis() as u64;
                    totals.total_duration_ms = duration_ms;
                    let summary = if text.is_empty() { None } else { Some(text) };
                    self.publish(Event::new(
                        agent_id.clone(),
                        invocation_id,
                        EventPayload::Completed(CompletedPayload {
                            result_summary: summary.clone(),
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
                        "reducer invocation completed"
                    );

                    return Ok(InvocationOutcome::Completed {
                        invocation_id,
                        response: ChatResponse {
                            content: summary,
                            tool_calls: vec![],
                            stop_reason: events::StopReason::EndTurn,
                            usage: events::TokenUsage::default(),
                        },
                        cost: totals.total_cost,
                        duration_ms,
                    });
                }
                NextAction::Failed(err) => {
                    totals.total_duration_ms = start.elapsed().as_millis() as u64;
                    let kind = harness_error_to_failure_kind(&err);
                    self.emit_failed(
                        &agent_id,
                        invocation_id,
                        kind,
                        err.message.clone(),
                        FailurePhase::LlmResponse,
                        totals,
                    )
                    .await?;
                    return Err(ExecutorError::MaxIterationsExceeded);
                }
                NextAction::CallModel(request) => {
                    let outcome = self
                        .run_model_with_llm(
                            llm,
                            agent.budget(),
                            &agent_id,
                            invocation_id,
                            request,
                            &mut totals,
                            start,
                        )
                        .await?;
                    match outcome {
                        ModelOutcome::Response(resp) => {
                            last_result = Some(CapabilityResult::ModelResult(resp));
                        }
                        ModelOutcome::BudgetExceeded(cost) => {
                            return Ok(InvocationOutcome::BudgetExceeded {
                                invocation_id,
                                cost,
                            });
                        }
                    }
                }
                NextAction::CallTool(req) => {
                    let result = self
                        .run_tool(
                            agent,
                            &sandbox,
                            &agent_id,
                            invocation_id,
                            req,
                            &totals,
                            start,
                        )
                        .await?;
                    totals.total_tool_calls += 1;
                    last_result = Some(CapabilityResult::ToolResult(result));
                }
                NextAction::CallToolsParallel(reqs) => {
                    // For the prototype: dispatch sequentially in
                    // request order. The protocol contract is "host
                    // returns results in request order"; concurrency
                    // is a host implementation detail and tracking
                    // it is a phase-2 concern. The reducer cannot
                    // tell sequential from concurrent execution.
                    let mut results = Vec::with_capacity(reqs.len());
                    for req in reqs {
                        let result = self
                            .run_tool(
                                agent,
                                &sandbox,
                                &agent_id,
                                invocation_id,
                                req,
                                &totals,
                                start,
                            )
                            .await?;
                        totals.total_tool_calls += 1;
                        results.push(result);
                    }
                    last_result = Some(CapabilityResult::ParallelToolResults(results));
                }
            }
        }

        // Host step budget exhausted. Surface as a runtime failure.
        totals.total_duration_ms = start.elapsed().as_millis() as u64;
        self.emit_failed(
            &agent_id,
            invocation_id,
            FailureKind::RuntimeError,
            format!("host step budget exhausted ({HOST_STEP_BUDGET})"),
            FailurePhase::LlmResponse,
            totals,
        )
        .await?;
        Err(ExecutorError::MaxIterationsExceeded)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_tool(
        &self,
        agent: &Agent,
        sandbox: &ToolSandbox,
        agent_id: &str,
        invocation_id: Uuid,
        req: ToolCallRequest,
        totals: &InvocationTotals,
        start: Instant,
    ) -> Result<ToolCallResult, ExecutorError> {
        if !agent.tools().iter().any(|name| name == &req.tool_name) {
            return self
                .emit_synthetic_tool_error(
                    agent_id,
                    invocation_id,
                    &req,
                    ToolErrorKind::PermissionDenied,
                    format!("tool '{}' is not available to this agent", req.tool_name),
                )
                .await;
        }

        self.publish(Event::new(
            agent_id.to_string(),
            invocation_id,
            EventPayload::ToolCall(ToolCallPayload {
                tool_call_id: req.tool_call_id.clone(),
                tool_name: req.tool_name.clone(),
                parameters: req.parameters.clone(),
            }),
        ))
        .await?;

        // self_inspect is a host-fulfilled tool: the registry has the
        // schema but the data lives here. Intercept before falling
        // through to `Tool::execute` (which would surface a tripwire
        // error). See `crate::introspection`.
        if req.tool_name == SELF_INSPECT_TOOL_NAME {
            return self
                .run_self_inspect(agent, agent_id, invocation_id, req, totals, start)
                .await;
        }

        let tool = match self.tools.get(&req.tool_name) {
            Some(t) => t,
            None => {
                return self
                    .emit_synthetic_tool_error(
                        agent_id,
                        invocation_id,
                        &req,
                        ToolErrorKind::ExecutionFailed,
                        format!("no implementation registered for tool '{}'", req.tool_name),
                    )
                    .await;
            }
        };

        let ctx = ToolContext::new(sandbox);
        let tool_start = Instant::now();
        let outcome = tool.execute(&ctx, req.parameters.clone()).await;
        let duration_ms = tool_start.elapsed().as_millis() as u64;

        match outcome {
            Ok(result) => {
                self.publish(Event::new(
                    agent_id.to_string(),
                    invocation_id,
                    EventPayload::ToolResult(ToolResultPayload {
                        tool_call_id: req.tool_call_id.clone(),
                        output: result.output.clone(),
                        is_error: result.is_error,
                        error_kind: None,
                        duration_ms,
                    }),
                ))
                .await?;
                Ok(ToolCallResult {
                    tool_call_id: req.tool_call_id,
                    output: result.output,
                    is_error: result.is_error,
                    error_kind: None,
                    duration_ms,
                })
            }
            Err(err) => {
                let (kind, message) = classify_tool_error(&err);
                self.publish(Event::new(
                    agent_id.to_string(),
                    invocation_id,
                    EventPayload::ToolResult(ToolResultPayload {
                        tool_call_id: req.tool_call_id.clone(),
                        output: message.clone(),
                        is_error: true,
                        error_kind: Some(kind),
                        duration_ms,
                    }),
                ))
                .await?;
                Ok(ToolCallResult {
                    tool_call_id: req.tool_call_id,
                    output: message,
                    is_error: true,
                    error_kind: Some(kind),
                    duration_ms,
                })
            }
        }
    }

    async fn run_self_inspect(
        &self,
        agent: &Agent,
        agent_id: &str,
        invocation_id: Uuid,
        req: ToolCallRequest,
        totals: &InvocationTotals,
        start: Instant,
    ) -> Result<ToolCallResult, ExecutorError> {
        use crate::introspection::{HostInvocationStats, synthesize_self_inspect};

        let tool_start = Instant::now();
        let stats = HostInvocationStats {
            agent_id,
            model: agent.model(),
            allowed_tool_names: agent.tools(),
            budget: agent.budget(),
            // The reducer harness uses its `DEFAULT_MAX_ITERATIONS`
            // when AgentConfig.max_iterations is 0. Mirror that so
            // self_inspect's reported value matches what actually
            // bounds the agent.
            max_iterations: crate::reducer::harness::DEFAULT_MAX_ITERATIONS,
            totals: *totals,
            elapsed_ms: start.elapsed().as_millis() as u64,
        };
        let output = synthesize_self_inspect(&stats, req.parameters.clone());
        let duration_ms = tool_start.elapsed().as_millis() as u64;

        self.publish(Event::new(
            agent_id.to_string(),
            invocation_id,
            EventPayload::ToolResult(ToolResultPayload {
                tool_call_id: req.tool_call_id.clone(),
                output: output.clone(),
                is_error: false,
                error_kind: None,
                duration_ms,
            }),
        ))
        .await?;

        Ok(ToolCallResult {
            tool_call_id: req.tool_call_id,
            output,
            is_error: false,
            error_kind: None,
            duration_ms,
        })
    }

    async fn emit_synthetic_tool_error(
        &self,
        agent_id: &str,
        invocation_id: Uuid,
        req: &ToolCallRequest,
        kind: ToolErrorKind,
        message: String,
    ) -> Result<ToolCallResult, ExecutorError> {
        self.publish(Event::new(
            agent_id.to_string(),
            invocation_id,
            EventPayload::ToolResult(ToolResultPayload {
                tool_call_id: req.tool_call_id.clone(),
                output: message.clone(),
                is_error: true,
                error_kind: Some(kind),
                duration_ms: 0,
            }),
        ))
        .await?;
        Ok(ToolCallResult {
            tool_call_id: req.tool_call_id.clone(),
            output: message,
            is_error: true,
            error_kind: Some(kind),
            duration_ms: 0,
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
            "reducer invocation failed"
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

    fn write_logs(&self, agent_id: &str, invocation_id: Uuid, logs: &[LogEntry]) {
        for entry in logs {
            match entry.level {
                LogLevel::Trace => tracing::trace!(
                    agent_id = %agent_id, invocation_id = %invocation_id,
                    "{}", entry.message
                ),
                LogLevel::Debug => tracing::debug!(
                    agent_id = %agent_id, invocation_id = %invocation_id,
                    "{}", entry.message
                ),
                LogLevel::Info => tracing::info!(
                    agent_id = %agent_id, invocation_id = %invocation_id,
                    "{}", entry.message
                ),
                LogLevel::Warn => tracing::warn!(
                    agent_id = %agent_id, invocation_id = %invocation_id,
                    "{}", entry.message
                ),
                LogLevel::Error => tracing::error!(
                    agent_id = %agent_id, invocation_id = %invocation_id,
                    "{}", entry.message
                ),
            }
        }
    }

    fn emit_semantic_events(&self, events: &[EmittedEvent]) {
        // Reserved for guest-emitted semantic events. The
        // canonical lifecycle events go through `publish` from
        // the host directly. For the prototype we just trace the
        // payload — wiring these to NATS is straightforward but
        // not load-bearing for the reducer claim.
        for ev in events {
            tracing::debug!(kind = %ev.kind, payload = %ev.payload, "guest semantic event");
        }
    }
}

/// Internal: factor out the LLM dispatch path so the loop body
/// stays readable.
impl ReducerRunner {
    #[allow(clippy::too_many_arguments)]
    async fn run_model_with_llm(
        &self,
        llm: &dyn LlmClient,
        budget: Option<f64>,
        agent_id: &str,
        invocation_id: Uuid,
        request: ModelRequest,
        totals: &mut InvocationTotals,
        start: Instant,
    ) -> Result<ModelOutcome, ExecutorError> {
        let call_id = Uuid::now_v7();
        let chat_request = ChatRequest {
            model: request.model.clone(),
            messages: request.messages.clone(),
            tools: request.tools.clone(),
            params: request.params.clone(),
        };

        self.publish(Event::new(
            agent_id.to_string(),
            invocation_id,
            EventPayload::LlmRequest(LlmRequestPayload {
                call_id,
                model: chat_request.model.clone(),
                messages: chat_request.messages.clone(),
                tools_available: chat_request.tools.clone(),
                request_params: chat_request.params.clone(),
            }),
        ))
        .await?;

        let response = match llm.chat(chat_request).await {
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

        let pricing = self.pricing.lookup(&request.model);
        if pricing.is_none() {
            warn!(
                model = %request.model,
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
                model: request.model.clone(),
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

        if let Some(budget) = budget
            && totals.total_cost > budget
        {
            totals.total_duration_ms = start.elapsed().as_millis() as u64;
            self.emit_failed(
                agent_id,
                invocation_id,
                FailureKind::BudgetExceeded,
                format!(
                    "cost ${:.6} exceeded budget ${budget:.2}",
                    totals.total_cost
                ),
                FailurePhase::LlmResponse,
                *totals,
            )
            .await?;
            return Ok(ModelOutcome::BudgetExceeded(totals.total_cost));
        }

        Ok(ModelOutcome::Response(ModelResponse {
            content: response.content,
            tool_calls: response.tool_calls,
            stop_reason: response.stop_reason,
            usage: response.usage,
        }))
    }
}

enum ModelOutcome {
    Response(ModelResponse),
    BudgetExceeded(f64),
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

fn harness_error_to_failure_kind(err: &HarnessError) -> FailureKind {
    use super::types::HarnessErrorKind::*;
    match err.kind {
        MaxIterations => FailureKind::RuntimeError,
        InternalError => FailureKind::RuntimeError,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn rand_u64() -> u64 {
    // Cheap host-side randomness. The reducer is not allowed to
    // read time/randomness directly, but that's enforced by the
    // boundary: it can only see what we put in `StepInput`.
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    //! Behavioural-equivalence and end-to-end tests for the
    //! reducer host loop. These need NATS, so they skip when
    //! `FQ_NATS_URL` is unset — same pattern as the legacy
    //! executor's tests.
    //!
    //! The point of these tests is the *equivalence* claim:
    //! given the same scripted LLM responses and the same
    //! agent definition, the reducer path must produce the
    //! same canonical event sequence as the legacy executor.
    //! If that holds, dispatching through the reducer path is
    //! invisible to downstream observers.
    //!
    //! What's *not* tested here: cost numbers (already covered
    //! by the legacy executor tests, and the runner reuses the
    //! exact same pricing code path), and the deeper purity
    //! claims (covered by the unit tests in `harness.rs`).
    use super::*;
    use crate::agent::{Agent, Sandbox};
    use crate::bus::EventBus;
    use crate::events::{StopReason, TokenUsage};
    use crate::executor::AgentExecutor;
    use crate::llm::fixture::FixtureClient;
    use crate::pricing::ModelPricing;
    use crate::reducer::Harness;
    use crate::tools::ToolRegistry;
    use crate::{events::EventPayload, llm::ChatResponse};
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

    fn canned(text: &str, input: u32, output: u32) -> ChatResponse {
        ChatResponse {
            content: Some(text.to_string()),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        }
    }

    fn tool_use(name: &str, call_id: &str, params: Value, tokens: (u32, u32)) -> ChatResponse {
        ChatResponse {
            content: None,
            tool_calls: vec![crate::events::MessageToolCall {
                tool_call_id: call_id.to_string(),
                tool_name: name.to_string(),
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

    use crate::test_support::events::event_kind;

    /// Run the same scripted scenario through both executors
    /// and collect each emitted event sequence. The bus is
    /// distinct per agent, so no cross-talk.
    async fn run_through_legacy_and_reducer(
        url: &str,
        agent_factory: impl Fn(String) -> Agent,
        responses: impl Fn() -> Vec<ChatResponse>,
        expected_events: usize,
    ) -> (Vec<&'static str>, Vec<&'static str>) {
        let bus = EventBus::connect(url).await.expect("connect to NATS");

        let collect_events = async |agent: Agent, ev_count: usize| -> Vec<Event> {
            let mut sub = bus
                .subscribe(format!("fq.agent.{}.>", agent.id().as_str()))
                .await
                .expect("subscribe");
            tokio::time::sleep(Duration::from_millis(50)).await;
            let llm = FixtureClient::new();
            for r in responses() {
                llm.push_response(r);
            }
            let outcome = AgentExecutor::new(
                bus.clone(),
                test_pricing(),
                Arc::new(ToolRegistry::with_builtins()),
            )
            .run(
                &agent,
                &llm,
                TriggerSource::Manual,
                None,
                json!({"input": "go"}),
            )
            .await;
            let _ = outcome;
            let mut out = Vec::new();
            for _ in 0..ev_count {
                let event = tokio::time::timeout(Duration::from_secs(2), sub.next())
                    .await
                    .expect("legacy timeout")
                    .expect("legacy stream closed")
                    .expect("legacy deserialise");
                out.push(event);
            }
            out
        };

        let collect_reducer = async |agent: Agent, ev_count: usize| -> Vec<Event> {
            let mut sub = bus
                .subscribe(format!("fq.agent.{}.>", agent.id().as_str()))
                .await
                .expect("subscribe");
            tokio::time::sleep(Duration::from_millis(50)).await;
            let llm = FixtureClient::new();
            for r in responses() {
                llm.push_response(r);
            }
            let runner = ReducerRunner::new(
                bus.clone(),
                test_pricing(),
                Arc::new(ToolRegistry::with_builtins()),
            );
            let _ = runner
                .run(
                    &Harness::new(),
                    &agent,
                    &llm,
                    TriggerSource::Manual,
                    None,
                    json!({"input": "go"}),
                )
                .await;
            let mut out = Vec::new();
            for _ in 0..ev_count {
                let event = tokio::time::timeout(Duration::from_secs(2), sub.next())
                    .await
                    .expect("reducer timeout")
                    .expect("reducer stream closed")
                    .expect("reducer deserialise");
                out.push(event);
            }
            out
        };

        let legacy_events =
            collect_events(agent_factory(unique_agent_id("legacy")), expected_events).await;
        let reducer_events =
            collect_reducer(agent_factory(unique_agent_id("reducer")), expected_events).await;

        let legacy_kinds: Vec<&'static str> = legacy_events.iter().map(event_kind).collect();
        let reducer_kinds: Vec<&'static str> = reducer_events.iter().map(event_kind).collect();
        (legacy_kinds, reducer_kinds)
    }

    #[tokio::test]
    async fn equivalent_event_sequence_for_simple_completion() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        let (legacy, reducer) = run_through_legacy_and_reducer(
            &url,
            |id| {
                Agent::builder()
                    .id(id)
                    .model("claude-haiku")
                    .system_prompt("You are a test agent.")
                    .budget(1.0)
                    .build()
                    .unwrap()
            },
            || vec![canned("Hello.", 100, 50)],
            5,
        )
        .await;

        assert_eq!(
            legacy,
            vec![
                "triggered",
                "llm_request",
                "llm_response",
                "cost",
                "completed"
            ]
        );
        assert_eq!(
            reducer, legacy,
            "reducer path must match legacy event order"
        );
    }

    #[tokio::test]
    async fn equivalent_event_sequence_for_tool_call_loop() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        let dir = tempdir().unwrap();
        let target = dir.path().join("hello.md");
        std::fs::write(&target, "# hello").unwrap();
        let target_path = target.to_string_lossy().to_string();
        let allowed_dir = dir.path().to_string_lossy().to_string();

        let (legacy, reducer) = run_through_legacy_and_reducer(
            &url,
            |id| {
                Agent::builder()
                    .id(id)
                    .model("claude-haiku")
                    .system_prompt("Use tools when asked.")
                    .tools(["file_read"])
                    .sandbox(Sandbox::new().fs_read(allowed_dir.clone()))
                    .budget(1.0)
                    .build()
                    .unwrap()
            },
            || {
                vec![
                    tool_use(
                        "file_read",
                        "call_abc",
                        json!({"path": target_path.clone()}),
                        (100, 50),
                    ),
                    canned("Got it.", 150, 20),
                ]
            },
            10,
        )
        .await;

        let expected = vec![
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
        ];
        assert_eq!(legacy, expected);
        assert_eq!(reducer, expected, "reducer must emit the same sequence");
    }

    #[tokio::test]
    async fn reducer_suspend_resume_yields_same_completion() {
        // Demonstrates the suspend/resume claim end-to-end:
        // run the reducer until step boundary N, capture the
        // opaque state, throw the runner away, run a fresh
        // runner from the captured state, and check the final
        // completion is structurally the same.
        //
        // For the prototype this is implemented at the
        // reducer-state level (no host bus interleaving),
        // matching the unit-test `state_round_trips` pattern
        // but starting from the runner-built `AgentConfig`.
        use crate::reducer::types::{
            AgentConfig, CapabilityResult, ModelResponse, NextAction, StepInput, TriggerPayload,
            TriggerSourceKind,
        };

        let cfg = AgentConfig {
            agent_id: "suspend-resume".to_string(),
            model: "claude-haiku".to_string(),
            system_prompt: "be brief.".to_string(),
            tools_available: vec![],
            allowed_tool_names: vec![],
            max_iterations: 0,
        };
        let trig = TriggerPayload {
            source: TriggerSourceKind::Manual,
            subject: None,
            payload: json!("ping"),
        };

        let h1 = Harness::new();
        let s0 = h1
            .step(StepInput {
                config: cfg.clone(),
                trigger: trig.clone(),
                state: vec![],
                last_result: None,
                now_ms: 0,
                random_seed: 0,
                step_index: 0,
            })
            .unwrap();
        // Suspended snapshot.
        let snapshot = s0.state.clone();

        // Drop and replace the reducer.
        drop(h1);
        let h2 = Harness::new();

        let s1 = h2
            .step(StepInput {
                config: cfg,
                trigger: trig,
                state: snapshot,
                last_result: Some(CapabilityResult::ModelResult(ModelResponse {
                    content: Some("pong".to_string()),
                    tool_calls: vec![],
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::default(),
                })),
                now_ms: 1,
                random_seed: 1,
                step_index: 1,
            })
            .unwrap();

        match s1.next_action {
            NextAction::Complete(text) => assert_eq!(text, "pong"),
            other => panic!("expected Complete after resume, got {other:?}"),
        }
    }

    /// `self_inspect` is a host-fulfilled tool: the schema lives
    /// in `fq-tools` but the data is synthesised by the runner.
    /// This test runs an agent that calls `self_inspect`, lets
    /// the reducer drive a real two-turn loop (call → result →
    /// final), and asserts the tool result message contains
    /// the synthesised JSON fields.
    #[tokio::test]
    async fn self_inspect_is_dispatched_by_the_runner() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        let agent_id = unique_agent_id("self-inspect");
        let agent = Agent::builder()
            .id(agent_id.clone())
            .model("claude-haiku")
            .system_prompt("Inspect yourself when asked.")
            .tools(["self_inspect"])
            .budget(0.50)
            .build()
            .unwrap();

        let llm = FixtureClient::new();
        // Turn 1: model asks for self_inspect.
        llm.push_response(tool_use("self_inspect", "call_si", json!({}), (100, 50)));
        // Turn 2: model summarises and finishes.
        llm.push_response(canned("I have one budget left.", 150, 30));

        let bus = EventBus::connect(&url).await.expect("connect to NATS");
        let runner = ReducerRunner::new(
            bus.clone(),
            test_pricing(),
            Arc::new(ToolRegistry::with_builtins()),
        );

        let mut sub = bus
            .subscribe(format!("fq.agent.{agent_id}.>"))
            .await
            .expect("subscribe");
        tokio::time::sleep(Duration::from_millis(50)).await;

        runner
            .run(
                &Harness::new(),
                &agent,
                &llm,
                TriggerSource::Manual,
                None,
                json!({}),
            )
            .await
            .expect("invocation");

        let mut tool_result_output: Option<String> = None;
        for _ in 0..15 {
            let event = tokio::time::timeout(Duration::from_secs(2), sub.next())
                .await
                .expect("timeout")
                .expect("stream closed")
                .expect("deserialise");
            if let EventPayload::ToolResult(p) = &event.payload {
                tool_result_output = Some(p.output.clone());
                break;
            }
        }
        let raw = tool_result_output.expect("no tool.result observed");
        let parsed: Value = serde_json::from_str(&raw).expect("self_inspect output is JSON");
        assert!(parsed.get("model").is_some(), "missing model section");
        assert!(parsed.get("budget").is_some(), "missing budget section");
        assert!(parsed.get("tools").is_some(), "missing tools section");
        assert_eq!(parsed["model"], "claude-haiku");
        // The agent has just made its first LLM call when self_inspect
        // is dispatched; tool counter is still 0 at synthesis time.
        assert_eq!(parsed["iterations"]["llm_calls_made"], 1);
        assert_eq!(parsed["iterations"]["tool_calls_made"], 0);
    }

    /// The motivating test for picking SelfInspect as the first
    /// reducer-aware feature: suspension across a tool dispatch.
    /// We let the harness produce the `CallTool(self_inspect)`
    /// step, capture state, drop the harness, run the synthetic
    /// tool-fulfilment ourselves, and resume with a fresh
    /// harness on the captured state. The final completion
    /// must match a non-suspended run.
    #[tokio::test]
    async fn reducer_suspends_and_resumes_across_tool_dispatch() {
        use crate::introspection::{HostInvocationStats, synthesize_self_inspect};
        use crate::reducer::types::{
            AgentConfig, CapabilityResult, ModelResponse, NextAction, StepInput, ToolCallResult,
            TriggerPayload, TriggerSourceKind,
        };

        let cfg = AgentConfig {
            agent_id: "suspend-tools".to_string(),
            model: "claude-haiku".to_string(),
            system_prompt: "introspect on demand.".to_string(),
            tools_available: vec![],
            allowed_tool_names: vec!["self_inspect".to_string()],
            max_iterations: 0,
        };
        let trig = TriggerPayload {
            source: TriggerSourceKind::Manual,
            subject: None,
            payload: json!("inspect"),
        };

        let mk = |state: Vec<u8>, last: Option<CapabilityResult>, idx: u32| StepInput {
            config: cfg.clone(),
            trigger: trig.clone(),
            state,
            last_result: last,
            now_ms: idx as u64,
            random_seed: idx as u64,
            step_index: idx,
        };

        // Step 0: seed → CallModel.
        let h = Harness::new();
        let s0 = h.step(mk(vec![], None, 0)).unwrap();

        // Step 1: model returns a self_inspect tool_use → CallTool.
        let s1 = h
            .step(mk(
                s0.state,
                Some(CapabilityResult::ModelResult(ModelResponse {
                    content: None,
                    tool_calls: vec![crate::events::MessageToolCall {
                        tool_call_id: "si".to_string(),
                        tool_name: "self_inspect".to_string(),
                        parameters: json!({"include": ["budget"]}),
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: TokenUsage::default(),
                })),
                1,
            ))
            .unwrap();
        let _call_request = match s1.next_action {
            NextAction::CallTool(req) => req,
            other => panic!("expected CallTool, got {other:?}"),
        };

        // Suspension point: we have `state` and the pending tool
        // call. Persist them. (In a real durable-resume scenario
        // these would be written to disk together — same shape.)
        let suspended_state = s1.state.clone();

        // Drop the entire harness and conjure a fresh one. This
        // is the load-bearing assertion: nothing in-process state
        // survives the boundary.
        drop(h);

        // Synthesise the tool result host-side, exactly like the
        // runner would have. This is the "tool was dispatched
        // while we were suspended" case.
        let tool_output = synthesize_self_inspect(
            &HostInvocationStats {
                agent_id: "suspend-tools",
                model: "claude-haiku",
                allowed_tool_names: &["self_inspect".to_string()],
                budget: Some(0.50),
                max_iterations: 20,
                totals: InvocationTotals {
                    total_llm_calls: 1,
                    total_tool_calls: 0,
                    total_cost: 0.0001,
                    total_duration_ms: 0,
                },
                elapsed_ms: 0,
            },
            json!({"include": ["budget"]}),
        );

        let h2 = Harness::new();

        // Step 2 (post-resume): feed the tool result. Reducer
        // integrates it and asks for the next model turn.
        let s2 = h2
            .step(mk(
                suspended_state,
                Some(CapabilityResult::ToolResult(ToolCallResult {
                    tool_call_id: "si".to_string(),
                    output: tool_output.clone(),
                    is_error: false,
                    error_kind: None,
                    duration_ms: 0,
                })),
                2,
            ))
            .unwrap();
        let next_req = match s2.next_action {
            NextAction::CallModel(req) => req,
            other => panic!("expected CallModel after tool result, got {other:?}"),
        };
        // The conversation history must contain the tool message
        // we just resumed with — verifies state round-tripping.
        assert!(
            next_req
                .messages
                .iter()
                .any(|m| matches!(m.role, crate::events::MessageRole::Tool)
                    && m.content.as_deref() == Some(tool_output.as_str())),
            "resumed conversation missing tool message"
        );

        // Step 3: model answers based on the inspected state.
        let s3 = h2
            .step(mk(
                s2.state,
                Some(CapabilityResult::ModelResult(ModelResponse {
                    content: Some("inspected.".to_string()),
                    tool_calls: vec![],
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::default(),
                })),
                3,
            ))
            .unwrap();

        match s3.next_action {
            NextAction::Complete(text) => assert_eq!(text, "inspected."),
            other => panic!("expected Complete after resumed inspection, got {other:?}"),
        }
    }
}
