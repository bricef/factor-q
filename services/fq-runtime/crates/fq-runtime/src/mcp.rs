//! MCP (Model Context Protocol) client support.
//!
//! Provides [`McpTool`], which adapts a tool from an external MCP server
//! into the [`fq_tools::Tool`] trait so it can be registered in the
//! [`ToolRegistry`](crate::tools::ToolRegistry) alongside built-in tools.
//!
//! [`McpClientManager`] owns the lifecycle of MCP server child processes:
//! starting them, discovering their tools, and shutting them down.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use fq_tools::{Tool, ToolContext, ToolError, ToolResult};
use rmcp::ClientHandler;
use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientInfo, CompletionContext, CompletionInfo,
    CreateElicitationRequestParams, CreateElicitationResult, CreateMessageRequestMethod,
    CreateMessageRequestParams, CreateMessageResult, ElicitationAction, ElicitationCapability,
    FormElicitationCapability, GetPromptRequestParams, GetPromptResult, JsonObject,
    ListRootsResult, LoggingLevel, LoggingMessageNotificationParam, Prompt, PromptMessageContent,
    PromptMessageRole, ReadResourceRequestParams, ReadResourceResult, Resource, ResourceContents,
    ResourceTemplate, ResourceUpdatedNotificationParam, Root, RootsCapabilities,
    SamplingCapability, ServerCapabilities, SetLevelRequestParams, SubscribeRequestParams,
};
use rmcp::service::{
    MaybeSendFuture, NotificationContext, RequestContext, RoleClient, RunningService,
};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::agent::{RootsGrant, Sandbox};
use crate::validation::ValidatorChain;

/// Configuration for an MCP server to be started as a child process.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    /// Human-readable name for logging.
    pub name: String,
    /// Executable to spawn.
    pub command: String,
    /// Command-line arguments.
    pub args: Vec<String>,
    /// Environment variables to set on the child process.
    pub env: Vec<(String, String)>,
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

/// Derive the `file://` roots a server should be advertised from an
/// agent's sandbox filesystem grant (ADR-0018): the union of the
/// sandbox's read and write paths, deduplicated, each as a `file://`
/// root named by its path. This enforces *advertised roots ⊆ sandbox
/// boundary* by construction — only granted paths are ever advertised.
/// `file://` only for v1.
pub fn roots_from_sandbox(sandbox: &Sandbox) -> Vec<Root> {
    let mut seen = std::collections::BTreeSet::new();
    let mut roots = Vec::new();
    for path in sandbox
        .fs_read_paths()
        .iter()
        .chain(sandbox.fs_write_paths())
    {
        let uri = format!("file://{path}");
        if seen.insert(uri.clone()) {
            roots.push(Root::new(uri).with_name(path.clone()));
        }
    }
    roots
}

/// Compute the roots to advertise to `server`: nothing unless the
/// agent's [`RootsGrant`] permits it, otherwise the sandbox-derived
/// roots run through the outbound validator chain (ADR-0018 §4). A
/// `Deny` from the chain advertises nothing rather than a partial set.
pub fn advertised_roots(
    sandbox: &Sandbox,
    grant: Option<&RootsGrant>,
    server: &str,
    validators: &ValidatorChain<Vec<Root>>,
) -> Vec<Root> {
    if !grant.is_some_and(|g| g.permits(server)) {
        return Vec::new();
    }
    validators
        .run(roots_from_sandbox(sandbox))
        .unwrap_or_default()
}

/// factor-q's MCP client handler.
///
/// Advertises the client-side capabilities factor-q intends to honour
/// (roots, sampling, elicitation) during the initialize handshake, and
/// forwards resource notifications to a sink when one is wired.
/// Server-initiated *requests* (sampling, elicitation, roots) still use
/// rmcp's default handlers — declining or returning empty — until
/// Steps 5–6 of the full-spec plan implement them under ADR-0017's
/// autonomous-resolution policy.
#[derive(Default)]
pub struct FactorQClientHandler {
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

    /// The client capabilities factor-q advertises during initialize:
    /// roots + `list_changed`, sampling, and form-mode elicitation with
    /// schema validation — the inbound surface ADR-0017 governs.
    pub fn advertised_capabilities() -> ClientCapabilities {
        let mut capabilities = ClientCapabilities::default();
        capabilities.roots = Some(RootsCapabilities {
            list_changed: Some(true),
        });
        capabilities.sampling = Some(SamplingCapability::default());
        capabilities.elicitation = Some(ElicitationCapability {
            form: Some(FormElicitationCapability {
                schema_validation: Some(true),
            }),
            url: None,
        });
        capabilities
    }
}

impl ClientHandler for FactorQClientHandler {
    fn get_info(&self) -> ClientInfo {
        let mut info = ClientInfo::default();
        info.capabilities = Self::advertised_capabilities();
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

/// A single tool from an MCP server, adapted to the fq-tools [`Tool`] trait.
///
/// Holds an `Arc` to the shared client handle so multiple tools from the
/// same server share one connection.
pub struct McpTool {
    tool_name: String,
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

        let request = CallToolRequestParams::new(self.tool_name.clone()).with_arguments(arguments);

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

        let (tools, _roots) = self.start_inner(config, None, Vec::new()).await?;
        self.started.insert(key);
        Ok(tools)
    }

    /// Start a *per-invocation* MCP server instance with a wired
    /// server-initiated request channel and advertised `roots`
    /// (ADR-0018).
    ///
    /// Unlike [`start_server`], this never deduplicates: a server
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
    ) -> Result<
        (
            Vec<Arc<dyn Tool>>,
            mpsc::UnboundedReceiver<ServerRequest>,
            RootsHandle,
        ),
        McpError,
    > {
        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let (tools, roots_handle) = self.start_inner(config, Some(req_tx), roots).await?;
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
    ) -> Result<(Vec<Arc<dyn Tool>>, RootsHandle), McpError> {
        info!(
            server = %config.name,
            command = %config.command,
            args = ?config.args,
            "starting MCP server"
        );

        // Build the child process command.
        let env_vars = config.env.clone();
        let args = config.args.clone();
        let transport = TokioChildProcess::new(Command::new(&config.command).configure(|cmd| {
            cmd.args(&args);
            for (k, v) in &env_vars {
                cmd.env(k, v);
            }
        }))
        .map_err(|err| McpError::ServerStart {
            command: config.command.clone(),
            reason: err.to_string(),
        })?;

        // Perform the MCP initialize handshake. The handler advertises
        // factor-q's client capabilities (roots/sampling/elicitation),
        // forwards resource notifications to `notif_rx`, and — on the
        // per-invocation path — bridges server-initiated requests.
        let (notif_tx, notif_rx) = mpsc::unbounded_channel();
        let roots_cell = Arc::new(Mutex::new(roots));
        let mut handler =
            FactorQClientHandler::with_notifications(notif_tx).with_roots(Arc::clone(&roots_cell));
        if let Some(req_tx) = server_request_tx {
            handler = handler.with_server_requests(req_tx);
        }
        let client = handler
            .serve(transport)
            .await
            .map_err(|err| McpError::ServerStart {
                command: config.command.clone(),
                reason: err.to_string(),
            })?;

        let client = Arc::new(client);
        let roots_handle = RootsHandle {
            server: config.name.clone(),
            roots: roots_cell,
            client: Arc::clone(&client),
        };

        // Discover tools.
        let mcp_tools = client
            .list_all_tools()
            .await
            .map_err(|err| McpError::ToolDiscovery {
                command: config.command.clone(),
                reason: err.to_string(),
            })?;

        info!(
            server = %config.name,
            tool_count = mcp_tools.len(),
            "discovered MCP tools"
        );

        let mut tools: Vec<Arc<dyn Tool>> = Vec::with_capacity(mcp_tools.len());
        let mut tool_names: Vec<String> = Vec::with_capacity(mcp_tools.len());

        for mcp_tool in mcp_tools {
            let name = mcp_tool.name.to_string();
            let description = mcp_tool.description.as_deref().unwrap_or("").to_string();

            // Convert the Arc<JsonObject> input_schema to a serde_json::Value.
            let input_schema = serde_json::to_value(&*mcp_tool.input_schema)
                .unwrap_or(Value::Object(serde_json::Map::new()));

            debug!(
                server = %config.name,
                tool = %name,
                "registered MCP tool"
            );

            tool_names.push(name.clone());
            tools.push(Arc::new(McpTool {
                tool_name: name,
                tool_description: description,
                tool_input_schema: input_schema,
                client: Arc::clone(&client),
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
                McpResourceTool::list(&config.name, Arc::clone(&client)),
                McpResourceTool::read(&config.name, Arc::clone(&client)),
                McpResourceTool::list_templates(&config.name, Arc::clone(&client)),
            ] {
                debug!(
                    server = %config.name,
                    tool = %resource_tool.name(),
                    "registered MCP resource tool"
                );
                tool_names.push(resource_tool.name().to_string());
                tools.push(Arc::new(resource_tool));
            }
        }

        self.servers.push(RunningServer {
            name: config.name,
            client,
            tool_names,
            notifications: Mutex::new(notif_rx),
        });

        Ok((tools, roots_handle))
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

    /// Gracefully shut down all managed MCP server processes.
    pub async fn shutdown(&mut self) {
        for server in &mut self.servers {
            info!(
                server = %server.name,
                tools = ?server.tool_names,
                "shutting down MCP server"
            );
            // RunningService is behind Arc — we need to get a mutable
            // reference. If other Arcs still exist (McpTool instances),
            // we can't call close(), but the drop guard on the
            // RunningService will cancel the child process anyway.
            if let Some(client) = Arc::get_mut(&mut server.client) {
                if let Err(err) = client.close().await {
                    warn!(
                        server = %server.name,
                        error = %err,
                        "error during MCP server shutdown"
                    );
                }
            } else {
                debug!(
                    server = %server.name,
                    "MCP client has outstanding references, relying on drop guard"
                );
            }
        }
        self.servers.clear();
        self.started.clear();
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
    fn advertises_roots_sampling_elicitation() {
        let capabilities = FactorQClientHandler::advertised_capabilities();
        assert!(capabilities.roots.is_some(), "roots advertised");
        assert!(capabilities.sampling.is_some(), "sampling advertised");
        assert!(capabilities.elicitation.is_some(), "elicitation advertised");
    }

    #[test]
    fn get_info_carries_advertised_capabilities() {
        let info = FactorQClientHandler::default().get_info();
        assert!(info.capabilities.roots.is_some());
        assert!(info.capabilities.sampling.is_some());
        assert!(info.capabilities.elicitation.is_some());
    }
}
