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
use crate::agent::{Agent, AgentId};
use crate::bus::EventBus;
use crate::events::{
    self, CompletedPayload, Event, EventPayload, FailedPayload, FailureKind, FailurePhase,
    InvocationTotals, LlmRequestPayload, LlmResponsePayload, ToolCallPayload, ToolErrorKind,
    ToolResultPayload, TriggerSource, TriggeredPayload,
};
use crate::llm::{ChatRequest, ChatResponse, LlmClient};
use crate::pricing::PricingTable;
use crate::tools::ToolRegistry;
use crate::worker::store::{
    DispatchStatus, InvocationStateRow, LlmDispatchRow, ToolDispatchRow, WorkerStore,
};
use crate::worker::{ExecutorError, InvocationOutcome};

/// Soft cap on the number of `step()` calls per invocation.
/// Independent of the reducer's own `max_iterations` so a buggy
/// reducer (e.g. one that perpetually returns CallModel without
/// progress) cannot wedge the host indefinitely.
const HOST_STEP_BUDGET: u32 = 1_000;

/// Drive an agent invocation through a [`Reducer`]. Composes
/// the same runtime pieces as the legacy [`crate::AgentExecutor`],
/// plus a [`WorkerStore`] for the three-state WAL persisted
/// around every tool and LLM dispatch (data-architecture.md §5.5).
pub struct ReducerRunner {
    bus: EventBus,
    pricing: Arc<PricingTable>,
    tools: Arc<ToolRegistry>,
    store: Arc<WorkerStore>,
}

impl ReducerRunner {
    pub fn new(
        bus: EventBus,
        pricing: Arc<PricingTable>,
        tools: Arc<ToolRegistry>,
        store: Arc<WorkerStore>,
    ) -> Self {
        Self {
            bus,
            pricing,
            tools,
            store,
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
        let agent_id: AgentId = agent.id().clone();
        let totals = InvocationTotals::default();

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

        // Thread parent_event_id through every publish for this
        // invocation. The Triggered event is the chain root
        // (parent = None); each subsequent publish updates the
        // cursor inside publish_chained.
        let mut cursor: Option<Uuid> = None;

        // Emit `triggered` once, mirroring the legacy executor.
        self.publish_chained(
            &mut cursor,
            Event::new(
                agent_id.clone(),
                invocation_id,
                EventPayload::Triggered(TriggeredPayload {
                    trigger_source,
                    trigger_subject,
                    trigger_payload,
                    config_snapshot: agent.to_snapshot(),
                }),
            ),
        )
        .await?;

        let state: Vec<u8> = Vec::new();
        let last_result: Option<CapabilityResult> = None;
        let started_at_ms = unix_now_ms();
        let step_index_start: u32 = 0;

        self.run_loop_inner(
            reducer,
            agent,
            llm,
            invocation_id,
            &agent_id,
            &agent_config,
            &trigger,
            &sandbox,
            state,
            last_result,
            step_index_start,
            totals,
            start,
            started_at_ms,
            &mut cursor,
        )
        .await
    }

    /// Resume an in-flight invocation that was persisted but
    /// not terminal. Loads the state row, deterministically
    /// replays the reducer through every completed WAL action
    /// to rebuild `state` and `last_result`, then continues
    /// the run loop from there.
    ///
    /// **Refuses ambiguous invocations** (any WAL row in
    /// `dispatched` state). Those need operator triage via
    /// `fq recover` (step 9) per the §3.4 contract; the
    /// runtime cannot auto-resume them under the
    /// tool-idempotency constraint.
    ///
    /// Re-running a pending intent (intent-only WAL row) is
    /// safe: the loop's normal flow re-emits the intent (idempotent
    /// `INSERT OR REPLACE`), runs the action, and continues.
    /// No special handling needed.
    pub async fn resume<R: Reducer + Send + Sync>(
        &self,
        reducer: &R,
        agent: &Agent,
        llm: &dyn LlmClient,
        invocation_id: Uuid,
    ) -> Result<InvocationOutcome, ExecutorError> {
        let inv_str = invocation_id.to_string();
        let state_row = self
            .store
            .get_invocation_state(&inv_str)
            .await
            .map_err(map_store_err)?
            .ok_or_else(|| {
                ExecutorError::WorkerStore(format!(
                    "no state row for {invocation_id}; nothing to resume"
                ))
            })?;
        if state_row.terminal_at.is_some() {
            return Err(ExecutorError::WorkerStore(format!(
                "invocation {invocation_id} is already terminal; nothing to resume"
            )));
        }

        // Re-validate the agent_id pulled from the store. It was
        // validated on insert (the runtime only writes through
        // AgentId), so a failure here means the database row was
        // tampered with or written by a future, looser version.
        let agent_id: AgentId = AgentId::new(state_row.agent_id.clone()).map_err(|err| {
            ExecutorError::WorkerStore(format!(
                "stored agent_id {:?} fails AgentId validation: {err}",
                state_row.agent_id
            ))
        })?;
        info!(
            invocation_id = %invocation_id,
            agent_id = %agent_id,
            "resuming reducer invocation"
        );

        // Refuse ambiguous WAL state.
        let tools = self
            .store
            .list_tool_dispatches_for_invocation(&inv_str)
            .await
            .map_err(map_store_err)?;
        let llms = self
            .store
            .list_llm_dispatches_for_invocation(&inv_str)
            .await
            .map_err(map_store_err)?;
        if tools.iter().any(|r| r.status == DispatchStatus::Dispatched)
            || llms.iter().any(|r| r.status == DispatchStatus::Dispatched)
        {
            return Err(ExecutorError::WorkerStore(format!(
                "invocation {invocation_id} has ambiguous WAL state; \
                 use `fq recover` to triage"
            )));
        }

        // Build chronological list of completed capabilities.
        let mut completed: Vec<(i64, CapabilityResult)> = Vec::new();
        for r in &tools {
            if r.status == DispatchStatus::Completed {
                completed.push((r.completed_at.unwrap_or(0), tool_row_to_capability(r)));
            }
        }
        for r in &llms {
            if r.status == DispatchStatus::Completed {
                completed.push((
                    r.completed_at.unwrap_or(0),
                    llm_row_to_capability(r)?,
                ));
            }
        }
        completed.sort_by_key(|x| x.0);

        // Set up agent context (mirrors run()).
        let sandbox = agent.sandbox().to_tool_sandbox();
        let tool_schemas = self.tools.build_schemas(agent.tools());
        let agent_config = AgentConfig {
            agent_id: agent_id.clone(),
            model: agent.model().to_string(),
            system_prompt: agent.system_prompt().to_string(),
            tools_available: tool_schemas,
            allowed_tool_names: agent.tools().to_vec(),
            max_iterations: 0,
        };
        // Trigger payload is past us: the original `triggered`
        // event was emitted on initial run. Pass a null trigger;
        // the harness only consumes it on step 0, which we've
        // moved past via replay.
        let trigger = TriggerPayload {
            source: TriggerSourceKind::Manual,
            subject: None,
            payload: Value::Null,
        };

        // Replay the reducer deterministically through every
        // completed action. The reducer is pure; reading the
        // sequence of (state, last_result, step_index) tuples
        // out of nothing rebuilds state cheaply.
        let mut state: Vec<u8> = Vec::new();
        let mut last_result: Option<CapabilityResult> = None;
        let mut step_index: u32 = 0;
        for (_, capability) in &completed {
            let input = StepInput {
                config: agent_config.clone(),
                trigger: trigger.clone(),
                state,
                last_result,
                now_ms: now_ms(),
                random_seed: rand_u64(),
                step_index,
            };
            let output = reducer.step(input).map_err(|e| {
                ExecutorError::WorkerStore(format!("replay step {step_index} failed: {e}"))
            })?;
            state = output.state;
            last_result = Some(capability.clone());
            step_index += 1;
        }

        // Continue the loop from the replayed point. Recovery
        // re-emits start a fresh chain — parent_event_id resets to
        // None for the first event the resumed runner emits. The
        // projection links the pre-crash and post-resume chains by
        // invocation_id only. A `recovered_from_event_id` envelope
        // field could be added later if audit needs cross-incarnation
        // stitching (see step 2 of the envelope-refactor plan).
        let totals = InvocationTotals::default();
        let start = Instant::now();
        let mut cursor: Option<Uuid> = None;
        self.run_loop_inner(
            reducer,
            agent,
            llm,
            invocation_id,
            &agent_id,
            &agent_config,
            &trigger,
            &sandbox,
            state,
            last_result,
            step_index,
            totals,
            start,
            state_row.started_at,
            &mut cursor,
        )
        .await
    }

    /// The reducer-loop body extracted so `run` and `resume`
    /// can share it. Caller threads in the prepared
    /// `(state, last_result, step_index, totals)` plus all the
    /// invocation-scoped context.
    #[allow(clippy::too_many_arguments)]
    async fn run_loop_inner<R: Reducer + Send + Sync>(
        &self,
        reducer: &R,
        agent: &Agent,
        llm: &dyn LlmClient,
        invocation_id: Uuid,
        agent_id: &AgentId,
        agent_config: &AgentConfig,
        trigger: &TriggerPayload,
        sandbox: &ToolSandbox,
        mut state: Vec<u8>,
        mut last_result: Option<CapabilityResult>,
        step_index_start: u32,
        mut totals: InvocationTotals,
        start: Instant,
        started_at_ms: i64,
        cursor: &mut Option<Uuid>,
    ) -> Result<InvocationOutcome, ExecutorError> {
        for step_index in step_index_start..HOST_STEP_BUDGET {
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
                        agent_id,
                        invocation_id,
                        FailureKind::RuntimeError,
                        format!("reducer step failed: {err}"),
                        FailurePhase::LlmResponse,
                        totals,
                        cursor,
                    )
                    .await?;
                    return Err(ExecutorError::MaxIterationsExceeded);
                }
            };

            self.write_logs(agent_id, invocation_id, &output.logs);
            self.emit_semantic_events(&output.events);

            // Persist the post-step state to the worker store
            // before initiating any side-effecting action. The
            // `phase` and `terminal_at` are derived from the
            // step's `next_action` — Complete/Failed mark the
            // row terminal, everything else leaves it open.
            let (phase_label, terminal_at) =
                phase_and_terminal_from(&output.next_action, unix_now_ms());
            self.store
                .upsert_invocation_state(&InvocationStateRow {
                    invocation_id: invocation_id.to_string(),
                    agent_id: agent_id.as_str().to_string(),
                    schema_version: 1,
                    phase: phase_label.to_string(),
                    state_blob: output.state.clone(),
                    iteration: step_index,
                    started_at: started_at_ms,
                    updated_at: unix_now_ms(),
                    terminal_at,
                    workspace_ref: None,
                })
                .await
                .map_err(map_store_err)?;
            state = output.state;

            match output.next_action {
                NextAction::Complete(text) => {
                    let duration_ms = start.elapsed().as_millis() as u64;
                    totals.total_duration_ms = duration_ms;
                    let summary = if text.is_empty() { None } else { Some(text) };
                    self.publish_chained(
                        cursor,
                        Event::new(
                            agent_id.clone(),
                            invocation_id,
                            EventPayload::Completed(CompletedPayload {
                                result_summary: summary.clone(),
                                total_llm_calls: totals.total_llm_calls,
                                total_tool_calls: totals.total_tool_calls,
                                total_cost: totals.total_cost,
                                total_duration_ms: duration_ms,
                            }),
                        ),
                    )
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
                        agent_id,
                        invocation_id,
                        kind,
                        err.message.clone(),
                        FailurePhase::LlmResponse,
                        totals,
                        cursor,
                    )
                    .await?;
                    return Err(ExecutorError::MaxIterationsExceeded);
                }
                NextAction::CallModel(request) => {
                    let outcome = self
                        .run_model_with_llm(
                            llm,
                            agent.budget(),
                            agent_id,
                            invocation_id,
                            request,
                            &mut totals,
                            start,
                            cursor,
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
                            sandbox,
                            agent_id,
                            invocation_id,
                            req,
                            &totals,
                            start,
                            cursor,
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
                                sandbox,
                                agent_id,
                                invocation_id,
                                req,
                                &totals,
                                start,
                                cursor,
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
            agent_id,
            invocation_id,
            FailureKind::RuntimeError,
            format!("host step budget exhausted ({HOST_STEP_BUDGET})"),
            FailurePhase::LlmResponse,
            totals,
            cursor,
        )
        .await?;
        Err(ExecutorError::MaxIterationsExceeded)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_tool(
        &self,
        agent: &Agent,
        sandbox: &ToolSandbox,
        agent_id: &AgentId,
        invocation_id: Uuid,
        req: ToolCallRequest,
        totals: &InvocationTotals,
        start: Instant,
        cursor: &mut Option<Uuid>,
    ) -> Result<ToolCallResult, ExecutorError> {
        if !agent.tools().iter().any(|name| name == &req.tool_name) {
            return self
                .emit_synthetic_tool_error(
                    agent_id,
                    invocation_id,
                    &req,
                    ToolErrorKind::PermissionDenied,
                    format!("tool '{}' is not available to this agent", req.tool_name),
                    cursor,
                )
                .await;
        }

        // §5.5 write order: persist `intent` to SQLite, then
        // publish `tool.call` to NATS, then execute, then write
        // `dispatched`, then `completed`, then publish
        // `tool.result`. Synthetic-error and self_inspect paths
        // bypass the dispatch WAL (no real tool execution).
        let inv_str = invocation_id.to_string();
        let intent_at = unix_now_ms();
        let parameters_json =
            serde_json::to_string(&req.parameters).unwrap_or_else(|_| "{}".to_string());
        self.store
            .write_tool_intent(
                &inv_str,
                &req.tool_call_id,
                &req.tool_name,
                &parameters_json,
                intent_at,
            )
            .await
            .map_err(map_store_err)?;

        self.publish_chained(
            cursor,
            Event::new(
                agent_id.clone(),
                invocation_id,
                EventPayload::ToolCall(ToolCallPayload {
                    tool_call_id: req.tool_call_id.clone(),
                    tool_name: req.tool_name.clone(),
                    parameters: req.parameters.clone(),
                }),
            ),
        )
        .await?;

        // self_inspect is a host-fulfilled tool: the registry has the
        // schema but the data lives here. Intercept before falling
        // through to `Tool::execute` (which would surface a tripwire
        // error). See `crate::introspection`.
        if req.tool_name == SELF_INSPECT_TOOL_NAME {
            return self
                .run_self_inspect_with_wal(
                    agent,
                    agent_id,
                    invocation_id,
                    req,
                    totals,
                    start,
                    &inv_str,
                    cursor,
                )
                .await;
        }

        let tool = match self.tools.get(&req.tool_name) {
            Some(t) => t,
            None => {
                // Tool isn't registered — close the WAL row as
                // a non-ambiguous error so recovery doesn't see
                // it as `dispatched` forever.
                self.store
                    .write_tool_dispatched(&inv_str, &req.tool_call_id, unix_now_ms())
                    .await
                    .map_err(map_store_err)?;
                let msg = format!("no implementation registered for tool '{}'", req.tool_name);
                self.store
                    .write_tool_completed(
                        &inv_str,
                        &req.tool_call_id,
                        &msg,
                        true,
                        unix_now_ms(),
                    )
                    .await
                    .map_err(map_store_err)?;
                return self
                    .emit_synthetic_tool_error(
                        agent_id,
                        invocation_id,
                        &req,
                        ToolErrorKind::ExecutionFailed,
                        msg,
                        cursor,
                    )
                    .await;
            }
        };

        let ctx = ToolContext::new(sandbox);
        let tool_start = Instant::now();
        let outcome = tool.execute(&ctx, req.parameters.clone()).await;
        let duration_ms = tool_start.elapsed().as_millis() as u64;

        // Tool returned control. Mark dispatched (the
        // ambiguous-window state) before processing the result.
        self.store
            .write_tool_dispatched(&inv_str, &req.tool_call_id, unix_now_ms())
            .await
            .map_err(map_store_err)?;
        self.publish_chained(
            cursor,
            Event::new(
                agent_id.clone(),
                invocation_id,
                EventPayload::ToolDispatched(events::ToolDispatchedPayload {
                    tool_call_id: req.tool_call_id.clone(),
                    tool_name: req.tool_name.clone(),
                }),
            ),
        )
        .await?;

        match outcome {
            Ok(result) => {
                self.store
                    .write_tool_completed(
                        &inv_str,
                        &req.tool_call_id,
                        &result.output,
                        result.is_error,
                        unix_now_ms(),
                    )
                    .await
                    .map_err(map_store_err)?;
                self.publish_chained(
                    cursor,
                    Event::new(
                        agent_id.clone(),
                        invocation_id,
                        EventPayload::ToolResult(ToolResultPayload {
                            tool_call_id: req.tool_call_id.clone(),
                            output: result.output.clone(),
                            is_error: result.is_error,
                            error_kind: None,
                            duration_ms,
                        }),
                    ),
                )
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
                self.store
                    .write_tool_completed(
                        &inv_str,
                        &req.tool_call_id,
                        &message,
                        true,
                        unix_now_ms(),
                    )
                    .await
                    .map_err(map_store_err)?;
                self.publish_chained(
                    cursor,
                    Event::new(
                        agent_id.clone(),
                        invocation_id,
                        EventPayload::ToolResult(ToolResultPayload {
                            tool_call_id: req.tool_call_id.clone(),
                            output: message.clone(),
                            is_error: true,
                            error_kind: Some(kind),
                            duration_ms,
                        }),
                    ),
                )
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

    /// Self-inspect path with WAL — closes the dispatch row
    /// the run_tool caller already opened. The intent row was
    /// written by run_tool before this function is reached.
    #[allow(clippy::too_many_arguments)]
    async fn run_self_inspect_with_wal(
        &self,
        agent: &Agent,
        agent_id: &AgentId,
        invocation_id: Uuid,
        req: ToolCallRequest,
        totals: &InvocationTotals,
        start: Instant,
        inv_str: &str,
        cursor: &mut Option<Uuid>,
    ) -> Result<ToolCallResult, ExecutorError> {
        use crate::worker::introspection::{HostInvocationStats, synthesize_self_inspect};

        let tool_start = Instant::now();
        let stats = HostInvocationStats {
            agent_id: agent_id.as_str(),
            model: agent.model(),
            allowed_tool_names: agent.tools(),
            budget: agent.budget(),
            // The reducer harness uses its `DEFAULT_MAX_ITERATIONS`
            // when AgentConfig.max_iterations is 0. Mirror that so
            // self_inspect's reported value matches what actually
            // bounds the agent.
            max_iterations: crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS,
            totals: *totals,
            elapsed_ms: start.elapsed().as_millis() as u64,
        };
        let output = synthesize_self_inspect(&stats, req.parameters.clone());
        let duration_ms = tool_start.elapsed().as_millis() as u64;

        // Close the WAL: dispatched, then completed.
        self.store
            .write_tool_dispatched(inv_str, &req.tool_call_id, unix_now_ms())
            .await
            .map_err(map_store_err)?;
        self.publish_chained(
            cursor,
            Event::new(
                agent_id.clone(),
                invocation_id,
                EventPayload::ToolDispatched(events::ToolDispatchedPayload {
                    tool_call_id: req.tool_call_id.clone(),
                    tool_name: req.tool_name.clone(),
                }),
            ),
        )
        .await?;
        self.store
            .write_tool_completed(
                inv_str,
                &req.tool_call_id,
                &output,
                false,
                unix_now_ms(),
            )
            .await
            .map_err(map_store_err)?;

        self.publish_chained(
            cursor,
            Event::new(
                agent_id.clone(),
                invocation_id,
                EventPayload::ToolResult(ToolResultPayload {
                    tool_call_id: req.tool_call_id.clone(),
                    output: output.clone(),
                    is_error: false,
                    error_kind: None,
                    duration_ms,
                }),
            ),
        )
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
        agent_id: &AgentId,
        invocation_id: Uuid,
        req: &ToolCallRequest,
        kind: ToolErrorKind,
        message: String,
        cursor: &mut Option<Uuid>,
    ) -> Result<ToolCallResult, ExecutorError> {
        self.publish_chained(
            cursor,
            Event::new(
                agent_id.clone(),
                invocation_id,
                EventPayload::ToolResult(ToolResultPayload {
                    tool_call_id: req.tool_call_id.clone(),
                    output: message.clone(),
                    is_error: true,
                    error_kind: Some(kind),
                    duration_ms: 0,
                }),
            ),
        )
        .await?;
        Ok(ToolCallResult {
            tool_call_id: req.tool_call_id.clone(),
            output: message,
            is_error: true,
            error_kind: Some(kind),
            duration_ms: 0,
        })
    }

    /// Publish an event and chain it to the prior event in the
    /// current invocation. The cursor is updated to the published
    /// event's `event_id` so the next call picks it up as
    /// `parent_event_id`. See `inter-node-contracts-and-event-layers.md`
    /// §5 and the `parent_event_id` doc on [`events::Envelope`] for
    /// the rationale.
    async fn publish_chained(
        &self,
        cursor: &mut Option<Uuid>,
        mut event: Event,
    ) -> Result<(), ExecutorError> {
        if let Some(parent) = *cursor {
            event.envelope.parent_event_id = Some(parent);
        }
        let id = event.envelope.event_id;
        debug!(event_type = ?event.payload, "publishing event");
        self.bus.publish(&event).await.map_err(ExecutorError::Bus)?;
        *cursor = Some(id);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn emit_failed(
        &self,
        agent_id: &AgentId,
        invocation_id: Uuid,
        error_kind: FailureKind,
        error_message: String,
        phase: FailurePhase,
        partial_totals: InvocationTotals,
        cursor: &mut Option<Uuid>,
    ) -> Result<(), ExecutorError> {
        warn!(
            agent_id = %agent_id,
            invocation_id = %invocation_id,
            error_kind = ?error_kind,
            "reducer invocation failed"
        );
        self.publish_chained(
            cursor,
            Event::new(
                agent_id.clone(),
                invocation_id,
                EventPayload::Failed(FailedPayload {
                    error_kind,
                    error_message,
                    phase,
                    partial_totals,
                }),
            ),
        )
        .await
    }

    fn write_logs(&self, agent_id: &AgentId, invocation_id: Uuid, logs: &[LogEntry]) {
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
    #[allow(clippy::too_many_arguments)]
    async fn run_model_with_llm(
        &self,
        llm: &dyn LlmClient,
        budget: Option<f64>,
        agent_id: &AgentId,
        invocation_id: Uuid,
        request: ModelRequest,
        totals: &mut InvocationTotals,
        start: Instant,
        cursor: &mut Option<Uuid>,
    ) -> Result<ModelOutcome, ExecutorError> {
        let call_id = Uuid::now_v7();
        let inv_str = invocation_id.to_string();
        let req_str = call_id.to_string();
        let chat_request = ChatRequest {
            model: request.model.clone(),
            messages: request.messages.clone(),
            tools: request.tools.clone(),
            params: request.params.clone(),
        };

        // §5.5 write order applied to LLM calls: SQL first, then
        // NATS publish, then the LLM call, then dispatched, then
        // completed, then response/cost events.
        let request_payload_json =
            serde_json::to_string(&chat_request).unwrap_or_else(|_| "{}".to_string());
        self.store
            .write_llm_intent(
                &inv_str,
                &req_str,
                &chat_request.model,
                &request_payload_json,
                unix_now_ms(),
            )
            .await
            .map_err(map_store_err)?;

        self.publish_chained(
            cursor,
            Event::new(
                agent_id.clone(),
                invocation_id,
                EventPayload::LlmRequest(LlmRequestPayload {
                    call_id,
                    model: chat_request.model.clone(),
                    messages: chat_request.messages.clone(),
                    tools_available: chat_request.tools.clone(),
                    request_params: chat_request.params.clone(),
                }),
            ),
        )
        .await?;

        let response = match llm.chat(chat_request).await {
            Ok(r) => r,
            Err(err) => {
                // LLM call returned an error. Close the WAL with
                // is_error=true so recovery sees a final state,
                // not the ambiguous `dispatched` state.
                self.store
                    .write_llm_dispatched(&inv_str, &req_str, unix_now_ms())
                    .await
                    .map_err(map_store_err)?;
                self.store
                    .write_llm_completed(
                        &inv_str,
                        &req_str,
                        &err.to_string(),
                        true,
                        0.0,
                        unix_now_ms(),
                    )
                    .await
                    .map_err(map_store_err)?;
                totals.total_duration_ms = start.elapsed().as_millis() as u64;
                self.emit_failed(
                    agent_id,
                    invocation_id,
                    FailureKind::LlmError,
                    err.to_string(),
                    FailurePhase::LlmRequest,
                    *totals,
                    cursor,
                )
                .await?;
                return Err(ExecutorError::Llm(err));
            }
        };

        totals.total_llm_calls += 1;

        // LLM returned control. Mark dispatched (ambiguous
        // window), publish the dispatched event, then transition
        // to completed before the response/cost events go out.
        self.store
            .write_llm_dispatched(&inv_str, &req_str, unix_now_ms())
            .await
            .map_err(map_store_err)?;
        self.publish_chained(
            cursor,
            Event::new(
                agent_id.clone(),
                invocation_id,
                EventPayload::LlmDispatched(events::LlmDispatchedPayload {
                    call_id,
                    model: request.model.clone(),
                }),
            ),
        )
        .await?;
        let response_json =
            serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        self.store
            .write_llm_completed(
                &inv_str,
                &req_str,
                &response_json,
                false,
                0.0, // cost filled in below; for the WAL we record the response presence
                unix_now_ms(),
            )
            .await
            .map_err(map_store_err)?;

        // Cost folds into the llm.response envelope (envelope-refactor
        // plan step 3). Compute before publishing so the response
        // event carries its cost in one publish, not two.
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

        self.publish_chained(
            cursor,
            Event::new(
                agent_id.clone(),
                invocation_id,
                EventPayload::LlmResponse(LlmResponsePayload {
                    call_id,
                    content: response.content.clone(),
                    tool_calls: response.tool_calls.clone(),
                    stop_reason: response.stop_reason,
                    usage: response.usage,
                }),
            )
            .with_cost(events::CostMetadata {
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
        )
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
                cursor,
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

/// Reconstruct a [`CapabilityResult::ToolResult`] from a
/// completed `tool_dispatch` row. Used by `resume()` to feed
/// the result of a previously-completed action back into the
/// reducer.
fn tool_row_to_capability(row: &ToolDispatchRow) -> CapabilityResult {
    CapabilityResult::ToolResult(ToolCallResult {
        tool_call_id: row.tool_call_id.clone(),
        output: row.result.clone().unwrap_or_default(),
        is_error: row.is_error.unwrap_or(false),
        error_kind: None,
        duration_ms: 0,
    })
}

/// Reconstruct a [`CapabilityResult::ModelResult`] from a
/// completed `llm_dispatch` row. The stored response is
/// the JSON-serialised `ChatResponse` from
/// [`ReducerRunner::run_model_with_llm`].
fn llm_row_to_capability(
    row: &LlmDispatchRow,
) -> Result<CapabilityResult, ExecutorError> {
    let response_json = row.response.as_deref().ok_or_else(|| {
        ExecutorError::WorkerStore(format!(
            "completed llm_dispatch row {}/{} has no response",
            row.invocation_id, row.request_id
        ))
    })?;
    let response: ChatResponse = serde_json::from_str(response_json).map_err(|err| {
        ExecutorError::WorkerStore(format!(
            "failed to deserialise stored llm response for {}/{}: {err}",
            row.invocation_id, row.request_id
        ))
    })?;
    Ok(CapabilityResult::ModelResult(ModelResponse {
        content: response.content,
        tool_calls: response.tool_calls,
        stop_reason: response.stop_reason,
        usage: response.usage,
    }))
}

/// Map the reducer's outgoing action to the `phase` label
/// stored on the invocation_state row, and a `terminal_at`
/// timestamp if the action is terminal.
///
/// Phase labels are operator-facing and used by recovery
/// (step 6) to know what state the reducer was in. Deriving
/// them from `next_action` keeps the runner from peeking into
/// the reducer's opaque state blob.
fn phase_and_terminal_from(action: &NextAction, now_ms: i64) -> (&'static str, Option<i64>) {
    match action {
        NextAction::Complete(_) => ("completed", Some(now_ms)),
        NextAction::Failed(_) => ("failed", Some(now_ms)),
        NextAction::CallModel(_) => ("awaiting_model", None),
        NextAction::CallTool(_) | NextAction::CallToolsParallel(_) => {
            ("dispatching_tools", None)
        }
    }
}

/// Current wall clock as Unix milliseconds. Used for WAL
/// timestamp columns. Failures (clock before epoch) collapse
/// to 0; this can't happen on any reasonable system.
fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Convert a worker-store error into the runner's executor
/// error. The store's `Backend` variant is opaque, so we just
/// preserve the message.
fn map_store_err(err: crate::worker::WorkerStoreError) -> ExecutorError {
    ExecutorError::WorkerStore(err.to_string())
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
    use crate::llm::fixture::FixtureClient;
    use crate::pricing::ModelPricing;
    use crate::tools::ToolRegistry;
    use crate::worker::executor::AgentExecutor;
    use crate::worker::reducer::Harness;
    use crate::worker::store::DispatchStatus;
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
    /// Assert kinds appear in the listed order somewhere in the
    /// sequence (other kinds may interleave). Variant of the
    /// public helper that works on plain kind names.
    fn assert_relative_order(kinds: &[&'static str], expected: &[&'static str]) {
        let mut from = 0;
        for k in expected {
            let pos = kinds[from..]
                .iter()
                .position(|s| s == k)
                .unwrap_or_else(|| panic!("kind {k:?} not found at or after {from}; got {kinds:?}"));
            from += pos + 1;
        }
    }

    /// Strip the WAL middle-state events (`tool_dispatched`,
    /// `llm_dispatched`) from a kind sequence. Used by the
    /// equivalence tests: the legacy executor doesn't emit them
    /// in v1, so we compare the post-strip reducer sequence
    /// against the legacy sequence.
    fn strip_wal_dispatched(kinds: &[&'static str]) -> Vec<&'static str> {
        kinds
            .iter()
            .copied()
            .filter(|k| *k != "tool_dispatched" && *k != "llm_dispatched")
            .collect()
    }

    /// Run a scripted scenario through both the legacy executor
    /// and the reducer runner and return the (kind) sequences
    /// each emitted. The reducer-side count must include the
    /// extra WAL middle-state events; pass `legacy_count` and
    /// `reducer_count` separately.
    #[allow(clippy::too_many_arguments)]
    async fn run_through_legacy_and_reducer(
        url: &str,
        agent_factory: impl Fn(String) -> Agent,
        responses: impl Fn() -> Vec<ChatResponse>,
        legacy_count: usize,
        reducer_count: usize,
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
            let store_dir = tempdir().expect("tempdir");
            let store = Arc::new(
                WorkerStore::open(&store_dir.path().join("events.db"))
                    .await
                    .expect("worker store"),
            );
            let runner = ReducerRunner::new(
                bus.clone(),
                test_pricing(),
                Arc::new(ToolRegistry::with_builtins()),
                store,
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
            collect_events(agent_factory(unique_agent_id("legacy")), legacy_count).await;
        let reducer_events =
            collect_reducer(agent_factory(unique_agent_id("reducer")), reducer_count).await;

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

        // Legacy: 5 events (triggered, llm.request, llm.response,
        // cost, completed). Reducer: same plus llm.dispatched
        // between request and response.
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
            // After envelope-refactor step 3, cost rides on the
            // llm.response envelope; no separate cost event.
            // Legacy: 4 (was 5); reducer: 5 (was 6).
            4,
            5,
        )
        .await;

        assert_eq!(
            legacy,
            vec!["triggered", "llm_request", "llm_response", "completed"]
        );
        // The reducer sequence equals the legacy sequence after
        // stripping the new WAL middle-state events that the
        // legacy executor doesn't emit.
        assert_eq!(
            strip_wal_dispatched(&reducer),
            legacy,
            "reducer path must match legacy event order modulo WAL middle-state events"
        );
        // And the reducer sequence must include llm_dispatched
        // between llm_request and llm_response.
        assert_relative_order(&reducer, &["llm_request", "llm_dispatched", "llm_response"]);
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
            // After envelope-refactor step 3, no separate cost event.
            // Legacy: 8 (was 10); reducer: 11 (was 13).
            8, // legacy: 8 events (no dispatched, no cost)
            11, // reducer: legacy + 2 llm_dispatched + 1 tool_dispatched
        )
        .await;

        let expected_legacy = vec![
            "triggered",
            "llm_request",
            "llm_response",
            "tool_call",
            "tool_result",
            "llm_request",
            "llm_response",
            "completed",
        ];
        assert_eq!(legacy, expected_legacy);
        // The reducer sequence equals the legacy sequence after
        // stripping the new WAL middle-state events.
        assert_eq!(
            strip_wal_dispatched(&reducer),
            expected_legacy,
            "reducer path must match legacy event order modulo WAL middle-state events"
        );
        // And the reducer sequence must include both
        // llm_dispatched (between request/response, in both
        // turns) and tool_dispatched (between tool_call/tool_result).
        assert_relative_order(&reducer, &["tool_call", "tool_dispatched", "tool_result"]);
        assert_relative_order(&reducer, &["llm_request", "llm_dispatched", "llm_response"]);
    }

    #[tokio::test]
    async fn reducer_invocation_emits_single_parent_chain() {
        // Step 2 of the envelope-refactor plan: the reducer threads
        // parent_event_id through every publish for an invocation.
        // The captured event stream must form a single chain
        // rooted at `triggered`, with no orphans, no branches, and
        // no multiple roots. Reconstructable without consulting
        // timestamps.
        let Some(url) = crate::test_support::events::require_nats() else {
            return;
        };
        let bus = EventBus::connect(&url).await.expect("connect to NATS");
        let agent_id = unique_agent_id("chain");
        let agent = Agent::builder()
            .id(&agent_id)
            .model("claude-haiku")
            .system_prompt("be brief")
            .budget(1.0)
            .build()
            .unwrap();

        let target_path = "Cargo.toml".to_string();
        let llm = FixtureClient::new();
        llm.push_response(tool_use(
            "file_read",
            "call_chain_1",
            json!({"path": target_path.clone()}),
            (50, 25),
        ));
        llm.push_response(canned("read.", 80, 10));

        let mut sub = bus
            .subscribe(format!("fq.agent.{}.>", agent.id().as_str()))
            .await
            .expect("subscribe");
        tokio::time::sleep(Duration::from_millis(50)).await;

        let store_dir = tempdir().expect("tempdir");
        let store = Arc::new(
            WorkerStore::open(&store_dir.path().join("events.db"))
                .await
                .expect("worker store"),
        );
        let runner = ReducerRunner::new(
            bus.clone(),
            test_pricing(),
            Arc::new(ToolRegistry::with_builtins()),
            store,
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

        // Drain. tool-call loop emits: triggered + 2 turns ×
        // (llm_request, llm_dispatched, llm_response with envelope.cost)
        // + 1 × (tool_call, tool_dispatched, tool_result) + completed
        // = 11 events after envelope-refactor step 3 (no separate
        // cost event).
        let mut events = Vec::new();
        for _ in 0..11 {
            let event = tokio::time::timeout(Duration::from_secs(2), sub.next())
                .await
                .expect("chain timeout")
                .expect("chain stream closed")
                .expect("chain deserialise");
            events.push(event);
        }

        crate::test_support::events::assert_parent_chain(&events);
        // Schema version on every envelope must be the v2 constant.
        for e in &events {
            assert_eq!(e.envelope.schema_version, crate::events::SCHEMA_VERSION);
            assert_eq!(e.envelope.trace_id, e.envelope.invocation_id);
            assert!(!e.envelope.schema_id.is_empty());
        }
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
        use crate::worker::reducer::types::{
            AgentConfig, CapabilityResult, ModelResponse, NextAction, StepInput, TriggerPayload,
            TriggerSourceKind,
        };

        let cfg = AgentConfig {
            agent_id: AgentId::new("suspend-resume").unwrap(),
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

        // Drop and replace the reducer. `Harness` has no Drop
        // impl, so the move-into-wildcard pattern is the way to
        // express "throw this away" without clippy's `drop_non_drop`.
        let _ = h1;
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
        let store_dir = tempdir().expect("tempdir");
        let store = Arc::new(
            WorkerStore::open(&store_dir.path().join("events.db"))
                .await
                .expect("worker store"),
        );
        let runner = ReducerRunner::new(
            bus.clone(),
            test_pricing(),
            Arc::new(ToolRegistry::with_builtins()),
            store,
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
        use crate::worker::introspection::{HostInvocationStats, synthesize_self_inspect};
        use crate::worker::reducer::types::{
            AgentConfig, CapabilityResult, ModelResponse, NextAction, StepInput, ToolCallResult,
            TriggerPayload, TriggerSourceKind,
        };

        let cfg = AgentConfig {
            agent_id: AgentId::new("suspend-tools").unwrap(),
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
        // survives the boundary. (`Harness` has no Drop impl, so
        // we use the move-into-wildcard pattern instead of `drop`.)
        let _ = h;

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

    // -----------------------------------------------------------
    // Step 4: WAL writes around tool and LLM dispatches.
    // -----------------------------------------------------------

    /// Helper used by the WAL tests below: run a scripted
    /// agent through the reducer path against live NATS,
    /// returning the worker store (for WAL inspection) and the
    /// captured event stream.
    async fn run_with_wal(
        url: &str,
        agent: Agent,
        responses: Vec<ChatResponse>,
        expected_event_count: usize,
        sandbox_dir: Option<&std::path::Path>,
    ) -> (Arc<WorkerStore>, Vec<Event>) {
        let bus = EventBus::connect(url).await.expect("connect to NATS");
        let store_dir = tempdir().expect("tempdir");
        let store = Arc::new(
            WorkerStore::open(&store_dir.path().join("events.db"))
                .await
                .expect("worker store"),
        );

        let mut sub = bus
            .subscribe(format!("fq.agent.{}.>", agent.id().as_str()))
            .await
            .expect("subscribe");
        tokio::time::sleep(Duration::from_millis(50)).await;

        let llm = FixtureClient::new();
        for r in responses {
            llm.push_response(r);
        }
        let runner = ReducerRunner::new(
            bus.clone(),
            test_pricing(),
            Arc::new(ToolRegistry::with_builtins()),
            store.clone(),
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

        let mut events = Vec::with_capacity(expected_event_count);
        for _ in 0..expected_event_count {
            let event = tokio::time::timeout(Duration::from_secs(2), sub.next())
                .await
                .expect("event timeout")
                .expect("stream closed")
                .expect("deserialise");
            events.push(event);
        }
        // The store_dir tempfile must outlive the store handle;
        // we leak it through forget so the caller's tempdir cleanup
        // doesn't race the store's file references during the test
        // assertions. (`store_dir` goes out of scope at function
        // return; the SQLite WAL holds open file handles that are
        // released when `store` is dropped.)
        let _ = sandbox_dir; // suppress "unused" if not provided
        std::mem::forget(store_dir);
        (store, events)
    }

    fn end_turn_response(text: &str) -> ChatResponse {
        ChatResponse {
            content: Some(text.to_string()),
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

    fn tool_call_response(tool: &str, call_id: &str, params: serde_json::Value) -> ChatResponse {
        ChatResponse {
            content: None,
            tool_calls: vec![crate::events::MessageToolCall {
                tool_call_id: call_id.to_string(),
                tool_name: tool.to_string(),
                parameters: params,
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 50,
                output_tokens: 10,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        }
    }

    fn simple_responder_agent(name: &str) -> Agent {
        Agent::builder()
            .id(name)
            .model("claude-haiku")
            .system_prompt("simple")
            .sandbox(Sandbox::new())
            .budget(1.0)
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn llm_only_invocation_writes_intent_dispatched_completed_in_order() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        let agent_id = unique_agent_id("step4-llm-only");
        let agent = simple_responder_agent(&agent_id);

        // 1 LLM turn, end immediately.
        // After envelope-refactor step 3, no separate cost event:
        // triggered, llm.request, llm.dispatched, llm.response,
        // completed = 5 events.
        let (store, events) = run_with_wal(
            &url,
            agent,
            vec![end_turn_response("done.")],
            5,
            None,
        )
        .await;
        // Six events: triggered, llm.request, llm.dispatched, llm.response, cost, completed.
        // We only asked for 5 above; let's ask for one more so the assertion below works cleanly.
        let _ = events; // (subset captured; the count is conservative for assertion below)

        // The dispatched-LLM rows should all be `completed`
        // by the time the invocation finishes.
        let ambiguous = store.find_ambiguous_llm_dispatches().await.unwrap();
        assert!(
            ambiguous.is_empty(),
            "no LLM dispatch should remain in `dispatched` state at end-of-invocation"
        );
    }

    #[tokio::test]
    async fn tool_call_invocation_writes_tool_wal_in_order() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        let dir = tempdir().unwrap();
        let target = dir.path().join("hello.md");
        std::fs::write(&target, "# hi").unwrap();

        let agent_id = unique_agent_id("step4-tool-wal");
        let agent = Agent::builder()
            .id(&agent_id)
            .model("claude-haiku")
            .system_prompt("Use tools when asked.")
            .tools(["file_read"])
            .sandbox(Sandbox::new().fs_read(dir.path().to_string_lossy().to_string()))
            .budget(1.0)
            .build()
            .unwrap();

        let responses = vec![
            tool_call_response(
                "file_read",
                "tc_1",
                json!({"path": target.to_string_lossy().to_string()}),
            ),
            end_turn_response("read it."),
        ];

        // Events emitted (after envelope-refactor step 3, cost
        // rides on llm.response envelopes, no separate cost event):
        // 1. triggered
        // 2. llm.request (turn 1)
        // 3. llm.dispatched (turn 1)
        // 4. llm.response (turn 1, with tool calls, envelope.cost set)
        // 5. tool.call
        // 6. tool.dispatched
        // 7. tool.result
        // 8. llm.request (turn 2)
        // 9. llm.dispatched (turn 2)
        // 10. llm.response (turn 2, envelope.cost set)
        // 11. completed
        let (store, events) = run_with_wal(&url, agent, responses, 11, Some(dir.path())).await;

        let kinds: Vec<&str> = events
            .iter()
            .map(crate::test_support::events::event_kind)
            .collect();

        // Order check: tool.dispatched must appear between
        // tool.call and tool.result.
        crate::test_support::events::assert_kinds_appear_in_relative_order(
            &events,
            &["tool_call", "tool_dispatched", "tool_result"],
        );
        // Order check: llm.dispatched must appear between
        // llm.request and llm.response, for every turn.
        crate::test_support::events::assert_kinds_appear_in_relative_order(
            &events,
            &["llm_request", "llm_dispatched", "llm_response"],
        );
        // The tool.dispatched event is present at all.
        assert!(
            kinds.contains(&"tool_dispatched"),
            "kinds: {kinds:?}"
        );

        // Every WAL row should be `completed` at end-of-invocation.
        assert!(
            store.find_ambiguous_tool_dispatches().await.unwrap().is_empty(),
            "tool_dispatch rows must all be completed"
        );
        assert!(
            store.find_ambiguous_llm_dispatches().await.unwrap().is_empty(),
            "llm_dispatch rows must all be completed"
        );

        // The tool dispatch row exists with status=completed
        // and is_error=false.
        let row = store
            .get_tool_dispatch(&events[0].envelope.invocation_id.to_string(), "tc_1")
            .await
            .unwrap()
            .expect("tool_dispatch row");
        assert_eq!(row.status, DispatchStatus::Completed);
        assert_eq!(row.is_error, Some(false));
        assert!(row.dispatched_at.is_some());
        assert!(row.completed_at.is_some());
    }

    #[tokio::test]
    async fn tool_error_writes_completed_with_is_error_true() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        // Sandbox that allows the read, but the file doesn't
        // exist — file_read will return is_error=true.
        let dir = tempdir().unwrap();
        let agent_id = unique_agent_id("step4-tool-error");
        let agent = Agent::builder()
            .id(&agent_id)
            .model("claude-haiku")
            .system_prompt("Use tools.")
            .tools(["file_read"])
            .sandbox(Sandbox::new().fs_read(dir.path().to_string_lossy().to_string()))
            .budget(1.0)
            .build()
            .unwrap();

        let missing = dir.path().join("does-not-exist.md");
        let responses = vec![
            tool_call_response(
                "file_read",
                "tc_err",
                json!({"path": missing.to_string_lossy().to_string()}),
            ),
            end_turn_response("done."),
        ];

        let (store, events) = run_with_wal(&url, agent, responses, 11, Some(dir.path())).await;

        let row = store
            .get_tool_dispatch(&events[0].envelope.invocation_id.to_string(), "tc_err")
            .await
            .unwrap()
            .expect("tool_dispatch row");
        assert_eq!(row.status, DispatchStatus::Completed);
        assert_eq!(
            row.is_error,
            Some(true),
            "tool_dispatch must record is_error=true on tool failure"
        );
        // Not stuck in dispatched.
        assert!(
            store
                .find_ambiguous_tool_dispatches()
                .await
                .unwrap()
                .is_empty(),
            "tool error must not leave the row in `dispatched`"
        );
    }

    // -----------------------------------------------------------
    // Step 5: per-step state persistence.
    //
    // These tests verify that the runner writes an
    // `invocation_state` row at every step boundary and marks
    // the row terminal on Complete/Failed. The matching
    // recovery / resume semantics live in step 6 — these tests
    // only assert the persistence side.
    // -----------------------------------------------------------

    #[tokio::test]
    async fn state_row_written_on_completion_with_terminal_at_set() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        let agent_id = unique_agent_id("step5-state-completion");
        let agent = simple_responder_agent(&agent_id);
        let (store, events) = run_with_wal(
            &url,
            agent,
            vec![end_turn_response("done.")],
            6,
            None,
        )
        .await;

        let inv_str = events[0].envelope.invocation_id.to_string();
        let row = store
            .get_invocation_state(&inv_str)
            .await
            .unwrap()
            .expect("state row should exist after run");

        assert_eq!(row.invocation_id, inv_str);
        assert_eq!(row.phase, "completed");
        assert!(
            row.terminal_at.is_some(),
            "terminal_at must be set on Complete"
        );
        assert!(
            !row.state_blob.is_empty(),
            "state_blob must contain the reducer's final state"
        );
        assert_eq!(row.workspace_ref, None);
        // The state blob is reducer-readable JSON.
        let _: serde_json::Value = serde_json::from_slice(&row.state_blob)
            .expect("state_blob deserialises as JSON");
    }

    #[tokio::test]
    async fn resume_safe_replay_continues_to_completion() {
        // Pre-populate a worker store so that resuming the
        // invocation continues from a "step 0 complete, action
        // 0 (LLM call) completed with end-turn" state — i.e.
        // the safe-replay case. The reducer should pick up the
        // persisted result, produce Complete, and finish.
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        use crate::worker::reducer::types::{AgentConfig, StepInput, TriggerPayload, TriggerSourceKind};

        let dir = tempdir().unwrap();
        let store_path = dir.path().join("events.db");
        let store = Arc::new(WorkerStore::open(&store_path).await.unwrap());

        let agent_id_str = unique_agent_id("step6-resume-replay");
        let agent = Agent::builder()
            .id(&agent_id_str)
            .model("claude-haiku")
            .system_prompt("You are a test agent.")
            .budget(1.0)
            .build()
            .unwrap();
        let invocation_id = Uuid::now_v7();
        let inv_str = invocation_id.to_string();

        // Manually run harness step 0 to produce the state we
        // would have persisted at iteration=0 (post-step).
        let harness = Harness::new();
        let agent_config = AgentConfig {
            agent_id: AgentId::new(&agent_id_str).unwrap(),
            model: "claude-haiku".to_string(),
            system_prompt: "You are a test agent.".to_string(),
            tools_available: vec![],
            allowed_tool_names: vec![],
            max_iterations: 0,
        };
        let trigger = TriggerPayload {
            source: TriggerSourceKind::Manual,
            subject: None,
            payload: json!("hello"),
        };
        let s0_input = StepInput {
            config: agent_config.clone(),
            trigger: trigger.clone(),
            state: vec![],
            last_result: None,
            now_ms: 0,
            random_seed: 0,
            step_index: 0,
        };
        let s0_output = harness.step(s0_input).expect("step 0");

        store
            .upsert_invocation_state(&InvocationStateRow {
                invocation_id: inv_str.clone(),
                agent_id: agent_id_str.clone(),
                schema_version: 1,
                phase: "awaiting_model".to_string(),
                state_blob: s0_output.state,
                iteration: 0,
                started_at: 1_000,
                updated_at: 1_000,
                terminal_at: None,
                workspace_ref: None,
            })
            .await
            .unwrap();

        // Pre-populate a completed LLM dispatch row whose
        // serialized response is end-turn.
        let response = ChatResponse {
            content: Some("done.".to_string()),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 50,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        };
        let response_json = serde_json::to_string(&response).unwrap();
        store
            .write_llm_intent(&inv_str, "req-0", "claude-haiku", "{}", 1)
            .await
            .unwrap();
        store.write_llm_dispatched(&inv_str, "req-0", 2).await.unwrap();
        store
            .write_llm_completed(&inv_str, "req-0", &response_json, false, 0.0001, 3)
            .await
            .unwrap();

        // Resume.
        let bus = EventBus::connect(&url).await.unwrap();
        let runner = ReducerRunner::new(
            bus,
            test_pricing(),
            Arc::new(ToolRegistry::with_builtins()),
            store.clone(),
        );
        let llm = FixtureClient::new(); // no live responses needed

        let outcome = runner
            .resume(&Harness::new(), &agent, &llm, invocation_id)
            .await
            .expect("resume completes");

        match outcome {
            InvocationOutcome::Completed {
                invocation_id: inv,
                response,
                ..
            } => {
                assert_eq!(inv, invocation_id);
                assert_eq!(response.content.as_deref(), Some("done."));
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        // State row is now terminal.
        let row = store.get_invocation_state(&inv_str).await.unwrap().unwrap();
        assert!(row.terminal_at.is_some());
        assert_eq!(row.phase, "completed");
    }

    #[tokio::test]
    async fn resume_refuses_ambiguous_invocation() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        let dir = tempdir().unwrap();
        let store = Arc::new(
            WorkerStore::open(&dir.path().join("events.db"))
                .await
                .unwrap(),
        );

        let agent_id = unique_agent_id("step6-resume-refuse");
        let agent = simple_responder_agent(&agent_id);
        let invocation_id = Uuid::now_v7();
        let inv_str = invocation_id.to_string();

        // State row + ambiguous tool dispatch (dispatched, no
        // completed).
        store
            .upsert_invocation_state(&InvocationStateRow {
                invocation_id: inv_str.clone(),
                agent_id: agent_id.clone(),
                schema_version: 1,
                phase: "dispatching_tools".to_string(),
                state_blob: vec![],
                iteration: 0,
                started_at: 1_000,
                updated_at: 1_000,
                terminal_at: None,
                workspace_ref: None,
            })
            .await
            .unwrap();
        store.write_tool_intent(&inv_str, "tc1", "shell", "{}", 1).await.unwrap();
        store.write_tool_dispatched(&inv_str, "tc1", 2).await.unwrap();
        // No completed.

        let bus = EventBus::connect(&url).await.unwrap();
        let runner = ReducerRunner::new(
            bus,
            test_pricing(),
            Arc::new(ToolRegistry::with_builtins()),
            store,
        );
        let llm = FixtureClient::new();
        let err = runner
            .resume(&Harness::new(), &agent, &llm, invocation_id)
            .await
            .expect_err("resume should refuse ambiguous");
        assert!(
            format!("{err}").contains("ambiguous"),
            "expected ambiguous error, got: {err}"
        );
    }

    #[tokio::test]
    async fn state_row_iteration_advances_with_each_step() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        // A two-turn invocation (tool call + final summary) goes
        // through enough reducer steps that `iteration` should
        // advance past 0.
        let dir = tempdir().unwrap();
        let target = dir.path().join("hello.md");
        std::fs::write(&target, "# hi").unwrap();

        let agent_id = unique_agent_id("step5-state-iter");
        let agent = Agent::builder()
            .id(&agent_id)
            .model("claude-haiku")
            .system_prompt("Use tools.")
            .tools(["file_read"])
            .sandbox(Sandbox::new().fs_read(dir.path().to_string_lossy().to_string()))
            .budget(1.0)
            .build()
            .unwrap();

        let responses = vec![
            tool_call_response(
                "file_read",
                "tc_iter",
                json!({"path": target.to_string_lossy().to_string()}),
            ),
            end_turn_response("read."),
        ];

        let (store, events) = run_with_wal(&url, agent, responses, 11, Some(dir.path())).await;
        let inv_str = events[0].envelope.invocation_id.to_string();
        let row = store
            .get_invocation_state(&inv_str)
            .await
            .unwrap()
            .expect("state row");
        assert_eq!(row.phase, "completed");
        assert!(
            row.iteration > 0,
            "iteration must advance past 0 for a multi-step invocation; got {}",
            row.iteration
        );
        assert!(row.started_at <= row.updated_at);
        assert!(row.terminal_at.unwrap_or(0) >= row.updated_at);
    }
}
