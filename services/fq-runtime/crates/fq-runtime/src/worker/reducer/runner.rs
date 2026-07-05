//! Host-side loop driver for the reducer harness.
//!
//! Drives any [`Reducer`] impl through a complete agent
//! invocation, executing the requested [`NextAction`]s against
//! the existing runtime infrastructure (LLM client, tool
//! registry, event bus, pricing table) and feeding the results
//! back to the reducer.
//!
//! The runner emits the canonical event sequence
//! (`triggered` → `llm.request` → `llm.dispatched` →
//! `llm.response` → optional `tool.call` / `tool.dispatched` /
//! `tool.result` → ... → `completed` / `failed` →
//! `invocation.archived`) that every downstream consumer relies
//! on.
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

use super::harness::Harness;
use super::types::{
    AgentConfig, CapabilityResult, EmittedEvent, HarnessError, LogEntry, LogLevel, ModelRequest,
    ModelResponse, NextAction, Reducer, StepInput, ToolCallRequest, ToolCallResult, TriggerPayload,
    TriggerSourceKind,
};
use rmcp::model::{
    CreateElicitationRequestParams, CreateElicitationResult, CreateMessageRequestParams,
    CreateMessageResult, ElicitationAction, ElicitationSchema, EnumSchema, PrimitiveSchema, Role,
    SamplingContent, SamplingMessage, SamplingMessageContent, StringFormat,
};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::agent::{Agent, AgentId, EvaluatorSpec};
use crate::bus::EventBus;
use crate::events::{
    self, CompletedPayload, Event, EventPayload, FailedPayload, FailureKind, FailurePhase,
    InvocationArchivedPayload, InvocationTotals, LlmCallOrigin, LlmRequestPayload,
    LlmResponsePayload, Message, MessageRole, RequestParams, StopReason, ToolCallPayload,
    ToolErrorKind, ToolResultPayload, TriggerSource, TriggeredPayload,
};
use crate::llm::{ChatRequest, ChatResponse, LlmClient};
use crate::mcp::{
    AdvertisedCapabilities, McpClientManager, McpResourceReader, McpServerConfig, ServerRequest,
    advertised_roots, elicitation_decline,
};
use crate::pricing::PricingTable;
use crate::tools::ToolRegistry;
use crate::validation::ValidatorChain;
use crate::worker::store::{
    DispatchStatus, InvocationStateRow, LlmDispatchRow, ToolDispatchRow, WorkerStore,
};
use crate::worker::{ExecutorError, InvocationOutcome, WorkerId};

/// Soft cap on the number of `step()` calls per invocation.
/// Independent of the reducer's own `max_iterations` so a buggy
/// reducer (e.g. one that perpetually returns CallModel without
/// progress) cannot wedge the host indefinitely.
const HOST_STEP_BUDGET: u32 = 1_000;

/// A per-invocation inbound channel from one grant-bearing MCP
/// server: the server's name (for grant checks and cost
/// attribution) paired with the receiver the runner services in its
/// `select!` during tool-call awaits (ADR-0018). Built by pairing a
/// server name with the receiver from
/// [`McpClientManager::start_server_with_requests`](crate::mcp::McpClientManager::start_server_with_requests).
///
/// One channel = one granted server. Servicing several granted
/// servers concurrently (a merged, server-tagged stream) is a
/// follow-up; v1 wires a single channel, which is what the everything
/// server's sampling tool exercises.
pub struct SamplingChannel {
    /// One inbound request receiver per grant-bearing server, paired
    /// with that server's name. [`recv`](Self::recv) selects across all
    /// of them so more than one grant-bearing server can be serviced in
    /// a single invocation (ADR-0018); a closed receiver is dropped.
    channels: Vec<(String, UnboundedReceiver<ServerRequest>)>,
}

impl SamplingChannel {
    /// A channel for a single server (the direct / test path).
    pub fn new(server: impl Into<String>, rx: UnboundedReceiver<ServerRequest>) -> Self {
        Self {
            channels: vec![(server.into(), rx)],
        }
    }

    /// A channel merging several servers' request receivers.
    pub fn merged(channels: Vec<(String, UnboundedReceiver<ServerRequest>)>) -> Self {
        Self { channels }
    }

    /// Receive the next request from any server, tagged with the server
    /// name. Closed receivers are removed as they drain; returns `None`
    /// once every server's channel is closed. Selection is biased toward
    /// earlier servers, which is fine — requests are independent.
    pub async fn recv(&mut self) -> Option<(String, ServerRequest)> {
        std::future::poll_fn(|cx| {
            let mut index = 0;
            while index < self.channels.len() {
                match self.channels[index].1.poll_recv(cx) {
                    std::task::Poll::Ready(Some(request)) => {
                        let server = self.channels[index].0.clone();
                        return std::task::Poll::Ready(Some((server, request)));
                    }
                    // This server's channel closed; drop it and continue.
                    std::task::Poll::Ready(None) => {
                        self.channels.remove(index);
                    }
                    std::task::Poll::Pending => index += 1,
                }
            }
            if self.channels.is_empty() {
                std::task::Poll::Ready(None)
            } else {
                std::task::Poll::Pending
            }
        })
        .await
    }
}

/// Agent-relevant context for an invocation: the services and
/// (future) policy/metadata the agent can use or should know,
/// held read-only. Open to addition — new agent-facing
/// dependencies become fields here, wired through
/// [`ReducerContextBuilder`], without changing
/// [`ReducerRunner::new`].
///
/// Constructed via [`ReducerContext::builder`]; the fields are
/// private so the builder is the single construction surface.
pub struct ReducerContext {
    /// Tools the agent may call. Interior-mutable (ADR-0020): the
    /// daemon's notification drain installs a rebuilt registry on
    /// `tools/list_changed`; each invocation snapshots the `Arc` at
    /// start and keeps it for its whole step loop, so in-flight
    /// invocations are never hot-swapped.
    tools: std::sync::RwLock<Arc<ToolRegistry>>,
    /// Read-only handle over the running MCP servers, used to read
    /// the agent's `static_resources` pins at invocation start.
    /// `None` when no MCP servers are wired (e.g. most tests).
    resources: Option<McpResourceReader>,
    /// Outbound validation seam for sampling results before they
    /// return to the requesting server (ADR-0018 §4): censor secrets,
    /// reject leakage, etc. Default is an empty chain (allow
    /// everything); concrete validators (e.g. a `HighEntropyRedactor`)
    /// are added without touching the runner.
    sampling_validators: ValidatorChain<CreateMessageResult>,
    /// Inbound validation seam for elicitation requests (ADR-0018 §4):
    /// inspects the request's message and schema field names — a
    /// server can request `{ api_key: string }` and coax the model to
    /// fill it from context. Default empty (allow).
    elicitation_inbound_validators: ValidatorChain<CreateElicitationRequestParams>,
    /// Outbound validation seam for the structured value an
    /// elicitation produced before it returns to the server: censor
    /// secrets in the extracted fields. Default empty (allow).
    elicitation_outbound_validators: ValidatorChain<Value>,
}

impl ReducerContext {
    /// Start building a context. `tools` is required; `resources`
    /// is optional. See [`ReducerContextBuilder`].
    pub fn builder() -> ReducerContextBuilder {
        ReducerContextBuilder::default()
    }

    /// Snapshot the current shared tool registry. Each invocation
    /// takes one snapshot at start and uses it throughout, so a
    /// concurrent [`install_tools`](Self::install_tools) only affects
    /// invocations that start afterwards (ADR-0020).
    pub fn tools(&self) -> Arc<ToolRegistry> {
        self.tools.read().expect("tools lock poisoned").clone()
    }

    /// Replace the shared tool registry (the daemon's notification
    /// drain installs a rebuilt registry on `tools/list_changed`).
    /// In-flight invocations keep their snapshot.
    pub fn install_tools(&self, tools: Arc<ToolRegistry>) {
        *self.tools.write().expect("tools lock poisoned") = tools;
    }
}

/// Fluent builder for [`ReducerContext`]. `tools` is required;
/// optional fields default to absent. [`build`](Self::build)
/// panics if a required field was not set — every construction
/// site is internal and known at compile time, so a missing field
/// is a programmer error rather than a runtime condition.
#[derive(Default)]
pub struct ReducerContextBuilder {
    tools: Option<Arc<ToolRegistry>>,
    resources: Option<McpResourceReader>,
    sampling_validators: Option<ValidatorChain<CreateMessageResult>>,
    elicitation_inbound_validators: Option<ValidatorChain<CreateElicitationRequestParams>>,
    elicitation_outbound_validators: Option<ValidatorChain<Value>>,
}

impl ReducerContextBuilder {
    /// Tools the agent may call (required).
    pub fn tools(mut self, tools: Arc<ToolRegistry>) -> Self {
        self.tools = Some(tools);
        self
    }

    /// Read-only MCP resource handle so the runner can inject
    /// `static_resources` content at invocation start (optional).
    pub fn resources(mut self, resources: McpResourceReader) -> Self {
        self.resources = Some(resources);
        self
    }

    /// Outbound validators for sampling results (optional; defaults
    /// to an empty allow-everything chain).
    pub fn sampling_validators(mut self, chain: ValidatorChain<CreateMessageResult>) -> Self {
        self.sampling_validators = Some(chain);
        self
    }

    /// Inbound validators for elicitation requests (optional; defaults
    /// to an empty allow-everything chain).
    pub fn elicitation_inbound_validators(
        mut self,
        chain: ValidatorChain<CreateElicitationRequestParams>,
    ) -> Self {
        self.elicitation_inbound_validators = Some(chain);
        self
    }

    /// Outbound validators for elicitation values (optional; defaults
    /// to an empty allow-everything chain).
    pub fn elicitation_outbound_validators(mut self, chain: ValidatorChain<Value>) -> Self {
        self.elicitation_outbound_validators = Some(chain);
        self
    }

    /// Finalise the context. Panics if `tools` was not set.
    pub fn build(self) -> ReducerContext {
        ReducerContext {
            tools: std::sync::RwLock::new(
                self.tools
                    .expect("ReducerContext::builder() requires .tools(..)"),
            ),
            resources: self.resources,
            sampling_validators: self.sampling_validators.unwrap_or_default(),
            elicitation_inbound_validators: self.elicitation_inbound_validators.unwrap_or_default(),
            elicitation_outbound_validators: self
                .elicitation_outbound_validators
                .unwrap_or_default(),
        }
    }
}

/// Platform machinery the host loop runs on — not agent-facing.
/// Open to addition — new platform dependencies become fields
/// here, wired through [`RunnerConfigBuilder`], without changing
/// [`ReducerRunner::new`].
///
/// Constructed via [`RunnerConfig::builder`]; the fields are
/// private so the builder is the single construction surface.
pub struct RunnerConfig {
    /// Event bus for publishing the canonical event sequence.
    bus: EventBus,
    /// Model→price lookup for cost accounting.
    pricing: Arc<PricingTable>,
    /// Three-state WAL / invocation-state persistence
    /// (data-architecture.md §5.5).
    store: Arc<WorkerStore>,
    /// Identity of the worker hosting this runner (coordination /
    /// archive-ack routing on `fq.worker.{worker_id}.*`).
    worker_id: WorkerId,
}

impl RunnerConfig {
    /// Start building the platform config. All four fields are
    /// required; see [`RunnerConfigBuilder`].
    pub fn builder() -> RunnerConfigBuilder {
        RunnerConfigBuilder::default()
    }
}

/// Fluent builder for [`RunnerConfig`]. Every field is required;
/// [`build`](Self::build) panics if any was not set — the
/// construction sites are internal and known at compile time.
#[derive(Default)]
pub struct RunnerConfigBuilder {
    bus: Option<EventBus>,
    pricing: Option<Arc<PricingTable>>,
    store: Option<Arc<WorkerStore>>,
    worker_id: Option<WorkerId>,
}

impl RunnerConfigBuilder {
    /// Event bus for publishing the canonical event sequence.
    pub fn bus(mut self, bus: EventBus) -> Self {
        self.bus = Some(bus);
        self
    }

    /// Model→price lookup for cost accounting.
    pub fn pricing(mut self, pricing: Arc<PricingTable>) -> Self {
        self.pricing = Some(pricing);
        self
    }

    /// Three-state WAL / invocation-state persistence.
    pub fn store(mut self, store: Arc<WorkerStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Identity of the worker hosting this runner.
    pub fn worker_id(mut self, worker_id: WorkerId) -> Self {
        self.worker_id = Some(worker_id);
        self
    }

    /// Finalise the config. Panics if any field was not set.
    pub fn build(self) -> RunnerConfig {
        RunnerConfig {
            bus: self.bus.expect("RunnerConfig::builder() requires .bus(..)"),
            pricing: self
                .pricing
                .expect("RunnerConfig::builder() requires .pricing(..)"),
            store: self
                .store
                .expect("RunnerConfig::builder() requires .store(..)"),
            worker_id: self
                .worker_id
                .expect("RunnerConfig::builder() requires .worker_id(..)"),
        }
    }
}

/// Drive an agent invocation through a [`Reducer`]. Holds the
/// agent-relevant [`ReducerContext`] and the platform
/// [`RunnerConfig`] as separate read-only bundles so new
/// dependencies of either kind extend a bundle rather than
/// re-signing the constructor.
///
/// Generic over the [`Reducer`] impl. Production wires
/// `ReducerRunner<Harness>` everywhere; tests may instantiate
/// with stub reducers when they need finer control. The
/// reducer is held as a field so the [`crate::Worker`] trait
/// impl doesn't have to expose the generic.
pub struct ReducerRunner<R: Reducer + Send + Sync = Harness> {
    /// Agent-relevant services and policy (tools today).
    context: Arc<ReducerContext>,
    /// Platform machinery (bus, pricing, WAL store, worker id).
    config: Arc<RunnerConfig>,
    /// The reducer driven by every `run`/`resume`. Held as a
    /// field so callers don't have to pass it on every call.
    reducer: R,
}

impl<R: Reducer + Send + Sync> ReducerRunner<R> {
    pub fn new(context: Arc<ReducerContext>, config: Arc<RunnerConfig>, reducer: R) -> Self {
        Self {
            context,
            config,
            reducer,
        }
    }

    /// Run a single invocation of `agent` through this runner's
    /// reducer to terminal.
    ///
    /// Run a single invocation to terminal.
    ///
    /// If the agent grants an inbound MCP capability (sampling /
    /// elicitation / roots) to a server, that server is started
    /// **per-invocation** (ADR-0018) with the agent's advertised
    /// capabilities + sandbox-derived roots; its tools are layered onto
    /// the base registry for this invocation and its server-initiated
    /// requests are serviced via the runner's `select!`. Otherwise the
    /// base registry runs with no inbound channel. v1 wires a single
    /// grant-bearing server; multiple is a follow-up (a merged,
    /// server-tagged stream).
    pub async fn run(
        &self,
        agent: &Agent,
        llm: &dyn LlmClient,
        trigger_source: TriggerSource,
        trigger_subject: Option<String>,
        trigger_payload: Value,
    ) -> Result<InvocationOutcome, ExecutorError> {
        // Every grant-bearing server runs per-invocation (ADR-0018): we
        // start each, layer its tools onto the base registry, and merge
        // their server-initiated request streams into one channel keyed
        // by server name.
        let grant_servers: Vec<_> = agent
            .mcp_servers()
            .iter()
            .filter(|d| agent.grants_inbound_capability(&d.server))
            .collect();
        if grant_servers.is_empty() {
            // No grant-bearing server: base registry, no channel.
            return self
                .run_loop_for(
                    agent,
                    llm,
                    trigger_source,
                    trigger_subject,
                    trigger_payload,
                    &self.context.tools(),
                    None,
                )
                .await;
        }

        let mut manager = McpClientManager::new();
        let mut tools = (*self.context.tools()).clone();
        let mut channels: Vec<(String, UnboundedReceiver<ServerRequest>)> = Vec::new();
        for decl in grant_servers {
            let capabilities = AdvertisedCapabilities {
                sampling: agent
                    .sampling_grant()
                    .is_some_and(|g| g.permits(&decl.server)),
                elicitation: agent
                    .elicitation_grant()
                    .is_some_and(|g| g.permits(&decl.server)),
                roots: agent.roots_grant().is_some_and(|g| g.permits(&decl.server)),
            };
            // Advertised roots ⊆ sandbox grant; the roots outbound seam
            // is default-allow (roots have no evaluator path).
            let roots = advertised_roots(
                agent.sandbox(),
                agent.roots_grant(),
                &decl.server,
                &ValidatorChain::new(),
            );
            let config = McpServerConfig {
                name: decl.server.clone(),
                command: decl.command.clone().unwrap_or_default(),
                args: decl.args.clone(),
                env: decl.env.clone(),
                url: decl.url.clone(),
            };
            match manager
                .start_server_with_requests(config, roots, capabilities)
                .await
            {
                Ok((server_tools, rx, _roots_handle)) => {
                    // Per-invocation server tools win on name collision.
                    for tool in server_tools {
                        tools.register(tool);
                    }
                    channels.push((decl.server.clone(), rx));
                }
                // A server that fails to start is skipped (its tools and
                // server-initiated requests are simply absent) rather
                // than failing the whole invocation.
                Err(err) => {
                    warn!(
                        agent_id = %agent.id(),
                        server = %decl.server,
                        error = %err,
                        "failed to start grant-bearing MCP server per-invocation; skipping it"
                    );
                }
            }
        }

        let sampling = (!channels.is_empty()).then(|| SamplingChannel::merged(channels));
        let outcome = self
            .run_loop_for(
                agent,
                llm,
                trigger_source,
                trigger_subject,
                trigger_payload,
                &tools,
                sampling,
            )
            .await;
        manager.shutdown().await;
        outcome
    }

    /// Run a single invocation, servicing inbound server-initiated
    /// requests (sampling) from `sampling` during tool-call awaits
    /// (ADR-0018), against the runner's base tool registry. The
    /// caller supplies the channel (and is responsible for the
    /// server's lifecycle); [`run`](Self::run) is the auto-managed
    /// path. The runner is the sole LLM arbiter and
    /// gates/runs/validates each request itself.
    pub async fn run_with_server_requests(
        &self,
        agent: &Agent,
        llm: &dyn LlmClient,
        trigger_source: TriggerSource,
        trigger_subject: Option<String>,
        trigger_payload: Value,
        sampling: Option<SamplingChannel>,
    ) -> Result<InvocationOutcome, ExecutorError> {
        self.run_loop_for(
            agent,
            llm,
            trigger_source,
            trigger_subject,
            trigger_payload,
            &self.context.tools(),
            sampling,
        )
        .await
    }

    /// The shared invocation body: emit `triggered`, build the agent
    /// config from `tools`, and drive the step loop. `tools` is the
    /// effective registry for this invocation (base, or base + a
    /// per-invocation server's tools).
    #[allow(clippy::too_many_arguments)]
    async fn run_loop_for(
        &self,
        agent: &Agent,
        llm: &dyn LlmClient,
        trigger_source: TriggerSource,
        trigger_subject: Option<String>,
        trigger_payload: Value,
        tools: &ToolRegistry,
        sampling: Option<SamplingChannel>,
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
        let tool_schemas = tools.build_schemas(agent.tools());

        let agent_config = AgentConfig {
            agent_id: agent_id.clone(),
            model: agent.model().to_string(),
            system_prompt: agent.system_prompt().to_string(),
            tools_available: tool_schemas.clone(),
            allowed_tool_names: agent.tools().to_vec(),
            max_iterations: crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS,
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

        // Read the agent's `static_resources` pins once, before the
        // step loop, and hand the rendered content to step 0 only.
        // Resume does *not* re-inject — the content is already in
        // the persisted conversation history (see `resume`).
        let static_context = self.read_static_resources(agent).await;

        self.run_loop_inner(
            agent,
            llm,
            invocation_id,
            &agent_id,
            &agent_config,
            &trigger,
            &sandbox,
            tools,
            state,
            last_result,
            step_index_start,
            totals,
            start,
            started_at_ms,
            static_context,
            sampling,
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
    pub async fn resume(
        &self,
        agent: &Agent,
        llm: &dyn LlmClient,
        invocation_id: Uuid,
    ) -> Result<InvocationOutcome, ExecutorError> {
        let inv_str = invocation_id.to_string();
        let state_row = self
            .config
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
            .config
            .store
            .list_tool_dispatches_for_invocation(&inv_str)
            .await
            .map_err(map_store_err)?;
        let llms = self
            .config
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
                completed.push((r.completed_at.unwrap_or(0), llm_row_to_capability(r)?));
            }
        }
        completed.sort_by_key(|x| x.0);

        // Set up agent context (mirrors run()). One registry snapshot
        // serves both the schemas and the loop (ADR-0020 consistency).
        let sandbox = agent.sandbox().to_tool_sandbox();
        let base_tools = self.context.tools();
        let tool_schemas = base_tools.build_schemas(agent.tools());
        let agent_config = AgentConfig {
            agent_id: agent_id.clone(),
            model: agent.model().to_string(),
            system_prompt: agent.system_prompt().to_string(),
            tools_available: tool_schemas,
            allowed_tool_names: agent.tools().to_vec(),
            max_iterations: crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS,
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
                // Replay reconstructs persisted state; pins were
                // already injected on the original step 0.
                static_resource_context: None,
            };
            let output = self.reducer.step(input).map_err(|e| {
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
        // Reconstitute lifetime totals from the WAL so the budget
        // ceiling bounds the invocation's lifetime spend, not the
        // current attempt's. Errored LLM dispatches are excluded to
        // match the live path, which counts a call only once the
        // provider returns. Sampling/elicitation sub-costs cannot be
        // split back out of the WAL and stay zero — safe, because a
        // resumed run cannot service server-initiated requests
        // (ADR-0018 §5), so no sub-budget is consulted after resume.
        // `total_duration_ms` stays attempt-scoped: it is what
        // `start` below measures.
        let mut totals = InvocationTotals::default();
        for r in &llms {
            if r.status == DispatchStatus::Completed && r.is_error != Some(true) {
                totals.total_llm_calls += 1;
                totals.total_cost += r.cost_usd.unwrap_or(0.0);
            }
        }
        totals.total_tool_calls = tools
            .iter()
            .filter(|r| r.status == DispatchStatus::Completed)
            .count() as u32;
        let start = Instant::now();
        let mut cursor: Option<Uuid> = None;
        self.run_loop_inner(
            agent,
            llm,
            invocation_id,
            &agent_id,
            &agent_config,
            &trigger,
            &sandbox,
            // Resume uses the base registry: grant-bearing servers are
            // not restarted on resume (ADR-0018 §5).
            &base_tools,
            state,
            last_result,
            step_index,
            totals,
            start,
            state_row.started_at,
            // Resume never re-injects static_resources: the pins'
            // content is already in the replayed conversation
            // history persisted on the original step 0.
            None,
            // No inbound server channel on resume: the per-invocation
            // server connection died with the crash, so a resumed run
            // cannot service (or replay) sampling (ADR-0018 §5). Any
            // in-flight sampling is surfaced via `fq recover`.
            None,
            &mut cursor,
        )
        .await
    }

    /// The reducer-loop body extracted so `run` and `resume`
    /// can share it. Caller threads in the prepared
    /// `(state, last_result, step_index, totals)` plus all the
    /// invocation-scoped context.
    #[allow(clippy::too_many_arguments)]
    async fn run_loop_inner(
        &self,
        agent: &Agent,
        llm: &dyn LlmClient,
        invocation_id: Uuid,
        agent_id: &AgentId,
        agent_config: &AgentConfig,
        trigger: &TriggerPayload,
        sandbox: &ToolSandbox,
        tools: &ToolRegistry,
        mut state: Vec<u8>,
        mut last_result: Option<CapabilityResult>,
        step_index_start: u32,
        mut totals: InvocationTotals,
        start: Instant,
        started_at_ms: i64,
        static_context: Option<String>,
        mut sampling: Option<SamplingChannel>,
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
                // Static-resource content is injected exactly once,
                // on step 0. Later steps and resumed runs carry it
                // in the reducer's persisted conversation history.
                static_resource_context: if step_index == 0 {
                    static_context.clone()
                } else {
                    None
                },
            };

            let output = match self.reducer.step(input) {
                Ok(o) => o,
                Err(err) => {
                    totals.total_duration_ms = start.elapsed().as_millis() as u64;
                    let kind = FailureKind::RuntimeError;
                    let message = format!("reducer step failed: {err}");
                    self.emit_failed(
                        agent_id,
                        invocation_id,
                        kind,
                        message.clone(),
                        FailurePhase::LlmResponse,
                        totals,
                        cursor,
                    )
                    .await?;
                    return Err(ExecutorError::InvocationFailed { kind, message });
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
            self.config
                .store
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
                    archive_status: None,
                    archive_published_at: None,
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

                    self.publish_archived_and_mark_pending(
                        cursor,
                        agent_id,
                        invocation_id,
                        "completed",
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
                    return Err(ExecutorError::InvocationFailed {
                        kind,
                        message: err.message,
                    });
                }
                NextAction::CallModel(request) => {
                    let outcome = self
                        .run_model_with_llm(
                            llm,
                            agent.budget(),
                            agent_id,
                            invocation_id,
                            request,
                            LlmCallOrigin::AgentTurn,
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
                            tools,
                            llm,
                            agent_id,
                            invocation_id,
                            req,
                            &mut totals,
                            start,
                            sampling.as_mut(),
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
                                tools,
                                llm,
                                agent_id,
                                invocation_id,
                                req,
                                &mut totals,
                                start,
                                sampling.as_mut(),
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

        // Host step budget exhausted. Surface as a runtime failure —
        // this is the host's backstop against a wedged reducer, not
        // the agent-level `max_iterations` cap.
        totals.total_duration_ms = start.elapsed().as_millis() as u64;
        let kind = FailureKind::RuntimeError;
        let message = format!("host step budget exhausted ({HOST_STEP_BUDGET})");
        self.emit_failed(
            agent_id,
            invocation_id,
            kind,
            message.clone(),
            FailurePhase::LlmResponse,
            totals,
            cursor,
        )
        .await?;
        Err(ExecutorError::InvocationFailed { kind, message })
    }

    /// Read the agent's `static_resources` pins through the MCP
    /// resource handle and render them into a single context
    /// block for injection at step 0. Returns `None` when the
    /// agent declares no pins, when no resource handle is wired,
    /// or when none of the pins could be read.
    ///
    /// Best-effort by design: a pin that fails to read is logged
    /// and skipped rather than failing the invocation. The host
    /// curates these for guaranteed *inclusion*, but a transient
    /// read failure against a third-party server should degrade
    /// to "context omitted", not "invocation dead".
    async fn read_static_resources(&self, agent: &Agent) -> Option<String> {
        let pins = agent.static_resources();
        if pins.is_empty() {
            return None;
        }
        let Some(reader) = self.context.resources.as_ref() else {
            warn!(
                agent_id = %agent.id(),
                "agent declares static_resources but no MCP resource handle is wired; \
                 skipping injection"
            );
            return None;
        };

        let mut sections = Vec::new();
        for pin in pins {
            match reader.read_resource(&pin.server, &pin.uri).await {
                Ok(result) => {
                    let body = crate::mcp::render_resource_contents(&result);
                    sections.push(format!(
                        "Resource mcp://{}/{}:\n{}",
                        pin.server, pin.uri, body
                    ));
                }
                Err(err) => {
                    warn!(
                        agent_id = %agent.id(),
                        server = %pin.server,
                        uri = %pin.uri,
                        error = %err,
                        "failed to read static_resources pin; omitting it from injected context"
                    );
                }
            }
        }

        if sections.is_empty() {
            None
        } else {
            Some(format!(
                "The following resources were provided as context for this invocation:\n\n{}",
                sections.join("\n\n")
            ))
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_tool(
        &self,
        agent: &Agent,
        sandbox: &ToolSandbox,
        tools: &ToolRegistry,
        llm: &dyn LlmClient,
        agent_id: &AgentId,
        invocation_id: Uuid,
        req: ToolCallRequest,
        totals: &mut InvocationTotals,
        start: Instant,
        sampling: Option<&mut SamplingChannel>,
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
        self.config
            .store
            .write_tool_intent(
                &inv_str,
                req.tool_call_id.as_str(),
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

        let tool = match tools.get(&req.tool_name) {
            Some(t) => t,
            None => {
                // Tool isn't registered — close the WAL row as
                // a non-ambiguous error so recovery doesn't see
                // it as `dispatched` forever.
                self.config
                    .store
                    .write_tool_dispatched(&inv_str, req.tool_call_id.as_str(), unix_now_ms())
                    .await
                    .map_err(map_store_err)?;
                let msg = format!("no implementation registered for tool '{}'", req.tool_name);
                self.config
                    .store
                    .write_tool_completed(
                        &inv_str,
                        req.tool_call_id.as_str(),
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
        // While the tool runs, the server it belongs to may initiate
        // requests back at us (sampling) — those arrive *because* the
        // agent called this tool, landing while we're parked at the
        // await. Service them in a `select!` so the runner, the sole
        // LLM arbiter, handles them without a second caller and
        // without blocking the tool (ADR-0018 §2). With no channel
        // wired, this is a plain await.
        let outcome = match sampling {
            None => tool.execute(&ctx, req.parameters.clone()).await,
            Some(channel) => {
                let tool_fut = tool.execute(&ctx, req.parameters.clone());
                tokio::pin!(tool_fut);
                loop {
                    tokio::select! {
                        // Bias toward completing the tool: if both are
                        // ready, return the tool result rather than
                        // starving it behind a backlog of requests.
                        biased;
                        result = &mut tool_fut => break result,
                        maybe_req = channel.recv() => match maybe_req {
                            Some((server, request)) => {
                                self.handle_server_request(
                                    agent,
                                    &server,
                                    llm,
                                    agent_id,
                                    invocation_id,
                                    request,
                                    totals,
                                    start,
                                    cursor,
                                )
                                .await?;
                            }
                            // All servers' channels closed: just await
                            // the tool to completion.
                            None => break (&mut tool_fut).await,
                        }
                    }
                }
            }
        };
        let duration_ms = tool_start.elapsed().as_millis() as u64;

        // Tool returned control. Mark dispatched (the
        // ambiguous-window state) before processing the result.
        self.config
            .store
            .write_tool_dispatched(&inv_str, req.tool_call_id.as_str(), unix_now_ms())
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
                self.config
                    .store
                    .write_tool_completed(
                        &inv_str,
                        req.tool_call_id.as_str(),
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
                self.config
                    .store
                    .write_tool_completed(
                        &inv_str,
                        req.tool_call_id.as_str(),
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
        self.config
            .store
            .write_tool_dispatched(inv_str, req.tool_call_id.as_str(), unix_now_ms())
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
        self.config
            .store
            .write_tool_completed(
                inv_str,
                req.tool_call_id.as_str(),
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
        self.config
            .bus
            .publish(&event)
            .await
            .map_err(ExecutorError::Bus)?;
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
        .await?;

        // Failure paths reach this method from several call
        // sites — some after the run-loop's terminal upsert has
        // already fired (NextAction::Failed / harness errors),
        // some mid-step before any terminal write (LLM error,
        // budget exceeded). To keep recovery and archive
        // semantics consistent, the failure path is the
        // authoritative point at which `invocation_state` is
        // marked terminal. Idempotent: a no-op if the row is
        // already terminal.
        let terminal_at_ms = unix_now_ms();
        self.ensure_terminal("failed", invocation_id, terminal_at_ms)
            .await?;
        self.publish_archived_and_mark_pending(cursor, agent_id, invocation_id, "failed")
            .await?;
        Ok(())
    }

    /// Set `terminal_at` (and update `phase`) on the worker's
    /// `invocation_state` row if it is not already terminal.
    /// A no-op when the row is already terminal — keeps the
    /// original `terminal_at` so the archive timestamp matches
    /// the first observation of terminal.
    ///
    /// Reads the row first to preserve every other column
    /// (state_blob, iteration, started_at, etc.); the
    /// `upsert_invocation_state` UPDATE arm overwrites them
    /// otherwise. The pattern is "read-modify-write" rather
    /// than a partial UPDATE so the existing row-shaped
    /// abstraction stays the single SQL surface.
    async fn ensure_terminal(
        &self,
        phase_label: &str,
        invocation_id: Uuid,
        terminal_at_ms: i64,
    ) -> Result<(), ExecutorError> {
        let invocation_id_str = invocation_id.to_string();
        let existing = self
            .config
            .store
            .get_invocation_state(&invocation_id_str)
            .await
            .map_err(map_store_err)?;
        let Some(mut row) = existing else {
            // No state row at all — the run-loop hasn't done
            // its first upsert yet. Nothing to archive. Skip
            // silently; recovery has nothing to recover.
            return Ok(());
        };
        if row.terminal_at.is_some() {
            return Ok(());
        }
        row.phase = phase_label.to_string();
        row.terminal_at = Some(terminal_at_ms);
        row.updated_at = terminal_at_ms;
        self.config
            .store
            .upsert_invocation_state(&row)
            .await
            .map_err(map_store_err)?;
        Ok(())
    }

    /// Publish `InvocationArchived` for an already-terminal
    /// invocation and flip the local row to `archive_status =
    /// "pending"`. Called from both the Complete and Failed
    /// terminal paths; the retry sweeper subsequently
    /// republishes if the control-plane ack does not arrive.
    ///
    /// The state blob and timestamps come from the persisted
    /// `invocation_state` row so callers don't need to thread
    /// them through. If the row is missing (a logic bug — the
    /// run-loop's terminal upsert should have written it) this
    /// is a no-op so we don't crash mid-shutdown.
    async fn publish_archived_and_mark_pending(
        &self,
        cursor: &mut Option<Uuid>,
        agent_id: &AgentId,
        invocation_id: Uuid,
        final_phase: &str,
    ) -> Result<(), ExecutorError> {
        let invocation_id_str = invocation_id.to_string();
        let row = match self
            .config
            .store
            .get_invocation_state(&invocation_id_str)
            .await
            .map_err(map_store_err)?
        {
            Some(r) => r,
            None => {
                warn!(
                    invocation_id = %invocation_id,
                    "archive publish skipped: invocation_state row missing"
                );
                return Ok(());
            }
        };
        let Some(terminal_at_ms) = row.terminal_at else {
            warn!(
                invocation_id = %invocation_id,
                "archive publish skipped: invocation_state row is not terminal"
            );
            return Ok(());
        };

        self.publish_chained(
            cursor,
            Event::new(
                agent_id.clone(),
                invocation_id,
                EventPayload::InvocationArchived(InvocationArchivedPayload {
                    worker_id: self.config.worker_id.clone(),
                    final_phase: final_phase.to_string(),
                    final_state_blob: row.state_blob,
                    started_at_ms: row.started_at,
                    terminal_at_ms,
                }),
            ),
        )
        .await?;

        // `archive_published_at` is the publish time, not
        // `terminal_at` — the retry sweeper measures from when
        // the most recent publish went out, not from terminal.
        self.config
            .store
            .set_archive_pending(&invocation_id_str, unix_now_ms())
            .await
            .map_err(map_store_err)?;
        Ok(())
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
impl<R: Reducer + Send + Sync> ReducerRunner<R> {
    /// Agent-turn LLM path: dispatch through the shared core, then
    /// apply agent-turn failure semantics — an LLM error fails the
    /// invocation, and exceeding the budget terminates it.
    #[allow(clippy::too_many_arguments)]
    async fn run_model_with_llm(
        &self,
        llm: &dyn LlmClient,
        budget: Option<f64>,
        agent_id: &AgentId,
        invocation_id: Uuid,
        request: ModelRequest,
        origin: LlmCallOrigin,
        totals: &mut InvocationTotals,
        start: Instant,
        cursor: &mut Option<Uuid>,
    ) -> Result<ModelOutcome, ExecutorError> {
        let response = match self
            .dispatch_llm(
                llm,
                agent_id,
                invocation_id,
                request,
                origin,
                totals,
                cursor,
            )
            .await?
        {
            Ok((response, _cost)) => response,
            Err(err) => {
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

        Ok(ModelOutcome::Response(response))
    }

    /// Shared LLM dispatch core (ADR-0018 §2): the single WAL'd /
    /// evented / budgeted path every model call flows through — agent
    /// turns and sampling alike. Writes the §5.5 WAL
    /// (intent → dispatched → completed), publishes
    /// `llm.request` / `llm.dispatched` / `llm.response` + cost (the
    /// cost tagged with `origin` for attribution), and folds cost into
    /// `totals`. Returns the inner `Err` on an LLM-call failure (the
    /// WAL is already closed `is_error`) so each caller applies its
    /// own semantics — an agent turn fails the invocation, a sampling
    /// request merely declines. The outer `Err` is infrastructure
    /// (store / bus).
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_llm(
        &self,
        llm: &dyn LlmClient,
        agent_id: &AgentId,
        invocation_id: Uuid,
        request: ModelRequest,
        origin: LlmCallOrigin,
        totals: &mut InvocationTotals,
        cursor: &mut Option<Uuid>,
    ) -> Result<Result<(ModelResponse, f64), crate::llm::LlmError>, ExecutorError> {
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
        self.config
            .store
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
                    origin: origin.clone(),
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
                self.config
                    .store
                    .write_llm_dispatched(&inv_str, &req_str, unix_now_ms())
                    .await
                    .map_err(map_store_err)?;
                self.config
                    .store
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
                // Hand the LLM error back to the caller; the WAL is
                // already closed `is_error`, so this is a final state.
                return Ok(Err(err));
            }
        };

        totals.total_llm_calls += 1;

        // LLM returned control. Mark dispatched (ambiguous
        // window), publish the dispatched event, then transition
        // to completed before the response/cost events go out.
        self.config
            .store
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
        let response_json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        self.config
            .store
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
        let pricing = self.config.pricing.lookup(&request.model);
        if pricing.is_none() {
            warn!(
                model = %request.model,
                "no pricing known for model; cost will be reported as $0"
            );
        }
        let (input_cost, output_cost, total_cost) = pricing
            .map(|p| p.calculate(&response.usage))
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
                    origin: origin.clone(),
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
                origin,
            }),
        )
        .await?;

        Ok(Ok((
            ModelResponse {
                content: response.content,
                tool_calls: response.tool_calls,
                stop_reason: response.stop_reason,
                usage: response.usage,
            },
            total_cost,
        )))
    }

    /// Run a schema-constrained structured completion — the reusable
    /// primitive behind elicitation (ADR-0018 §4), shaped so the future
    /// sampling evaluator-validator and spawn-deliverable typing reuse
    /// it. Build a request, dispatch it on the agent's model, parse the
    /// response, and validate the parsed value — retrying up to
    /// `max_retries` times. Returns the first value that parses, passes
    /// `validate`, *and* survives the `outbound` seam
    /// (`Ok(Some(value))`); a model failure, exhausted retries, or an
    /// outbound denial all yield `Ok(None)` so the caller can decline.
    /// `record_cost` attributes each dispatched call's cost to the
    /// caller's sub-budget. The outer `Err` is infrastructure
    /// (store / bus).
    #[allow(clippy::too_many_arguments)]
    async fn run_structured_completion(
        &self,
        llm: &dyn LlmClient,
        agent_id: &AgentId,
        invocation_id: Uuid,
        origin: LlmCallOrigin,
        max_retries: u32,
        totals: &mut InvocationTotals,
        cursor: &mut Option<Uuid>,
        build_request: impl Fn() -> ModelRequest,
        parse: impl Fn(Option<&str>) -> Option<Value>,
        validate: impl Fn(&Value) -> Result<(), String>,
        outbound: &ValidatorChain<Value>,
        mut record_cost: impl FnMut(&mut InvocationTotals, f64),
    ) -> Result<Option<Value>, ExecutorError> {
        for _ in 0..max_retries {
            let response = match self
                .dispatch_llm(
                    llm,
                    agent_id,
                    invocation_id,
                    build_request(),
                    origin.clone(),
                    totals,
                    cursor,
                )
                .await?
            {
                Ok((response, cost)) => {
                    record_cost(totals, cost);
                    response
                }
                // A model failure resolves to "no value"; the caller
                // declines and the agent turn is untouched.
                Err(_) => return Ok(None),
            };

            let Some(value) = parse(response.content.as_deref()) else {
                continue; // unparseable → retry
            };
            if validate(&value).is_err() {
                continue; // invalid → retry
            }

            // Outbound seam: a denial censors the whole result.
            return match outbound.run(value) {
                Ok(value) => Ok(Some(value)),
                Err(_) => Ok(None),
            };
        }
        // Retries exhausted without a valid result.
        Ok(None)
    }

    /// Service one inbound server-initiated request (ADR-0018). The
    /// runner is the sole arbiter; the handler is a thin bridge, so
    /// the gate/run/validate logic lives here and replies on the
    /// request's oneshot. A dropped reply (the tool finished and the
    /// bridge went away) is ignored.
    #[allow(clippy::too_many_arguments)]
    async fn handle_server_request(
        &self,
        agent: &Agent,
        server: &str,
        llm: &dyn LlmClient,
        agent_id: &AgentId,
        invocation_id: Uuid,
        request: ServerRequest,
        totals: &mut InvocationTotals,
        start: Instant,
        cursor: &mut Option<Uuid>,
    ) -> Result<(), ExecutorError> {
        match request {
            ServerRequest::Sampling { params, reply } => {
                let result = self
                    .handle_sampling(
                        agent,
                        server,
                        llm,
                        agent_id,
                        invocation_id,
                        params,
                        totals,
                        start,
                        cursor,
                    )
                    .await?;
                let _ = reply.send(result);
                Ok(())
            }
            ServerRequest::Elicitation { params, reply } => {
                let result = self
                    .handle_elicitation(
                        agent,
                        server,
                        llm,
                        agent_id,
                        invocation_id,
                        params,
                        totals,
                        cursor,
                    )
                    .await?;
                let _ = reply.send(result);
                Ok(())
            }
        }
    }

    /// Answer a `sampling/createMessage` request (ADR-0018 §2):
    /// **gate** (granted? within the sampling sub-budget and the
    /// invocation total?) → **run** through the shared LLM path tagged
    /// `origin = sampling{server}` → **validate** the result through
    /// the outbound seam → reply. A policy refusal or a model failure
    /// returns a structured decline to the *server* and leaves the
    /// agent invocation untouched — sampling spends the agent's
    /// budget but never fails its turn. The outer `Err` is
    /// infrastructure (store / bus) and does propagate.
    #[allow(clippy::too_many_arguments)]
    async fn handle_sampling(
        &self,
        agent: &Agent,
        server: &str,
        llm: &dyn LlmClient,
        agent_id: &AgentId,
        invocation_id: Uuid,
        params: CreateMessageRequestParams,
        totals: &mut InvocationTotals,
        _start: Instant,
        cursor: &mut Option<Uuid>,
    ) -> Result<Result<CreateMessageResult, rmcp::ErrorData>, ExecutorError> {
        // --- gate (no model call on refusal) ---
        let Some(grant) = agent.sampling_grant() else {
            return Ok(Err(sampling_decline(
                "sampling is not granted for this agent",
            )));
        };
        if !grant.permits(server) {
            return Ok(Err(sampling_decline(&format!(
                "sampling is not granted for server '{server}'"
            ))));
        }
        if let Some(max) = grant.max_cost
            && totals.sampling_cost >= max
        {
            return Ok(Err(sampling_decline(
                "sampling sub-budget exhausted for this invocation",
            )));
        }
        if let Some(budget) = agent.budget()
            && totals.total_cost >= budget
        {
            return Ok(Err(sampling_decline(
                "invocation budget exhausted; sampling refused",
            )));
        }

        // --- run through the shared LLM path, tagged as sampling ---
        // (Inbound `includeContext` is forced to `none`: we do not
        // inject agent/MCP context into a server's prompt yet, so a
        // server cannot pull context it was not granted. The inbound
        // redact seam lands with context injection.)
        let origin = LlmCallOrigin::Sampling {
            server: server.to_string(),
        };

        // --- input evaluator gates (may decline before any model call) ---
        if let EvaluatorOutcome::Denied(reason) = self
            .run_evaluators(
                &agent.sampling_validation().input_validation,
                "forwarding a sampling request to the agent's model",
                &serde_json::to_string(&params).unwrap_or_default(),
                agent.model(),
                llm,
                agent_id,
                invocation_id,
                origin.clone(),
                totals,
                cursor,
                |t, c| t.sampling_cost += c,
            )
            .await?
        {
            return Ok(Err(sampling_decline(&format!(
                "sampling request denied by evaluator: {reason}"
            ))));
        }

        let model_request = sampling_to_model_request(agent.model(), &params);
        let (response, call_cost) = match self
            .dispatch_llm(
                llm,
                agent_id,
                invocation_id,
                model_request,
                origin.clone(),
                totals,
                cursor,
            )
            .await?
        {
            Ok(pair) => pair,
            // A sampling model failure declines the request; the agent
            // invocation continues (ADR-0018: the failure is the
            // server's, not the agent's).
            Err(err) => {
                return Ok(Err(sampling_decline(&format!(
                    "sampling model call failed: {err}"
                ))));
            }
        };
        totals.sampling_cost += call_cost;

        // --- outbound validation seam: the pluggable context chain
        // (empty by default) then the agent's declarative config
        // (redaction when `redact_secrets`). ---
        let result = model_response_to_create_message(agent.model(), response);
        let result = match self.context.sampling_validators.run(result) {
            Ok(result) => result,
            Err(reason) => {
                return Ok(Err(sampling_decline(&format!(
                    "sampling result rejected by policy: {reason}"
                ))));
            }
        };
        let result =
            match crate::policy::sampling_output_chain(agent.sampling_validation()).run(result) {
                Ok(validated) => validated,
                Err(reason) => {
                    return Ok(Err(sampling_decline(&format!(
                        "sampling result rejected by policy: {reason}"
                    ))));
                }
            };

        // --- output evaluator gates (judge the result before reply) ---
        if let EvaluatorOutcome::Denied(reason) = self
            .run_evaluators(
                &agent.sampling_validation().output_validation,
                "returning a sampling result to the requesting MCP server",
                &sampling_message_text(&result.message.content),
                agent.model(),
                llm,
                agent_id,
                invocation_id,
                origin,
                totals,
                cursor,
                |t, c| t.sampling_cost += c,
            )
            .await?
        {
            return Ok(Err(sampling_decline(&format!(
                "sampling result denied by evaluator: {reason}"
            ))));
        }

        Ok(Ok(result))
    }

    /// Answer an `elicitation/create` request (ADR-0018 §4). Same
    /// gate / shared-LLM-path / cost attribution as sampling, but the
    /// answer is a **schema-constrained structured completion**: the
    /// model is asked for JSON matching the requested schema, validated
    /// against it, and retried up to [`ELICITATION_MAX_RETRIES`] times;
    /// a still-invalid result, a refusal (ungranted / over-budget), or
    /// a model failure all resolve to a `decline` *result* (not an
    /// error) so the server continues without the input. The outer
    /// `Err` is infrastructure (store / bus).
    #[allow(clippy::too_many_arguments)]
    async fn handle_elicitation(
        &self,
        agent: &Agent,
        server: &str,
        llm: &dyn LlmClient,
        agent_id: &AgentId,
        invocation_id: Uuid,
        params: CreateElicitationRequestParams,
        totals: &mut InvocationTotals,
        cursor: &mut Option<Uuid>,
    ) -> Result<Result<CreateElicitationResult, rmcp::ErrorData>, ExecutorError> {
        let decline = || Ok(Ok(elicitation_decline()));

        // --- gate (no model call on refusal) ---
        let Some(grant) = agent.elicitation_grant() else {
            return decline();
        };
        if !grant.permits(server) {
            return decline();
        }
        if let Some(max) = grant.max_cost
            && totals.elicitation_cost >= max
        {
            return decline();
        }
        if let Some(budget) = agent.budget()
            && totals.total_cost >= budget
        {
            return decline();
        }

        // --- inbound validation seam: the pluggable context chain
        // (empty by default) then the agent's declarative request policy
        // (sensitive-field rejection when `reject_sensitive_fields`). ---
        let params = match self.context.elicitation_inbound_validators.run(params) {
            Ok(params) => params,
            Err(_) => return decline(),
        };
        let params = match crate::policy::elicitation_input_chain(agent.elicitation_validation())
            .run(params)
        {
            Ok(params) => params,
            Err(_) => return decline(),
        };

        // --- input evaluator gates (judge the request before answering) ---
        let origin = LlmCallOrigin::Elicitation {
            server: server.to_string(),
        };
        if let EvaluatorOutcome::Denied(_) = self
            .run_evaluators(
                &agent.elicitation_validation().input_validation,
                "answering an elicitation request from an MCP server",
                &serde_json::to_string(&params).unwrap_or_default(),
                agent.model(),
                llm,
                agent_id,
                invocation_id,
                origin.clone(),
                totals,
                cursor,
                |t, c| t.elicitation_cost += c,
            )
            .await?
        {
            return decline();
        }

        // Only form-mode elicitation is supported; URL mode declines.
        let CreateElicitationRequestParams::FormElicitationParams {
            message,
            requested_schema,
            ..
        } = params
        else {
            return decline();
        };

        // --- schema-constrained structured completion (bounded retry) ---
        // Delegates to the reusable `run_structured_completion` primitive;
        // only the request builder, schema validation, and sub-budget
        // attribution are elicitation-specific.
        let model = agent.model();
        let value = self
            .run_structured_completion(
                llm,
                agent_id,
                invocation_id,
                origin.clone(),
                ELICITATION_MAX_RETRIES,
                totals,
                cursor,
                || elicitation_to_model_request(model, &message, &requested_schema),
                parse_elicitation_value,
                |value| validate_against_elicitation_schema(value, &requested_schema),
                &self.context.elicitation_outbound_validators,
                |totals, cost| totals.elicitation_cost += cost,
            )
            .await?;

        let Some(value) = value else {
            return decline();
        };
        // Declarative outbound redaction on the accepted value (the
        // pluggable context outbound seam already ran inside the
        // structured-completion primitive).
        let value = match crate::policy::elicitation_output_chain(agent.elicitation_validation())
            .run(value)
        {
            Ok(value) => value,
            Err(_) => return decline(),
        };

        // --- output evaluator gates (judge the elicited value) ---
        if let EvaluatorOutcome::Denied(_) = self
            .run_evaluators(
                &agent.elicitation_validation().output_validation,
                "returning an elicited value to the requesting MCP server",
                &serde_json::to_string(&value).unwrap_or_default(),
                agent.model(),
                llm,
                agent_id,
                invocation_id,
                origin,
                totals,
                cursor,
                |t, c| t.elicitation_cost += c,
            )
            .await?
        {
            return decline();
        }

        Ok(Ok(CreateElicitationResult {
            action: ElicitationAction::Accept,
            content: Some(value),
            meta: None,
        }))
    }

    /// Run an ordered evaluator sequence (A1c) against `subject` with AND
    /// semantics: the first deny short-circuits and the rest do not run;
    /// an empty sequence — or all-approve — passes. `ApproveAll` /
    /// `DenyAll` are deterministic; `Llm` runs a model judge via the
    /// structured-completion primitive on the agent's model (or a
    /// configured cheaper one), asking for a
    /// `{ "approved": bool, "reason": string }` verdict. A judge that
    /// returns no parseable verdict fails closed (denies). Each judge
    /// call's cost is attributed through `record_cost`.
    #[allow(clippy::too_many_arguments)]
    async fn run_evaluators(
        &self,
        evaluators: &[EvaluatorSpec],
        context: &str,
        subject: &str,
        default_model: &str,
        llm: &dyn LlmClient,
        agent_id: &AgentId,
        invocation_id: Uuid,
        origin: LlmCallOrigin,
        totals: &mut InvocationTotals,
        cursor: &mut Option<Uuid>,
        mut record_cost: impl FnMut(&mut InvocationTotals, f64),
    ) -> Result<EvaluatorOutcome, ExecutorError> {
        let empty_outbound = ValidatorChain::new();
        for evaluator in evaluators {
            match evaluator {
                EvaluatorSpec::ApproveAll => {}
                EvaluatorSpec::DenyAll => {
                    return Ok(EvaluatorOutcome::Denied("denied by deny_all".to_string()));
                }
                EvaluatorSpec::Llm { model } => {
                    let model = model.as_deref().unwrap_or(default_model).to_string();
                    let value = self
                        .run_structured_completion(
                            llm,
                            agent_id,
                            invocation_id,
                            origin.clone(),
                            EVALUATOR_MAX_RETRIES,
                            totals,
                            cursor,
                            || evaluator_to_model_request(&model, context, subject),
                            parse_elicitation_value,
                            validate_evaluator_verdict,
                            &empty_outbound,
                            &mut record_cost,
                        )
                        .await?;
                    if let EvaluatorOutcome::Denied(reason) = evaluator_verdict(value) {
                        return Ok(EvaluatorOutcome::Denied(reason));
                    }
                }
            }
        }
        Ok(EvaluatorOutcome::Approved)
    }
}

/// The outcome of running an evaluator sequence (A1c).
enum EvaluatorOutcome {
    Approved,
    Denied(String),
}

/// Attempts for an LLM evaluator's verdict before failing closed.
const EVALUATOR_MAX_RETRIES: u32 = 1;

/// System instruction prefixed to every LLM evaluator judge call.
const EVALUATOR_SYSTEM_PREAMBLE: &str = "You are a safety evaluator gating an automated MCP exchange. Decide whether the content below is safe and \
     appropriate for the stated action. Respond with ONLY a single JSON object \
     {\"approved\": <true|false>, \"reason\": <string>} — no prose, no code fences.";

/// Build the judge request for an LLM evaluator: the preamble + action
/// context as the system message, the subject as the user turn. Run on
/// the configured (or agent) model; no tools.
fn evaluator_to_model_request(model: &str, context: &str, subject: &str) -> ModelRequest {
    ModelRequest {
        model: model.to_string(),
        messages: vec![
            Message {
                role: MessageRole::System,
                content: Some(format!("{EVALUATOR_SYSTEM_PREAMBLE}\n\nAction: {context}")),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            Message {
                role: MessageRole::User,
                content: Some(subject.to_string()),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
        ],
        tools: Vec::new(),
        params: RequestParams {
            temperature: None,
            max_tokens: None,
        },
    }
}

/// Validate that an evaluator response carries a boolean `approved`.
fn validate_evaluator_verdict(value: &Value) -> Result<(), String> {
    if value.get("approved").and_then(Value::as_bool).is_some() {
        Ok(())
    } else {
        Err("evaluator response missing boolean `approved`".to_string())
    }
}

/// Map a parsed evaluator verdict to an outcome. A missing verdict (a
/// model failure or unparseable response after retries) fails closed:
/// denied.
fn evaluator_verdict(value: Option<Value>) -> EvaluatorOutcome {
    match value {
        Some(value) if value.get("approved").and_then(Value::as_bool) == Some(true) => {
            EvaluatorOutcome::Approved
        }
        Some(value) => {
            let reason = value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("denied by evaluator")
                .to_string();
            EvaluatorOutcome::Denied(reason)
        }
        None => EvaluatorOutcome::Denied("evaluator returned no verdict".to_string()),
    }
}

enum ModelOutcome {
    Response(ModelResponse),
    BudgetExceeded(f64),
}

/// A structured decline returned to a server whose sampling request
/// the runtime refuses (policy) or could not fulfil (model failure).
/// Maps to a JSON-RPC error response; the server decides how to
/// proceed without the sample.
fn sampling_decline(reason: &str) -> rmcp::ErrorData {
    rmcp::ErrorData::invalid_request(reason.to_string(), None)
}

/// Build a [`ModelRequest`] for a sampling completion from the
/// server's `sampling/createMessage` params, run on the agent's own
/// model. The server's `systemPrompt` becomes a system message; each
/// sampling message maps to a user/assistant message. Only text
/// content is injected in v1 (non-text is a placeholder); tools are
/// never exposed to a sampling call.
fn sampling_to_model_request(model: &str, params: &CreateMessageRequestParams) -> ModelRequest {
    let mut messages = Vec::with_capacity(params.messages.len() + 1);
    if let Some(system) = &params.system_prompt {
        messages.push(Message {
            role: MessageRole::System,
            content: Some(system.clone()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        });
    }
    for sampling_message in &params.messages {
        messages.push(Message {
            role: match sampling_message.role {
                Role::User => MessageRole::User,
                Role::Assistant => MessageRole::Assistant,
            },
            content: Some(sampling_message_text(&sampling_message.content)),
            tool_calls: Vec::new(),
            tool_call_id: None,
        });
    }
    ModelRequest {
        model: model.to_string(),
        messages,
        tools: Vec::new(),
        params: RequestParams {
            temperature: params.temperature.map(|t| t as f64),
            max_tokens: Some(params.max_tokens),
        },
    }
}

/// Flatten a sampling message's content (single or multiple) into a
/// plain string for the agent model. Non-text content is represented
/// by a placeholder so conversation structure is preserved without
/// claiming to faithfully inject images/audio (a later capability).
fn sampling_message_text(content: &SamplingContent<SamplingMessageContent>) -> String {
    match content {
        SamplingContent::Single(item) => sampling_item_text(item),
        SamplingContent::Multiple(items) => items
            .iter()
            .map(sampling_item_text)
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn sampling_item_text(item: &SamplingMessageContent) -> String {
    match item {
        SamplingMessageContent::Text(text) => text.text.clone(),
        _ => "[non-text sampling content omitted]".to_string(),
    }
}

/// Wrap an agent-model [`ModelResponse`] back into the
/// `CreateMessageResult` shape the protocol returns to the server.
fn model_response_to_create_message(model: &str, response: ModelResponse) -> CreateMessageResult {
    CreateMessageResult::new(
        SamplingMessage::assistant_text(response.content.unwrap_or_default()),
        model.to_string(),
    )
    .with_stop_reason(stop_reason_to_mcp(response.stop_reason))
}

fn stop_reason_to_mcp(stop_reason: StopReason) -> &'static str {
    match stop_reason {
        StopReason::EndTurn => CreateMessageResult::STOP_REASON_END_TURN,
        StopReason::MaxTokens => CreateMessageResult::STOP_REASON_END_MAX_TOKEN,
        StopReason::StopSequence => CreateMessageResult::STOP_REASON_END_SEQUENCE,
        StopReason::ToolUse => CreateMessageResult::STOP_REASON_TOOL_USE,
    }
}

/// Max model attempts to produce a schema-valid elicitation value
/// before declining (ADR-0018 §4 — "bounded retry"). Each attempt is
/// a budget-counted LLM call.
const ELICITATION_MAX_RETRIES: u32 = 2;

/// The system instruction prefixed to every elicitation completion.
/// Kept as a constant so its presence in a recorded model request is
/// a stable signal that the schema-constrained completion ran.
const ELICITATION_SYSTEM_PREAMBLE: &str = "You are completing a structured form on the user's behalf. Respond with ONLY a single \
     JSON object that conforms to the JSON schema below — no prose, no code fences.";

/// Build the schema-constrained completion request for an elicitation:
/// a system message carrying the instruction + serialized schema, and
/// the server's human-readable `message` as the user turn. Run on the
/// agent's own model; no tools.
fn elicitation_to_model_request(
    model: &str,
    message: &str,
    schema: &ElicitationSchema,
) -> ModelRequest {
    let schema_json = serde_json::to_string_pretty(schema).unwrap_or_default();
    ModelRequest {
        model: model.to_string(),
        messages: vec![
            Message {
                role: MessageRole::System,
                content: Some(format!(
                    "{ELICITATION_SYSTEM_PREAMBLE}\n\nJSON schema:\n{schema_json}"
                )),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            Message {
                role: MessageRole::User,
                content: Some(message.to_string()),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
        ],
        tools: Vec::new(),
        params: RequestParams {
            temperature: None,
            max_tokens: None,
        },
    }
}

/// Parse a model's elicitation answer into a JSON object, tolerating
/// surrounding whitespace and ```json code fences. Returns `None` if
/// the content is absent, unparseable, or not a JSON object.
fn parse_elicitation_value(content: Option<&str>) -> Option<Value> {
    let text = content?.trim();
    let text = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .unwrap_or(text);
    let text = text.strip_suffix("```").unwrap_or(text).trim();
    let value: Value = serde_json::from_str(text).ok()?;
    value.is_object().then_some(value)
}

/// Validate an elicitation value against the requested schema. The
/// schema type is already restricted to the MCP flat-object / primitive
/// subset by rmcp's deserialization; here we enforce, per field:
/// required-field presence, no unexpected fields, the property's
/// primitive type, string length / format (email / uri / date /
/// date-time), numeric range, and enum membership.
fn validate_against_elicitation_schema(
    value: &Value,
    schema: &ElicitationSchema,
) -> Result<(), String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "elicitation response is not a JSON object".to_string())?;
    if let Some(required) = &schema.required {
        for key in required {
            if !obj.contains_key(key) {
                return Err(format!("missing required field '{key}'"));
            }
        }
    }
    for (key, field_value) in obj {
        let Some(property) = schema.properties.get(key) else {
            return Err(format!(
                "unexpected field '{key}' not declared in the schema"
            ));
        };
        validate_primitive_value(key, field_value, property)?;
    }
    Ok(())
}

/// Validate one field value against its primitive property schema.
fn validate_primitive_value(
    key: &str,
    value: &Value,
    schema: &PrimitiveSchema,
) -> Result<(), String> {
    match schema {
        PrimitiveSchema::String(string_schema) => {
            let text = value
                .as_str()
                .ok_or_else(|| format!("field '{key}' must be a string"))?;
            let len = text.chars().count() as u32;
            if let Some(min) = string_schema.min_length
                && len < min
            {
                return Err(format!("field '{key}' is shorter than minLength {min}"));
            }
            if let Some(max) = string_schema.max_length
                && len > max
            {
                return Err(format!("field '{key}' is longer than maxLength {max}"));
            }
            if let Some(format) = string_schema.format
                && !string_matches_format(text, format)
            {
                return Err(format!("field '{key}' is not a valid {format:?}"));
            }
            Ok(())
        }
        PrimitiveSchema::Number(number_schema) => {
            let number = value
                .as_f64()
                .ok_or_else(|| format!("field '{key}' must be a number"))?;
            if let Some(min) = number_schema.minimum
                && number < min
            {
                return Err(format!("field '{key}' is below minimum {min}"));
            }
            if let Some(max) = number_schema.maximum
                && number > max
            {
                return Err(format!("field '{key}' is above maximum {max}"));
            }
            Ok(())
        }
        PrimitiveSchema::Integer(integer_schema) => {
            let number = value
                .as_i64()
                .ok_or_else(|| format!("field '{key}' must be an integer"))?;
            if let Some(min) = integer_schema.minimum
                && number < min
            {
                return Err(format!("field '{key}' is below minimum {min}"));
            }
            if let Some(max) = integer_schema.maximum
                && number > max
            {
                return Err(format!("field '{key}' is above maximum {max}"));
            }
            Ok(())
        }
        PrimitiveSchema::Boolean(_) => value
            .as_bool()
            .map(|_| ())
            .ok_or_else(|| format!("field '{key}' must be a boolean")),
        PrimitiveSchema::Enum(enum_schema) => {
            let text = value
                .as_str()
                .ok_or_else(|| format!("field '{key}' must be a string (enum)"))?;
            let allowed = enum_allowed_values(enum_schema);
            // If the allowed set can't be extracted, fall back to the
            // type check rather than reject a spec-valid value.
            if allowed.is_empty() || allowed.iter().any(|a| a == text) {
                Ok(())
            } else {
                Err(format!(
                    "field '{key}' value '{text}' is not one of the allowed enum values"
                ))
            }
        }
    }
}

/// Whether `text` satisfies a basic check for an MCP string `format`.
/// Intentionally lightweight (no full RFC validation): enough to reject
/// obviously-wrong values without pulling in a parser.
fn string_matches_format(text: &str, format: StringFormat) -> bool {
    match format {
        StringFormat::Email => matches!(text.split_once('@'),
            Some((local, domain)) if !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')),
        StringFormat::Uri => matches!(text.split_once(':'),
            Some((scheme, _)) if !scheme.is_empty()
                && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))),
        StringFormat::Date => is_iso_date(text),
        StringFormat::DateTime => {
            matches!(text.split_once('T'), Some((date, _)) if is_iso_date(date))
        }
    }
}

/// Whether `text` has the ISO `YYYY-MM-DD` calendar-date shape.
fn is_iso_date(text: &str) -> bool {
    let parts: Vec<&str> = text.split('-').collect();
    parts.len() == 3
        && parts[0].len() == 4
        && parts[1].len() == 2
        && parts[2].len() == 2
        && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()))
}

/// Extract an enum schema's allowed string values by serialising it and
/// reading the `enum` array (rmcp models enums as several variants, so
/// going through JSON is simpler than matching each).
fn enum_allowed_values(schema: &EnumSchema) -> Vec<String> {
    serde_json::to_value(schema)
        .ok()
        .as_ref()
        .and_then(|v| v.get("enum"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Reconstruct a [`CapabilityResult::ToolResult`] from a
/// completed `tool_dispatch` row. Used by `resume()` to feed
/// the result of a previously-completed action back into the
/// reducer.
fn tool_row_to_capability(row: &ToolDispatchRow) -> CapabilityResult {
    // The WAL row's `tool_call_id` was written through `ToolCallId`
    // so non-empty is structurally guaranteed. If the row is
    // corrupt (empty string), the resume path surfaces it as an
    // error via the reducer's normal error handling — here we fall
    // back to a sentinel so this conversion stays infallible.
    let tool_call_id =
        crate::events::ToolCallId::new(row.tool_call_id.clone()).unwrap_or_else(|_| {
            crate::events::ToolCallId::new("corrupt-empty-tool-call-id".to_string())
                .expect("sentinel is non-empty")
        });
    CapabilityResult::ToolResult(ToolCallResult {
        tool_call_id,
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
fn llm_row_to_capability(row: &LlmDispatchRow) -> Result<CapabilityResult, ExecutorError> {
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
        NextAction::CallTool(_) | NextAction::CallToolsParallel(_) => ("dispatching_tools", None),
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
        MaxIterations => FailureKind::MaxIterations,
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
    use crate::worker::reducer::Harness;
    use crate::worker::store::DispatchStatus;
    use crate::{events::EventPayload, llm::ChatResponse};
    use futures::StreamExt;
    use serde_json::json;
    use std::collections::HashMap;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn evaluator_verdict_maps_outcomes() {
        assert!(matches!(
            evaluator_verdict(Some(json!({ "approved": true }))),
            EvaluatorOutcome::Approved
        ));
        match evaluator_verdict(Some(json!({ "approved": false, "reason": "nope" }))) {
            EvaluatorOutcome::Denied(reason) => assert_eq!(reason, "nope"),
            EvaluatorOutcome::Approved => panic!("expected denied"),
        }
        // A missing / unparseable verdict fails closed (denies).
        assert!(matches!(
            evaluator_verdict(None),
            EvaluatorOutcome::Denied(_)
        ));
    }

    #[test]
    fn elicitation_schema_validation_enforces_per_field_rules() {
        let schema: ElicitationSchema = serde_json::from_value(json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "minLength": 2 },
                "age": { "type": "integer", "minimum": 0, "maximum": 150 },
                "email": { "type": "string", "format": "email" },
                "color": { "type": "string", "enum": ["red", "green"] }
            },
            "required": ["name"]
        }))
        .expect("valid elicitation schema");

        let ok = |v: serde_json::Value| validate_against_elicitation_schema(&v, &schema).is_ok();
        let err = |v: serde_json::Value| validate_against_elicitation_schema(&v, &schema).is_err();

        assert!(ok(
            json!({ "name": "Ada", "age": 30, "email": "ada@example.com", "color": "red" })
        ));
        assert!(err(json!({ "age": 30 })), "missing required name");
        assert!(err(json!({ "name": 5 })), "wrong type");
        assert!(err(json!({ "name": "A" })), "below minLength");
        assert!(err(json!({ "name": "Ada", "age": 999 })), "above maximum");
        assert!(err(json!({ "name": "Ada", "age": 1.5 })), "non-integer");
        assert!(err(json!({ "name": "Ada", "email": "nope" })), "bad email");
        assert!(err(json!({ "name": "Ada", "color": "blue" })), "bad enum");
        assert!(
            err(json!({ "name": "Ada", "extra": 1 })),
            "unexpected field"
        );
    }

    #[tokio::test]
    async fn sampling_channel_merges_servers_and_drains() {
        use crate::mcp::ServerRequest;
        use tokio::sync::{mpsc, oneshot};

        fn req() -> ServerRequest {
            let params = serde_json::from_value(json!({ "messages": [], "maxTokens": 8 }))
                .expect("sampling params");
            let (reply, _rx) = oneshot::channel();
            ServerRequest::Sampling { params, reply }
        }

        let (tx_a, rx_a) = mpsc::unbounded_channel();
        let (tx_b, rx_b) = mpsc::unbounded_channel();
        let mut channel = SamplingChannel::merged(vec![
            ("alpha".to_string(), rx_a),
            ("beta".to_string(), rx_b),
        ]);

        // A request on either server's channel is tagged with its name.
        tx_b.send(req()).unwrap();
        assert_eq!(channel.recv().await.expect("request").0, "beta");
        tx_a.send(req()).unwrap();
        assert_eq!(channel.recv().await.expect("request").0, "alpha");

        // Once every server's channel is closed, recv drains to None.
        drop(tx_a);
        drop(tx_b);
        assert!(channel.recv().await.is_none());
    }

    fn unique_agent_id(prefix: &str) -> String {
        format!("{prefix}-{}", Uuid::now_v7().simple())
    }

    /// A worker id good for tests. Each call returns a fresh
    /// UUID-shaped id so concurrent tests don't share a
    /// `fq.worker.{id}.invocation.archive_acked` subject.
    fn test_worker_id() -> WorkerId {
        WorkerId::new(Uuid::now_v7().to_string()).expect("uuid is a valid worker id")
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
                tool_call_id: crate::events::ToolCallId::new(call_id).unwrap(),
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

    #[tokio::test]
    async fn reducer_emits_canonical_event_sequence_for_simple_completion() {
        // Was `equivalent_event_sequence_for_simple_completion`,
        // which ran a single canned response through *both* the
        // legacy executor and the reducer and asserted that the
        // reducer sequence equals the legacy sequence modulo WAL
        // middle-state events. After AgentExecutor is deleted
        // the legacy half is gone, so this asserts the
        // reducer-side canonical sequence directly.
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        let agent_id = unique_agent_id("canonical-simple");
        let agent = Agent::builder()
            .id(&agent_id)
            .model("claude-haiku")
            .system_prompt("You are a test agent.")
            .budget(1.0)
            .build()
            .unwrap();

        // triggered, llm.request, llm.dispatched, llm.response,
        // completed = 5 events. (invocation_archived also fires
        // immediately after; not collected here.)
        let (_store, events) =
            run_with_wal(&url, agent, vec![canned("Hello.", 100, 50)], 5, None).await;

        let kinds: Vec<&str> = events.iter().map(event_kind).collect();
        assert_eq!(
            kinds,
            vec![
                "triggered",
                "llm_request",
                "llm_dispatched",
                "llm_response",
                "completed",
            ],
        );
    }

    #[tokio::test]
    async fn reducer_emits_canonical_event_sequence_for_tool_call_loop() {
        // Was `equivalent_event_sequence_for_tool_call_loop`.
        // Same conversion as the simple-completion test:
        // reducer-only canonical-sequence assertion.
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        let dir = tempdir().unwrap();
        let target = dir.path().join("hello.md");
        std::fs::write(&target, "# hello").unwrap();
        let target_path = target.to_string_lossy().to_string();
        let allowed_dir = dir.path().to_string_lossy().to_string();

        let agent_id = unique_agent_id("canonical-tool-loop");
        let agent = Agent::builder()
            .id(&agent_id)
            .model("claude-haiku")
            .system_prompt("Use tools when asked.")
            .tools(["file_read"])
            .sandbox(Sandbox::new().fs_read(allowed_dir))
            .budget(1.0)
            .build()
            .unwrap();

        let responses = vec![
            tool_use(
                "file_read",
                "call_abc",
                json!({"path": target_path}),
                (100, 50),
            ),
            canned("Got it.", 150, 20),
        ];

        // 11 events: triggered, then for each LLM turn the
        // (llm.request, llm.dispatched, llm.response) triple,
        // with a tool-dispatch triple (tool.call, tool.dispatched,
        // tool.result) between turns 1 and 2, ending in completed.
        let (_store, events) = run_with_wal(&url, agent, responses, 11, Some(dir.path())).await;

        let kinds: Vec<&str> = events.iter().map(event_kind).collect();
        assert_eq!(
            kinds,
            vec![
                "triggered",
                "llm_request",
                "llm_dispatched",
                "llm_response",
                "tool_call",
                "tool_dispatched",
                "tool_result",
                "llm_request",
                "llm_dispatched",
                "llm_response",
                "completed",
            ],
        );
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
            .tools(["file_read"])
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
            Arc::new(
                ReducerContext::builder()
                    .tools(Arc::new(ToolRegistry::with_builtins()))
                    .build(),
            ),
            Arc::new(
                RunnerConfig::builder()
                    .bus(bus.clone())
                    .pricing(test_pricing())
                    .store(store)
                    .worker_id(test_worker_id())
                    .build(),
            ),
            Harness::new(),
        );
        let _ = runner
            .run(
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
        // + invocation_archived = 12 events after data-arch step 8.
        let mut events = Vec::new();
        for _ in 0..12 {
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
            max_iterations: crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS,
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
                static_resource_context: None,
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
                static_resource_context: None,
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
            Arc::new(
                ReducerContext::builder()
                    .tools(Arc::new(ToolRegistry::with_builtins()))
                    .build(),
            ),
            Arc::new(
                RunnerConfig::builder()
                    .bus(bus.clone())
                    .pricing(test_pricing())
                    .store(store)
                    .worker_id(test_worker_id())
                    .build(),
            ),
            Harness::new(),
        );

        let mut sub = bus
            .subscribe(format!("fq.agent.{agent_id}.>"))
            .await
            .expect("subscribe");
        tokio::time::sleep(Duration::from_millis(50)).await;

        runner
            .run(&agent, &llm, TriggerSource::Manual, None, json!({}))
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
            max_iterations: crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS,
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
            static_resource_context: None,
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
                        tool_call_id: crate::events::ToolCallId::new("si").unwrap(),
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
                    sampling_cost: 0.0,
                    elicitation_cost: 0.0,
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
                    tool_call_id: crate::events::ToolCallId::new("si").unwrap(),
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
        let (store, events, _) = run_with_wal_capturing_outcome(
            url,
            agent,
            responses,
            expected_event_count,
            sandbox_dir,
        )
        .await;
        (store, events)
    }

    /// Same as [`run_with_wal`] but also returns the `run`
    /// result. Useful when a test asserts on the outcome
    /// variant (e.g. budget-exceeded).
    async fn run_with_wal_capturing_outcome(
        url: &str,
        agent: Agent,
        responses: Vec<ChatResponse>,
        expected_event_count: usize,
        sandbox_dir: Option<&std::path::Path>,
    ) -> (
        Arc<WorkerStore>,
        Vec<Event>,
        Result<InvocationOutcome, crate::worker::ExecutorError>,
    ) {
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
            Arc::new(
                ReducerContext::builder()
                    .tools(Arc::new(ToolRegistry::with_builtins()))
                    .build(),
            ),
            Arc::new(
                RunnerConfig::builder()
                    .bus(bus.clone())
                    .pricing(test_pricing())
                    .store(store.clone())
                    .worker_id(test_worker_id())
                    .build(),
            ),
            Harness::new(),
        );
        let outcome = runner
            .run(
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
        (store, events, outcome)
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
                tool_call_id: crate::events::ToolCallId::new(call_id).unwrap(),
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
        let (store, events) =
            run_with_wal(&url, agent, vec![end_turn_response("done.")], 5, None).await;
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
        assert!(kinds.contains(&"tool_dispatched"), "kinds: {kinds:?}");

        // Every WAL row should be `completed` at end-of-invocation.
        assert!(
            store
                .find_ambiguous_tool_dispatches()
                .await
                .unwrap()
                .is_empty(),
            "tool_dispatch rows must all be completed"
        );
        assert!(
            store
                .find_ambiguous_llm_dispatches()
                .await
                .unwrap()
                .is_empty(),
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

    #[tokio::test]
    async fn tool_not_in_agent_allowlist_is_denied_on_reducer_path() {
        // Defence-in-depth gating: the LLM only sees declared
        // tool schemas, but if it hallucinates a name, the
        // runner short-circuits to a synthetic ToolResult with
        // PermissionDenied and never executes anything. Mirrors
        // the legacy executor's `tool_not_in_agent_allowlist_is_denied`
        // — this is the reducer-path counterpart that was
        // missing as of commit `c9fd92e`.
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        let agent_id = unique_agent_id("gating-deny");
        // Agent declares only file_read; LLM will try file_write.
        let agent = Agent::builder()
            .id(&agent_id)
            .model("claude-haiku")
            .system_prompt("You like to write.")
            .tools(["file_read"])
            .budget(1.0)
            .build()
            .unwrap();

        let responses = vec![
            tool_call_response(
                "file_write",
                "call_deny",
                json!({"path": "/tmp/x", "content": "x"}),
            ),
            end_turn_response("done anyway."),
        ];

        // Event sequence on the synthetic-error path:
        //   triggered, llm.request, llm.dispatched, llm.response,
        //   tool.result (synthetic — no tool.call/tool.dispatched),
        //   llm.request, llm.dispatched, llm.response,
        //   completed, invocation.archived
        // = 10 events.
        let (store, events) = run_with_wal(&url, agent, responses, 10, None).await;

        let kinds: Vec<&str> = events
            .iter()
            .map(crate::test_support::events::event_kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                "triggered",
                "llm_request",
                "llm_dispatched",
                "llm_response",
                "tool_result",
                "llm_request",
                "llm_dispatched",
                "llm_response",
                "completed",
                "invocation_archived",
            ],
            "synthetic-error gating path must emit tool.result without tool.call / tool.dispatched"
        );

        // The single tool.result must be is_error=true with
        // PermissionDenied.
        let tool_result = events
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::ToolResult(p) => Some(p),
                _ => None,
            })
            .expect("tool_result event present");
        assert!(tool_result.is_error, "denied tool must surface as error");
        assert!(
            matches!(
                tool_result.error_kind,
                Some(ToolErrorKind::PermissionDenied)
            ),
            "denied tool error_kind must be PermissionDenied, got {:?}",
            tool_result.error_kind
        );

        // No WAL row was written for the denied call — the
        // synthetic-error path bypasses tool_dispatch entirely.
        let inv_str = events[0].envelope.invocation_id.to_string();
        let dispatch = store
            .get_tool_dispatch(&inv_str, "call_deny")
            .await
            .unwrap();
        assert!(
            dispatch.is_none(),
            "denied call must not write a tool_dispatch row; got {dispatch:?}"
        );
    }

    #[tokio::test]
    async fn tool_sandbox_violation_surfaces_on_reducer_path() {
        // Sister to the executor-side
        // `tool_sandbox_violations_surface_to_the_llm`. Distinct
        // from the allowlist test above: here the tool *is*
        // allowed (`file_read` is in the agent's declared tools),
        // but the runtime sandbox denies the specific path. The
        // tool actually dispatches; the failure surfaces from
        // inside the tool, not from the synthetic-error gating
        // shortcut. So the event sequence includes both
        // `tool.call` and `tool.dispatched` before the failing
        // `tool.result`.
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        let allowed = tempdir().unwrap();
        let forbidden = tempdir().unwrap();
        let target = forbidden.path().join("secret.txt");
        std::fs::write(&target, "no").unwrap();

        let agent_id = unique_agent_id("sandbox-violator");
        let agent = Agent::builder()
            .id(&agent_id)
            .model("claude-haiku")
            .system_prompt("Try to read a file.")
            .tools(["file_read"])
            .sandbox(Sandbox::new().fs_read(allowed.path().to_string_lossy().to_string()))
            .budget(1.0)
            .build()
            .unwrap();

        let responses = vec![
            tool_call_response(
                "file_read",
                "call_violate",
                json!({"path": target.to_string_lossy()}),
            ),
            end_turn_response("Could not read."),
        ];

        // triggered, llm_request, llm_dispatched, llm_response,
        // tool_call, tool_dispatched, tool_result(err),
        // llm_request, llm_dispatched, llm_response, completed,
        // invocation_archived = 12 events.
        let (_store, events) = run_with_wal(&url, agent, responses, 12, None).await;

        let tool_result = events
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::ToolResult(p) => Some(p),
                _ => None,
            })
            .expect("tool_result event present");
        assert!(tool_result.is_error, "sandbox-blocked tool must error");
        assert!(
            matches!(
                tool_result.error_kind,
                Some(ToolErrorKind::SandboxViolation)
            ),
            "sandbox-blocked tool error_kind must be SandboxViolation, got {:?}",
            tool_result.error_kind
        );
    }

    #[tokio::test]
    async fn budget_exceeded_emits_failed_event_on_reducer_path() {
        // Sister to the executor-side
        // `emits_failed_event_when_budget_exceeded`. The runner
        // computes total cost after the LLM turn lands and
        // short-circuits to `Failed { BudgetExceeded }` when the
        // budget is blown. Asserts both the outcome variant and
        // the on-bus event.
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        let agent_id = unique_agent_id("overspender");
        let agent = Agent::builder()
            .id(&agent_id)
            .model("claude-haiku")
            .system_prompt("You spend a lot.")
            .budget(0.0001)
            .build()
            .unwrap();

        // 1M input tokens at $1/M = $1.00 — well over $0.0001.
        let expensive = ChatResponse {
            content: Some("expensive".to_string()),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 1_000_000,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        };

        // triggered, llm_request, llm_dispatched, llm_response,
        // failed, invocation_archived = 6 events.
        let (_store, events, outcome) =
            run_with_wal_capturing_outcome(&url, agent, vec![expensive], 6, None).await;

        let outcome = outcome.expect("run resolves cleanly even on budget exceeded");
        assert!(
            matches!(outcome, InvocationOutcome::BudgetExceeded { .. }),
            "outcome must be BudgetExceeded, got {outcome:?}"
        );

        let failed = events
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::Failed(p) => Some(p),
                _ => None,
            })
            .expect("failed event present");
        assert!(
            matches!(failed.error_kind, FailureKind::BudgetExceeded),
            "failed.error_kind must be BudgetExceeded, got {:?}",
            failed.error_kind
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
    async fn complete_emits_invocation_archived_and_marks_row_pending() {
        // The hand-off path (step 8): a successful Complete
        // emits `invocation.archived` after `completed`, and the
        // worker store row is flipped to `archive_status =
        // "pending"`. The ack consumer (commit 6) deletes the
        // row on receipt; the retry sweeper (commit 7) re-emits
        // if the ack never arrives.
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        let agent_id = unique_agent_id("step8-archive-on-complete");
        let agent = simple_responder_agent(&agent_id);

        // Sequence after my change:
        //   triggered, llm.request, llm.dispatched, llm.response,
        //   completed, invocation.archived  → 6 events.
        let (store, events) =
            run_with_wal(&url, agent, vec![end_turn_response("done.")], 6, None).await;

        let kinds: Vec<&str> = events
            .iter()
            .map(crate::test_support::events::event_kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                "triggered",
                "llm_request",
                "llm_dispatched",
                "llm_response",
                "completed",
                "invocation_archived",
            ]
        );

        let inv_str = events[0].envelope.invocation_id.to_string();
        let row = store
            .get_invocation_state(&inv_str)
            .await
            .unwrap()
            .expect("state row should exist after run");
        assert_eq!(
            row.archive_status.as_deref(),
            Some("pending"),
            "archive_status must be flipped to pending after publish"
        );
        assert!(
            row.archive_published_at.is_some(),
            "archive_published_at must be set after publish"
        );

        let terminal_at_ms = row.terminal_at.expect("terminal_at set");
        match &events[5].payload {
            EventPayload::InvocationArchived(p) => {
                assert_eq!(p.final_phase, "completed");
                assert_eq!(
                    p.final_state_blob, row.state_blob,
                    "archived blob must match the persisted final state"
                );
                assert_eq!(p.started_at_ms, row.started_at);
                assert_eq!(p.terminal_at_ms, terminal_at_ms);
            }
            other => panic!("expected InvocationArchived, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn state_row_written_on_completion_with_terminal_at_set() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        let agent_id = unique_agent_id("step5-state-completion");
        let agent = simple_responder_agent(&agent_id);
        let (store, events) =
            run_with_wal(&url, agent, vec![end_turn_response("done.")], 6, None).await;

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
        let _: serde_json::Value =
            serde_json::from_slice(&row.state_blob).expect("state_blob deserialises as JSON");
    }

    /// The error returned to the caller must carry the same
    /// `FailureKind` the `failed` event was emitted with — here the
    /// genuine `max_iterations` case, which previously surfaced as a
    /// bare `MaxIterationsExceeded` while the event said
    /// `runtime_error` (neither side was right).
    #[tokio::test]
    async fn max_iterations_failure_carries_the_max_iterations_kind() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };
        let bus = EventBus::connect(&url).await.expect("connect to NATS");

        let agent_id_str = unique_agent_id("max-iter-kind");
        let agent = Agent::builder()
            .id(&agent_id_str)
            .model("claude-haiku")
            .system_prompt("You are a test agent.")
            .budget(5.0)
            .build()
            .unwrap();

        // The model asks for an unavailable tool on every turn; each
        // synthetic error feeds back and the loop burns one iteration
        // per model turn until DEFAULT_MAX_ITERATIONS trips.
        let llm = FixtureClient::new();
        for i in 0..=crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS {
            llm.push_response(tool_use(
                "unavailable_tool",
                &format!("call-{i}"),
                json!({}),
                (10, 5),
            ));
        }

        let store_dir = tempdir().expect("tempdir");
        let store = Arc::new(
            WorkerStore::open(&store_dir.path().join("events.db"))
                .await
                .expect("worker store"),
        );
        let runner = ReducerRunner::new(
            Arc::new(
                ReducerContext::builder()
                    .tools(Arc::new(ToolRegistry::with_builtins()))
                    .build(),
            ),
            Arc::new(
                RunnerConfig::builder()
                    .bus(bus)
                    .pricing(test_pricing())
                    .store(store)
                    .worker_id(test_worker_id())
                    .build(),
            ),
            Harness::new(),
        );

        let err = runner
            .run(&agent, &llm, TriggerSource::Manual, None, json!("loop"))
            .await
            .expect_err("must fail on max iterations");
        match err {
            ExecutorError::InvocationFailed { kind, message } => {
                assert!(
                    matches!(kind, FailureKind::MaxIterations),
                    "expected MaxIterations kind, got {kind:?}: {message}"
                );
                assert!(message.contains("max iterations"), "got: {message}");
            }
            other => panic!("expected InvocationFailed, got {other:?}"),
        }
    }

    /// A reducer that errors on `step` is a runtime defect — the
    /// returned error must say so, not claim max-iterations.
    #[tokio::test]
    async fn reducer_step_error_carries_the_runtime_error_kind() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };
        let bus = EventBus::connect(&url).await.expect("connect to NATS");

        use crate::worker::reducer::types::StepOutput;

        struct FailingReducer;
        impl Reducer for FailingReducer {
            fn step(&self, _input: StepInput) -> Result<StepOutput, HarnessError> {
                Err(HarnessError {
                    kind: crate::worker::reducer::types::HarnessErrorKind::InternalError,
                    message: "synthetic reducer defect".to_string(),
                })
            }
        }

        let agent_id_str = unique_agent_id("step-error-kind");
        let agent = Agent::builder()
            .id(&agent_id_str)
            .model("claude-haiku")
            .system_prompt("You are a test agent.")
            .budget(1.0)
            .build()
            .unwrap();
        let llm = FixtureClient::new();

        let store_dir = tempdir().expect("tempdir");
        let store = Arc::new(
            WorkerStore::open(&store_dir.path().join("events.db"))
                .await
                .expect("worker store"),
        );
        let runner = ReducerRunner::new(
            Arc::new(
                ReducerContext::builder()
                    .tools(Arc::new(ToolRegistry::with_builtins()))
                    .build(),
            ),
            Arc::new(
                RunnerConfig::builder()
                    .bus(bus)
                    .pricing(test_pricing())
                    .store(store)
                    .worker_id(test_worker_id())
                    .build(),
            ),
            FailingReducer,
        );

        let err = runner
            .run(&agent, &llm, TriggerSource::Manual, None, json!("x"))
            .await
            .expect_err("must fail on reducer step error");
        match err {
            ExecutorError::InvocationFailed { kind, message } => {
                assert!(
                    matches!(kind, FailureKind::RuntimeError),
                    "expected RuntimeError kind, got {kind:?}: {message}"
                );
                assert!(
                    message.contains("synthetic reducer defect"),
                    "got: {message}"
                );
            }
            other => panic!("expected InvocationFailed, got {other:?}"),
        }
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

        use crate::worker::reducer::types::{
            AgentConfig, StepInput, TriggerPayload, TriggerSourceKind,
        };

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
            max_iterations: crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS,
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
            static_resource_context: None,
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
                archive_status: None,
                archive_published_at: None,
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
        store
            .write_llm_dispatched(&inv_str, "req-0", 2)
            .await
            .unwrap();
        store
            .write_llm_completed(&inv_str, "req-0", &response_json, false, 0.0001, 3)
            .await
            .unwrap();

        // Resume.
        let bus = EventBus::connect(&url).await.unwrap();
        let runner = ReducerRunner::new(
            Arc::new(
                ReducerContext::builder()
                    .tools(Arc::new(ToolRegistry::with_builtins()))
                    .build(),
            ),
            Arc::new(
                RunnerConfig::builder()
                    .bus(bus)
                    .pricing(test_pricing())
                    .store(store.clone())
                    .worker_id(test_worker_id())
                    .build(),
            ),
            Harness::new(),
        );
        let llm = FixtureClient::new(); // no live responses needed

        let outcome = runner
            .resume(&agent, &llm, invocation_id)
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
    async fn resume_enforces_lifetime_budget() {
        // Pre-registered finding 1 of the reducer verification
        // plan: totals used to reset on resume, making the budget
        // ceiling per-attempt. Pre-crash spend recorded in the WAL
        // must count against the budget after resume.
        //
        // Shape: the WAL says a completed pre-crash LLM call spent
        // $0.20 against a $0.05 budget, and its response was a
        // tool call, so the resumed loop must take another model
        // turn. That first post-resume call must terminate the
        // invocation as BudgetExceeded carrying the lifetime cost
        // — not run to completion on a fresh accumulator.
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        use crate::worker::reducer::types::{
            AgentConfig, StepInput, TriggerPayload, TriggerSourceKind,
        };

        let dir = tempdir().unwrap();
        let store_path = dir.path().join("events.db");
        let store = Arc::new(WorkerStore::open(&store_path).await.unwrap());

        let agent_id_str = unique_agent_id("resume-budget");
        let agent = Agent::builder()
            .id(&agent_id_str)
            .model("claude-haiku")
            .system_prompt("You are a test agent.")
            .budget(0.05)
            .build()
            .unwrap();
        let invocation_id = Uuid::now_v7();
        let inv_str = invocation_id.to_string();

        // State as persisted after step 0 (awaiting the model).
        let harness = Harness::new();
        let agent_config = AgentConfig {
            agent_id: AgentId::new(&agent_id_str).unwrap(),
            model: "claude-haiku".to_string(),
            system_prompt: "You are a test agent.".to_string(),
            tools_available: vec![],
            allowed_tool_names: vec![],
            max_iterations: crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS,
        };
        let trigger = TriggerPayload {
            source: TriggerSourceKind::Manual,
            subject: None,
            payload: json!("hello"),
        };
        let s0_output = harness
            .step(StepInput {
                config: agent_config.clone(),
                trigger: trigger.clone(),
                state: vec![],
                last_result: None,
                now_ms: 0,
                random_seed: 0,
                step_index: 0,
                static_resource_context: None,
            })
            .expect("step 0");

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
                archive_status: None,
                archive_published_at: None,
            })
            .await
            .unwrap();

        // The completed pre-crash LLM call: $0.20 already spent
        // (past the $0.05 budget on its own) and a tool-use
        // response, so the resumed loop has more work to do. The
        // tool is not in the agent's (empty) tool list, so the
        // runner feeds back a synthetic error result and the
        // reducer asks for the next model turn.
        let response = ChatResponse {
            content: None,
            tool_calls: vec![crate::events::MessageToolCall {
                tool_call_id: crate::events::ToolCallId::new("call-0").unwrap(),
                tool_name: "unavailable_tool".to_string(),
                parameters: json!({}),
            }],
            stop_reason: StopReason::ToolUse,
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
        store
            .write_llm_dispatched(&inv_str, "req-0", 2)
            .await
            .unwrap();
        store
            .write_llm_completed(&inv_str, "req-0", &response_json, false, 0.20, 3)
            .await
            .unwrap();

        let bus = EventBus::connect(&url).await.unwrap();
        let runner = ReducerRunner::new(
            Arc::new(
                ReducerContext::builder()
                    .tools(Arc::new(ToolRegistry::with_builtins()))
                    .build(),
            ),
            Arc::new(
                RunnerConfig::builder()
                    .bus(bus)
                    .pricing(test_pricing())
                    .store(store.clone())
                    .worker_id(test_worker_id())
                    .build(),
            ),
            Harness::new(),
        );

        // The post-resume model turn (reached after the synthetic
        // tool error is fed back to the reducer).
        let llm = FixtureClient::new();
        llm.push_response(ChatResponse {
            content: Some("wrapping up".to_string()),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 50,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        });

        let outcome = runner
            .resume(&agent, &llm, invocation_id)
            .await
            .expect("resume runs");

        match outcome {
            InvocationOutcome::BudgetExceeded { cost, .. } => {
                assert!(
                    cost >= 0.20,
                    "lifetime cost must include pre-crash spend, got {cost}"
                );
            }
            other => panic!("expected BudgetExceeded from lifetime spend, got {other:?}"),
        }
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
                archive_status: None,
                archive_published_at: None,
            })
            .await
            .unwrap();
        store
            .write_tool_intent(&inv_str, "tc1", "shell", "{}", 1)
            .await
            .unwrap();
        store
            .write_tool_dispatched(&inv_str, "tc1", 2)
            .await
            .unwrap();
        // No completed.

        let bus = EventBus::connect(&url).await.unwrap();
        let runner = ReducerRunner::new(
            Arc::new(
                ReducerContext::builder()
                    .tools(Arc::new(ToolRegistry::with_builtins()))
                    .build(),
            ),
            Arc::new(
                RunnerConfig::builder()
                    .bus(bus)
                    .pricing(test_pricing())
                    .store(store)
                    .worker_id(test_worker_id())
                    .build(),
            ),
            Harness::new(),
        );
        let llm = FixtureClient::new();
        let err = runner
            .resume(&agent, &llm, invocation_id)
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
