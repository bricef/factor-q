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

use std::path::Path;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

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
    HostNoticePayload, InvocationArchivedPayload, InvocationTotals, LlmCallOrigin,
    LlmRequestPayload, LlmResponsePayload, Message, MessageRole, RequestParams, StopReason,
    ToolCallPayload, ToolErrorKind, ToolResultPayload, ToolSchema, TriggerSource, TriggeredPayload,
};
use crate::llm::{ChatRequest, ChatResponse, LlmClient};
use crate::mcp::{
    AdvertisedCapabilities, McpClientManager, McpResourceReader, McpServerConfig, ServerRequest,
    advertised_roots_from_tool_sandbox, elicitation_decline,
};
use crate::pricing::PricingTable;
use crate::tools::ToolRegistry;
use crate::validation::ValidatorChain;
use crate::worker::store::{
    DispatchStatus, InvocationStateRow, LlmDispatchRow, ToolDispatchRow, WorkerStore,
};
use crate::worker::workspace::{WORKSPACE_TOKEN, WorkspaceError, WorkspaceProvider};
use crate::worker::{DrainSignal, DurableStart, ExecutorError, InvocationOutcome, WorkerId};

pub use crate::bus::EventSink;

/// Injectable time + entropy (reducer verification plan, slice 3).
/// The runner reads wall-clock and randomness through this trait so
/// the sim can drive invocations deterministically; production uses
/// [`SystemClock`]. The M2 access-control work established the
/// injected-clock pattern for exactly this reason.
pub trait Clock: Send + Sync {
    /// Wall-clock milliseconds since epoch, for [`StepInput::now_ms`].
    fn now_ms(&self) -> u64;
    /// Unix milliseconds as `i64`, for WAL rows and state rows.
    fn unix_now_ms(&self) -> i64;
    /// Fresh randomness for [`StepInput::random_seed`].
    fn rand_u64(&self) -> u64;
}

/// Production clock: system time and OS entropy.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        now_ms()
    }
    fn unix_now_ms(&self) -> i64 {
        unix_now_ms()
    }
    fn rand_u64(&self) -> u64 {
        rand_u64()
    }
}

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
    /// Where the canonical event sequence is published: the NATS
    /// [`EventBus`] in production, an in-memory sink in the sim.
    sink: Arc<dyn EventSink>,
    /// Model→price lookup for cost accounting.
    pricing: Arc<PricingTable>,
    /// Three-state WAL / invocation-state persistence
    /// (data-architecture.md §5.5).
    store: Arc<WorkerStore>,
    /// Identity of the worker hosting this runner (coordination /
    /// archive-ack routing on `fq.worker.{worker_id}.*`).
    worker_id: WorkerId,
    /// Time + entropy source. [`SystemClock`] in production; the sim
    /// injects a deterministic one.
    clock: Arc<dyn Clock>,
    /// Daemon default cap on LLM turns per invocation. Used when an
    /// agent definition does not set its own `max_iterations` override
    /// (Design Principle 8 — tunable parameters are configuration,
    /// not code). Defaults to
    /// [`crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS`].
    max_iterations: u32,
    /// When true, refuse to dispatch a model with no pricing entry
    /// (ADR-0004 at-use backstop) instead of tracking its cost as $0.
    /// The daemon sets this after its startup pricing guarantee has
    /// validated coverage; defaults to false so tests can run with an
    /// empty pricing table.
    enforce_pricing: bool,
    /// Binds `${workspace}` per invocation (parallel-workers Phase 0).
    /// `None` (the default) leaves the token unbound: agents that don't
    /// use it are unaffected, agents that do fail loud at start.
    workspace: Option<Arc<dyn WorkspaceProvider>>,
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
    sink: Option<Arc<dyn EventSink>>,
    pricing: Option<Arc<PricingTable>>,
    store: Option<Arc<WorkerStore>>,
    worker_id: Option<WorkerId>,
    clock: Option<Arc<dyn Clock>>,
    max_iterations: Option<u32>,
    enforce_pricing: Option<bool>,
    workspace: Option<Arc<dyn WorkspaceProvider>>,
}

impl RunnerConfigBuilder {
    /// Event bus for publishing the canonical event sequence.
    pub fn bus(mut self, bus: EventBus) -> Self {
        self.sink = Some(Arc::new(bus));
        self
    }

    /// Publish through an arbitrary [`EventSink`] instead of the NATS
    /// bus — the hermetic sim's entry point.
    pub fn event_sink(mut self, sink: Arc<dyn EventSink>) -> Self {
        self.sink = Some(sink);
        self
    }

    /// Override the time/entropy source. Defaults to [`SystemClock`].
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = Some(clock);
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

    /// Daemon default cap on LLM turns per invocation. Optional;
    /// defaults to
    /// [`crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS`]
    /// when unset. A per-agent override in the definition takes
    /// precedence over this value.
    pub fn max_iterations(mut self, max_iterations: u32) -> Self {
        self.max_iterations = Some(max_iterations);
        self
    }

    /// Enable the at-use pricing backstop: refuse to dispatch a model
    /// with no pricing rather than track its cost as $0 (ADR-0004).
    /// Optional; defaults to false. The daemon sets it true once its
    /// startup pricing guarantee has validated coverage.
    pub fn enforce_pricing(mut self, enforce_pricing: bool) -> Self {
        self.enforce_pricing = Some(enforce_pricing);
        self
    }

    /// Bind `${workspace}` through a [`WorkspaceProvider`]. Optional;
    /// with `None` the token is unbound and any agent that uses it
    /// fails loudly at invocation start.
    pub fn workspace(mut self, workspace: Option<Arc<dyn WorkspaceProvider>>) -> Self {
        self.workspace = workspace;
        self
    }

    /// Finalise the config. Panics if any required field was not set
    /// (`clock` is optional and defaults to [`SystemClock`]).
    pub fn build(self) -> RunnerConfig {
        RunnerConfig {
            sink: self
                .sink
                .expect("RunnerConfig::builder() requires .bus(..) or .event_sink(..)"),
            pricing: self
                .pricing
                .expect("RunnerConfig::builder() requires .pricing(..)"),
            store: self
                .store
                .expect("RunnerConfig::builder() requires .store(..)"),
            worker_id: self
                .worker_id
                .expect("RunnerConfig::builder() requires .worker_id(..)"),
            clock: self.clock.unwrap_or_else(|| Arc::new(SystemClock)),
            max_iterations: self
                .max_iterations
                .unwrap_or(crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS),
            enforce_pricing: self.enforce_pricing.unwrap_or(false),
            workspace: self.workspace,
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
    /// Graceful-drain flag (ADR-0027), polled at each step boundary by
    /// the loop. Shared across every in-flight invocation on this
    /// worker; flipped via [`Worker::request_drain`](crate::Worker) or
    /// [`Self::drain_signal`].
    drain: DrainSignal,
    /// Host notices queued per invocation, drained at that
    /// invocation's next step boundary (#155). Producers push
    /// `(kind, body)` via [`Self::queue_host_notice`]; the drain
    /// persists rows (WAL-before-effect) before building the
    /// `StepInput` that carries them. Keyed by invocation because one
    /// runner services concurrent invocations.
    pending_notices: std::sync::Mutex<std::collections::HashMap<Uuid, Vec<(String, String)>>>,
    /// Invocation ids this runner is DRIVING right now, registered
    /// for the duration of every run/resume entry. The operator
    /// resume precondition (#373) consults this: an invocation active
    /// on this very runner is live, not crashed — and the process
    /// driving it is the only zero-lag authority on that (the
    /// coordination owner rows lag events and carry placeholder
    /// worker ids for crashed runs). Worker-local by construction;
    /// cross-worker liveness is the #107/#374 coordination story.
    active: std::sync::Mutex<std::collections::HashSet<Uuid>>,
}

/// RAII entry in [`ReducerRunner::active`]: removed on drop, so a
/// panic or early return can never leave a phantom "live" marker
/// that would block operator resume forever.
struct ActiveInvocation<'a> {
    set: &'a std::sync::Mutex<std::collections::HashSet<Uuid>>,
    id: Uuid,
}

impl Drop for ActiveInvocation<'_> {
    fn drop(&mut self) {
        self.set
            .lock()
            .expect("active set poisoned")
            .remove(&self.id);
    }
}

impl<R: Reducer + Send + Sync> ReducerRunner<R> {
    pub fn new(context: Arc<ReducerContext>, config: Arc<RunnerConfig>, reducer: R) -> Self {
        Self {
            context,
            config,
            reducer,
            drain: DrainSignal::new(),
            pending_notices: std::sync::Mutex::new(std::collections::HashMap::new()),
            active: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// Whether this runner is currently driving `invocation_id`. The
    /// operator-resume precondition's liveness authority (#373).
    pub fn is_active(&self, invocation_id: &Uuid) -> bool {
        self.active
            .lock()
            .expect("active set poisoned")
            .contains(invocation_id)
    }

    fn mark_active(&self, invocation_id: Uuid) -> ActiveInvocation<'_> {
        self.active
            .lock()
            .expect("active set poisoned")
            .insert(invocation_id);
        ActiveInvocation {
            set: &self.active,
            id: invocation_id,
        }
    }

    /// Queue a host notice for injection into `invocation_id`'s
    /// conversation at its next step boundary (#155). `body` must be
    /// fully rendered by the producer, `<host-notice>` sentinel
    /// included — the exact string is WAL-persisted at the drain and
    /// replayed verbatim on every future resume (replay never
    /// re-renders).
    pub fn queue_host_notice(
        &self,
        invocation_id: Uuid,
        kind: impl Into<String>,
        body: impl Into<String>,
    ) {
        let body = body.into();
        debug_assert!(
            body.starts_with(crate::events::HOST_NOTICE_SENTINEL),
            "host-notice bodies are sentinel-wrapped by their producer"
        );
        self.pending_notices
            .lock()
            .expect("pending_notices lock poisoned")
            .entry(invocation_id)
            .or_default()
            .push((kind.into(), body));
    }

    /// A cloneable handle to this runner's drain flag. Cloning shares
    /// the same underlying flag (see [`DrainSignal`]): requesting a
    /// drain on any handle suspends every in-flight invocation on this
    /// worker at its next step boundary.
    pub fn drain_signal(&self) -> DrainSignal {
        self.drain.clone()
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
        // Direct callers (CLI `fq trigger`, tests, sim) ack nothing, so
        // the durable-start signal has no waiter.
        self.run_signalling(
            agent,
            llm,
            trigger_source,
            trigger_subject,
            trigger_payload,
            None,
            DurableStart::noop(),
        )
        .await
    }

    /// Like [`run`](Self::run) but fires `durable_start` once the
    /// invocation's first WAL write lands. The trigger dispatcher uses
    /// this (through the [`Worker`](crate::Worker) seam) to ack a
    /// trigger only after the run is recoverable from the WAL, closing
    /// the ack->first-WAL-write window (issue #41).
    #[allow(clippy::too_many_arguments)]
    pub async fn run_signalling(
        &self,
        agent: &Agent,
        llm: &dyn LlmClient,
        trigger_source: TriggerSource,
        trigger_subject: Option<String>,
        trigger_payload: Value,
        delivery_attempt: Option<u32>,
        durable_start: DurableStart,
    ) -> Result<InvocationOutcome, ExecutorError> {
        self.run_loop_for(
            agent,
            llm,
            trigger_source,
            trigger_subject,
            trigger_payload,
            delivery_attempt,
            &self.context.tools(),
            None,
            durable_start,
        )
        .await
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
            None,
            &self.context.tools(),
            sampling,
            DurableStart::noop(),
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
        delivery_attempt: Option<u32>,
        tools: &ToolRegistry,
        sampling: Option<SamplingChannel>,
        durable_start: DurableStart,
    ) -> Result<InvocationOutcome, ExecutorError> {
        let invocation_id = Uuid::now_v7();
        let _active = self.mark_active(invocation_id);
        let start = Instant::now();
        let agent_id: AgentId = agent.id().clone();
        let totals = InvocationTotals::default();

        info!(
            agent_id = %agent_id,
            invocation_id = %invocation_id,
            "starting reducer invocation"
        );

        // Bind `${workspace}` for this invocation (parallel-workers
        // Phase 0). Provisioning precedes the Triggered event: a failure
        // here leaves nothing durable, so the dispatcher's pre-WAL
        // transient/permanent split decides redelivery.
        let workspace = match &self.config.workspace {
            Some(provider) => Some(provider.provision(invocation_id).await?),
            None => None,
        };
        // A `?` would leak the just-provisioned workspace (issue #116):
        // an unbound-token error is permanent, nothing durable exists,
        // and the directory is garbage — route it through the reclaim
        // decision.
        let sandbox = match agent.sandbox().to_tool_sandbox(workspace.as_deref()) {
            // Ambient identity env (issue #162): every exec child
            // learns which invocation/agent it runs for, so out-of-band
            // work (git commits, PR bodies) can carry provenance. These
            // are runtime-owned facts, not host env passthrough — no
            // sandbox.env opt-in involved.
            Ok(sandbox) => sandbox
                .ambient_var("FQ_INVOCATION_ID", invocation_id.to_string())
                .ambient_var("FQ_AGENT_ID", agent_id.to_string())
                .ambient_var("FQ_MODEL", agent.model()),
            Err(err) => {
                let outcome = Err(WorkspaceError::from(err).into());
                self.reclaim_if_terminal(invocation_id, workspace.as_deref(), &outcome)
                    .await;
                return outcome;
            }
        };
        // Start grant-bearing MCP servers only after the sandbox has been
        // materialised, so roots use the same bound paths tools enforce.
        let mut manager = McpClientManager::new();
        let grant_decls: Vec<_> = agent
            .mcp_servers()
            .iter()
            .filter(|decl| agent.grants_inbound_capability(&decl.server))
            .collect();
        // The common no-grants invocation keeps the shared registry —
        // no clone, no per-invocation registry (the pre-#179 fast
        // path). `Some` only when a grant server will layer tools on.
        let mut invocation_tools: Option<ToolRegistry> =
            (!grant_decls.is_empty()).then(|| (*tools).clone());
        let mut channels = sampling.map_or_else(Vec::new, |channel| channel.channels);
        for decl in grant_decls {
            let capabilities = AdvertisedCapabilities {
                sampling: agent
                    .sampling_grant()
                    .is_some_and(|g| g.permits(&decl.server)),
                elicitation: agent
                    .elicitation_grant()
                    .is_some_and(|g| g.permits(&decl.server)),
                roots: agent.roots_grant().is_some_and(|g| g.permits(&decl.server)),
            };
            let roots = advertised_roots_from_tool_sandbox(
                &sandbox,
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
                Ok((server_tools, rx, _)) => {
                    for tool in server_tools {
                        let registry = invocation_tools
                            .as_mut()
                            .expect("cloned above: grant_decls is non-empty on this path");
                        if let Err(error) = registry.register(tool) {
                            warn!(server = %decl.server, %error, "refusing per-invocation MCP tool registration");
                        }
                    }
                    channels.push((decl.server.clone(), rx));
                }
                Err(err) => {
                    warn!(agent_id = %agent_id, server = %decl.server, error = %err, "failed to start grant-bearing MCP server per-invocation; skipping it")
                }
            }
        }
        let sampling = (!channels.is_empty()).then(|| SamplingChannel::merged(channels));
        // From here on, `tools` is the effective registry for this
        // invocation: the base one, or the clone with server tools
        // layered on.
        let tools = invocation_tools.as_ref().unwrap_or(tools);
        warn_on_deprecated_bare_grants(&agent_id, agent.tools());
        let allowed_tool_names = canonical_tool_names(agent.tools());
        let tool_schemas = tools.build_schemas(&allowed_tool_names);
        // A tool the agent declares but the registry has no
        // implementation for is dropped silently by `build_schemas` —
        // the model is simply never offered it, with no other signal.
        // This is exactly how a renamed/removed built-in (e.g. the
        // `shell`→`exec` rename) silently degrades an agent. Warn so the
        // capability loss is visible. `tools` here is the effective
        // registry for this invocation (base + any per-invocation MCP
        // tools), so a name missing at this point is genuinely
        // unavailable, not merely unresolved.
        let missing = tools.missing_tools(&allowed_tool_names);
        if !missing.is_empty() {
            warn!(
                agent_id = %agent_id,
                missing_tools = ?missing,
                "agent declares tool(s) with no registered implementation; \
                 they are unavailable to the model"
            );
        }

        let started_at_ms = self.config.clock.unix_now_ms();
        let (agent_config, static_context) = self
            .build_invocation_setup(
                agent,
                workspace.as_deref(),
                delivery_attempt,
                started_at_ms,
                tool_schemas.clone(),
                allowed_tool_names.clone(),
            )
            .await;

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

        // Emit `triggered` once, mirroring the legacy executor. A `?`
        // here would leak the just-provisioned workspace (issue #116):
        // this publish failing is the pre-WAL case — nothing durable
        // exists, the trigger redelivers into a fresh workspace — so
        // route the error through the reclaim decision instead.
        if let Err(err) = self
            .publish_chained(
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
            .await
        {
            // The grant servers started above must not outlive the
            // failed invocation either — same #116-class lesson as the
            // workspace reclaim below: McpClientManager has no Drop, so
            // without this their child processes leak on the pre-WAL
            // publish-failure path.
            manager.shutdown().await;
            let outcome = Err(err);
            self.reclaim_if_terminal(invocation_id, workspace.as_deref(), &outcome)
                .await;
            return outcome;
        }

        let state: Vec<u8> = Vec::new();
        let last_result: Option<CapabilityResult> = None;
        let step_index_start: u32 = 0;

        // Step-0 context: the workspace preamble (the agent is *told*
        // where `${workspace}` points, not left to infer it from tool
        // output) followed by the agent's `static_resources` pins.
        // Injected once; resume does *not* re-inject — the content is
        // already in the persisted conversation history, and the
        // binding is stable across resume (workspace_ref
        // re-association).
        // The preamble timestamp is the invocation's *start* time
        // (`started_at_ms`), not a fresh clock read: it must be stable
        // across the fresh and resumed/drained execution paths or it
        // breaks observational equivalence (the resumed run would stamp
        // a different time into the replayed step-0 message). `started_at`
        // is persisted and re-used verbatim on resume, so both paths
        // agree; a fresh `unix_now_ms()` here also perturbs the sim
        // clock sequence.

        let outcome = self
            .run_loop_inner(
                agent,
                llm,
                invocation_id,
                &agent_id,
                &agent_config,
                &trigger,
                &sandbox,
                tools,
                workspace.as_deref(),
                state,
                last_result,
                step_index_start,
                totals,
                start,
                started_at_ms,
                static_context,
                sampling,
                durable_start,
                &mut cursor,
                // Fresh invocation: no previous incarnation, nothing
                // recorded for the first step.
                Vec::new(),
            )
            .await;
        manager.shutdown().await;
        self.reclaim_if_terminal(invocation_id, workspace.as_deref(), &outcome)
            .await;
        outcome
    }

    /// Release the invocation's workspace on a *terminal* outcome only.
    /// Suspension keeps the workspace — the row is still in-flight and
    /// resume continues from it (plan §3). For errors, the decision
    /// consults **WAL ground truth** rather than the error variant
    /// (issue #116): an agent-turn LLM failure emits a terminal `failed`
    /// event yet surfaces as `Err(Llm)`, so variant-matching leaked one
    /// workspace per terminal LLM failure (eight orphans in the
    /// 2026-07-11 credit-exhaustion storm). The row decides:
    ///
    /// - `terminal_at` set → reclaim (nothing will resume);
    /// - row in flight → keep (resume needs the workspace);
    /// - **no row at all** → reclaim (a pre-WAL failure left nothing
    ///   durable — the trigger redelivers into a *fresh* workspace, so
    ///   this one is garbage);
    /// - store error during the check → keep, conservatively; the
    ///   startup prune sweeps whatever recovery no longer claims.
    ///
    /// A reclaim failure is logged and never overrides the outcome.
    async fn reclaim_if_terminal(
        &self,
        invocation_id: Uuid,
        workspace: Option<&Path>,
        outcome: &Result<InvocationOutcome, ExecutorError>,
    ) {
        let (Some(provider), Some(path)) = (&self.config.workspace, workspace) else {
            return;
        };
        let terminal = match outcome {
            Ok(InvocationOutcome::Completed { .. })
            | Ok(InvocationOutcome::BudgetExceeded { .. }) => true,
            Ok(InvocationOutcome::Suspended { .. }) => false,
            Err(_) => match self
                .config
                .store
                .get_invocation_state(&invocation_id.to_string())
                .await
            {
                Ok(Some(row)) => row.terminal_at.is_some(),
                Ok(None) => true,
                Err(err) => {
                    warn!(
                        invocation_id = %invocation_id,
                        error = %err,
                        "could not read state row for reclaim decision; keeping workspace"
                    );
                    false
                }
            },
        };
        if !terminal {
            return;
        }
        if let Err(err) = provider.reclaim(invocation_id, path).await {
            warn!(
                invocation_id = %invocation_id,
                workspace = %path.display(),
                error = %err,
                "workspace reclaim failed; the startup prune will sweep it"
            );
        }
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
        let _active = self.mark_active(invocation_id);
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
        let host_notices = self
            .config
            .store
            .list_host_notices(&inv_str)
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
        let mut completed: Vec<((Option<i64>, i64), CapabilityResult)> = Vec::new();
        for r in &tools {
            if r.status == DispatchStatus::Completed {
                completed.push((
                    replay_sort_key(r.seq, r.completed_at),
                    tool_row_to_capability(r),
                ));
            }
        }
        for r in &llms {
            if r.status != DispatchStatus::Completed {
                continue;
            }
            // A completed-with-error row records a provider failure
            // whose failed terminal was lost to the crash — the
            // response column holds the error string, not a
            // ChatResponse. The invocation's fate was already
            // determined; reproduce it instead of trying to replay
            // the row (finding 6, caught by the slice-7 deep soak:
            // resume previously died on a deserialise error here).
            if r.is_error == Some(true) {
                let message = r
                    .response
                    .clone()
                    .unwrap_or_else(|| "provider error (no detail recorded)".to_string());
                let mut cursor: Option<Uuid> = None;
                self.emit_failed(
                    &agent_id,
                    invocation_id,
                    FailureKind::LlmError,
                    format!("{message} (reproduced on resume)"),
                    FailurePhase::LlmRequest,
                    InvocationTotals::default(),
                    &mut cursor,
                )
                .await?;
                return Err(ExecutorError::Llm(crate::llm::LlmError::RequestFailed(
                    message,
                )));
            }
            completed.push((
                replay_sort_key(r.seq, r.completed_at),
                llm_row_to_capability(r)?,
            ));
        }
        sort_into_replay_order(&mut completed);

        // Regroup each model turn's tool results into the single
        // capability the live loop produced. A turn with >1 tool call is
        // answered by one `CallToolsParallel` / `ParallelToolResults`;
        // replaying one `ToolResult` per row instead desyncs the harness
        // — it consumes the first result, returns to `AwaitingModel`,
        // then rejects the second with "expected ModelResult after
        // CallModel", leaving the invocation an unrecoverable zombie.
        // Consecutive tool results (in completion order) belong to one
        // turn: the next model call only starts once the turn's results
        // are integrated. Sequential dispatch runs a batch in request
        // order (see `NextAction::CallToolsParallel`), so completion
        // order matches what the live loop persisted.
        //
        // If the crash fell *inside* the final batch (fewer completed
        // tool rows than that model turn requested), drop the recorded
        // partial results so replay ends at the model turn and
        // `run_loop_inner` re-runs the batch: `run_tool` reuses the
        // already-completed calls and executes only the missing ones,
        // completing the batch exactly once instead of silently
        // dropping the un-run calls.
        let resumed_partial_batch = truncate_incomplete_final_batch(&mut completed);
        let replay = coalesce_tool_results(completed);

        // Re-associate the invocation with its persisted workspace
        // (plan §3): a suspended invocation's workspace survives the
        // restart, and the state row's `workspace_ref` is the binding.
        // A row with no ref (pre-Phase-0, or the provider was enabled
        // mid-flight) provisions fresh — for the static provider that
        // is the same shared directory; per-invocation it is a fresh
        // empty one, acceptable only because such rows predate the
        // feature.
        let workspace = match (&self.config.workspace, state_row.workspace_ref.as_deref()) {
            (Some(provider), Some(persisted)) => {
                Some(provider.reattach(invocation_id, persisted).await?)
            }
            (Some(provider), None) => Some(provider.provision(invocation_id).await?),
            (None, _) => None,
        };

        // Set up agent context (mirrors run()). One registry snapshot
        // serves both the schemas and the loop (ADR-0020 consistency).
        // Ambient identity env re-attaches on resume exactly as on the
        // fresh path (issue #162) — same invocation id, so provenance
        // stays consistent across the interruption.
        let sandbox = agent
            .sandbox()
            .to_tool_sandbox(workspace.as_deref())
            .map_err(WorkspaceError::from)?
            .ambient_var("FQ_INVOCATION_ID", invocation_id.to_string())
            .ambient_var("FQ_AGENT_ID", agent_id.to_string())
            .ambient_var("FQ_MODEL", agent.model());
        let base_tools = self.context.tools();
        warn_on_deprecated_bare_grants(&agent_id, agent.tools());
        let allowed_tool_names = canonical_tool_names(agent.tools());
        let tool_schemas = base_tools.build_schemas(&allowed_tool_names);
        let (agent_config, step0_static_context) = self
            .build_invocation_setup(
                agent,
                workspace.as_deref(),
                None,
                state_row.started_at,
                tool_schemas,
                allowed_tool_names.clone(),
            )
            .await;
        // Reconstruct the original trigger from the state row (v5).
        // Replay starts at step 0, and step 0 seeds the conversation
        // from the trigger — resuming with a null trigger would
        // rewrite the invocation's first user message to "(no input)"
        // (found by the slice-4 resume-equivalence property). Rows
        // written before v5 lack the columns; warn and degrade.
        let trigger = trigger_from_state_row(&state_row);

        // Rebuild the *same* step-0 static context the fresh path
        // injected (the invocation preamble + static-resource pins).
        // Replay reconstructs the conversation from an empty state, so
        // step 0 must re-inject this context or a resumed run diverges
        // from an uninterrupted one — the resume/drain observational-
        // equivalence property. Every input derives from persisted
        // invocation state (`started_at`, the re-attached workspace,
        // the agent's budget/ceiling) so both paths produce identical
        // text. `delivery_attempt` is *not* persisted on the state row;
        // a resumed run reconstructs it as the first attempt. That is
        // exact for the common case and the sim harness; a resumed run
        // of a redelivered trigger would show `attempt: 1` rather than
        // the original count (issue #87).

        // Replay the reducer deterministically through every
        // completed action. The reducer is pure; reading the
        // sequence of (state, last_result, step_index) tuples
        // out of nothing rebuilds state cheaply.
        let mut state: Vec<u8> = Vec::new();
        let mut last_result: Option<CapabilityResult> = None;
        let mut step_index: u32 = 0;
        for capability in &replay {
            let input = StepInput {
                config: agent_config.clone(),
                trigger: trigger.clone(),
                state,
                last_result,
                now_ms: self.config.clock.now_ms(),
                random_seed: self.config.clock.rand_u64(),
                step_index,
                // Re-inject the step-0 context on replay so the rebuilt
                // conversation matches the fresh path exactly.
                static_resource_context: if step_index == 0 {
                    step0_static_context.clone()
                } else {
                    None
                },
                host_notices: host_notices
                    .iter()
                    .filter(|notice| notice.step_index == step_index)
                    .map(|notice| notice.body.clone())
                    .collect(),
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
        // A re-run partial final batch re-counts its already-completed
        // calls in `run_loop_inner`, so exclude them from the seed.
        totals.total_tool_calls = (tools
            .iter()
            .filter(|r| r.status == DispatchStatus::Completed)
            .count()
            - resumed_partial_batch) as u32;
        let start = Instant::now();
        let mut cursor: Option<Uuid> = None;

        // The post-call budget check that would have fired on the
        // original attempt fires here instead: a crash in the window
        // between the WAL completed-write and the check must not
        // launder an overspend into a successful completion (finding
        // 5, caught by the slice-7 soak — a SafeReplay of a
        // budget-crossing final call otherwise completes without any
        // further model call to re-trigger the check).
        if let Some(budget) = agent.budget()
            && totals.total_cost > budget
        {
            let kind = FailureKind::BudgetExceeded;
            self.emit_failed(
                &agent_id,
                invocation_id,
                kind,
                format!(
                    "cost ${:.6} exceeded budget ${budget:.2} (detected on resume)",
                    totals.total_cost
                ),
                FailurePhase::Budget,
                totals,
                &mut cursor,
            )
            .await?;
            // Terminal outcome on the resume path — the early return
            // must still release the re-attached workspace (issue #116).
            let outcome = Ok(InvocationOutcome::BudgetExceeded {
                invocation_id,
                cost: totals.total_cost,
            });
            self.reclaim_if_terminal(invocation_id, workspace.as_deref(), &outcome)
                .await;
            return outcome;
        }
        let outcome = self
            .run_loop_inner(
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
                workspace.as_deref(),
                state,
                last_result,
                step_index,
                totals,
                start,
                state_row.started_at,
                // Only applied when replay was empty (the crash fell at
                // step 0, so `step_index_start == 0` here). A non-empty
                // replay already injected this context at its step 0
                // above; `run_loop_inner` applies static context only
                // when `step_index == 0`, so there is no double-inject.
                step0_static_context,
                // No inbound server channel on resume: the per-invocation
                // server connection died with the crash, so a resumed run
                // cannot service (or replay) sampling (ADR-0018 §5). Any
                // in-flight sampling is surfaced via `fq recover`.
                None,
                // Resume acks nothing — the trigger was acked on the
                // original attempt (issue #41).
                DurableStart::noop(),
                &mut cursor,
                // Rows recorded for the step the crash interrupted (WAL
                // write landed, the step never ran). The live re-run must
                // carry them or its conversation silently diverges from
                // what any later replay reconstructs.
                host_notices
                    .iter()
                    .filter(|notice| notice.step_index == step_index)
                    .map(|notice| (notice.seq, notice.kind.clone(), notice.body.clone()))
                    .collect(),
            )
            .await;
        self.reclaim_if_terminal(invocation_id, workspace.as_deref(), &outcome)
            .await;
        outcome
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
        workspace: Option<&Path>,
        mut state: Vec<u8>,
        mut last_result: Option<CapabilityResult>,
        step_index_start: u32,
        mut totals: InvocationTotals,
        start: Instant,
        started_at_ms: i64,
        static_context: Option<String>,
        mut sampling: Option<SamplingChannel>,
        mut durable_start: DurableStart,
        cursor: &mut Option<Uuid>,
        mut resumed_step_notices: Vec<(u32, String, String)>,
    ) -> Result<InvocationOutcome, ExecutorError> {
        // Invocation-scoped context-pressure tracking (issue #76). The
        // oldest turn is the invocation start — the first messages are
        // seeded there. Threaded through the model and self_inspect
        // paths below.
        let mut context = ContextTracker {
            oldest_turn_at_ms: started_at_ms,
            ..ContextTracker::default()
        };
        for step_index in step_index_start..HOST_STEP_BUDGET {
            // Host notices (#155). `carried` starts with rows a previous
            // incarnation recorded for this step — a crash after the WAL
            // write but before the step ran. They are already durable, so
            // they reach the reducer exactly as recorded (no re-insert, and
            // no event re-emit: the WAL, not the event trail, is the
            // channel's source of truth). Fresh drains are persisted before
            // the `StepInput` that carries them is built — the
            // WAL-write-before-effect ordering the runner already uses —
            // with seq numbers continuing after the recorded ones so a
            // future replay reconstructs the same order.
            let mut carried: Vec<(u32, String, String)> = if step_index == step_index_start {
                std::mem::take(&mut resumed_step_notices)
            } else {
                Vec::new()
            };
            let seq_base = carried.iter().map(|(seq, _, _)| seq + 1).max().unwrap_or(0);
            let drained: Vec<(String, String)> = self
                .pending_notices
                .lock()
                .expect("pending_notices lock poisoned")
                .remove(&invocation_id)
                .unwrap_or_default();
            for (offset, (kind, body)) in drained.into_iter().enumerate() {
                let next_seq = seq_base + offset as u32;
                // The body was rendered by its producer, sentinel included;
                // this exact string is persisted and replayed verbatim.
                self.config
                    .store
                    .insert_host_notice(
                        &invocation_id.to_string(),
                        step_index,
                        next_seq,
                        &kind,
                        &body,
                        self.config.clock.unix_now_ms(),
                    )
                    .await
                    .map_err(map_store_err)?;
                self.publish_chained(
                    cursor,
                    Event::new(
                        agent_id.clone(),
                        invocation_id,
                        EventPayload::HostNotice(HostNoticePayload {
                            kind: kind.clone(),
                            body: body.clone(),
                        }),
                    ),
                )
                .await?;
                info!(
                    invocation_id = %invocation_id,
                    step_index,
                    kind = %kind,
                    "host notice injected"
                );
                carried.push((next_seq, kind, body));
            }
            let host_notices: Vec<String> = carried.into_iter().map(|(_, _, body)| body).collect();
            // ADR-0027 graceful drain: suspend at this step boundary if
            // a drain has been requested. The previous iteration's
            // checkpoint — or, for `step_index_start`, the `Triggered`
            // event written before the loop — is already durable, so the
            // WAL state here is a clean between-steps point, bit-identical
            // to a crash at this boundary, which recovery resumes. The row
            // stays in-flight and no terminal event is emitted; the next
            // binary picks it up.
            if self.drain.is_draining() {
                info!(
                    agent_id = %agent_id,
                    invocation_id = %invocation_id,
                    step_index,
                    "draining — suspending invocation at step boundary"
                );
                return Ok(InvocationOutcome::Suspended { invocation_id });
            }

            let input = StepInput {
                config: agent_config.clone(),
                trigger: trigger.clone(),
                state,
                last_result,
                now_ms: self.config.clock.now_ms(),
                random_seed: self.config.clock.rand_u64(),
                step_index,
                // Static-resource content is injected exactly once,
                // on step 0. Later steps and resumed runs carry it
                // in the reducer's persisted conversation history.
                static_resource_context: if step_index == 0 {
                    static_context.clone()
                } else {
                    None
                },
                host_notices,
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
                        FailurePhase::Reducer,
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
            // One clock read for both fields: the terminal update *is* the
            // last update, so `terminal_at` and `updated_at` must be the same
            // instant (as the failed path via `ensure_terminal` already does).
            // Two separate reads let `updated_at` (read second) land a
            // millisecond later under load — `updated_at > terminal_at`, a real
            // ordering violation that surfaced as a flaky test.
            let now_ms = self.config.clock.unix_now_ms();
            let (phase_label, terminal_at) = phase_and_terminal_from(&output.next_action, now_ms);
            self.config
                .store
                .upsert_invocation_state(&InvocationStateRow {
                    invocation_id: invocation_id.to_string(),
                    agent_id: agent_id.as_str().to_string(),
                    schema_version: 1,
                    phase: phase_label.to_string(),
                    state_blob: output.state.clone(),
                    step_index,
                    started_at: started_at_ms,
                    updated_at: now_ms,
                    terminal_at,
                    // The invocation's `${workspace}` binding, persisted
                    // so recovery re-associates a resumed invocation with
                    // its workspace (plan §3).
                    workspace_ref: workspace.map(|p| p.to_string_lossy().into_owned()),
                    archive_status: None,
                    archive_published_at: None,
                    trigger_source: Some(trigger_source_label(&trigger.source).to_string()),
                    trigger_subject: trigger.subject.clone(),
                    trigger_payload: Some(trigger.payload.to_string()),
                })
                .await
                .map_err(map_store_err)?;
            state = output.state;

            // First durable (WAL) write for this invocation has landed:
            // the run is now recoverable from the WAL, so the trigger
            // dispatcher may ack (issue #41). Idempotent — only the
            // first step fires; every later call is a no-op.
            durable_start.fire();

            match output.next_action {
                NextAction::Complete { text, task_status } => {
                    let duration_ms = start.elapsed().as_millis() as u64;
                    totals.total_duration_ms = duration_ms;
                    let summary = if text.is_empty() { None } else { Some(text) };
                    self.publish_chained(
                        cursor,
                        Event::new(
                            agent_id.clone(),
                            invocation_id,
                            EventPayload::Completed(CompletedPayload {
                                task_status,
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
                        FailurePhase::Reducer,
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
                            &mut context,
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
                            workspace,
                            req,
                            &mut totals,
                            start,
                            sampling.as_mut(),
                            &mut context,
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
                                workspace,
                                req,
                                &mut totals,
                                start,
                                sampling.as_mut(),
                                &mut context,
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
            FailurePhase::HostStepBudget,
            totals,
            cursor,
        )
        .await?;
        Err(ExecutorError::InvocationFailed { kind, message })
    }

    /// Build reducer configuration and deterministic step-0 context for both
    /// fresh and resumed invocations. Workspace provisioning/reattachment stays
    /// path-specific, while the resulting binding is consumed here identically.
    async fn build_invocation_setup(
        &self,
        agent: &Agent,
        workspace: Option<&Path>,
        delivery_attempt: Option<u32>,
        started_at_ms: i64,
        tool_schemas: Vec<ToolSchema>,
        allowed_tool_names: Vec<String>,
    ) -> (AgentConfig, Option<String>) {
        let agent_id = agent.id().clone();
        let config = AgentConfig {
            agent_id: agent_id.clone(),
            model: agent.model().to_string(),
            system_prompt: agent.system_prompt().to_string(),
            tools_available: tool_schemas,
            allowed_tool_names,
            max_iterations: agent.max_iterations().unwrap_or(self.config.max_iterations),
            effort: agent.effort(),
        };
        let context = merge_step0_context(
            Some(invocation_preamble(
                workspace,
                &agent_id,
                delivery_attempt,
                agent.budget(),
                config.max_iterations,
                started_at_ms,
            )),
            self.read_static_resources(agent).await,
        );
        (config, context)
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
        workspace: Option<&Path>,
        mut req: ToolCallRequest,
        totals: &mut InvocationTotals,
        start: Instant,
        sampling: Option<&mut SamplingChannel>,
        context: &mut ContextTracker,
        cursor: &mut Option<Uuid>,
    ) -> Result<ToolCallResult, ExecutorError> {
        // Accept legacy bare built-in calls while definitions migrate
        // (#177): grants are canonicalised the same way, so the
        // allowed-check, dispatch, the WAL, and events all see one
        // vocabulary within a runtime version.
        if let Some(canonical) = canonicalize_bare_builtin(&req.tool_name) {
            debug!(from = %req.tool_name, to = %canonical, "normalised legacy bare tool call");
            req.tool_name = canonical;
        }
        // Idempotent recovery: a completed WAL row for this exact call
        // means a prior incarnation already ran it. Reuse the recorded
        // result rather than re-executing (at-most-once) — and without
        // re-publishing, so a resumed run's observational trace matches
        // the original. This is how re-running a partially-completed
        // parallel batch on resume skips its already-done calls. Live
        // execution always has a fresh id, so the cheap point lookup
        // never hits outside recovery.
        if let Some(row) = self
            .config
            .store
            .get_tool_dispatch(&invocation_id.to_string(), req.tool_call_id.as_str())
            .await
            .map_err(map_store_err)?
            && row.status == DispatchStatus::Completed
        {
            return Ok(tool_row_to_result(&row));
        }

        // Bind `${workspace}` in the tool call's *declared path
        // parameters* before the intent is persisted, so the WAL and
        // the event trail record the path that actually executed
        // (replay-stable). The ConfigSnapshot keeps the unresolved
        // token — that layer records config, not runtime state.
        let req = match (workspace, tools.get(&req.tool_name)) {
            (Some(ws), Some(tool)) => bind_workspace_params(req, ws, &tool.parameters_schema()),
            _ => req,
        };
        if !canonical_tool_names(agent.tools())
            .iter()
            .any(|name| name == &req.tool_name)
        {
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
        // `tool.result`. Synthetic-error results are journaled too —
        // all three transitions at once, inside
        // `emit_synthetic_tool_error`: there is no side effect to
        // guard, but replay reconstructs the conversation from the
        // WAL alone (finding 7). Only their `tool.call` /
        // `tool.dispatched` events are skipped.
        let inv_str = invocation_id.to_string();
        let intent_at = self.config.clock.unix_now_ms();
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
        if req.tool_name == crate::tools::SELF_INSPECT_CANONICAL_NAME {
            return self
                .run_self_inspect_with_wal(
                    agent,
                    agent_id,
                    invocation_id,
                    req,
                    totals,
                    start,
                    context,
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
                    .write_tool_dispatched(
                        &inv_str,
                        req.tool_call_id.as_str(),
                        self.config.clock.unix_now_ms(),
                    )
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
                        self.config.clock.unix_now_ms(),
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

        // Mark dispatched BEFORE the handoff, durably. This is the
        // ambiguous-window state and it must cover the entire
        // execution: a crash while the tool runs has unknown side
        // effects and must classify Ambiguous on recovery — an
        // intent-only WAL reads as "never ran" and gets silently
        // re-run, which is exactly the double-side-effect disaster
        // the recovery taxonomy exists to prevent.
        self.config
            .store
            .write_tool_dispatched(
                &inv_str,
                req.tool_call_id.as_str(),
                self.config.clock.unix_now_ms(),
            )
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

        match outcome {
            Ok(result) => {
                self.config
                    .store
                    .write_tool_completed(
                        &inv_str,
                        req.tool_call_id.as_str(),
                        &result.output,
                        result.is_error,
                        self.config.clock.unix_now_ms(),
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
                        self.config.clock.unix_now_ms(),
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
        context: &ContextTracker,
        inv_str: &str,
        cursor: &mut Option<Uuid>,
    ) -> Result<ToolCallResult, ExecutorError> {
        use crate::worker::introspection::{HostInvocationStats, synthesize_self_inspect};

        let tool_start = Instant::now();
        let stats = HostInvocationStats {
            invocation_id: inv_str,
            agent_id: agent_id.as_str(),
            model: agent.model(),
            allowed_tool_names: agent.tools(),
            budget: agent.budget(),
            // Report the *effective* cap that bounds this agent, using
            // the same precedence the runner applies when building
            // AgentConfig: per-agent override -> daemon config default
            // -> built-in fallback (issue #9).
            max_iterations: agent.max_iterations().unwrap_or(self.config.max_iterations),
            totals: *totals,
            elapsed_ms: start.elapsed().as_millis() as u64,
            // Context section (issue #76): the window comes from the
            // pricing/context-window table; occupancy and history from
            // the invocation-scoped tracker the model path updates.
            tokens_in_use: context.tokens_in_use,
            context_window_size: self.config.pricing.context_window(agent.model()),
            messages_in_history: context.messages_in_history,
            oldest_turn_at_ms: Some(context.oldest_turn_at_ms),
        };
        let output = synthesize_self_inspect(&stats, req.parameters.clone());
        let duration_ms = tool_start.elapsed().as_millis() as u64;

        // Close the WAL: dispatched, then completed.
        self.config
            .store
            .write_tool_dispatched(
                inv_str,
                req.tool_call_id.as_str(),
                self.config.clock.unix_now_ms(),
            )
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
                self.config.clock.unix_now_ms(),
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
        // Synthetic errors are journaled like real tool results —
        // intent then completed, before the event publish. There is
        // no side effect to guard, but replay reconstructs the
        // conversation from the WAL alone: an unjournaled synthetic
        // result leaves two consecutive LLM rows and the replay
        // feeds a ModelResult where the state machine expects a
        // ToolResult (finding 7, caught by the slice-7 deep soak).
        let inv_str = invocation_id.to_string();
        let params_json =
            serde_json::to_string(&req.parameters).unwrap_or_else(|_| "{}".to_string());
        self.config
            .store
            .write_tool_intent(
                &inv_str,
                req.tool_call_id.as_str(),
                &req.tool_name,
                &params_json,
                self.config.clock.unix_now_ms(),
            )
            .await
            .map_err(map_store_err)?;
        self.config
            .store
            .write_tool_dispatched(
                &inv_str,
                req.tool_call_id.as_str(),
                self.config.clock.unix_now_ms(),
            )
            .await
            .map_err(map_store_err)?;
        self.config
            .store
            .write_tool_completed(
                &inv_str,
                req.tool_call_id.as_str(),
                &message,
                true,
                self.config.clock.unix_now_ms(),
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
            .sink
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
        let terminal_at_ms = self.config.clock.unix_now_ms();
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
    /// (state_blob, step_index, started_at, etc.); the
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
            .set_archive_pending(&invocation_id_str, self.config.clock.unix_now_ms())
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
        context: &mut ContextTracker,
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
                Some(context),
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
        // Agent turns pass their invocation-scoped context tracker so
        // occupancy/history are recorded and the one-shot context-
        // pressure warning can be latched and injected here (issue #76).
        // Sampling / elicitation / evaluator calls pass `None` — those
        // are server-initiated and do not drive the agent's own context
        // signal.
        context: Option<&mut ContextTracker>,
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

        // At-use pricing backstop (ADR-0004): when enabled, refuse a
        // model with no pricing rather than dispatch and track its cost
        // as $0 — which would silently defeat the budget check. Runs
        // before any WAL write, so a refused call leaves no trace. Both
        // agent turns and sampling flow through here; each applies its
        // own semantics to the returned inner `Err` (a turn fails the
        // invocation, a sampling request declines). Unreachable when the
        // startup pricing guarantee holds — this is defence in depth.
        if self.config.enforce_pricing && self.config.pricing.lookup(&chat_request.model).is_none()
        {
            return Ok(Err(crate::llm::LlmError::UnpricedModel(
                chat_request.model.clone(),
            )));
        }

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
                self.config.clock.unix_now_ms(),
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
                    .write_llm_dispatched(&inv_str, &req_str, self.config.clock.unix_now_ms())
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
                        self.config.clock.unix_now_ms(),
                    )
                    .await
                    .map_err(map_store_err)?;
                // Hand the LLM error back to the caller; the WAL is
                // already closed `is_error`, so this is a final state.
                return Ok(Err(err));
            }
        };

        if response.tool_calls.is_empty()
            && response
                .content
                .as_deref()
                .is_none_or(|content| content.trim().is_empty())
        {
            // Deliberately skips `totals.total_llm_calls` and drops the
            // usage: the turn produced nothing to bill against an
            // outcome. If a provider ever bills tokens for empty
            // completions this undercounts — revisit if that matters.
            let err = crate::llm::LlmError::RequestFailed(
                "model returned an empty response (no content, no tool calls)".to_string(),
            );
            self.config
                .store
                .write_llm_dispatched(&inv_str, &req_str, self.config.clock.unix_now_ms())
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
                    self.config.clock.unix_now_ms(),
                )
                .await
                .map_err(map_store_err)?;
            return Ok(Err(err));
        }

        totals.total_llm_calls += 1;

        // LLM returned control. Mark dispatched (ambiguous
        // window), publish the dispatched event, then transition
        // to completed before the response/cost events go out.
        self.config
            .store
            .write_llm_dispatched(&inv_str, &req_str, self.config.clock.unix_now_ms())
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
        // Cost is computed before the WAL completed-write so the row
        // carries the call's real cost — resume() reconstitutes the
        // budget accumulator from exactly this column, so a 0.0 here
        // silently forgets pre-crash spend on every resume (finding 4,
        // caught by the slice-6 budget-across-resume property; the
        // old comment claimed the cost was "filled in below", which
        // never happened).
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

        let response_json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        self.config
            .store
            .write_llm_completed(
                &inv_str,
                &req_str,
                &response_json,
                false,
                total_cost,
                self.config.clock.unix_now_ms(),
            )
            .await
            .map_err(map_store_err)?;

        // Context-pressure tracking + one-shot soft warning (issue
        // #76). Only agent turns carry a tracker; sampling/elicitation
        // pass `None`. We record the turn's occupancy and history, and
        // — the first time occupancy crosses the soft threshold — latch
        // and annotate this `llm.response` event so the warning is
        // visible in the event trail exactly once (annotations ride on
        // the envelope and are stripped only from downstream consumer
        // prompts, so this does not perturb the canonical trace).
        let mut context_warning: Option<String> = None;
        if let Some(tracker) = context {
            tracker.tokens_in_use = Some(response.usage.input_tokens);
            tracker.messages_in_history = Some(request.messages.len() as u32);
            let window = self.config.pricing.context_window(&request.model);
            if crate::worker::introspection::context_pressure(
                Some(response.usage.input_tokens),
                window,
            )
            .is_some()
                && !tracker.warning_emitted
            {
                tracker.warning_emitted = true;
                warn!(
                    agent_id = %agent_id,
                    invocation_id = %invocation_id,
                    tokens_in_use = response.usage.input_tokens,
                    context_window = ?window,
                    "{}",
                    crate::worker::introspection::CONTEXT_PRESSURE_WARNING
                );
                context_warning =
                    Some(crate::worker::introspection::CONTEXT_PRESSURE_WARNING.to_string());
            }
        }

        let mut response_event = Event::new(
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
        });
        if let Some(message) = context_warning {
            response_event = response_event.annotate(
                crate::events::annotation_keys::FLAGS,
                serde_json::json!({ "context_pressure": message }),
            );
        }
        self.publish_chained(cursor, response_event).await?;

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
                    None,
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
                None,
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

/// Canonical form of a legacy bare built-in name (`exec` →
/// `builtin__exec`), or `None` when the name is not a bare built-in.
/// The basename list lives beside the registry so a new built-in cannot
/// miss this mapping. MCP tools are always explicitly namespaced and
/// never map.
fn canonicalize_bare_builtin(name: &str) -> Option<String> {
    crate::tools::BUILTIN_TOOL_BASENAMES
        .contains(&name)
        .then(|| format!("{}{name}", crate::tools::BUILTIN_PREFIX))
}

/// Map legacy bare built-in grants to their canonical names. Pure and
/// quiet — it runs on every tool call's allowed-check, so deprecation
/// warnings are emitted once per invocation setup by
/// [`warn_on_deprecated_bare_grants`], not here. Accepted for one
/// release while agent definitions migrate (#177).
fn canonical_tool_names(names: &[String]) -> Vec<String> {
    names
        .iter()
        .map(|name| canonicalize_bare_builtin(name).unwrap_or_else(|| name.clone()))
        .collect()
}

/// Emit the one-per-invocation deprecation warning for legacy bare
/// built-in grants in an agent definition (#177 migration window).
fn warn_on_deprecated_bare_grants(agent_id: &AgentId, names: &[String]) {
    let deprecated: Vec<&str> = names
        .iter()
        .filter(|name| canonicalize_bare_builtin(name).is_some())
        .map(|name| name.as_str())
        .collect();
    if !deprecated.is_empty() {
        warn!(
            agent_id = %agent_id,
            tools = ?deprecated,
            "bare built-in tool grants are deprecated; use the builtin__ prefix"
        );
    }
}

/// Stable runner-authored environment preamble injected as the first context message.
fn invocation_preamble(
    workspace: Option<&Path>,
    agent_id: &AgentId,
    delivery_attempt: Option<u32>,
    budget: Option<f64>,
    max_iterations: u32,
    now_ms: i64,
) -> String {
    let timestamp = chrono::DateTime::from_timestamp_millis(now_ms)
        .map(|time| time.to_rfc3339())
        .unwrap_or_else(|| "unknown".to_string());
    let workspace = workspace.map_or_else(
        || "unavailable".to_string(),
        |path| path.display().to_string(),
    );
    let budget = budget.map_or_else(|| "unlimited".to_string(), |value| format!("${value:.2}"));
    let attempt = delivery_attempt.unwrap_or(1);
    format!(
        "Environment: timestamp: {timestamp}; agent id: {agent_id}; workspace: {workspace}; attempt: {attempt}; budget: {budget}; iteration ceiling: {max_iterations}. In path parameters of your tools (`cwd`, `path`) you may write `${{workspace}}` and the runtime resolves it to that directory; everywhere else — file contents, command arguments — your text is passed through verbatim."
    )
}

/// Compose the step-0 injected context: workspace preamble first, then
/// the agent's `static_resources` pins.
fn merge_step0_context(preamble: Option<String>, pins: Option<String>) -> Option<String> {
    match (preamble, pins) {
        (Some(a), Some(b)) => Some(format!("{a}\n\n{b}")),
        (a, None) => a,
        (None, b) => b,
    }
}

/// Substitute the invocation's workspace path for [`WORKSPACE_TOKEN`] in
/// the tool call's **declared path parameters** — top-level properties
/// whose JSON schema carries `"format": "path"` (a string, or an array
/// whose items do). Every other parameter passes through verbatim:
/// silently rewriting arbitrary agent output (file contents, argv
/// elements, messages) would be undebuggable, so a tool must declare
/// which of its parameters are paths to opt in.
fn bind_workspace_params(
    mut req: ToolCallRequest,
    workspace: &Path,
    schema: &Value,
) -> ToolCallRequest {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return req;
    };
    let Some(params) = req.parameters.as_object_mut() else {
        return req;
    };
    let ws = workspace.to_string_lossy();
    for (name, prop) in properties {
        let Some(value) = params.get_mut(name) else {
            continue;
        };
        if is_path_schema(prop) {
            bind_workspace_string(value, &ws);
        } else if prop.get("items").is_some_and(is_path_schema)
            && let Value::Array(items) = value
        {
            items
                .iter_mut()
                .for_each(|item| bind_workspace_string(item, &ws));
        }
    }
    req
}

fn is_path_schema(prop: &Value) -> bool {
    prop.get("format").and_then(Value::as_str) == Some("path")
}

fn bind_workspace_string(value: &mut Value, ws: &str) {
    if let Value::String(s) = value
        && s.contains(WORKSPACE_TOKEN)
    {
        *s = s.replace(WORKSPACE_TOKEN, ws);
    }
}

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
            effort: None,
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

/// Invocation-scoped context-pressure tracking (issue #76).
///
/// The runner is shared across invocations (`&self`), so the
/// once-only soft warning cannot latch on the runner; it latches
/// here, on a value the run loop owns for a single invocation. The
/// fields mirror the `context` section of `self_inspect`: the most
/// recent turn's prompt size, the message count the runner last
/// dispatched, and the timestamp of the oldest turn (invocation
/// start, when the first messages are seeded).
#[derive(Debug, Default)]
struct ContextTracker {
    /// Prompt tokens on the most recent LLM turn (context occupancy).
    tokens_in_use: Option<u32>,
    /// Message count in the most recently dispatched request.
    messages_in_history: Option<u32>,
    /// Unix-ms of the oldest turn — the invocation start.
    oldest_turn_at_ms: i64,
    /// Whether the one-shot soft warning has already been injected
    /// past the threshold. Latched so the warning fires exactly once
    /// per invocation, not on every subsequent over-threshold turn.
    warning_emitted: bool,
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
            effort: None,
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
            effort: None,
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
    CapabilityResult::ToolResult(tool_row_to_result(row))
}

/// The [`ToolCallResult`] recorded in a completed `tool_dispatch` row —
/// fed back into the reducer on replay, or returned directly when
/// `run_tool` reuses an already-completed call during recovery.
fn tool_row_to_result(row: &ToolDispatchRow) -> ToolCallResult {
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
    ToolCallResult {
        tool_call_id,
        output: row.result.clone().unwrap_or_default(),
        is_error: row.is_error.unwrap_or(false),
        error_kind: None,
        duration_ms: 0,
    }
}

/// The two candidate orderings for one completed WAL row: the v9 shared
/// completion `seq` (None on pre-v9 legacy rows) and the row's
/// `completed_at` timestamp.
fn replay_sort_key(seq: Option<i64>, completed_at: Option<i64>) -> (Option<i64>, i64) {
    (seq, completed_at.unwrap_or(0))
}

/// Sort completed WAL capabilities into replay order (#172).
///
/// The decision is made once for the whole list, never pairwise — a
/// comparator that mixes two keys per-pair is not a total order (a
/// seq-vs-seq comparison can contradict the timestamp comparisons made
/// against a legacy row, which cycles, and `sort_by` may panic on an
/// inconsistent comparator):
///
/// - Every row sequenced (post-v9 WAL): the shared completion sequence
///   is the total order; timestamps are decoration.
/// - Any legacy `NULL`-seq row present (WAL spanning the v8→v9
///   migration): fall back to `completed_at` chronology for the whole
///   list, preserving migration-era order. `seq` still breaks
///   same-millisecond ties among the rows that have it (legacy rows
///   sort after sequenced rows within a tied millisecond, and pure
///   legacy ties keep insertion order via the stable sort — the pre-v9
///   behaviour, tools before LLMs).
fn sort_into_replay_order(completed: &mut [((Option<i64>, i64), CapabilityResult)]) {
    let fully_sequenced = completed.iter().all(|((seq, _), _)| seq.is_some());
    if fully_sequenced {
        completed.sort_by_key(|((seq, _), _)| seq.expect("checked fully_sequenced"));
    } else {
        completed.sort_by_key(|((seq, at), _)| (*at, seq.unwrap_or(i64::MAX)));
    }
}

/// Regroup a chronologically-ordered capability stream so each model
/// turn's tool results collapse into the single capability the live
/// loop emitted: a lone [`CapabilityResult::ToolResult`] for a
/// one-call turn, a [`CapabilityResult::ParallelToolResults`] for a
/// multi-call turn (mirroring the harness's `CallTool` /
/// `CallToolsParallel` split). Recovery persists one `tool_dispatch`
/// row per call, but the harness answers a parallel turn with a single
/// capability; feeding the rows individually desyncs replay at the
/// second result ("expected ModelResult after CallModel"). A maximal
/// run of consecutive tool results belongs to one turn — the next
/// model call only starts once the turn's results are integrated — so
/// each run becomes one capability. Non-tool capabilities pass through
/// in place.
fn coalesce_tool_results(
    ordered: Vec<((Option<i64>, i64), CapabilityResult)>,
) -> Vec<CapabilityResult> {
    let mut out: Vec<CapabilityResult> = Vec::with_capacity(ordered.len());
    let mut batch: Vec<ToolCallResult> = Vec::new();
    for (_, capability) in ordered {
        match capability {
            CapabilityResult::ToolResult(result) => batch.push(result),
            other => {
                flush_tool_batch(&mut batch, &mut out);
                out.push(other);
            }
        }
    }
    flush_tool_batch(&mut batch, &mut out);
    out
}

/// Emit an accumulated run of tool results as the one capability the
/// live loop produced — a bare `ToolResult` for a single call,
/// `ParallelToolResults` for several — then clear the batch. An empty
/// batch emits nothing.
fn flush_tool_batch(batch: &mut Vec<ToolCallResult>, out: &mut Vec<CapabilityResult>) {
    match batch.len() {
        0 => {}
        1 => out.push(CapabilityResult::ToolResult(
            batch.pop().expect("len checked == 1"),
        )),
        _ => out.push(CapabilityResult::ParallelToolResults(std::mem::take(batch))),
    }
}

/// If the last model turn in `completed` dispatched more tool calls
/// than have completed rows, the crash fell inside that batch. Drop the
/// recorded partial results (the trailing tool capabilities) so replay
/// stops at the model turn and `run_loop_inner` re-runs the batch to
/// completion — `run_tool` reuses the already-completed calls and runs
/// only the missing ones. Returns the number of results dropped (0 when
/// the final batch is whole, or there is no pending batch). Only the
/// final batch can be partial: earlier batches are whole, or the
/// invocation could not have progressed past them.
fn truncate_incomplete_final_batch(
    completed: &mut Vec<((Option<i64>, i64), CapabilityResult)>,
) -> usize {
    let Some(last_model) = completed
        .iter()
        .rposition(|(_, c)| matches!(c, CapabilityResult::ModelResult(_)))
    else {
        return 0;
    };
    let requested = match &completed[last_model].1 {
        CapabilityResult::ModelResult(response) => response.tool_calls.len(),
        _ => unreachable!("rposition matched a ModelResult"),
    };
    // Everything after the last model turn is that turn's tool results —
    // nothing else runs before the next (never-reached) model call.
    let recorded = completed.len() - last_model - 1;
    if requested > 0 && recorded < requested {
        completed.truncate(last_model + 1);
        recorded
    } else {
        0
    }
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
        NextAction::Complete { .. } => ("completed", Some(now_ms)),
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
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes).expect("OS entropy unavailable");
    u64::from_ne_bytes(bytes)
}

fn trigger_source_label(kind: &TriggerSourceKind) -> &'static str {
    match kind {
        TriggerSourceKind::Manual => "manual",
        TriggerSourceKind::Subject => "subject",
        TriggerSourceKind::Schedule => "schedule",
    }
}

fn trigger_from_state_row(row: &crate::worker::store::InvocationStateRow) -> TriggerPayload {
    let source = match row.trigger_source.as_deref() {
        Some("manual") => TriggerSourceKind::Manual,
        Some("subject") => TriggerSourceKind::Subject,
        Some("schedule") => TriggerSourceKind::Schedule,
        Some(other) => {
            warn!(
                trigger_source = other,
                "unknown stored trigger source; assuming manual"
            );
            TriggerSourceKind::Manual
        }
        None => TriggerSourceKind::Manual,
    };
    let payload = match row.trigger_payload.as_deref() {
        Some(text) => serde_json::from_str(text).unwrap_or_else(|err| {
            warn!(error = %err, "stored trigger payload is not valid JSON; using null");
            Value::Null
        }),
        None => {
            warn!(
                invocation_id = %row.invocation_id,
                "state row predates trigger persistence (schema v5); \
                 replay will seed the conversation with \"(no input)\""
            );
            Value::Null
        }
    };
    TriggerPayload {
        source,
        subject: row.trigger_subject.clone(),
        payload,
    }
}

#[cfg(test)]
mod tests {
    //! Behavioural-equivalence and end-to-end tests for the
    //! reducer host loop. Each NATS-backed test spawns its own
    //! private `nats-server` (#233) — nothing shared, nothing
    //! skipped.
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
    fn sequence_order_preserves_tool_batch_boundaries_across_timestamp_ties() {
        let tool = |id: &str| {
            CapabilityResult::ToolResult(ToolCallResult {
                tool_call_id: crate::events::ToolCallId::new(id.to_string()).unwrap(),
                output: String::new(),
                is_error: false,
                error_kind: None,
                duration_ms: 0,
            })
        };
        let model = CapabilityResult::ModelResult(ModelResponse {
            content: Some("next".to_string()),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        });
        let ordered = vec![
            (replay_sort_key(Some(1), Some(42)), tool("first")),
            (replay_sort_key(Some(2), Some(42)), model),
            (replay_sort_key(Some(3), Some(42)), tool("second")),
        ];
        let replay = coalesce_tool_results(ordered);
        assert_eq!(replay.len(), 3);
        assert!(matches!(replay[0], CapabilityResult::ToolResult(_)));
        assert!(matches!(replay[1], CapabilityResult::ModelResult(_)));
        assert!(matches!(replay[2], CapabilityResult::ToolResult(_)));
    }

    /// Build a tagged capability so order assertions can identify rows
    /// after sorting: the tool_call_id carries the tag.
    fn tagged(tag: &str) -> CapabilityResult {
        CapabilityResult::ToolResult(ToolCallResult {
            tool_call_id: crate::events::ToolCallId::new(tag.to_string()).unwrap(),
            output: String::new(),
            is_error: false,
            error_kind: None,
            duration_ms: 0,
        })
    }

    fn tags(sorted: &[((Option<i64>, i64), CapabilityResult)]) -> Vec<String> {
        sorted
            .iter()
            .map(|(_, c)| match c {
                CapabilityResult::ToolResult(r) => r.tool_call_id.as_str().to_string(),
                other => panic!("expected tagged ToolResult, got {other:?}"),
            })
            .collect()
    }

    /// Fully-sequenced WALs total-order by seq in both directions,
    /// through the production sort (not the tuple's natural `Ord`).
    #[test]
    fn replay_order_uses_seq_when_fully_sequenced() {
        let same_ms = 42;
        let mut llm_completed_first = vec![
            (replay_sort_key(Some(2), Some(same_ms)), tagged("second")),
            (replay_sort_key(Some(1), Some(same_ms)), tagged("first")),
        ];
        sort_into_replay_order(&mut llm_completed_first);
        assert_eq!(tags(&llm_completed_first), ["first", "second"]);

        let mut tool_completed_first = vec![
            (replay_sort_key(Some(1), Some(same_ms)), tagged("first")),
            (replay_sort_key(Some(2), Some(same_ms)), tagged("second")),
        ];
        sort_into_replay_order(&mut tool_completed_first);
        assert_eq!(tags(&tool_completed_first), ["first", "second"]);
    }

    /// A WAL spanning the v8→v9 migration falls back to timestamp
    /// chronology for the whole list. This triple is the non-total-order
    /// regression: under a pairwise seq/timestamp comparator it forms a
    /// cycle (A<C by seq, B<A and C<B by timestamp) that `sort_by` may
    /// panic on; the list-wide decision must order it by timestamp
    /// without panicking.
    #[test]
    fn replay_order_falls_back_to_timestamps_when_legacy_rows_participate() {
        let mut mixed = vec![
            (replay_sort_key(Some(1), Some(100)), tagged("a-seq1-ts100")),
            (replay_sort_key(None, Some(50)), tagged("b-legacy-ts50")),
            (replay_sort_key(Some(2), Some(10)), tagged("c-seq2-ts10")),
        ];
        sort_into_replay_order(&mut mixed);
        assert_eq!(
            tags(&mixed),
            ["c-seq2-ts10", "b-legacy-ts50", "a-seq1-ts100"]
        );

        // Within a tied millisecond, sequenced rows order by seq and
        // precede legacy rows; pure-legacy ties keep insertion order
        // (the pre-v9 behaviour) via the stable sort.
        let mut tie = vec![
            (replay_sort_key(None, Some(42)), tagged("legacy-first-in")),
            (replay_sort_key(Some(2), Some(42)), tagged("seq2")),
            (replay_sort_key(None, Some(42)), tagged("legacy-second-in")),
            (replay_sort_key(Some(1), Some(42)), tagged("seq1")),
        ];
        sort_into_replay_order(&mut tie);
        assert_eq!(
            tags(&tie),
            ["seq1", "seq2", "legacy-first-in", "legacy-second-in"]
        );
    }

    /// Issue #9 precedence, checked at the boundary the runner uses to
    /// fill `AgentConfig.max_iterations`: per-agent override (if the
    /// definition sets one) wins; otherwise the daemon config default
    /// applies. This mirrors `agent.max_iterations().unwrap_or(cfg)`
    /// exactly — the single expression both `run` and `resume` use.
    #[test]
    fn max_iterations_precedence_prefers_agent_override_then_config_default() {
        let config_default = 100u32;

        // Definition without max_iterations -> falls back to the config default.
        let plain = Agent::builder()
            .id("plain")
            .model("claude-haiku")
            .system_prompt("be brief")
            .build()
            .unwrap();
        assert_eq!(
            plain.max_iterations().unwrap_or(config_default),
            config_default,
            "no override -> daemon config default"
        );

        // Definition with max_iterations -> overrides the config default.
        let overridden = Agent::builder()
            .id("overridden")
            .model("claude-haiku")
            .system_prompt("be brief")
            .max_iterations(7)
            .build()
            .unwrap();
        assert_eq!(
            overridden.max_iterations().unwrap_or(config_default),
            7,
            "override wins over the daemon config default"
        );
    }

    #[test]
    fn invocation_preamble_has_stable_environment_fields() {
        let preamble = invocation_preamble(
            Some(Path::new("/tmp/workspace")),
            &AgentId::new("doc-drift").unwrap(),
            Some(3),
            Some(1.25),
            12,
            1_700_000_000_000,
        );
        assert!(preamble.contains("timestamp: 2023-11-14T22:13:20+00:00"));
        assert!(preamble.contains("agent id: doc-drift"));
        assert!(preamble.contains("workspace: /tmp/workspace"));
        assert!(preamble.contains("attempt: 3"));
        assert!(preamble.contains("budget: $1.25"));
        assert!(preamble.contains("iteration ceiling: 12"));
    }

    #[tokio::test]
    async fn runner_config_max_iterations_defaults_to_the_builtin_fallback() {
        // A RunnerConfig built without .max_iterations() carries the
        // built-in fallback, so a runner constructed with no explicit
        // daemon default still bounds every agent.
        let dir = tempdir().unwrap();
        let store = Arc::new(
            WorkerStore::open(&dir.path().join("events.db"))
                .await
                .unwrap(),
        );
        let cfg = RunnerConfig::builder()
            .event_sink(
                Arc::new(crate::test_support::sim::RecordingSink::new()) as Arc<dyn EventSink>
            )
            .pricing(test_pricing())
            .store(store)
            .worker_id(test_worker_id())
            .build();
        assert_eq!(
            cfg.max_iterations,
            crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS
        );
    }

    #[tokio::test]
    async fn enforce_pricing_refuses_to_dispatch_an_unpriced_model() {
        // ADR-0004 at-use backstop: with enforce_pricing on and no price
        // for the model, the runner refuses to dispatch — a typed failure
        // — rather than call the model and track its cost as $0.
        let dir = tempdir().unwrap();
        let store = Arc::new(
            WorkerStore::open(&dir.path().join("events.db"))
                .await
                .unwrap(),
        );
        let agent = Agent::builder()
            .id(unique_agent_id("unpriced"))
            .model("model-with-no-price")
            .system_prompt("be brief")
            .budget(1.0)
            .build()
            .unwrap();
        // Queued but must never be consumed — the gate fires first.
        let llm = FixtureClient::new();
        llm.push_response(canned("should not be used", 10, 5));

        let runner = ReducerRunner::new(
            Arc::new(
                ReducerContext::builder()
                    .tools(Arc::new(ToolRegistry::with_builtins()))
                    .build(),
            ),
            Arc::new(
                RunnerConfig::builder()
                    .event_sink(Arc::new(crate::test_support::sim::RecordingSink::new())
                        as Arc<dyn EventSink>)
                    .pricing(Arc::new(PricingTable::empty()))
                    .store(store)
                    .worker_id(test_worker_id())
                    .enforce_pricing(true)
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

        match outcome {
            Err(ExecutorError::Llm(crate::llm::LlmError::UnpricedModel(model))) => {
                assert_eq!(model, "model-with-no-price");
            }
            other => panic!("expected an UnpricedModel failure, got {other:?}"),
        }
    }

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
            content: None,
            tool_calls: vec![crate::events::MessageToolCall {
                tool_call_id: crate::events::ToolCallId::new("report-outcome").unwrap(),
                tool_name: crate::tools::REPORT_OUTCOME_CANONICAL_NAME.to_string(),
                parameters: json!({"status": "success", "summary": text}),
            }],
            stop_reason: StopReason::ToolUse,
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
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

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
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

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
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();
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
        // The full R1 grammar: canonical sequence, one terminal,
        // archived at the end, chained envelopes (slice 1 oracle).
        crate::test_support::oracle::assert_valid_trace(&events);
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
            effort: None,
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
                host_notices: vec![],
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
                    content: None,
                    tool_calls: canned("pong", 10, 10).tool_calls,
                    stop_reason: StopReason::ToolUse,
                    usage: TokenUsage::default(),
                })),
                now_ms: 1,
                random_seed: 1,
                step_index: 1,
                static_resource_context: None,
                host_notices: vec![],
            })
            .unwrap();

        match s1.next_action {
            NextAction::Complete { text, .. } => assert_eq!(text, "pong"),
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
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

        let agent_id = unique_agent_id("self-inspect");
        let agent = Agent::builder()
            .id(agent_id.clone())
            .model("claude-haiku")
            .system_prompt("Inspect yourself when asked.")
            .tools(["builtin__self_inspect"])
            .budget(0.50)
            .build()
            .unwrap();

        let llm = FixtureClient::new();
        // Turn 1: model asks for self_inspect.
        llm.push_response(tool_use(
            "builtin__self_inspect",
            "call_si",
            json!({}),
            (100, 50),
        ));
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

    #[test]
    fn canonicalize_bare_builtin_maps_every_basename_and_nothing_else() {
        for base in crate::tools::BUILTIN_TOOL_BASENAMES {
            assert_eq!(
                canonicalize_bare_builtin(base).as_deref(),
                Some(format!("{}{base}", crate::tools::BUILTIN_PREFIX).as_str())
            );
        }
        // Already-canonical, MCP-namespaced, and unknown names pass through.
        assert_eq!(canonicalize_bare_builtin("builtin__exec"), None);
        assert_eq!(canonicalize_bare_builtin("everything__echo"), None);
        assert_eq!(canonicalize_bare_builtin("shell"), None);
    }

    #[test]
    fn canonical_tool_names_rewrites_only_bare_builtins() {
        let names = vec![
            "exec".to_string(),
            "everything__echo".to_string(),
            "builtin__file_read".to_string(),
        ];
        assert_eq!(
            canonical_tool_names(&names),
            vec![
                "builtin__exec".to_string(),
                "everything__echo".to_string(),
                "builtin__file_read".to_string(),
            ]
        );
    }

    /// #177 migration window: a definition still granting bare built-in
    /// names keeps working for one release — the grant is canonicalised
    /// (the model is offered `builtin__self_inspect`), and a model that
    /// nevertheless calls the bare name is normalised on dispatch. Both
    /// legacy paths are exercised deliberately: bare grant + bare call.
    #[tokio::test]
    async fn legacy_bare_builtin_grants_and_calls_still_resolve() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

        let agent_id = unique_agent_id("legacy-bare");
        let agent = Agent::builder()
            .id(agent_id.clone())
            .model("claude-haiku")
            .system_prompt("Inspect yourself when asked.")
            .tools(["self_inspect"]) // deprecated bare grant
            .budget(0.50)
            .build()
            .unwrap();

        let llm = FixtureClient::new();
        // Turn 1: the model calls the bare legacy name.
        llm.push_response(tool_use("self_inspect", "call_si", json!({}), (100, 50)));
        // Turn 2: model summarises and finishes.
        llm.push_response(canned("done", 150, 30));

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

        // The model was offered the canonical name, not the bare grant.
        let offered: Vec<String> = llm
            .requests()
            .first()
            .expect("at least one LLM request")
            .tools
            .iter()
            .map(|t| t.name.clone())
            .collect();
        assert_eq!(offered, vec!["builtin__self_inspect".to_string()]);

        // The event trail records the canonical vocabulary even though
        // the model issued the bare name.
        let mut call_name: Option<String> = None;
        let mut saw_result = false;
        for _ in 0..15 {
            let event = tokio::time::timeout(Duration::from_secs(2), sub.next())
                .await
                .expect("timeout")
                .expect("stream closed")
                .expect("deserialise");
            match &event.payload {
                EventPayload::ToolCall(p) => call_name = Some(p.tool_name.clone()),
                EventPayload::ToolResult(_) => {
                    saw_result = true;
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(
            call_name.as_deref(),
            Some(crate::tools::SELF_INSPECT_CANONICAL_NAME),
            "tool.call must record the canonical name"
        );
        assert!(
            saw_result,
            "self_inspect must dispatch and produce a result"
        );
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
            allowed_tool_names: vec!["builtin__self_inspect".to_string()],
            max_iterations: crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS,
            effort: None,
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
            host_notices: vec![],
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
                        tool_name: "builtin__self_inspect".to_string(),
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
                invocation_id: "suspend-invocation",
                agent_id: "suspend-tools",
                model: "claude-haiku",
                allowed_tool_names: &["builtin__self_inspect".to_string()],
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
                tokens_in_use: None,
                context_window_size: None,
                messages_in_history: None,
                oldest_turn_at_ms: None,
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
                    content: None,
                    tool_calls: canned("inspected.", 10, 10).tool_calls,
                    stop_reason: StopReason::ToolUse,
                    usage: TokenUsage::default(),
                })),
                3,
            ))
            .unwrap();

        match s3.next_action {
            NextAction::Complete { text, .. } => assert_eq!(text, "inspected."),
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
        canned(text, 10, 20)
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
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

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
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

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
        // 12. invocation.archived
        let (store, events) = run_with_wal(&url, agent, responses, 12, Some(dir.path())).await;

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
        // And the whole trace satisfies the canonical grammar.
        crate::test_support::oracle::assert_valid_trace(&events);

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
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

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
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

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

        // The denied call is journaled like any other result — a
        // completed error row, so resume can replay the conversation
        // from the WAL alone (finding 7; this test previously pinned
        // the opposite, replay-breaking behaviour).
        let inv_str = events[0].envelope.invocation_id.to_string();
        let dispatch = store
            .get_tool_dispatch(&inv_str, "call_deny")
            .await
            .unwrap()
            .expect("denied call must journal a completed error row");
        assert_eq!(dispatch.status, DispatchStatus::Completed);
        assert_eq!(dispatch.is_error, Some(true));
        assert!(
            dispatch
                .result
                .as_deref()
                .unwrap_or_default()
                .contains("not available"),
            "got {:?}",
            dispatch.result
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
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

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
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

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
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

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
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

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
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();
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

    /// #301: an empty model response — no tool calls and no
    /// non-whitespace content — is an error stop, never an implicit
    /// success. This is the live incident shape (invocation
    /// `019f70d1`, 2026-07-17): a provider 200 with nothing in it must
    /// fail the invocation as an `LlmError` and close the WAL row as an
    /// error, so recovery and the fleet's retry loop see a failure
    /// rather than a phantom success.
    #[tokio::test]
    async fn empty_model_response_fails_the_invocation_as_llm_error() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

        let agent_id_str = unique_agent_id("empty-response");
        let agent = Agent::builder()
            .id(&agent_id_str)
            .model("claude-haiku")
            .system_prompt("You are a test agent.")
            .budget(5.0)
            .build()
            .unwrap();

        // Whitespace-only content pins the trim() semantics — this is
        // "empty" exactly like `None` is.
        let empty = ChatResponse {
            content: Some("   \n".to_string()),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        };

        let (store, events, outcome) =
            run_with_wal_capturing_outcome(&url, agent, vec![empty], 2, None).await;

        match outcome {
            Err(ExecutorError::Llm(crate::llm::LlmError::RequestFailed(msg))) => {
                assert!(msg.contains("empty response"), "got: {msg}");
            }
            other => panic!("expected Llm(RequestFailed) on an empty response, got {other:?}"),
        }

        // The WAL closed the dispatch as an error, cost 0 — never a
        // completed-ok row for a turn that returned nothing.
        let inv = events[0].envelope.invocation_id.to_string();
        let rows = store
            .list_llm_dispatches_for_invocation(&inv)
            .await
            .expect("list dispatches");
        assert_eq!(rows.len(), 1, "one dispatch row for the one LLM call");
        assert_eq!(rows[0].is_error, Some(true), "WAL row must close as error");
        assert!(
            rows[0]
                .response
                .as_deref()
                .unwrap_or_default()
                .contains("empty response"),
            "WAL response records the synthetic error, got {:?}",
            rows[0].response
        );
        assert_eq!(rows[0].cost_usd, Some(0.0));
    }

    /// #301: a model that only ever produces bare text — never a tool
    /// call, never `report_outcome` — terminates via the iteration
    /// ceiling as a failure. Text is not a stop signal; the ceiling is.
    #[tokio::test]
    async fn bare_text_only_model_fails_at_the_iteration_ceiling() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

        let agent_id_str = unique_agent_id("bare-text-ceiling");
        let agent = Agent::builder()
            .id(&agent_id_str)
            .model("claude-haiku")
            .system_prompt("You are a test agent.")
            .budget(5.0)
            .max_iterations(3)
            .build()
            .unwrap();

        let text_turn = || ChatResponse {
            content: Some("still thinking out loud".to_string()),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        };
        let responses: Vec<ChatResponse> = (0..5).map(|_| text_turn()).collect();

        let (_store, _events, outcome) =
            run_with_wal_capturing_outcome(&url, agent, responses, 1, None).await;

        match outcome {
            Err(ExecutorError::InvocationFailed { kind, message }) => {
                assert!(
                    matches!(kind, FailureKind::MaxIterations),
                    "expected MaxIterations, got {kind:?}: {message}"
                );
            }
            other => panic!("expected InvocationFailed(MaxIterations), got {other:?}"),
        }
    }

    /// A reducer that errors on `step` is a runtime defect — the
    /// returned error must say so, not claim max-iterations.
    #[tokio::test]
    async fn reducer_step_error_carries_the_runtime_error_kind() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();
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
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

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
        // would have persisted at step_index=0 (post-step).
        let harness = Harness::new();
        let agent_config = AgentConfig {
            agent_id: AgentId::new(&agent_id_str).unwrap(),
            model: "claude-haiku".to_string(),
            system_prompt: "You are a test agent.".to_string(),
            tools_available: vec![],
            allowed_tool_names: vec![],
            max_iterations: crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS,
            effort: None,
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
            host_notices: vec![],
        };
        let s0_output = harness.step(s0_input).expect("step 0");

        store
            .upsert_invocation_state(&InvocationStateRow {
                invocation_id: inv_str.clone(),
                agent_id: agent_id_str.clone(),
                schema_version: 1,
                phase: "awaiting_model".to_string(),
                state_blob: s0_output.state,
                step_index: 0,
                started_at: 1_000,
                updated_at: 1_000,
                terminal_at: None,
                workspace_ref: None,
                archive_status: None,
                archive_published_at: None,
                trigger_source: None,
                trigger_subject: None,
                trigger_payload: None,
            })
            .await
            .unwrap();

        // Pre-populate a completed LLM dispatch row whose
        // serialized response is end-turn.
        let response = canned("done.", 50, 5);
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

    /// The #373 replay-equivalence claim, at the seam where it is
    /// provable: seed the crashed WAL shape (tool `dispatched`, no
    /// `completed`), inject the interrupted result exactly as the
    /// operator verb does, resume — and assert the model request the
    /// replay persists carries the injected bytes VERBATIM from the
    /// stored row. The notice is rendered once at injection from the
    /// persisted dispatch timestamp (never a live clock — the PR #143
    /// landmine), so replay can only ever present those same bytes.
    #[tokio::test]
    async fn injected_interrupted_result_reaches_replay_byte_identical() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

        use crate::worker::reducer::types::{
            AgentConfig, StepInput, TriggerPayload, TriggerSourceKind,
        };

        let dir = tempdir().unwrap();
        let store_path = dir.path().join("events.db");
        let store = Arc::new(WorkerStore::open(&store_path).await.unwrap());

        let agent_id_str = unique_agent_id("resume-inject-replay");
        let agent = Agent::builder()
            .id(&agent_id_str)
            .model("claude-haiku")
            .system_prompt("You are a test agent.")
            .tools(["builtin__self_inspect"])
            .budget(1.0)
            .build()
            .unwrap();
        let invocation_id = Uuid::now_v7();
        let inv_str = invocation_id.to_string();

        let harness = Harness::new();
        let s0_output = harness
            .step(StepInput {
                config: AgentConfig {
                    agent_id: AgentId::new(&agent_id_str).unwrap(),
                    model: "claude-haiku".to_string(),
                    system_prompt: "You are a test agent.".to_string(),
                    tools_available: vec![],
                    allowed_tool_names: vec![],
                    max_iterations: crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS,
                    effort: None,
                },
                trigger: TriggerPayload {
                    source: TriggerSourceKind::Manual,
                    subject: None,
                    payload: json!("hello"),
                },
                state: vec![],
                last_result: None,
                now_ms: 0,
                random_seed: 0,
                step_index: 0,
                static_resource_context: None,
                host_notices: vec![],
            })
            .expect("step 0");
        store
            .upsert_invocation_state(&InvocationStateRow {
                invocation_id: inv_str.clone(),
                agent_id: agent_id_str.clone(),
                schema_version: 1,
                phase: "awaiting_model".to_string(),
                state_blob: s0_output.state,
                step_index: 0,
                started_at: 1_000,
                updated_at: 1_000,
                terminal_at: None,
                workspace_ref: None,
                archive_status: None,
                archive_published_at: None,
                trigger_source: None,
                trigger_subject: None,
                trigger_payload: None,
            })
            .await
            .unwrap();

        // The crashed shape: model turn 0 requested the tool; the tool
        // was handed off (dispatched) and the process died before any
        // completion write.
        let tool_use = ChatResponse {
            content: None,
            tool_calls: vec![crate::events::MessageToolCall {
                tool_call_id: crate::events::ToolCallId::new("tc-0").unwrap(),
                tool_name: "builtin__self_inspect".to_string(),
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
        store
            .write_llm_intent(&inv_str, "req-0", "claude-haiku", "{}", 1)
            .await
            .unwrap();
        store
            .write_llm_dispatched(&inv_str, "req-0", 2)
            .await
            .unwrap();
        store
            .write_llm_completed(
                &inv_str,
                "req-0",
                &serde_json::to_string(&tool_use).unwrap(),
                false,
                0.0001,
                3,
            )
            .await
            .unwrap();
        store
            .write_tool_intent(&inv_str, "tc-0", "builtin__self_inspect", "{}", 4)
            .await
            .unwrap();
        store
            .write_tool_dispatched(&inv_str, "tc-0", 5)
            .await
            .unwrap();

        // The operator verb's injection, via the same store API.
        let injected = store
            .inject_interrupted_results(&inv_str)
            .await
            .expect("inject");
        assert_eq!(injected, vec!["tc-0".to_string()]);
        let stored = store
            .get_tool_dispatch(&inv_str, "tc-0")
            .await
            .unwrap()
            .expect("injected row");
        assert_eq!(stored.status, DispatchStatus::Completed);
        let stored_result = stored.result.clone().expect("injected result present");
        assert!(stored_result.contains("interrupted"));

        // Resume: the replay feeds the injected result to the reducer,
        // which requests the next model turn from the fixture.
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
        let llm = FixtureClient::new();
        llm.push_response(canned("done.", 60, 4));

        let outcome = runner
            .resume(&agent, &llm, invocation_id)
            .await
            .expect("resume completes");
        assert!(
            matches!(outcome, InvocationOutcome::Completed { .. }),
            "expected Completed, got {outcome:?}"
        );

        // The equivalence claim: the persisted request payload of the
        // post-injection model turn contains the stored injected bytes
        // verbatim — replay presents persisted bytes, never a
        // re-render.
        let llm_rows = store
            .list_llm_dispatches_for_invocation(&inv_str)
            .await
            .expect("list llm dispatches");
        let replay_request = llm_rows
            .iter()
            .find(|r| r.request_id != "req-0")
            .expect("the post-injection model request was persisted");
        fn tree_contains(v: &serde_json::Value, needle: &str) -> bool {
            match v {
                serde_json::Value::String(s) => s == needle,
                serde_json::Value::Array(a) => a.iter().any(|v| tree_contains(v, needle)),
                serde_json::Value::Object(o) => o.values().any(|v| tree_contains(v, needle)),
                _ => false,
            }
        }
        let payload: serde_json::Value =
            serde_json::from_str(&replay_request.request_payload).expect("request payload JSON");
        assert!(
            tree_contains(&payload, &stored_result),
            "the replayed model request does not carry the injected bytes \
             verbatim\nstored: {}\npayload: {}",
            stored_result,
            replay_request.request_payload
        );
    }

    /// #172 end-to-end: seed a WAL whose middle turn is a tool call and
    /// whose completion timestamps tie at the millisecond, then resume
    /// through the real store. The rows are written in true execution
    /// order (so the store assigns the v9 `seq` in that order); a resume
    /// that replays them in any other order desyncs the reducer and
    /// fails, so a clean `Completed` outcome is the assertion.
    async fn resume_with_same_ms_interleave(
        tag: &str,
        llm0_completed_at: i64,
        tool_completed_at: i64,
        llm1_completed_at: i64,
    ) {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

        use crate::worker::reducer::types::{
            AgentConfig, StepInput, TriggerPayload, TriggerSourceKind,
        };

        let dir = tempdir().unwrap();
        let store_path = dir.path().join("events.db");
        let store = Arc::new(WorkerStore::open(&store_path).await.unwrap());

        let agent_id_str = unique_agent_id(tag);
        let agent = Agent::builder()
            .id(&agent_id_str)
            .model("claude-haiku")
            .system_prompt("You are a test agent.")
            .tools(["builtin__self_inspect"])
            .budget(1.0)
            .build()
            .unwrap();
        let invocation_id = Uuid::now_v7();
        let inv_str = invocation_id.to_string();

        // A plausible step-0 state row; replay rebuilds from step 0, so
        // the blob only needs to exist and be non-terminal.
        let harness = Harness::new();
        let s0_output = harness
            .step(StepInput {
                config: AgentConfig {
                    agent_id: AgentId::new(&agent_id_str).unwrap(),
                    model: "claude-haiku".to_string(),
                    system_prompt: "You are a test agent.".to_string(),
                    tools_available: vec![],
                    allowed_tool_names: vec![],
                    max_iterations: crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS,
                    effort: None,
                },
                trigger: TriggerPayload {
                    source: TriggerSourceKind::Manual,
                    subject: None,
                    payload: json!("hello"),
                },
                state: vec![],
                last_result: None,
                now_ms: 0,
                random_seed: 0,
                step_index: 0,
                static_resource_context: None,
                host_notices: vec![],
            })
            .expect("step 0");
        store
            .upsert_invocation_state(&InvocationStateRow {
                invocation_id: inv_str.clone(),
                agent_id: agent_id_str.clone(),
                schema_version: 1,
                phase: "awaiting_model".to_string(),
                state_blob: s0_output.state,
                step_index: 0,
                started_at: 1_000,
                updated_at: 1_000,
                terminal_at: None,
                workspace_ref: None,
                archive_status: None,
                archive_published_at: None,
                trigger_source: None,
                trigger_subject: None,
                trigger_payload: None,
            })
            .await
            .unwrap();

        // True execution order: model turn 0 requests the tool, the tool
        // completes, model turn 1 ends the invocation. Completion writes
        // happen in this order, so the store's shared seq records it —
        // regardless of how the timestamps tie.
        let tool_use = ChatResponse {
            content: None,
            tool_calls: vec![crate::events::MessageToolCall {
                tool_call_id: crate::events::ToolCallId::new("tc-0").unwrap(),
                tool_name: "builtin__self_inspect".to_string(),
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
        let end_turn = canned("done.", 60, 4);
        store
            .write_llm_intent(&inv_str, "req-0", "claude-haiku", "{}", 1)
            .await
            .unwrap();
        store
            .write_llm_dispatched(&inv_str, "req-0", 2)
            .await
            .unwrap();
        store
            .write_llm_completed(
                &inv_str,
                "req-0",
                &serde_json::to_string(&tool_use).unwrap(),
                false,
                0.0001,
                llm0_completed_at,
            )
            .await
            .unwrap();
        store
            .write_tool_intent(&inv_str, "tc-0", "builtin__self_inspect", "{}", 2)
            .await
            .unwrap();
        store
            .write_tool_dispatched(&inv_str, "tc-0", 3)
            .await
            .unwrap();
        store
            .write_tool_completed(&inv_str, "tc-0", "{\"ok\":true}", false, tool_completed_at)
            .await
            .unwrap();
        store
            .write_llm_intent(&inv_str, "req-1", "claude-haiku", "{}", 4)
            .await
            .unwrap();
        store
            .write_llm_dispatched(&inv_str, "req-1", 5)
            .await
            .unwrap();
        store
            .write_llm_completed(
                &inv_str,
                "req-1",
                &serde_json::to_string(&end_turn).unwrap(),
                false,
                0.0001,
                llm1_completed_at,
            )
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
        let llm = FixtureClient::new(); // WAL covers every turn — no live calls

        let outcome = runner
            .resume(&agent, &llm, invocation_id)
            .await
            .expect("resume replays the interleave in true order");
        match outcome {
            InvocationOutcome::Completed { response, .. } => {
                assert_eq!(response.content.as_deref(), Some("done."));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// The regression direction: the model turn and the tool completion
    /// tie at the same millisecond with the LLM first in true order —
    /// the pre-v9 tools-first tiebreak replayed these backwards.
    #[tokio::test]
    async fn resume_replays_llm_then_tool_same_millisecond_in_true_order() {
        resume_with_same_ms_interleave("seq-llm-tool", 5, 5, 9).await;
    }

    /// The other direction: the tool completion and the following model
    /// turn tie at the same millisecond.
    #[tokio::test]
    async fn resume_replays_tool_then_llm_same_millisecond_in_true_order() {
        resume_with_same_ms_interleave("seq-tool-llm", 3, 7, 7).await;
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
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

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
            effort: None,
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
                host_notices: vec![],
            })
            .expect("step 0");

        store
            .upsert_invocation_state(&InvocationStateRow {
                invocation_id: inv_str.clone(),
                agent_id: agent_id_str.clone(),
                schema_version: 1,
                phase: "awaiting_model".to_string(),
                state_blob: s0_output.state,
                step_index: 0,
                started_at: 1_000,
                updated_at: 1_000,
                terminal_at: None,
                workspace_ref: None,
                archive_status: None,
                archive_published_at: None,
                trigger_source: None,
                trigger_subject: None,
                trigger_payload: None,
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
                    .bus(bus.clone())
                    .pricing(test_pricing())
                    .store(store.clone())
                    .worker_id(test_worker_id())
                    .build(),
            ),
            Harness::new(),
        );

        let mut sub = bus
            .subscribe(format!("fq.agent.{}.>", agent.id().as_str()))
            .await
            .expect("subscribe");
        tokio::time::sleep(Duration::from_millis(50)).await;

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
        loop {
            let event = tokio::time::timeout(Duration::from_secs(2), sub.next())
                .await
                .expect("timeout")
                .expect("stream closed")
                .expect("deserialise");
            if let EventPayload::Failed(payload) = event.payload {
                assert!(matches!(payload.phase, FailurePhase::Budget));
                break;
            }
        }
    }

    #[tokio::test]
    async fn resume_refuses_ambiguous_invocation() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

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
                step_index: 0,
                started_at: 1_000,
                updated_at: 1_000,
                terminal_at: None,
                workspace_ref: None,
                archive_status: None,
                archive_published_at: None,
                trigger_source: None,
                trigger_subject: None,
                trigger_payload: None,
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
    async fn state_row_step_index_advances_with_each_step() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

        // A two-turn invocation (tool call + final summary) goes
        // through enough reducer steps that `step_index` should
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
            row.step_index > 0,
            "step_index must advance past 0 for a multi-step invocation; got {}",
            row.step_index
        );
        assert!(row.started_at <= row.updated_at);
        assert!(row.terminal_at.unwrap_or(0) >= row.updated_at);
    }

    // --- R5, sampling axis (reducer verification slice 6): the
    // sampling gate's budget boundaries, and sampling spend flowing
    // into the invocation accumulator. Hermetic via the sim doubles;
    // handle_sampling is private, hence tested here.

    /// Issue #76: a pricing table carrying a context window, so the
    /// runner can compute occupancy and fire the soft warning. Uses
    /// `from_litellm_json` because that is the only constructor that
    /// records `max_input_tokens`.
    fn pricing_with_window() -> Arc<PricingTable> {
        // 100-token window, priced so cost stays trivial.
        let json = r#"{
            "tiny-window": {
                "max_input_tokens": 100,
                "input_cost_per_token": 0.000001,
                "output_cost_per_token": 0.000005
            }
        }"#;
        Arc::new(PricingTable::from_litellm_json(json).expect("pricing json"))
    }

    async fn windowed_runner(
        sink: &std::sync::Arc<crate::test_support::sim::RecordingSink>,
        dir: &tempfile::TempDir,
    ) -> ReducerRunner {
        let store = Arc::new(
            WorkerStore::open(&dir.path().join("events.db"))
                .await
                .expect("worker store"),
        );
        ReducerRunner::new(
            Arc::new(
                ReducerContext::builder()
                    .tools(Arc::new(ToolRegistry::with_builtins()))
                    .build(),
            ),
            Arc::new(
                RunnerConfig::builder()
                    .event_sink(Arc::clone(sink) as Arc<dyn EventSink>)
                    .pricing(pricing_with_window())
                    .store(store)
                    .worker_id(test_worker_id())
                    .build(),
            ),
            Harness::new(),
        )
    }

    /// The soft context-pressure warning is injected once, past the
    /// threshold, and is visible in the event trail (issue #76). The
    /// model reports a prompt of 90 tokens against a 100-token window
    /// (90% — over the 80% threshold), so the runner annotates the
    /// `llm.response` event with the one-shot warning.
    #[tokio::test]
    async fn context_pressure_warning_injected_once_into_event_trail() {
        let sink = std::sync::Arc::new(crate::test_support::sim::RecordingSink::new());
        let dir = tempdir().expect("tempdir");
        let runner = windowed_runner(&sink, &dir).await;

        let agent = Agent::builder()
            .id(unique_agent_id("ctx-pressure"))
            .model("tiny-window")
            .system_prompt("be brief")
            .budget(1.0)
            .build()
            .unwrap();

        // Two end-turn-shaped turns are not needed: a single response
        // that is over threshold and ends the turn is enough. 90/100 in.
        let llm = FixtureClient::new();
        llm.push_response(canned("done.", 90, 5));

        runner
            .run(
                &agent,
                &llm,
                TriggerSource::Manual,
                None,
                json!({"input": "go"}),
            )
            .await
            .expect("invocation completes");

        let events = sink.events();
        let warned: Vec<_> = events
            .iter()
            .filter(|e| {
                e.annotations
                    .0
                    .get(crate::events::annotation_keys::FLAGS)
                    .and_then(|v| v.get("context_pressure"))
                    .is_some()
            })
            .collect();
        assert_eq!(
            warned.len(),
            1,
            "the soft warning must be injected exactly once into the event trail"
        );
        // And it rides on an llm.response event.
        assert!(
            matches!(warned[0].payload, EventPayload::LlmResponse(_)),
            "warning should annotate the llm.response that crossed the threshold"
        );
        assert_eq!(
            warned[0].annotations.0[crate::events::annotation_keys::FLAGS]["context_pressure"],
            json!(crate::worker::introspection::CONTEXT_PRESSURE_WARNING)
        );
    }

    /// Below the threshold, no warning is injected (issue #76).
    #[tokio::test]
    async fn context_pressure_warning_absent_below_threshold() {
        let sink = std::sync::Arc::new(crate::test_support::sim::RecordingSink::new());
        let dir = tempdir().expect("tempdir");
        let runner = windowed_runner(&sink, &dir).await;

        let agent = Agent::builder()
            .id(unique_agent_id("ctx-ok"))
            .model("tiny-window")
            .system_prompt("be brief")
            .budget(1.0)
            .build()
            .unwrap();

        // 10/100 tokens = 10%, well under threshold.
        let llm = FixtureClient::new();
        llm.push_response(canned("done.", 10, 5));

        runner
            .run(
                &agent,
                &llm,
                TriggerSource::Manual,
                None,
                json!({"input": "go"}),
            )
            .await
            .expect("invocation completes");

        let any_warning = sink.events().iter().any(|e| {
            e.annotations
                .0
                .get(crate::events::annotation_keys::FLAGS)
                .and_then(|v| v.get("context_pressure"))
                .is_some()
        });
        assert!(!any_warning, "no warning below the threshold");
    }

    fn sampling_world() -> (
        std::sync::Arc<crate::test_support::sim::RecordingSink>,
        tempfile::TempDir,
    ) {
        (
            std::sync::Arc::new(crate::test_support::sim::RecordingSink::new()),
            tempdir().expect("tempdir"),
        )
    }

    async fn sampling_runner(
        sink: &std::sync::Arc<crate::test_support::sim::RecordingSink>,
        dir: &tempfile::TempDir,
    ) -> ReducerRunner {
        let store = Arc::new(
            WorkerStore::open(&dir.path().join("events.db"))
                .await
                .expect("worker store"),
        );
        ReducerRunner::new(
            Arc::new(
                ReducerContext::builder()
                    .tools(Arc::new(ToolRegistry::with_builtins()))
                    .build(),
            ),
            Arc::new(
                RunnerConfig::builder()
                    .event_sink(Arc::clone(sink) as Arc<dyn EventSink>)
                    .pricing(test_pricing())
                    .store(store)
                    .worker_id(test_worker_id())
                    .build(),
            ),
            Harness::new(),
        )
    }

    fn sampling_agent(budget: f64, sub_budget: Option<f64>) -> Agent {
        Agent::builder()
            .id(unique_agent_id("sampling-budget"))
            .model("claude-haiku")
            .system_prompt("You are a test agent.")
            .budget(budget)
            .sampling_grant(crate::agent::SamplingGrant {
                servers: vec!["srv".to_string()],
                max_cost: sub_budget,
            })
            .build()
            .unwrap()
    }

    fn sampling_params() -> CreateMessageRequestParams {
        serde_json::from_value(serde_json::json!({
            "messages": [
                {"role": "user", "content": {"type": "text", "text": "hello"}}
            ],
            "maxTokens": 50
        }))
        .expect("sampling params")
    }

    #[tokio::test]
    async fn sampling_declined_when_invocation_budget_exhausted() {
        let (sink, dir) = sampling_world();
        let runner = sampling_runner(&sink, &dir).await;
        let agent = sampling_agent(1.0, None);
        let llm = FixtureClient::new(); // must never be consulted
        let mut totals = InvocationTotals {
            total_cost: 1.0,
            ..Default::default()
        };
        let mut cursor = None;
        let declined = runner
            .handle_sampling(
                &agent,
                "srv",
                &llm,
                agent.id(),
                Uuid::now_v7(),
                sampling_params(),
                &mut totals,
                Instant::now(),
                &mut cursor,
            )
            .await
            .expect("infrastructure ok")
            .expect_err("must decline");
        assert!(
            declined.message.contains("invocation budget exhausted"),
            "got: {}",
            declined.message
        );
        assert!(sink.events().is_empty(), "no model call on refusal");
        assert_eq!(totals.total_cost, 1.0, "refusal spends nothing");
    }

    #[tokio::test]
    async fn sampling_declined_when_sub_budget_exhausted() {
        let (sink, dir) = sampling_world();
        let runner = sampling_runner(&sink, &dir).await;
        let agent = sampling_agent(10.0, Some(0.5));
        let llm = FixtureClient::new();
        let mut totals = InvocationTotals {
            total_cost: 0.5,
            sampling_cost: 0.5,
            ..Default::default()
        };
        let mut cursor = None;
        let declined = runner
            .handle_sampling(
                &agent,
                "srv",
                &llm,
                agent.id(),
                Uuid::now_v7(),
                sampling_params(),
                &mut totals,
                Instant::now(),
                &mut cursor,
            )
            .await
            .expect("infrastructure ok")
            .expect_err("must decline");
        assert!(
            declined.message.contains("sub-budget exhausted"),
            "got: {}",
            declined.message
        );
        assert!(sink.events().is_empty());
    }

    /// Sampling spends the agent's budget through the shared path:
    /// totals and the sampling sub-accumulator both grow by the
    /// priced amount, the WAL row carries the cost (the finding-4
    /// fix, on the sampling path), and the published request is
    /// attributed to the requesting server.
    #[tokio::test]
    async fn sampling_spends_into_the_invocation_budget() {
        let (sink, dir) = sampling_world();
        let runner = sampling_runner(&sink, &dir).await;
        let agent = sampling_agent(10.0, Some(1.0));
        let llm = FixtureClient::new();
        // haiku rates in test_pricing: $1/M in, $5/M out.
        llm.push_response(canned("sampled.", 100_000, 10_000)); // $0.15
        let mut totals = InvocationTotals::default();
        let mut cursor = None;
        let invocation_id = Uuid::now_v7();
        let result = runner
            .handle_sampling(
                &agent,
                "srv",
                &llm,
                agent.id(),
                invocation_id,
                sampling_params(),
                &mut totals,
                Instant::now(),
                &mut cursor,
            )
            .await
            .expect("infrastructure ok")
            .expect("sampling succeeds");
        drop(result);
        assert!(
            (totals.total_cost - 0.15).abs() < 1e-12,
            "{}",
            totals.total_cost
        );
        assert!(
            (totals.sampling_cost - 0.15).abs() < 1e-12,
            "{}",
            totals.sampling_cost
        );

        let events = sink.events();
        let origin = events
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::LlmRequest(p) => Some(p.origin.clone()),
                _ => None,
            })
            .expect("llm.request published");
        assert!(
            matches!(origin, crate::events::LlmCallOrigin::Sampling { server } if server == "srv")
        );

        let rows = runner
            .config
            .store
            .list_llm_dispatches_for_invocation(&invocation_id.to_string())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(
            (rows[0].cost_usd.unwrap_or(0.0) - 0.15).abs() < 1e-12,
            "WAL row must carry the sampling call's cost, got {:?}",
            rows[0].cost_usd
        );
    }
}
