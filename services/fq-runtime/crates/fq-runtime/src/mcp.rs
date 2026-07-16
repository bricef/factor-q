//! MCP (Model Context Protocol) client support.
//!
//! Provides [`McpTool`], which adapts a tool from an external MCP server
//! into the [`fq_tools::Tool`] trait so it can be registered in the
//! [`ToolRegistry`] alongside built-in tools.
//!
//! [`McpClientManager`] owns the lifecycle of MCP server child processes:
//! starting them, discovering their tools, and shutting them down.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use fq_tools::{Tool, ToolContext, ToolError, ToolResult, ToolSandbox};
use rmcp::ClientHandler;
use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, CancelledNotificationParam,
    ClientCapabilities, ClientInfo, ClientRequest, CompletionContext, CompletionInfo,
    CreateElicitationRequestParams, CreateElicitationResult, CreateMessageRequestMethod,
    CreateMessageRequestParams, CreateMessageResult, ElicitationAction, ElicitationCapability,
    FormElicitationCapability, GetPromptRequestParams, GetPromptResult, JsonObject,
    ListRootsResult, LoggingLevel, LoggingMessageNotificationParam, Meta, NumberOrString,
    ProgressNotificationParam, ProgressToken, Prompt, PromptMessageContent, PromptMessageRole,
    ReadResourceRequestParams, ReadResourceResult, Resource, ResourceContents, ResourceTemplate,
    ResourceUpdatedNotificationParam, Root, RootsCapabilities, SamplingCapability,
    ServerCapabilities, ServerResult, SetLevelRequestParams, SubscribeRequestParams,
};
use rmcp::service::{
    MaybeSendFuture, NotificationContext, PeerRequestOptions, RequestContext, RoleClient,
    RunningService,
};
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::{debug, info, warn};

use fq_tools::builtin::ExecConfig;

use crate::agent::RootsGrant;
use crate::tools::ToolRegistry;
use crate::validation::ValidatorChain;

/// Configuration for an MCP server: a stdio child process (`command`),
/// or — when `url` is set — a remote server reached over the Streamable
/// HTTP transport (the 2025-11-25 spec remote transport).
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    /// Human-readable name for logging.
    pub name: String,
    /// Executable to spawn (stdio transport).
    pub command: String,
    /// Command-line arguments (stdio transport).
    pub args: Vec<String>,
    /// Environment variables to set on the child process (stdio
    /// transport).
    pub env: Vec<(String, String)>,
    /// When set, the server is reached over the Streamable HTTP remote
    /// transport at this URL instead of a stdio child process;
    /// `command` / `args` / `env` are then unused.
    pub url: Option<String>,
}

/// Errors from MCP server lifecycle and tool calls.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("failed to start MCP server '{command}': {reason}")]
    ServerStart { command: String, reason: String },

    #[error("tool discovery failed for '{command}': {reason}")]
    ToolDiscovery { command: String, reason: String },

    #[error("tool call to '{tool_name}' failed: {reason}")]
    ToolCall { tool_name: String, reason: String },

    #[error("no MCP server named '{name}' is running")]
    UnknownServer { name: String },

    #[error("resource operation on '{server}' failed: {reason}")]
    ResourceOp { server: String, reason: String },

    #[error("prompt operation on '{server}' failed: {reason}")]
    PromptOp { server: String, reason: String },

    #[error("roots operation on '{server}' failed: {reason}")]
    RootsOp { server: String, reason: String },
}

// Type alias for the concrete client handle we store.
type McpClient = RunningService<RoleClient, FactorQClientHandler>;

/// Monotonic source of per-request progress tokens (Step 7). Each
/// outbound tool call gets a fresh token so a server that supports
/// progress can report against it via `notifications/progress`.
static PROGRESS_TOKEN_SEQ: AtomicI64 = AtomicI64::new(1);

fn next_progress_token() -> ProgressToken {
    ProgressToken(NumberOrString::Number(
        PROGRESS_TOKEN_SEQ.fetch_add(1, Ordering::Relaxed),
    ))
}

/// Render a progress token to a string for the neutral
/// [`ServerNotification::Progress`] (tokens are numeric here, but a
/// server may echo a string token).
fn progress_token_string(token: &ProgressToken) -> String {
    match &token.0 {
        NumberOrString::Number(n) => n.to_string(),
        NumberOrString::String(s) => s.to_string(),
    }
}

/// An out-of-band notification forwarded from a connected MCP server
/// to the host's notification sink: resource changes, capability-list
/// changes, log records, and progress (Step 7). The host drains these
/// from the per-server channel (see
/// [`McpClientManager::recv_notification`]) to react — refresh stale
/// caches, fold logs into tracing, surface progress, etc.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerNotification {
    /// A subscribed resource changed (`notifications/resources/updated`).
    ResourceUpdated { uri: String },
    /// The server's resource list changed
    /// (`notifications/resources/list_changed`).
    ResourceListChanged,
    /// The server's tool list changed
    /// (`notifications/tools/list_changed`).
    ToolListChanged,
    /// The server's prompt list changed
    /// (`notifications/prompts/list_changed`).
    PromptListChanged,
    /// A log record from the server (`notifications/message`). `level`
    /// is the MCP level name (`"debug"`..`"emergency"`).
    Log {
        level: String,
        logger: Option<String>,
        data: Value,
    },
    /// Progress on an in-flight request (`notifications/progress`),
    /// keyed by the `token` the host attached when issuing it.
    Progress {
        token: String,
        progress: f64,
        total: Option<f64>,
        message: Option<String>,
    },
}

/// A request a connected MCP server initiates back toward the host
/// *mid-invocation* (ADR-0018).
///
/// The handler ([`FactorQClientHandler`]) is a thin bridge: it
/// translates an inbound rmcp request into one of these variants,
/// forwards it on a per-invocation channel, and awaits the host's
/// reply on the embedded oneshot. The runner is the sole arbiter —
/// it gates, runs the LLM call through its single budgeted/WAL'd
/// path, validates the result, and replies. Step 5 wires the
/// sampling arm; Step 6 adds an `Elicitation` variant to the same
/// channel and `select!` arm.
pub enum ServerRequest {
    /// `sampling/createMessage` — the server asks the host to run an
    /// LLM completion and return the result to the *server* (not the
    /// agent's transcript). `reply` carries either the result or a
    /// structured decline (e.g. ungranted / over-budget); dropping
    /// the sender declines with `method_not_found`.
    Sampling {
        params: CreateMessageRequestParams,
        reply: oneshot::Sender<Result<CreateMessageResult, rmcp::ErrorData>>,
    },
    /// `elicitation/create` — the server asks for structured user
    /// input matching a schema. factor-q answers it autonomously on
    /// the agent's model (ADR-0017); `reply` carries the result, whose
    /// `action` is `accept` (with content) or `decline`. A dropped
    /// sender declines.
    Elicitation {
        params: CreateElicitationRequestParams,
        reply: oneshot::Sender<Result<CreateElicitationResult, rmcp::ErrorData>>,
    },
}

/// The structured decline an elicitation request resolves to when
/// refused (ungranted, over-budget, retries exhausted) or when the
/// host cannot service it. Per the protocol this is an ordinary
/// result with `action: decline`, not an error — the server continues
/// without the input.
pub(crate) fn elicitation_decline() -> CreateElicitationResult {
    CreateElicitationResult {
        action: ElicitationAction::Decline,
        content: None,
        meta: None,
    }
}

/// A host-side handle to a per-invocation server's advertised roots
/// (ADR-0018). Holds the shared roots cell the handler reads on
/// `roots/list`, plus the client to fire `roots/list_changed`. Lets
/// the host update the advertised workspace (e.g. when the agent's
/// sandbox changes) and notify the server, which re-fetches.
pub struct RootsHandle {
    server: String,
    roots: Arc<Mutex<Vec<Root>>>,
    client: Arc<McpClient>,
}

impl RootsHandle {
    /// Replace the advertised roots and notify the server via
    /// `roots/list_changed` so it re-fetches. The full dynamic-workspace
    /// trigger (recomputing from a changed sandbox) is a later
    /// "Workspace state" concern; this exposes the mechanism.
    pub async fn set_roots(&self, roots: Vec<Root>) -> Result<(), McpError> {
        *self.roots.lock().await = roots;
        self.client
            .notify_roots_list_changed()
            .await
            .map_err(|err| McpError::RootsOp {
                server: self.server.clone(),
                reason: err.to_string(),
            })
    }
}

/// Derive the `file://` roots a server should be advertised from the
/// invocation's **materialised** tool sandbox (ADR-0018): the union of
/// its read and write prefixes, deduplicated, each as a `file://` root
/// named by its path. Taking the [`ToolSandbox`] — the same object the
/// tools enforce against, with `${workspace}` already bound (#179) —
/// makes *advertised roots ⊆ enforced boundary* hold by construction;
/// the declared sandbox's raw strings may still carry placeholders.
/// `file://` only for v1.
pub fn roots_from_tool_sandbox(sandbox: &ToolSandbox) -> Vec<Root> {
    let mut seen = std::collections::BTreeSet::new();
    let mut roots = Vec::new();
    for path in sandbox
        .read_prefixes()
        .iter()
        .chain(sandbox.write_prefixes())
    {
        let path = path.to_string_lossy().into_owned();
        let uri = format!("file://{path}");
        if seen.insert(uri.clone()) {
            roots.push(Root::new(uri).with_name(path));
        }
    }
    roots
}

/// Compute the roots to advertise to `server`: nothing unless the
/// agent's [`RootsGrant`] permits it, otherwise the sandbox-derived
/// roots run through the outbound validator chain (ADR-0018 §4). A
/// `Deny` from the chain advertises nothing rather than a partial set.
pub fn advertised_roots_from_tool_sandbox(
    sandbox: &ToolSandbox,
    grant: Option<&RootsGrant>,
    server: &str,
    validators: &ValidatorChain<Vec<Root>>,
) -> Vec<Root> {
    if !grant.is_some_and(|g| g.permits(server)) {
        return Vec::new();
    }
    validators
        .run(roots_from_tool_sandbox(sandbox))
        .unwrap_or_default()
}

/// Which inbound (server-initiated) capabilities factor-q advertises to
/// a given server during the initialize handshake (ADR-0017,
/// nothing-by-default). Derived per-server from the agent's grants:
/// a server not granted a capability is not told the client supports
/// it, so a well-behaved server won't even register the corresponding
/// tool (e.g. the everything server gates `trigger-sampling-request` on
/// the client advertising `sampling`). Resources/prompts are *server*
/// capabilities and unaffected by this.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AdvertisedCapabilities {
    pub sampling: bool,
    pub elicitation: bool,
    pub roots: bool,
}

impl AdvertisedCapabilities {
    /// Advertise nothing inbound (a shared, tool-only server).
    pub fn none() -> Self {
        Self::default()
    }

    /// Advertise all three (used by tests that exercise the full
    /// server-initiated surface).
    pub fn all() -> Self {
        Self {
            sampling: true,
            elicitation: true,
            roots: true,
        }
    }
}

/// factor-q's MCP client handler.
///
/// Advertises the client-side capabilities the agent granted this
/// server (roots, sampling, elicitation) during the initialize
/// handshake, forwards out-of-band notifications to a sink, and — on
/// the per-invocation path — bridges server-initiated requests
/// (sampling, elicitation) to the runner and answers `roots/list`.
#[derive(Default)]
pub struct FactorQClientHandler {
    /// Inbound capabilities advertised to this server (per-server
    /// grant). Default: nothing (tool-only).
    capabilities: AdvertisedCapabilities,
    /// Sink for resource notifications forwarded from the connected
    /// server (`resources/updated`, `resources/list_changed`).
    notifications: Option<mpsc::UnboundedSender<ServerNotification>>,
    /// Sink for server-initiated requests (sampling, and later
    /// elicitation) bridged to the runner. `None` for shared,
    /// tool-only servers, which decline inbound requests per the rmcp
    /// default (ADR-0018: only grant-bearing servers run
    /// per-invocation with a wired channel).
    server_requests: Option<mpsc::UnboundedSender<ServerRequest>>,
    /// Workspace roots advertised to the server on `roots/list`
    /// (ADR-0018). Shared (interior-mutable) with the [`RootsHandle`]
    /// so the host can update them and fire `roots/list_changed`.
    /// Empty by default — roots are nothing-by-default and derived
    /// from the agent's sandbox grant.
    roots: Arc<Mutex<Vec<Root>>>,
}

impl FactorQClientHandler {
    /// Build a handler that forwards resource notifications to `tx`.
    fn with_notifications(tx: mpsc::UnboundedSender<ServerNotification>) -> Self {
        Self {
            notifications: Some(tx),
            ..Default::default()
        }
    }

    /// Set the inbound capabilities advertised to this server (derived
    /// from the agent's per-server grants). Default is none.
    fn with_capabilities(mut self, capabilities: AdvertisedCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Wire a sink for server-initiated requests (sampling /
    /// elicitation). Used on the per-invocation start path; absent
    /// for shared tool-only servers.
    fn with_server_requests(mut self, tx: mpsc::UnboundedSender<ServerRequest>) -> Self {
        self.server_requests = Some(tx);
        self
    }

    /// Share the advertised-roots cell with this handler so
    /// `roots/list` reflects host updates. Used on the per-invocation
    /// start path; the same `Arc` is held by the [`RootsHandle`].
    fn with_roots(mut self, roots: Arc<Mutex<Vec<Root>>>) -> Self {
        self.roots = roots;
        self
    }

    /// Build the `ClientCapabilities` advertised during initialize from
    /// the per-server grant: each of roots (+`list_changed`), sampling,
    /// and form-mode elicitation is advertised only if granted
    /// (ADR-0017, nothing-by-default). An ungranted capability is left
    /// `None`, so the server is never told the client supports it.
    pub fn advertised_capabilities(granted: AdvertisedCapabilities) -> ClientCapabilities {
        let mut capabilities = ClientCapabilities::default();
        if granted.roots {
            capabilities.roots = Some(RootsCapabilities {
                list_changed: Some(true),
            });
        }
        if granted.sampling {
            capabilities.sampling = Some(SamplingCapability::default());
        }
        if granted.elicitation {
            capabilities.elicitation = Some(ElicitationCapability {
                form: Some(FormElicitationCapability {
                    schema_validation: Some(true),
                }),
                url: None,
            });
        }
        capabilities
    }
}

impl ClientHandler for FactorQClientHandler {
    fn get_info(&self) -> ClientInfo {
        let mut info = ClientInfo::default();
        info.capabilities = Self::advertised_capabilities(self.capabilities);
        info
    }

    /// Bridge a `sampling/createMessage` request to the runner.
    ///
    /// The handler does no policy and no LLM call (ADR-0018 §2): it
    /// forwards the params on the per-invocation channel and awaits
    /// the runner's reply. With no channel wired (a shared tool-only
    /// server) or no runner listening, it declines with
    /// `method_not_found` — the rmcp default.
    fn create_message(
        &self,
        params: CreateMessageRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> impl std::future::Future<Output = Result<CreateMessageResult, rmcp::ErrorData>>
    + MaybeSendFuture
    + '_ {
        let sink = self.server_requests.clone();
        async move {
            let decline = || {
                Err(rmcp::ErrorData::method_not_found::<
                    CreateMessageRequestMethod,
                >())
            };
            let Some(tx) = sink else {
                return decline();
            };
            let (reply_tx, reply_rx) = oneshot::channel();
            if tx
                .send(ServerRequest::Sampling {
                    params,
                    reply: reply_tx,
                })
                .is_err()
            {
                // Runner gone — no one will service this request.
                return decline();
            }
            match reply_rx.await {
                Ok(result) => result,
                // Reply sender dropped without answering → decline.
                Err(_) => decline(),
            }
        }
    }

    /// Bridge an `elicitation/create` request to the runner (ADR-0018).
    /// Like [`create_message`](Self::create_message), the handler does
    /// no policy and no LLM call: it forwards the params and awaits the
    /// runner's reply. With no channel wired or no runner listening it
    /// declines (an ordinary `action: decline` result — the rmcp
    /// default).
    fn create_elicitation(
        &self,
        params: CreateElicitationRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> impl std::future::Future<Output = Result<CreateElicitationResult, rmcp::ErrorData>>
    + MaybeSendFuture
    + '_ {
        let sink = self.server_requests.clone();
        async move {
            let Some(tx) = sink else {
                return Ok(elicitation_decline());
            };
            let (reply_tx, reply_rx) = oneshot::channel();
            if tx
                .send(ServerRequest::Elicitation {
                    params,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Ok(elicitation_decline());
            }
            match reply_rx.await {
                Ok(result) => result,
                // Reply sender dropped without answering → decline.
                Err(_) => Ok(elicitation_decline()),
            }
        }
    }

    /// Answer `roots/list` with the workspace roots advertised to this
    /// server (ADR-0018). Handler-only: no LLM, no budget — roots are
    /// invocation-scoped config. Empty when the agent granted no roots
    /// to this server.
    fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> impl std::future::Future<Output = Result<ListRootsResult, rmcp::ErrorData>> + MaybeSendFuture + '_
    {
        let roots = Arc::clone(&self.roots);
        async move { Ok(ListRootsResult::new(roots.lock().await.clone())) }
    }

    fn on_resource_updated(
        &self,
        params: ResourceUpdatedNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + MaybeSendFuture + '_ {
        if let Some(tx) = &self.notifications {
            let _ = tx.send(ServerNotification::ResourceUpdated { uri: params.uri });
        }
        std::future::ready(())
    }

    fn on_resource_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + MaybeSendFuture + '_ {
        if let Some(tx) = &self.notifications {
            let _ = tx.send(ServerNotification::ResourceListChanged);
        }
        std::future::ready(())
    }

    /// The server's tool list changed (`notifications/tools/list_changed`).
    /// Forward it so the host can re-discover via
    /// [`McpClientManager::refresh_tools`] rather than serving the
    /// startup-time set.
    fn on_tool_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + MaybeSendFuture + '_ {
        if let Some(tx) = &self.notifications {
            let _ = tx.send(ServerNotification::ToolListChanged);
        }
        std::future::ready(())
    }

    /// The server's prompt list changed
    /// (`notifications/prompts/list_changed`). Prompts are fetched
    /// on-demand, so this is informational — forward it for observers.
    fn on_prompt_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + MaybeSendFuture + '_ {
        if let Some(tx) = &self.notifications {
            let _ = tx.send(ServerNotification::PromptListChanged);
        }
        std::future::ready(())
    }

    /// Fold a server log record (`notifications/message`) into the
    /// host's `tracing` output at the mapped level, and forward it on
    /// the notification sink so consumers (tests, a future event-bus
    /// bridge) can observe it. The server respects the client's
    /// `logging/setLevel` choice, so filtering happens server-side.
    fn on_logging_message(
        &self,
        params: LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + MaybeSendFuture + '_ {
        let level = logging_level_name(params.level);
        let logger = params.logger.as_deref().unwrap_or("mcp-server");
        // Dynamic level → a static-level dispatch (tracing levels are
        // const). MCP's eight levels collapse onto tracing's five.
        match params.level {
            LoggingLevel::Debug => {
                debug!(target: "mcp.server.log", %level, logger, data = %params.data)
            }
            LoggingLevel::Info | LoggingLevel::Notice => {
                info!(target: "mcp.server.log", %level, logger, data = %params.data)
            }
            LoggingLevel::Warning => {
                warn!(target: "mcp.server.log", %level, logger, data = %params.data)
            }
            LoggingLevel::Error
            | LoggingLevel::Critical
            | LoggingLevel::Alert
            | LoggingLevel::Emergency => {
                tracing::error!(target: "mcp.server.log", %level, logger, data = %params.data)
            }
        }
        if let Some(tx) = &self.notifications {
            let _ = tx.send(ServerNotification::Log {
                level: level.to_string(),
                logger: params.logger,
                data: params.data,
            });
        }
        std::future::ready(())
    }

    /// Forward `notifications/progress` for an in-flight request: trace
    /// it and forward a [`ServerNotification::Progress`] on the sink.
    fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + MaybeSendFuture + '_ {
        let token = progress_token_string(&params.progress_token);
        debug!(
            target: "mcp.server.progress",
            token = %token,
            progress = params.progress,
            total = ?params.total,
            "progress"
        );
        if let Some(tx) = &self.notifications {
            let _ = tx.send(ServerNotification::Progress {
                token,
                progress: params.progress,
                total: params.total,
                message: params.message,
            });
        }
        std::future::ready(())
    }
}

/// Map an MCP logging level to its canonical lowercase name.
fn logging_level_name(level: LoggingLevel) -> &'static str {
    match level {
        LoggingLevel::Debug => "debug",
        LoggingLevel::Info => "info",
        LoggingLevel::Notice => "notice",
        LoggingLevel::Warning => "warning",
        LoggingLevel::Error => "error",
        LoggingLevel::Critical => "critical",
        LoggingLevel::Alert => "alert",
        LoggingLevel::Emergency => "emergency",
    }
}

/// MCP server ids form the namespace in provider-visible tool names
/// (`<server>__<tool>`, #177). The charset excludes `_` so the first
/// `__` in a canonical name unambiguously splits namespace from tool,
/// and the whole name stays inside provider tool-name rules
/// (`[a-zA-Z0-9_-]`, e.g. Anthropic's).
fn validate_server_name(name: &str) -> Result<(), McpError> {
    if name == "builtin" {
        return Err(McpError::ToolDiscovery {
            command: name.to_string(),
            reason: "server id 'builtin' is reserved for runtime tools".to_string(),
        });
    }
    if name.is_empty()
        || name.len() > 48
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(McpError::ToolDiscovery {
            command: name.to_string(),
            reason: "server id must match [a-z0-9-]+ and be at most 48 characters".to_string(),
        });
    }
    Ok(())
}

/// Compose the canonical, provider-visible name for a server's tool and
/// enforce the 64-character combined bound (the strictest provider
/// tool-name length, Anthropic's). Failing one tool fails the server's
/// discovery loudly rather than silently offering a partial tool set.
fn namespaced_tool_name(server_name: &str, remote_name: &str) -> Result<String, McpError> {
    let name = format!("{server_name}__{remote_name}");
    if name.len() > 64 {
        return Err(McpError::ToolDiscovery {
            command: server_name.to_string(),
            reason: format!("namespaced tool name '{name}' exceeds 64 characters"),
        });
    }
    Ok(name)
}

/// A single tool from an MCP server, adapted to the fq-tools [`Tool`] trait.
///
/// Holds an `Arc` to the shared client handle so multiple tools from the
/// same server share one connection.
pub struct McpTool {
    tool_name: String,
    remote_tool_name: String,
    tool_description: String,
    tool_input_schema: Value,
    client: Arc<McpClient>,
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn description(&self) -> &str {
        &self.tool_description
    }

    fn parameters_schema(&self) -> Value {
        self.tool_input_schema.clone()
    }

    async fn execute(
        &self,
        _ctx: &ToolContext<'_>,
        params: Value,
    ) -> Result<ToolResult, ToolError> {
        let arguments = match params.as_object() {
            Some(obj) => obj.clone(),
            None if params.is_null() => serde_json::Map::new(),
            None => {
                return Err(ToolError::InvalidParameters(
                    "MCP tool parameters must be a JSON object".to_string(),
                ));
            }
        };

        let mut request =
            CallToolRequestParams::new(self.remote_tool_name.clone()).with_arguments(arguments);
        // Attach a progress token so progress-capable servers can
        // report against this call (`notifications/progress`); servers
        // that don't support progress ignore it.
        request.meta = Some(Meta::with_progress_token(next_progress_token()));

        let result = self
            .client
            .call_tool(request)
            .await
            .map_err(|err| ToolError::ExecutionFailed(err.to_string()))?;

        // Extract text content from the response. Non-text content
        // (images, resources) is noted but not included — the LLM
        // only sees textual tool output in factor-q today.
        let output: String = result
            .content
            .iter()
            .filter_map(|c| c.raw.as_text().map(|t| t.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n");

        let is_error = result.is_error.unwrap_or(false);

        Ok(ToolResult { output, is_error })
    }
}

/// Which resource operation a synthesized tool performs.
#[derive(Clone, Copy)]
enum ResourceOp {
    List,
    Read,
    ListTemplates,
}

/// A host-synthesized tool exposing a server's MCP resources to the
/// agent's LLM (model-controlled access). One pair per server that
/// advertises the resources capability — `<server>__list_resources`
/// and `<server>__read_resource`. Mirrors [`McpTool`]: it holds the
/// shared client handle and registers in the [`ToolRegistry`] like any
/// other tool, so no reducer-runner changes are needed. (Host-curated
/// injection of declared resources is a separate path — see the plan's
/// step 3d.)
pub struct McpResourceTool {
    name: String,
    description: String,
    schema: Value,
    op: ResourceOp,
    client: Arc<McpClient>,
}

impl McpResourceTool {
    fn list(server: &str, client: Arc<McpClient>) -> Self {
        Self {
            name: format!("{server}__list_resources"),
            description: format!(
                "List the resources available from the '{server}' MCP server, \
                 returning each resource's URI, name, and description."
            ),
            schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            op: ResourceOp::List,
            client,
        }
    }

    fn read(server: &str, client: Arc<McpClient>) -> Self {
        Self {
            name: format!("{server}__read_resource"),
            description: format!(
                "Read a resource from the '{server}' MCP server by its URI \
                 (discover URIs with {server}__list_resources)."
            ),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "uri": { "type": "string", "description": "The resource URI to read." }
                },
                "required": ["uri"],
                "additionalProperties": false
            }),
            op: ResourceOp::Read,
            client,
        }
    }

    fn list_templates(server: &str, client: Arc<McpClient>) -> Self {
        Self {
            name: format!("{server}__list_resource_templates"),
            description: format!(
                "List the resource templates the '{server}' MCP server exposes \
                 (URI templates like scheme://path/{{param}}); fill in the params \
                 and read the concrete URI with {server}__read_resource."
            ),
            schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            op: ResourceOp::ListTemplates,
            client,
        }
    }
}

#[async_trait::async_trait]
impl Tool for McpResourceTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Value {
        self.schema.clone()
    }

    async fn execute(
        &self,
        _ctx: &ToolContext<'_>,
        params: Value,
    ) -> Result<ToolResult, ToolError> {
        match self.op {
            ResourceOp::List => {
                let resources = self
                    .client
                    .list_all_resources()
                    .await
                    .map_err(|err| ToolError::ExecutionFailed(err.to_string()))?;
                let mut output = String::new();
                for resource in &resources {
                    let raw = &resource.raw;
                    output.push_str(&raw.uri);
                    output.push_str(" — ");
                    output.push_str(&raw.name);
                    if let Some(description) = raw.description.as_deref() {
                        output.push_str(": ");
                        output.push_str(description);
                    }
                    output.push('\n');
                }
                if output.is_empty() {
                    output.push_str("(no resources)");
                }
                Ok(ToolResult {
                    output,
                    is_error: false,
                })
            }
            ResourceOp::Read => {
                let uri = params.get("uri").and_then(Value::as_str).ok_or_else(|| {
                    ToolError::InvalidParameters(
                        "read_resource requires a 'uri' string".to_string(),
                    )
                })?;
                let result = self
                    .client
                    .read_resource(ReadResourceRequestParams::new(uri))
                    .await
                    .map_err(|err| ToolError::ExecutionFailed(err.to_string()))?;
                Ok(ToolResult {
                    output: render_resource_contents(&result),
                    is_error: false,
                })
            }
            ResourceOp::ListTemplates => {
                let templates = self
                    .client
                    .list_all_resource_templates()
                    .await
                    .map_err(|err| ToolError::ExecutionFailed(err.to_string()))?;
                let mut output = String::new();
                for template in &templates {
                    let raw = &template.raw;
                    output.push_str(&raw.uri_template);
                    output.push_str(" — ");
                    output.push_str(&raw.name);
                    if let Some(description) = raw.description.as_deref() {
                        output.push_str(": ");
                        output.push_str(description);
                    }
                    output.push('\n');
                }
                if output.is_empty() {
                    output.push_str("(no resource templates)");
                }
                Ok(ToolResult {
                    output,
                    is_error: false,
                })
            }
        }
    }
}

/// Tracks a running MCP server and its client handle.
struct RunningServer {
    name: String,
    client: Arc<McpClient>,
    tool_names: Vec<String>,
    /// Receiver for resource notifications the handler forwards.
    notifications: Mutex<mpsc::UnboundedReceiver<ServerNotification>>,
}

/// Manages the lifecycle of MCP server child processes.
///
/// Starts servers, discovers their tools (wrapping each as an [`McpTool`]),
/// and provides graceful shutdown. Deduplicates servers by `(command, args)`
/// so the same server declared by multiple agents is only started once.
pub struct McpClientManager {
    servers: Vec<RunningServer>,
    /// Track `(command, args)` tuples to deduplicate.
    started: HashSet<(String, Vec<String>)>,
}

impl Default for McpClientManager {
    fn default() -> Self {
        Self::new()
    }
}

impl McpClientManager {
    pub fn new() -> Self {
        Self {
            servers: Vec::new(),
            started: HashSet::new(),
        }
    }

    /// Start an MCP server, discover its tools, and return them as
    /// `Arc<dyn Tool>` values ready for registration in a [`ToolRegistry`].
    ///
    /// If a server with the same `(command, args)` has already been started,
    /// this is a no-op and returns an empty vec (the tools were already
    /// registered on the first call).
    pub async fn start_server(
        &mut self,
        config: McpServerConfig,
    ) -> Result<Vec<Arc<dyn Tool>>, McpError> {
        let key = (config.command.clone(), config.args.clone());
        if self.started.contains(&key) {
            debug!(
                server = %config.name,
                command = %config.command,
                "MCP server already started, skipping duplicate"
            );
            return Ok(Vec::new());
        }

        // Shared servers are tool-only: advertise no inbound capabilities
        // (a grant-bearing server runs per-invocation instead — ADR-0018).
        let (tools, _roots) = self
            .start_inner(config, None, Vec::new(), AdvertisedCapabilities::none())
            .await?;
        self.started.insert(key);
        Ok(tools)
    }

    /// Start a *per-invocation* MCP server instance with a wired
    /// server-initiated request channel and advertised `roots`
    /// (ADR-0018).
    ///
    /// Unlike [`Self::start_server`], this never deduplicates: a server
    /// granted an inbound capability (sampling, elicitation, roots)
    /// runs as its own child process per invocation, so its
    /// server-initiated requests attribute to the right invocation's
    /// budget, grant, and event chain. Returns the discovered tools,
    /// the receiver the runner services in its `select!` loop, and a
    /// [`RootsHandle`] for updating the advertised roots. Pass empty
    /// `roots` when the agent grants none.
    pub async fn start_server_with_requests(
        &mut self,
        config: McpServerConfig,
        roots: Vec<Root>,
        capabilities: AdvertisedCapabilities,
    ) -> Result<
        (
            Vec<Arc<dyn Tool>>,
            mpsc::UnboundedReceiver<ServerRequest>,
            RootsHandle,
        ),
        McpError,
    > {
        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let (tools, roots_handle) = self
            .start_inner(config, Some(req_tx), roots, capabilities)
            .await?;
        Ok((tools, req_rx, roots_handle))
    }

    /// Shared start path: spawn the child process, run the initialize
    /// handshake, discover tools, and register the [`RunningServer`].
    /// `server_request_tx` wires the per-invocation sampling /
    /// elicitation bridge; `None` leaves the server tool-only (inbound
    /// requests decline). `roots` seeds the advertised workspace.
    /// Deduplication is the caller's concern. Returns the tools and a
    /// [`RootsHandle`] over the (possibly empty) advertised roots.
    async fn start_inner(
        &mut self,
        config: McpServerConfig,
        server_request_tx: Option<mpsc::UnboundedSender<ServerRequest>>,
        roots: Vec<Root>,
        capabilities: AdvertisedCapabilities,
    ) -> Result<(Vec<Arc<dyn Tool>>, RootsHandle), McpError> {
        info!(
            server = %config.name,
            command = %config.command,
            args = ?config.args,
            "starting MCP server"
        );

        // The handler advertises factor-q's client capabilities
        // (roots/sampling/elicitation), forwards resource notifications
        // to `notif_rx`, and — on the per-invocation path — bridges
        // server-initiated requests. It is then served over whichever
        // transport the config selects; the MCP initialize handshake and
        // every subsequent operation are transport-agnostic.
        let (notif_tx, notif_rx) = mpsc::unbounded_channel();
        let roots_cell = Arc::new(Mutex::new(roots));
        let mut handler = FactorQClientHandler::with_notifications(notif_tx)
            .with_roots(Arc::clone(&roots_cell))
            .with_capabilities(capabilities);
        if let Some(req_tx) = server_request_tx {
            handler = handler.with_server_requests(req_tx);
        }
        let client = match &config.url {
            // Streamable HTTP (remote) transport — the 2025-11-25 spec
            // transport.
            Some(url) => handler
                .serve(StreamableHttpClientTransport::from_uri(url.clone()))
                .await
                .map_err(|err| McpError::ServerStart {
                    command: url.clone(),
                    reason: err.to_string(),
                })?,
            // stdio child-process transport.
            None => {
                let env_vars = config.env.clone();
                let args = config.args.clone();
                let transport =
                    TokioChildProcess::new(Command::new(&config.command).configure(|cmd| {
                        cmd.args(&args);
                        for (k, v) in &env_vars {
                            cmd.env(k, v);
                        }
                    }))
                    .map_err(|err| McpError::ServerStart {
                        command: config.command.clone(),
                        reason: err.to_string(),
                    })?;
                handler
                    .serve(transport)
                    .await
                    .map_err(|err| McpError::ServerStart {
                        command: config.command.clone(),
                        reason: err.to_string(),
                    })?
            }
        };

        let client = Arc::new(client);
        let roots_handle = RootsHandle {
            server: config.name.clone(),
            roots: roots_cell,
            client: Arc::clone(&client),
        };

        // Discover tools (shared with `refresh_tools`).
        let (tools, tool_names) = Self::discover_tools(&client, &config.name).await?;

        self.servers.push(RunningServer {
            name: config.name,
            client,
            tool_names,
            notifications: Mutex::new(notif_rx),
        });

        Ok((tools, roots_handle))
    }

    /// Discover a server's current tools: the regular MCP tools plus
    /// the synthesized host-fulfilled resource tools (step 3b) when the
    /// server advertises resources. Shared by initial startup and
    /// [`refresh_tools`](Self::refresh_tools) (Step 7,
    /// `notifications/tools/list_changed`). Returns the tool wrappers
    /// and their names.
    async fn discover_tools(
        client: &Arc<McpClient>,
        server_name: &str,
    ) -> Result<(Vec<Arc<dyn Tool>>, Vec<String>), McpError> {
        validate_server_name(server_name)?;
        let mcp_tools = client
            .list_all_tools()
            .await
            .map_err(|err| McpError::ToolDiscovery {
                command: server_name.to_string(),
                reason: err.to_string(),
            })?;

        info!(
            server = %server_name,
            tool_count = mcp_tools.len(),
            "discovered MCP tools"
        );

        let mut tools: Vec<Arc<dyn Tool>> = Vec::with_capacity(mcp_tools.len());
        let mut tool_names: Vec<String> = Vec::with_capacity(mcp_tools.len());

        for mcp_tool in mcp_tools {
            let remote_name = mcp_tool.name.to_string();
            let name = namespaced_tool_name(server_name, &remote_name)?;
            let description = mcp_tool.description.as_deref().unwrap_or("").to_string();

            // Convert the Arc<JsonObject> input_schema to a serde_json::Value.
            let input_schema = serde_json::to_value(&*mcp_tool.input_schema)
                .unwrap_or(Value::Object(serde_json::Map::new()));

            debug!(server = %server_name, tool = %name, "registered MCP tool");

            tool_names.push(name.clone());
            tools.push(Arc::new(McpTool {
                tool_name: name,
                remote_tool_name: remote_name,
                tool_description: description,
                tool_input_schema: input_schema,
                client: Arc::clone(client),
            }));
        }

        // Synthesize host-fulfilled resource tools (step 3b) when the
        // server advertises the resources capability, so the agent's LLM
        // can list/read its resources on demand.
        let advertises_resources = client
            .peer_info()
            .and_then(|info| info.capabilities.resources.as_ref())
            .is_some();
        if advertises_resources {
            for resource_tool in [
                McpResourceTool::list(server_name, Arc::clone(client)),
                McpResourceTool::read(server_name, Arc::clone(client)),
                McpResourceTool::list_templates(server_name, Arc::clone(client)),
            ] {
                debug!(
                    server = %server_name,
                    tool = %resource_tool.name(),
                    "registered MCP resource tool"
                );
                tool_names.push(resource_tool.name().to_string());
                tools.push(Arc::new(resource_tool));
            }
        }

        Ok((tools, tool_names))
    }

    /// Re-discover a server's tools and refresh the cached tool-name
    /// list, reacting to `notifications/tools/list_changed` (Step 7).
    /// Returns the current tool set so the caller can re-register it in
    /// its [`ToolRegistry`] rather than
    /// serving the stale set discovered at startup. Resources and
    /// prompts are fetched on-demand (never cached), so they need no
    /// refresh.
    pub async fn refresh_tools(&mut self, server: &str) -> Result<Vec<Arc<dyn Tool>>, McpError> {
        let idx = self
            .servers
            .iter()
            .position(|s| s.name == server)
            .ok_or_else(|| McpError::UnknownServer {
                name: server.to_string(),
            })?;
        let client = Arc::clone(&self.servers[idx].client);
        let (tools, tool_names) = Self::discover_tools(&client, server).await?;
        self.servers[idx].tool_names = tool_names;
        Ok(tools)
    }

    /// The capabilities a started server advertised during the initialize
    /// handshake, looked up by server name. `None` if no server with that
    /// name is running or the handshake produced no peer info.
    pub fn server_capabilities(&self, name: &str) -> Option<ServerCapabilities> {
        self.servers
            .iter()
            .find(|server| server.name == name)
            .and_then(|server| server.client.peer_info())
            .map(|info| info.capabilities.clone())
    }

    /// Find the client handle for a running server by name.
    fn client_for(&self, name: &str) -> Result<&Arc<McpClient>, McpError> {
        self.servers
            .iter()
            .find(|server| server.name == name)
            .map(|server| &server.client)
            .ok_or_else(|| McpError::UnknownServer {
                name: name.to_string(),
            })
    }

    /// List all resources a running server exposes (auto-paginated).
    pub async fn list_resources(&self, server: &str) -> Result<Vec<Resource>, McpError> {
        self.client_for(server)?
            .list_all_resources()
            .await
            .map_err(|err| McpError::ResourceOp {
                server: server.to_string(),
                reason: err.to_string(),
            })
    }

    /// Read a single resource from a running server by URI.
    pub async fn read_resource(
        &self,
        server: &str,
        uri: &str,
    ) -> Result<ReadResourceResult, McpError> {
        self.client_for(server)?
            .read_resource(ReadResourceRequestParams::new(uri))
            .await
            .map_err(|err| McpError::ResourceOp {
                server: server.to_string(),
                reason: err.to_string(),
            })
    }

    /// List the resource templates a running server exposes (auto-paginated).
    pub async fn list_resource_templates(
        &self,
        server: &str,
    ) -> Result<Vec<ResourceTemplate>, McpError> {
        self.client_for(server)?
            .list_all_resource_templates()
            .await
            .map_err(|err| McpError::ResourceOp {
                server: server.to_string(),
                reason: err.to_string(),
            })
    }

    /// List the prompts a running server exposes (auto-paginated).
    /// Returns rmcp's discovery type, mirroring [`Self::list_resources`];
    /// the owned, lossless representation is reserved for the fetched
    /// prompt itself (see [`Self::get_prompt`]).
    pub async fn list_prompts(&self, server: &str) -> Result<Vec<Prompt>, McpError> {
        self.client_for(server)?
            .list_all_prompts()
            .await
            .map_err(|err| McpError::PromptOp {
                server: server.to_string(),
                reason: err.to_string(),
            })
    }

    /// Fetch a prompt by name with bound arguments and materialise it
    /// into an owned, reusable [`PromptSeed`](crate::prompt::PromptSeed)
    /// (Step 4's seed value: message sequence + bound args + provenance).
    /// This is the rmcp boundary — the seed itself is provider-neutral.
    pub async fn get_prompt(
        &self,
        server: &str,
        name: &str,
        arguments: BTreeMap<String, String>,
    ) -> Result<crate::prompt::PromptSeed, McpError> {
        let mut params = GetPromptRequestParams::new(name);
        if !arguments.is_empty() {
            let obj: JsonObject = arguments
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect();
            params = params.with_arguments(obj);
        }
        let result = self
            .client_for(server)?
            .get_prompt(params)
            .await
            .map_err(|err| McpError::PromptOp {
                server: server.to_string(),
                reason: err.to_string(),
            })?;
        Ok(prompt_seed_from_rmcp(server, name, arguments, result))
    }

    /// Request argument completion for a prompt argument
    /// (`completion/complete`). Per ADR-0017 prompts are
    /// model-controlled, so this is the agent's tool, not a human menu.
    /// `context` carries previously-resolved arguments for
    /// dependent completions (e.g. the everything server's `name`
    /// argument depends on `department`).
    pub async fn complete_prompt(
        &self,
        server: &str,
        prompt: &str,
        argument: &str,
        value: &str,
        context: Option<CompletionContext>,
    ) -> Result<CompletionInfo, McpError> {
        self.client_for(server)?
            .complete_prompt_argument(prompt, argument, value, context)
            .await
            .map_err(|err| McpError::PromptOp {
                server: server.to_string(),
                reason: err.to_string(),
            })
    }

    /// A cloneable read-only handle for reading resources from the
    /// currently-running servers — used to inject `static_resources`
    /// at invocation start without sharing the manager's lifecycle.
    pub fn resource_reader(&self) -> McpResourceReader {
        McpResourceReader {
            clients: self
                .servers
                .iter()
                .map(|server| (server.name.clone(), Arc::clone(&server.client)))
                .collect(),
        }
    }

    /// A cloneable handle for re-discovering the running servers'
    /// tools — used by the daemon's notification drain to rebuild the
    /// shared registry on `tools/list_changed` (ADR-0020) without
    /// sharing the manager's `&mut` lifecycle (same pattern as
    /// [`resource_reader`](Self::resource_reader)).
    ///
    /// `exec_config` carries the `[tools.exec]` timeouts so a rebuilt
    /// registry keeps the daemon's configured `exec` limits instead of
    /// reverting to the crate defaults on the next `tools/list_changed`.
    pub fn tool_refresher(&self, exec_config: ExecConfig) -> McpToolRefresher {
        McpToolRefresher {
            clients: self
                .servers
                .iter()
                .map(|server| (server.name.clone(), Arc::clone(&server.client)))
                .collect(),
            exec_config,
        }
    }

    /// Extract every server's notification receiver so a drain task
    /// can own them outright (ADR-0020). Each receiver is replaced
    /// with a closed dummy, so a later
    /// [`recv_notification`](Self::recv_notification) for that server
    /// returns `None` immediately rather than racing the drain.
    pub async fn take_notifications(
        &mut self,
    ) -> Vec<(String, mpsc::UnboundedReceiver<ServerNotification>)> {
        let mut out = Vec::with_capacity(self.servers.len());
        for server in &self.servers {
            let mut guard = server.notifications.lock().await;
            let (_closed_tx, closed_rx) = mpsc::unbounded_channel();
            let rx = std::mem::replace(&mut *guard, closed_rx);
            out.push((server.name.clone(), rx));
        }
        out
    }

    /// Subscribe to update notifications for a resource on a server.
    /// Updates arrive via [`Self::recv_notification`].
    pub async fn subscribe(&self, server: &str, uri: &str) -> Result<(), McpError> {
        self.client_for(server)?
            .subscribe(SubscribeRequestParams::new(uri))
            .await
            .map(|_| ())
            .map_err(|err| McpError::ResourceOp {
                server: server.to_string(),
                reason: err.to_string(),
            })
    }

    /// Await the next out-of-band notification a server's handler
    /// forwarded (resource change, list-changed, log, progress).
    /// `None` if the server is unknown or its channel closed.
    pub async fn recv_notification(&self, server: &str) -> Option<ServerNotification> {
        let server = self.servers.iter().find(|s| s.name == server)?;
        server.notifications.lock().await.recv().await
    }

    /// Call a tool, racing it against a `cancel` future (Step 7). If
    /// the tool completes first, return its result as `Some`. If
    /// `cancel` fires first, send `notifications/cancelled` to the
    /// server (asking it to abort) and return `None`, abandoning the
    /// in-flight request. This is how a host aborts a stuck or
    /// no-longer-needed tool call (timeout, shutdown, budget) without
    /// blocking on it.
    pub async fn call_tool_cancellable<F>(
        &self,
        server: &str,
        tool_name: &str,
        arguments: JsonObject,
        cancel: F,
    ) -> Result<Option<CallToolResult>, McpError>
    where
        F: std::future::Future<Output = ()>,
    {
        let canonical_prefix = format!("{server}__");
        let remote_tool_name = tool_name
            .strip_prefix(&canonical_prefix)
            .unwrap_or(tool_name);
        let params =
            CallToolRequestParams::new(remote_tool_name.to_string()).with_arguments(arguments);
        let mut handle = self
            .client_for(server)?
            .peer()
            .send_cancellable_request(
                ClientRequest::CallToolRequest(CallToolRequest::new(params)),
                PeerRequestOptions::no_options(),
            )
            .await
            .map_err(|err| McpError::ToolCall {
                tool_name: tool_name.to_string(),
                reason: err.to_string(),
            })?;

        // Clone what's needed to cancel without consuming the handle
        // (the `select!` borrows `handle.rx`).
        let request_id = handle.id.clone();
        let peer = handle.peer.clone();
        let tool_call_error = |reason: String| McpError::ToolCall {
            tool_name: tool_name.to_string(),
            reason,
        };

        tokio::pin!(cancel);
        tokio::select! {
            result = &mut handle.rx => match result {
                Ok(Ok(ServerResult::CallToolResult(result))) => Ok(Some(result)),
                Ok(Ok(_)) => Err(tool_call_error("unexpected response type".to_string())),
                Ok(Err(err)) => Err(tool_call_error(err.to_string())),
                Err(_) => Err(tool_call_error("transport closed".to_string())),
            },
            _ = &mut cancel => {
                // Best-effort: tell the server to abort. We stop
                // awaiting the response regardless.
                let _ = peer
                    .notify_cancelled(CancelledNotificationParam {
                        request_id,
                        reason: Some("cancelled by host".to_string()),
                    })
                    .await;
                Ok(None)
            }
        }
    }

    /// Set the minimum logging level the server should send
    /// (`logging/setLevel`). The server filters below this level, so
    /// only messages at or above `level` arrive on the notification
    /// sink thereafter.
    pub async fn set_logging_level(
        &self,
        server: &str,
        level: LoggingLevel,
    ) -> Result<(), McpError> {
        self.client_for(server)?
            .set_level(SetLevelRequestParams::new(level))
            .await
            .map(|_| ())
            .map_err(|err| McpError::ResourceOp {
                server: server.to_string(),
                reason: err.to_string(),
            })
    }

    /// How long [`shutdown`](Self::shutdown) waits for a stdio child to
    /// exit gracefully after we've cancelled the service (which sends
    /// the child EOF on stdin) but can't `close().await` it directly
    /// because tool `Arc`s are still outstanding. rmcp's child-process
    /// transport itself waits up to 3s for the child before force-killing;
    /// we give it a little more headroom so the *graceful* path (EOF →
    /// the server tears its stdio down and exits) wins the race against
    /// the abrupt drop-guard kill, which is what causes the flaky
    /// teardown `EPIPE` on the Node stdio servers (see issue #25).
    const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(4);

    /// Gracefully shut down all managed MCP server processes.
    ///
    /// Teardown ordering matters for stdio (child-process) servers: an
    /// MCP server mid-write when its stdin/stdout pipe is closed abruptly
    /// hits `EPIPE`, and the `@modelcontextprotocol/sdk` stdio transport
    /// installs no socket `error` handler, so Node throws on the
    /// unhandled `'error'` event and the whole process exits 101 — even
    /// though every request already completed. That reddens CI as pure
    /// teardown noise (issue #25).
    ///
    /// The fix is to always tear the server down *gracefully*: cancel the
    /// service so rmcp closes the transport (which sends the child EOF on
    /// stdin and waits for it to exit) rather than letting the child be
    /// abruptly killed mid-write.
    ///
    /// - When no tool `Arc`s are outstanding we can take `&mut` and
    ///   `close().await`, which cancels *and* awaits the background task's
    ///   graceful transport close to completion — the cleanest path.
    /// - Otherwise (tool wrappers still hold client `Arc`s) we can't get
    ///   `&mut`, so we cancel the service via its cancellation token —
    ///   which drives the same graceful transport close on the background
    ///   task — and then give the child a bounded window to receive EOF
    ///   and exit before we drop our handle. Without this wait, dropping
    ///   the `RunningService` here lets its drop guard cancel and the
    ///   child-process transport kill the child *abruptly*, racing the
    ///   server's final writes → the flaky `EPIPE` crash.
    pub async fn shutdown(&mut self) {
        for server in &mut self.servers {
            info!(
                server = %server.name,
                tools = ?server.tool_names,
                "shutting down MCP server"
            );
            match Arc::get_mut(&mut server.client) {
                // Sole owner: cancel and await the graceful transport
                // close to completion.
                Some(client) => {
                    if let Err(err) = client.close().await {
                        warn!(
                            server = %server.name,
                            error = %err,
                            "error during MCP server shutdown"
                        );
                    }
                }
                // Tool wrappers still hold client Arcs, so we can't take
                // `&mut` to `close().await`. Cancel the service anyway —
                // that drives the same graceful transport close (EOF to
                // the child, wait for it to exit) on the background task —
                // then wait for the child to exit before we drop, so it
                // isn't killed mid-write (issue #25).
                None => {
                    debug!(
                        server = %server.name,
                        "MCP client has outstanding references; cancelling and \
                         awaiting graceful child exit before drop"
                    );
                    server.client.cancellation_token().cancel();
                    Self::await_graceful_close(&server.client, Self::SHUTDOWN_GRACE).await;
                }
            }
        }
        self.servers.clear();
        self.started.clear();
    }

    /// After cancelling a service we can't `close().await` (outstanding
    /// tool `Arc`s), wait — up to `grace` — for its background task to
    /// finish the graceful transport close so the stdio child exits on
    /// EOF instead of being killed mid-write. Polls the service's
    /// closed/transport-closed state, which flips once the background
    /// loop has run its `transport.close()` (the EOF + child-exit path).
    /// Bounded so a wedged child can't hang shutdown — the drop guard
    /// force-kills it after we return.
    async fn await_graceful_close(client: &Arc<McpClient>, grace: std::time::Duration) {
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            if client.is_transport_closed() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                debug!("timed out awaiting graceful MCP child exit; drop guard will force-kill");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }
}

/// A cheap, cloneable read-only handle over a manager's connected
/// servers for reading resources, without the manager's `&mut`
/// lifecycle. [`ReducerContext`](crate::ReducerContext) holds one so
/// the runner can read `static_resources` pins at invocation start
/// while `main` keeps the manager for graceful shutdown.
#[derive(Clone, Default)]
pub struct McpResourceReader {
    clients: HashMap<String, Arc<McpClient>>,
}

impl McpResourceReader {
    /// Read a resource by `(server, uri)`.
    pub async fn read_resource(
        &self,
        server: &str,
        uri: &str,
    ) -> Result<ReadResourceResult, McpError> {
        let client = self
            .clients
            .get(server)
            .ok_or_else(|| McpError::UnknownServer {
                name: server.to_string(),
            })?;
        client
            .read_resource(ReadResourceRequestParams::new(uri))
            .await
            .map_err(|err| McpError::ResourceOp {
                server: server.to_string(),
                reason: err.to_string(),
            })
    }
}

/// A cheap, cloneable handle for re-discovering connected servers'
/// tools (ADR-0020), without the manager's `&mut` lifecycle. The
/// daemon's notification drain holds one and rebuilds the shared
/// registry when a server signals `tools/list_changed`.
#[derive(Clone, Default)]
pub struct McpToolRefresher {
    clients: Vec<(String, Arc<McpClient>)>,
    /// The `[tools.exec]` timeouts to re-apply to the `exec` built-in on
    /// every rebuild, so a refresh never silently reverts to the crate
    /// defaults.
    exec_config: ExecConfig,
}

impl McpToolRefresher {
    /// Rebuild the full shared registry: built-ins plus every
    /// connected server's currently-advertised tools. The registry is
    /// append-only, so rebuild-from-scratch *is* the refresh
    /// operation. A server whose re-discovery fails contributes no
    /// tools (its calls would fail anyway) and is logged.
    pub async fn rebuild_registry(&self) -> ToolRegistry {
        let mut registry = ToolRegistry::with_builtins_exec(self.exec_config.clone());
        for (name, client) in &self.clients {
            match McpClientManager::discover_tools(client, name).await {
                Ok((tools, _)) => {
                    for tool in tools {
                        if let Err(error) = registry.register(tool) {
                            warn!(server = %name, %error, "refusing MCP tool registration");
                        }
                    }
                }
                Err(err) => {
                    warn!(
                        server = %name,
                        error = %err,
                        "tool re-discovery failed during refresh; this server's \
                         tools are absent from the rebuilt registry"
                    );
                }
            }
        }
        registry
    }
}

/// Drain every shared server's notification stream in the daemon
/// (ADR-0020). Logs and progress are already folded into `tracing` by
/// the handler — consuming them here is what stops the unbounded
/// channels growing. `tools/list_changed` re-discovers via the
/// `refresher` and hands the rebuilt registry to `on_tools_changed`
/// (the daemon installs it into the shared `ReducerContext`, so the
/// *next* invocation sees it). Returns when every server's channel has
/// closed (shutdown). Log records are forwarded to `on_log` (the
/// event-bus bridge, plan B2); progress is consumed. Everything is
/// already folded into `tracing` at the handler.
pub async fn drain_server_notifications<F, G>(
    mut channels: Vec<(String, mpsc::UnboundedReceiver<ServerNotification>)>,
    refresher: McpToolRefresher,
    on_tools_changed: F,
    on_log: G,
) where
    F: Fn(ToolRegistry) + Send + Sync + 'static,
    G: Fn(String, String, Option<String>, Value) + Send + Sync + 'static,
{
    // Receive the next notification from any server, tagged with the
    // server name; closed channels drop out (same merge shape as the
    // runner's server-request channel).
    async fn recv_any(
        channels: &mut Vec<(String, mpsc::UnboundedReceiver<ServerNotification>)>,
    ) -> Option<(String, ServerNotification)> {
        std::future::poll_fn(|cx| {
            let mut index = 0;
            while index < channels.len() {
                match channels[index].1.poll_recv(cx) {
                    std::task::Poll::Ready(Some(notification)) => {
                        let server = channels[index].0.clone();
                        return std::task::Poll::Ready(Some((server, notification)));
                    }
                    std::task::Poll::Ready(None) => {
                        channels.remove(index);
                    }
                    std::task::Poll::Pending => index += 1,
                }
            }
            if channels.is_empty() {
                std::task::Poll::Ready(None)
            } else {
                std::task::Poll::Pending
            }
        })
        .await
    }

    while let Some((server, notification)) = recv_any(&mut channels).await {
        match notification {
            ServerNotification::ToolListChanged => {
                info!(server = %server, "tools/list_changed: rebuilding the shared registry");
                on_tools_changed(refresher.rebuild_registry().await);
            }
            // Bridge log records onto the event bus (plan B2); they are
            // already traced at the handler.
            ServerNotification::Log {
                level,
                logger,
                data,
            } => on_log(server, level, logger, data),
            // Progress is consumed so the channel drains; surfacing it
            // to an operator is an Observability follow-up.
            ServerNotification::Progress { .. } => {}
            // Future notification->action loops (ADR-0020): resource
            // invalidation, prompt-list refresh.
            ServerNotification::ResourceUpdated { uri } => {
                debug!(server = %server, uri = %uri, "resource updated (no action wired)");
            }
            note @ (ServerNotification::ResourceListChanged
            | ServerNotification::PromptListChanged) => {
                debug!(server = %server, ?note, "list changed (fetched on demand; no cache to refresh)");
            }
        }
    }
}

/// Render a [`ReadResourceResult`]'s contents into a plain-text
/// block. Text contents are concatenated verbatim; binary (blob)
/// contents are summarised with their size and mime type, since
/// they are not meaningful as model-visible text. Shared by the
/// model-controlled read tool ([`McpResourceTool`]) and the
/// runner's `static_resources` injection so both render identically.
pub fn render_resource_contents(result: &ReadResourceResult) -> String {
    let mut output = String::new();
    for contents in &result.contents {
        match contents {
            ResourceContents::TextResourceContents { text, .. } => {
                output.push_str(text);
                output.push('\n');
            }
            ResourceContents::BlobResourceContents {
                blob, mime_type, ..
            } => {
                output.push_str(&format!(
                    "[binary resource: {} base64 chars, mime {}]\n",
                    blob.len(),
                    mime_type.as_deref().unwrap_or("unknown")
                ));
            }
        }
    }
    output
}

/// Convert a fetched rmcp `GetPromptResult` into the owned, lossless
/// [`PromptSeed`](crate::prompt::PromptSeed) — the rmcp → factor-q
/// boundary for prompts. Everything downstream is provider-neutral.
///
/// rmcp 1.4–1.7 omit `Audio` from `PromptMessageContent` and reject
/// it on the wire, so audio prompt content never reaches here (the
/// fetch fails first). Our [`PromptContent`](crate::prompt::PromptContent)
/// keeps the spec-canonical `Audio` variant regardless — see the
/// `docs/plans/backlog.md` gap note.
fn prompt_seed_from_rmcp(
    server: &str,
    name: &str,
    arguments: BTreeMap<String, String>,
    result: GetPromptResult,
) -> crate::prompt::PromptSeed {
    use crate::prompt::{PromptRole, PromptSeed, PromptSeedMessage};
    let messages = result
        .messages
        .iter()
        .map(|m| PromptSeedMessage {
            role: match m.role {
                PromptMessageRole::User => PromptRole::User,
                PromptMessageRole::Assistant => PromptRole::Assistant,
            },
            content: prompt_content_from_rmcp(&m.content),
        })
        .collect();
    PromptSeed {
        server: server.to_string(),
        name: name.to_string(),
        arguments,
        description: result.description.clone(),
        messages,
    }
}

/// Map one rmcp prompt content block to the owned [`PromptContent`].
/// Captures the primary fields plus annotations / `_meta` (verbatim,
/// as opaque JSON) so the conversion is lossless for everything rmcp
/// can deliver.
fn prompt_content_from_rmcp(content: &PromptMessageContent) -> crate::prompt::PromptContent {
    use crate::prompt::{EmbeddedResource, PromptContent};
    match content {
        PromptMessageContent::Text { text } => PromptContent::Text {
            text: text.clone(),
            meta: crate::prompt::ContentMeta::default(),
        },
        PromptMessageContent::Image { image } => PromptContent::Image {
            data: image.raw.data.clone(),
            mime_type: image.raw.mime_type.clone(),
            meta: content_meta(image.annotations.as_ref(), image.raw.meta.as_ref()),
        },
        PromptMessageContent::ResourceLink { link } => PromptContent::ResourceLink {
            uri: link.raw.uri.clone(),
            name: link.raw.name.clone(),
            meta: content_meta(link.annotations.as_ref(), link.raw.meta.as_ref()),
        },
        PromptMessageContent::Resource { resource } => {
            let annotations = resource.annotations.as_ref();
            let embedded = match &resource.raw.resource {
                ResourceContents::TextResourceContents {
                    uri,
                    mime_type,
                    text,
                    meta,
                } => EmbeddedResource::Text {
                    uri: uri.clone(),
                    mime_type: mime_type.clone(),
                    text: text.clone(),
                    meta: content_meta(annotations, meta.as_ref()),
                },
                ResourceContents::BlobResourceContents {
                    uri,
                    mime_type,
                    blob,
                    meta,
                } => EmbeddedResource::Blob {
                    uri: uri.clone(),
                    mime_type: mime_type.clone(),
                    blob: blob.clone(),
                    meta: content_meta(annotations, meta.as_ref()),
                },
            };
            PromptContent::EmbeddedResource(embedded)
        }
    }
}

/// Capture optional rmcp annotations + `_meta` as opaque JSON,
/// keeping the owned prompt types lossless without re-modelling the
/// (large, evolving) annotation schema. Generic so the rmcp
/// `Annotations` / `Meta` types stay out of the owned `prompt` module.
fn content_meta<A: serde::Serialize, M: serde::Serialize>(
    annotations: Option<&A>,
    meta: Option<&M>,
) -> crate::prompt::ContentMeta {
    crate::prompt::ContentMeta {
        annotations: annotations.and_then(|a| serde_json::to_value(a).ok()),
        meta: meta.and_then(|m| serde_json::to_value(m).ok()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_name_validation_enforces_charset_length_and_reservation() {
        for ok in ["everything", "a", "srv-2", &"x".repeat(48)] {
            assert!(validate_server_name(ok).is_ok(), "'{ok}' should be valid");
        }
        for (bad, why) in [
            ("", "empty"),
            ("Server", "uppercase"),
            ("my_server", "underscore breaks __ splitting"),
            ("srv.1", "dot violates provider tool-name rules"),
            (&"x".repeat(49), "over the 48-char bound"),
            ("builtin", "reserved runtime namespace"),
        ] {
            assert!(validate_server_name(bad).is_err(), "'{bad}' ({why})");
        }
        // The reservation gets its own message so the failure is
        // self-explaining, not a charset complaint.
        let err = validate_server_name("builtin").unwrap_err();
        assert!(format!("{err}").contains("reserved"), "{err}");
    }

    #[test]
    fn namespaced_tool_names_are_bounded_to_provider_limits() {
        assert_eq!(
            namespaced_tool_name("everything", "echo").unwrap(),
            "everything__echo"
        );
        // 48 (max server) + 2 + 14 = 64: exactly at the bound is fine.
        let server = "x".repeat(48);
        assert!(namespaced_tool_name(&server, &"t".repeat(14)).is_ok());
        // One more character crosses the provider bound and must fail
        // loudly at discovery, not at the first LLM call.
        let err = namespaced_tool_name(&server, &"t".repeat(15)).unwrap_err();
        assert!(format!("{err}").contains("64"), "{err}");
        // A remote tool name containing `__` is legal — only the FIRST
        // `__` is the namespace split (server ids cannot contain `_`).
        assert_eq!(
            namespaced_tool_name("srv", "get__thing").unwrap(),
            "srv__get__thing"
        );
    }

    #[test]
    fn advertised_capabilities_reflect_the_grant() {
        let all = FactorQClientHandler::advertised_capabilities(AdvertisedCapabilities::all());
        assert!(all.roots.is_some() && all.sampling.is_some() && all.elicitation.is_some());

        let none = FactorQClientHandler::advertised_capabilities(AdvertisedCapabilities::none());
        assert!(
            none.roots.is_none() && none.sampling.is_none() && none.elicitation.is_none(),
            "nothing is advertised without a grant"
        );

        // Partial grant: only the granted capability is advertised.
        let sampling_only = FactorQClientHandler::advertised_capabilities(AdvertisedCapabilities {
            sampling: true,
            ..AdvertisedCapabilities::none()
        });
        assert!(sampling_only.sampling.is_some());
        assert!(sampling_only.roots.is_none() && sampling_only.elicitation.is_none());
    }

    #[test]
    fn get_info_carries_granted_capabilities() {
        // Default handler (tool-only) advertises nothing inbound.
        let tool_only = FactorQClientHandler::default().get_info();
        assert!(tool_only.capabilities.sampling.is_none());
        assert!(tool_only.capabilities.roots.is_none());
        assert!(tool_only.capabilities.elicitation.is_none());

        // A fully-granted handler advertises all three.
        let granted = FactorQClientHandler::default()
            .with_capabilities(AdvertisedCapabilities::all())
            .get_info();
        assert!(granted.capabilities.sampling.is_some());
        assert!(granted.capabilities.roots.is_some());
        assert!(granted.capabilities.elicitation.is_some());
    }

    // --- D1: in-process mock MCP server (pagination + mutation) ---------
    // The everything server neither paginates its tool list nor mutates
    // it, so these tests serve a small in-process MCP server over a
    // duplex to exercise cursor-following discovery and re-discovery
    // after a tool-list change (the refresh path).
    use rmcp::ServerHandler;
    use rmcp::model::{ListToolsResult, PaginatedRequestParams, ServerInfo, Tool};

    struct MockToolServer {
        tools: Arc<Mutex<Vec<Tool>>>,
        page_size: usize,
    }

    impl ServerHandler for MockToolServer {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(
                ServerCapabilities::builder()
                    .enable_tools()
                    .enable_tool_list_changed()
                    .build(),
            )
        }

        async fn list_tools(
            &self,
            request: Option<PaginatedRequestParams>,
            _context: RequestContext<rmcp::RoleServer>,
        ) -> Result<ListToolsResult, rmcp::ErrorData> {
            let tools = self.tools.lock().await.clone();
            let start: usize = request
                .and_then(|r| r.cursor)
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
            let end = (start + self.page_size).min(tools.len());
            let next_cursor = (end < tools.len()).then(|| end.to_string());
            Ok(ListToolsResult {
                tools: tools[start..end].to_vec(),
                next_cursor,
                ..Default::default()
            })
        }
    }

    fn mock_tool(name: &str) -> Tool {
        Tool::new(
            name.to_string(),
            "mock tool".to_string(),
            Arc::new(serde_json::Map::new()),
        )
    }

    async fn serve_mock(tools: Arc<Mutex<Vec<Tool>>>, page_size: usize) -> Arc<McpClient> {
        let (server_transport, client_transport) = tokio::io::duplex(4096);
        let server = MockToolServer { tools, page_size };
        tokio::spawn(async move {
            if let Ok(running) = server.serve(server_transport).await {
                let _ = running.waiting().await;
            }
        });
        let client = FactorQClientHandler::default()
            .with_capabilities(AdvertisedCapabilities::none())
            .serve(client_transport)
            .await
            .expect("client serves over the duplex");
        Arc::new(client)
    }

    #[tokio::test]
    async fn discover_follows_the_pagination_cursor() {
        // 5 tools, 2 per page → 3 pages; discovery must follow the cursor.
        let tools = Arc::new(Mutex::new(
            (0..5)
                .map(|i| mock_tool(&format!("t{i}")))
                .collect::<Vec<_>>(),
        ));
        let client = serve_mock(tools, 2).await;
        let (_, names) = McpClientManager::discover_tools(&client, "mock")
            .await
            .expect("discover");
        assert_eq!(names.len(), 5, "all pages should be walked");
    }

    /// Parallel-workers Phase 2 (audit H3): concurrent invocations
    /// share the base MCP connections, a load pattern serial dispatch
    /// never produced. N concurrent callers over ONE shared client each
    /// walk a full multi-page discovery — if rmcp's multiplexing
    /// cross-routed or wedged concurrent requests on the one
    /// connection, an interleaved cursor walk would come back short,
    /// wrong, or not at all.
    #[tokio::test]
    async fn concurrent_callers_multiplex_over_one_shared_client() {
        let tools = Arc::new(Mutex::new(
            (0..7)
                .map(|i| mock_tool(&format!("t{i}")))
                .collect::<Vec<_>>(),
        ));
        // Page size 2 → four pages per discovery, so concurrent walks
        // genuinely interleave requests on the shared connection.
        let client = serve_mock(tools, 2).await;

        let mut set = tokio::task::JoinSet::new();
        for caller in 0..4 {
            let client = Arc::clone(&client);
            set.spawn(async move {
                let (_, names) = McpClientManager::discover_tools(&client, "mock")
                    .await
                    .expect("concurrent discover");
                (caller, names)
            });
        }
        while let Some(joined) = set.join_next().await {
            let (caller, names) = joined.expect("caller task");
            assert_eq!(
                names.len(),
                7,
                "caller {caller} must see the complete tool set despite \
                 interleaving with its siblings"
            );
        }
    }

    #[tokio::test]
    async fn rediscovery_reflects_a_mutated_tool_list() {
        let tools = Arc::new(Mutex::new(vec![mock_tool("a"), mock_tool("b")]));
        let client = serve_mock(tools.clone(), 10).await;
        let (_, before) = McpClientManager::discover_tools(&client, "mock")
            .await
            .expect("discover");
        assert_eq!(before.len(), 2);

        // The server mutates its tool list (what tools/list_changed
        // signals); a re-discovery (the refresh path) must reflect it.
        tools.lock().await.push(mock_tool("c"));
        let (_, after) = McpClientManager::discover_tools(&client, "mock")
            .await
            .expect("re-discover");
        assert_eq!(
            after.len(),
            3,
            "refresh re-discovery should see the new tool"
        );
    }

    /// B1 / ADR-0020: the drain consumes notifications and, on
    /// `tools/list_changed`, hands a rebuilt registry (built-ins +
    /// the server's *current* tools) to the install callback.
    #[tokio::test]
    async fn drain_rebuilds_the_registry_on_tool_list_changed() {
        let tools = Arc::new(Mutex::new(vec![mock_tool("alpha")]));
        let client = serve_mock(tools.clone(), 10).await;
        let refresher = McpToolRefresher {
            clients: vec![("mock".to_string(), client)],
            exec_config: fq_tools::builtin::ExecConfig::default(),
        };

        let (notif_tx, notif_rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let (log_tx, mut log_rx) = mpsc::unbounded_channel();
        let drain = tokio::spawn(drain_server_notifications(
            vec![("mock".to_string(), notif_rx)],
            refresher,
            move |registry| {
                let _ = out_tx.send(registry);
            },
            move |server, level, _logger, _data| {
                let _ = log_tx.send((server, level));
            },
        ));

        // Logs are consumed without rebuilding anything.
        notif_tx
            .send(ServerNotification::Log {
                level: "info".to_string(),
                logger: None,
                data: Value::String("hello".to_string()),
            })
            .expect("send log");

        // The server mutates its tool list and signals list_changed;
        // the drain rebuilds and the new tool is in the registry.
        tools.lock().await.push(mock_tool("beta"));
        notif_tx
            .send(ServerNotification::ToolListChanged)
            .expect("send list_changed");

        let rebuilt = tokio::time::timeout(std::time::Duration::from_secs(10), out_rx.recv())
            .await
            .expect("drain should rebuild before the timeout")
            .expect("registry");
        assert!(
            rebuilt.get("mock__alpha").is_some(),
            "existing tool present"
        );
        assert!(rebuilt.get("mock__beta").is_some(), "new tool present");
        assert!(
            rebuilt.get("builtin__file_read").is_some(),
            "built-ins present"
        );
        assert_eq!(
            out_rx.try_recv().ok().map(|_| ()),
            None,
            "the log record must not trigger a rebuild"
        );

        // The log record was forwarded to the bus bridge (B2).
        let (log_server, log_level) =
            tokio::time::timeout(std::time::Duration::from_secs(5), log_rx.recv())
                .await
                .expect("log forwarded before timeout")
                .expect("log record");
        assert_eq!(log_server, "mock");
        assert_eq!(log_level, "info");

        // Closing the last channel ends the drain.
        drop(notif_tx);
        tokio::time::timeout(std::time::Duration::from_secs(5), drain)
            .await
            .expect("drain exits when channels close")
            .expect("drain task");
    }

    /// Regression guard for issue #25 (teardown seam, deterministic
    /// variant): after cancelling a service whose client `Arc` is still
    /// held elsewhere (the condition under which `shutdown` can't
    /// `close().await`), `await_graceful_close` observes the background
    /// task finish its graceful transport close and returns *well within*
    /// the grace window — it does not block for the full timeout and does
    /// not require a force-kill. The stdio (child-process) EPIPE crash the
    /// issue describes is exercised end to end by the `require_npx`-gated
    /// `stdio_shutdown_with_outstanding_tool_arc_is_graceful` integration
    /// test; this one pins the wait logic without needing a child process.
    #[tokio::test]
    async fn await_graceful_close_returns_once_the_service_tears_down() {
        let tools = Arc::new(Mutex::new(vec![mock_tool("only")]));
        let client = serve_mock(tools, 10).await;

        // A second Arc, standing in for a tool wrapper still holding the
        // client — exactly the case where `Arc::get_mut` fails and
        // `shutdown` must fall back to cancel + await.
        let held = Arc::clone(&client);

        client.cancellation_token().cancel();
        // Generous grace; the mock tears down in milliseconds, so this
        // must return well before the deadline (proving it waited for the
        // teardown rather than timing out or force-killing).
        let start = std::time::Instant::now();
        McpClientManager::await_graceful_close(&client, std::time::Duration::from_secs(5)).await;
        assert!(
            client.is_transport_closed(),
            "the service transport should be closed after cancellation"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "await_graceful_close should return on teardown, not time out (took {:?})",
            start.elapsed()
        );
        drop(held);
    }
}
