//! The two read-only bundles a [`ReducerRunner`] is built from, and
//! their builders.
//!
//! Split from `runner.rs` (#78). This is construction only — no loop,
//! no IO, no decisions. It moved without waiting for the
//! `InvocationCtx` bundling the other extractions need, for the same
//! reason `replay` did: nothing here touches the invocation-scoped
//! parameter cluster.
//!
//! The split between the two is deliberate and predates this move:
//! [`ReducerContext`] is what the *agent* may use, [`RunnerConfig`] is
//! what the *platform* provides. A new dependency extends whichever
//! bundle it belongs to instead of re-signing `ReducerRunner::new`.

use std::path::PathBuf;

use super::*;

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
    pub(super) tools: std::sync::RwLock<Arc<ToolRegistry>>,
    /// Read-only handle over the running MCP servers, used to read
    /// the agent's `static_resources` pins at invocation start.
    /// `None` when no MCP servers are wired (e.g. most tests).
    pub(super) resources: Option<McpResourceReader>,
    /// Outbound validation seam for sampling results before they
    /// return to the requesting server (ADR-0018 §4): censor secrets,
    /// reject leakage, etc. Default is an empty chain (allow
    /// everything); concrete validators (e.g. a `HighEntropyRedactor`)
    /// are added without touching the runner.
    pub(super) sampling_validators: ValidatorChain<CreateMessageResult>,
    /// Inbound validation seam for elicitation requests (ADR-0018 §4):
    /// inspects the request's message and schema field names — a
    /// server can request `{ api_key: string }` and coax the model to
    /// fill it from context. Default empty (allow).
    pub(super) elicitation_inbound_validators: ValidatorChain<CreateElicitationRequestParams>,
    /// Outbound validation seam for the structured value an
    /// elicitation produced before it returns to the server: censor
    /// secrets in the extracted fields. Default empty (allow).
    pub(super) elicitation_outbound_validators: ValidatorChain<Value>,
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
    pub(super) sink: Arc<dyn EventSink>,
    /// Model→price lookup for cost accounting.
    pub(super) pricing: Arc<PricingTable>,
    /// Three-state WAL / invocation-state persistence
    /// (data-architecture.md §5.5).
    pub(super) store: Arc<WorkerStore>,
    /// Identity of the worker hosting this runner (coordination /
    /// archive-ack routing on `fq.worker.{worker_id}.*`).
    pub(super) worker_id: WorkerId,
    /// Time + entropy source. [`SystemClock`] in production; the sim
    /// injects a deterministic one.
    pub(super) clock: Arc<dyn Clock>,
    /// Daemon default cap on LLM turns per invocation. Used when an
    /// agent definition does not set its own `max_iterations` override
    /// (Design Principle 8 — tunable parameters are configuration,
    /// not code). Defaults to
    /// [`crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS`].
    pub(super) max_iterations: u32,
    /// When true, refuse to dispatch a model with no pricing entry
    /// (ADR-0004 at-use backstop) instead of tracking its cost as $0.
    /// The daemon sets this after its startup pricing guarantee has
    /// validated coverage; defaults to false so tests can run with an
    /// empty pricing table.
    pub(super) enforce_pricing: bool,
    /// Binds `${workspace}` per invocation (parallel-workers Phase 0).
    /// `None` (the default) leaves the token unbound: agents that don't
    /// use it are unaffected, agents that do fail loud at start.
    pub(super) workspace: Option<Arc<dyn WorkspaceProvider>>,
    /// Where the per-invocation (grant-bearing) stdio MCP servers get
    /// their working directories, `<root>/<server>` (#541). The daemon
    /// passes `<state dir>/mcp`, the same root its shared servers use;
    /// the default is the temp-dir root, never the process cwd.
    pub(super) mcp_server_root: PathBuf,
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
    mcp_server_root: Option<PathBuf>,
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

    /// Root for the per-invocation stdio MCP servers' working
    /// directories (#541). Optional; defaults to the temp-dir root the
    /// [`McpClientManager`](crate::McpClientManager) uses when given
    /// none. The daemon passes `<state dir>/mcp`.
    pub fn mcp_server_root(mut self, root: PathBuf) -> Self {
        self.mcp_server_root = Some(root);
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
            mcp_server_root: self
                .mcp_server_root
                .unwrap_or_else(crate::mcp::default_server_root),
        }
    }
}
