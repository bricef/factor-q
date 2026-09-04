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

use super::emit::FailedCall;
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
    self, AssistantPart, CompletedPayload, Event, EventPayload, FailedPayload, FailureKind,
    FailurePhase, HostNoticePayload, InvocationArchivedPayload, InvocationTotals, LlmCallOrigin,
    LlmRequestPayload, Message, RequestParams, StopReason, ToolCallPayload, ToolErrorKind,
    ToolSchema, TriggerSource, TriggeredPayload,
};
use crate::llm::{ChatRequest, ChatResponse, LlmClient};
use crate::mcp::{
    AdvertisedCapabilities, McpClientManager, McpResourceReader, McpServerConfig, ServerRequest,
    advertised_roots_from_tool_sandbox, elicitation_decline,
};
use crate::pricing::PricingTable;
use crate::tools::ToolRegistry;
use crate::trigger::Trigger;
use crate::validation::ValidatorChain;
use crate::worker::store::{
    DispatchStatus, InvocationStateRow, LlmDispatchRow, ToolDispatchRow, WorkerStore,
};
use crate::worker::workspace::{WORKSPACE_TOKEN, WorkspaceError, WorkspaceProvider};
use crate::worker::{DrainSignal, DurableStart, ExecutorError, InvocationOutcome, WorkerId};

use replay::{
    coalesce_tool_results, replay_sort_key, sort_into_replay_order, truncate_incomplete_final_batch,
};

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
    /// The current Round per driven invocation (Phase 3d) — see
    /// [`super::rounds::RoundLedger`].
    rounds: super::rounds::RoundLedger,
    /// What this runner is driving right now, and the halts armed
    /// against it — see [`super::liveness::LiveRegistry`]. The zero-lag
    /// authority behind both operator preconditions: resume refuses a
    /// live invocation (#373), and `invocation.drop` gates its kill
    /// switch on it and resolves the invocation from it (#107).
    live: super::liveness::LiveRegistry,
}

/// Triage guidance for an ambiguous WAL. Both verbs are real — `fq-cli`'s
/// `error_commands_gate` checks they still parse.
const AMBIGUOUS_WAL_TRIAGE: &str = "has ambiguous WAL state; triage with \
     `fq invocation resume <id>` to reconcile and continue, or \
     `fq invocation drop <id> --reason ...` to abandon it";

impl<R: Reducer + Send + Sync> ReducerRunner<R> {
    pub fn new(context: Arc<ReducerContext>, config: Arc<RunnerConfig>, reducer: R) -> Self {
        Self {
            context,
            config,
            reducer,
            drain: DrainSignal::new(),
            pending_notices: std::sync::Mutex::new(std::collections::HashMap::new()),
            rounds: super::rounds::RoundLedger::default(),
            live: super::liveness::LiveRegistry::default(),
        }
    }

    /// The agent this runner is driving `invocation_id` for, or `None`
    /// when it is not driving it. Liveness *and* identity from the one
    /// authority that has both with zero lag: a command that has
    /// established the invocation is live can name it without asking a
    /// store that may not have heard of it yet (#107).
    pub fn active_agent(&self, invocation_id: &Uuid) -> Option<AgentId> {
        self.live.agent_for(invocation_id)
    }

    /// Whether this runner is currently driving `invocation_id`. The
    /// operator-resume precondition's liveness authority (#373).
    pub fn is_active(&self, invocation_id: &Uuid) -> bool {
        self.live.is_active(invocation_id)
    }

    /// Request a halt at the next step boundary. Returns false without
    /// changing state when this runner is not currently driving the invocation.
    pub fn request_halt(&self, invocation_id: Uuid) -> bool {
        self.live.request_halt(invocation_id)
    }

    fn mark_active(
        &self,
        invocation_id: Uuid,
        agent_id: AgentId,
    ) -> super::liveness::ActiveInvocation<'_> {
        self.live.enter(invocation_id, agent_id, &self.rounds)
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
        // Direct callers (tests, sim) ack nothing, so the durable-start
        // signal has no waiter — and nothing published this trigger, so
        // nothing has named it: mint the identity here, which is where
        // the runtime takes responsibility for it.
        self.run_signalling(
            agent,
            llm,
            Trigger::mint(trigger_source, trigger_subject, trigger_payload),
            None,
            DurableStart::noop(),
        )
        .await
    }

    /// Like [`run`](Self::run) but fires `durable_start` once the
    /// invocation's first WAL write lands. The trigger dispatcher uses
    /// this (through the [`Worker`](crate::Worker) seam) to ack a
    /// trigger only after the run is recoverable from the WAL, closing
    /// the ack->first-WAL-write window (issue #41). The trigger arrives
    /// already named — the dispatcher honoured or assigned its identity
    /// before handing it over.
    pub async fn run_signalling(
        &self,
        agent: &Agent,
        llm: &dyn LlmClient,
        trigger: Trigger,
        delivery_attempt: Option<u32>,
        durable_start: DurableStart,
    ) -> Result<InvocationOutcome, ExecutorError> {
        self.run_loop_for(
            agent,
            llm,
            trigger,
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
            Trigger::mint(trigger_source, trigger_subject, trigger_payload),
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
        trigger: Trigger,
        delivery_attempt: Option<u32>,
        tools: &ToolRegistry,
        sampling: Option<SamplingChannel>,
        durable_start: DurableStart,
    ) -> Result<InvocationOutcome, ExecutorError> {
        let invocation_id = Uuid::now_v7();
        let agent_id: AgentId = agent.id().clone();
        let _active = self.mark_active(invocation_id, agent_id.clone());
        let start = Instant::now();
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
        let mut manager = McpClientManager::with_server_root(self.config.mcp_server_root.clone());
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
        let allowed_tool_names = effective_tool_names(agent.tools());
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

        let step_trigger = TriggerPayload {
            source: match trigger.source {
                TriggerSource::Manual => TriggerSourceKind::Manual,
                TriggerSource::Subject => TriggerSourceKind::Subject,
                TriggerSource::Schedule => TriggerSourceKind::Schedule,
            },
            subject: trigger.subject.clone(),
            payload: trigger.payload.clone(),
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
                        trigger_id: Some(trigger.id),
                        trigger_source: trigger.source,
                        trigger_subject: trigger.subject,
                        trigger_payload: trigger.payload,
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
                &step_trigger,
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
    /// `fq invocation resume`/`drop` per the §3.4 contract; the
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
        let _active = self.mark_active(invocation_id, agent.id().clone());
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
                "invocation {invocation_id} {AMBIGUOUS_WAL_TRIAGE}"
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
        let allowed_tool_names = effective_tool_names(agent.tools());
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
        (totals.total_llm_calls, totals.total_cost) =
            self.rounds.seed_from_wal(invocation_id, &llms);
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
                // in-flight sampling is surfaced via `fq invocation list`.
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
            // A live operator drop uses the same step-boundary semantics as
            // drain, but is invocation-scoped. The coordination consumer marks
            // the WAL row terminal from the operator-drop event, so this path
            // only stops the in-memory driver and emits no competing terminal.
            if self.live.take_halt(invocation_id) {
                info!(
                    agent_id = %agent_id,
                    invocation_id = %invocation_id,
                    step_index,
                    "operator halt — stopping invocation at step boundary"
                );
                return Ok(InvocationOutcome::Suspended { invocation_id });
            }

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
                        response: ChatResponse::completed(summary),
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
                    let mut ctx =
                        InvocationCtx::new(llm, agent_id, invocation_id, &mut totals, cursor);
                    let outcome = self
                        .run_model_with_llm(
                            &mut ctx,
                            agent.budget(),
                            request,
                            LlmCallOrigin::AgentTurn,
                            start,
                            &mut context,
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
        if !effective_tool_names(agent.tools())
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
                    round: self.rounds.current(invocation_id),
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
                                let mut ctx = InvocationCtx::new(
                                    llm, agent_id, invocation_id, totals, cursor,
                                );
                                self.handle_server_request(
                                    &mut ctx,
                                    agent,
                                    &server,
                                    request,
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
                    super::emit::tool_result_event(
                        self.rounds.current(invocation_id),
                        agent_id,
                        invocation_id,
                        &req,
                        result.output.clone(),
                        result.is_error,
                        None,
                        duration_ms,
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
                    super::emit::tool_result_event(
                        self.rounds.current(invocation_id),
                        agent_id,
                        invocation_id,
                        &req,
                        message.clone(),
                        true,
                        Some(kind),
                        duration_ms,
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
            super::emit::tool_result_event(
                self.rounds.current(invocation_id),
                agent_id,
                invocation_id,
                &req,
                output.clone(),
                false,
                None,
                duration_ms,
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
            super::emit::tool_result_event(
                self.rounds.current(invocation_id),
                agent_id,
                invocation_id,
                req,
                message.clone(),
                true,
                Some(kind),
                0,
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

/// What every model call needs to know about the invocation it belongs
/// to: who it is charged to, and the two accumulators it advances.
///
/// These five values were threaded individually through every function
/// below, which is why they all carried
/// `#[allow(clippy::too_many_arguments)]` (#78). Bundling them is not
/// cosmetic — it is what lets this group become its own module. Moved
/// as loose parameters, each of these functions would drag a 9-to-13
/// argument list across the boundary, which is a worse seam than no
/// seam.
///
/// Built fresh at each of the two places the host loop enters this
/// group (`run_loop_inner`'s `CallModel` arm and `run_tool`'s
/// server-request select). It borrows `totals` and `cursor` mutably, so
/// a short-lived value keeps those borrows from spanning the loop body;
/// everything inside this group forwards the same `ctx` rather than
/// rebuilding it.
pub(super) struct InvocationCtx<'a> {
    /// The client this invocation's calls go to.
    pub(super) llm: &'a dyn LlmClient,
    /// Whose spend and events these are.
    pub(super) agent_id: &'a AgentId,
    /// The invocation every emitted event is stamped with.
    pub(super) invocation_id: Uuid,
    /// Running cost and token totals, advanced by each call.
    pub(super) totals: &'a mut InvocationTotals,
    /// The event-chain cursor: the id of the last event emitted, which
    /// the next one records as its parent.
    pub(super) cursor: &'a mut Option<Uuid>,
}

impl<'a> InvocationCtx<'a> {
    /// Build the context for one entry into the model-calling group.
    ///
    /// A constructor rather than a struct literal at the call sites:
    /// the literal costs seven lines inside `run_loop_inner` and
    /// `run_tool`, both of which sit on a function-size budget that
    /// only ever tightens.
    pub(super) fn new(
        llm: &'a dyn LlmClient,
        agent_id: &'a AgentId,
        invocation_id: Uuid,
        totals: &'a mut InvocationTotals,
        cursor: &'a mut Option<Uuid>,
    ) -> Self {
        Self {
            llm,
            agent_id,
            invocation_id,
            totals,
            cursor,
        }
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
        parts: response.parts,
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

mod config;
mod failure;
mod llm;
mod replay;
mod server_request;

pub use config::{ReducerContext, ReducerContextBuilder, RunnerConfig, RunnerConfigBuilder};

#[cfg(test)]
mod tests;
mod tool_names;

pub(crate) use tool_names::{
    canonicalize_bare_builtin, effective_tool_names, warn_on_deprecated_bare_grants,
};
