//! Server lifecycle: starting MCP servers, discovering what they
//! advertise, calling into them, and tearing them down.
//!
//! [`McpClientManager`] is the one owner of every running server. It
//! deduplicates shared servers by transport identity, wraps each
//! discovered tool ([`McpTool`](super::McpTool)) and each synthesized
//! resource tool ([`McpResourceTool`](super::McpResourceTool)), exposes
//! the per-server request surface (resources, prompts, logging,
//! cancellable tool calls), and owns the graceful-shutdown ordering the
//! stdio transport needs (#25).

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use fq_tools::Tool;
use fq_tools::builtin::ExecConfig;
use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, CancelledNotificationParam,
    ClientRequest, CompletionContext, CompletionInfo, GetPromptRequestParams, JsonObject,
    LoggingLevel, Prompt, ReadResourceRequestParams, ReadResourceResult, Resource,
    ResourceTemplate, Root, ServerCapabilities, ServerResult, SetLevelRequestParams,
    SubscribeRequestParams,
};
use rmcp::service::PeerRequestOptions;
use rmcp::transport::StreamableHttpClientTransport;
use serde_json::Value;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};

use super::naming::{namespaced_tool_name, validate_server_name};
use super::prompt_convert::prompt_seed_from_rmcp;
use super::server_config::SharedServerKey;
use super::{
    AdvertisedCapabilities, FactorQClientHandler, McpClient, McpError, McpResourceReader,
    McpResourceTool, McpServerConfig, McpTool, McpToolRefresher, RootsHandle, ServerNotification,
    ServerRequest, default_server_root, stdio,
};

/// Tracks a running MCP server and its client handle.
pub(super) struct RunningServer {
    pub(super) name: String,
    pub(super) client: Arc<McpClient>,
    pub(super) tool_names: Vec<String>,
    /// Receiver for resource notifications the handler forwards.
    pub(super) notifications: Mutex<mpsc::UnboundedReceiver<ServerNotification>>,
}

/// Manages the lifecycle of MCP server child processes.
///
/// Starts servers, discovers their tools (wrapping each as an [`McpTool`]),
/// and provides graceful shutdown. Deduplicates servers by transport
/// identity — the stdio process spawned, or the remote endpoint dialled
/// — so the same server declared by multiple agents starts only once.
pub struct McpClientManager {
    pub(super) servers: Vec<RunningServer>,
    /// Transport identities already started, to deduplicate.
    started: HashSet<SharedServerKey>,
    /// Root of the stdio servers' working directories,
    /// `<root>/<server>` (see [`stdio`](super::stdio)).
    server_root: std::path::PathBuf,
}

impl Default for McpClientManager {
    fn default() -> Self {
        Self::new()
    }
}

impl McpClientManager {
    /// Stdio servers start under the default root (the temp dir, never the cwd).
    pub fn new() -> Self {
        Self::with_server_root(default_server_root())
    }

    /// Stdio servers start in `<server_root>/<server>`, created on demand (#541).
    pub fn with_server_root(server_root: std::path::PathBuf) -> Self {
        Self {
            servers: Vec::new(),
            started: HashSet::new(),
            server_root,
        }
    }

    /// Start an MCP server, discover its tools, and return them as
    /// `Arc<dyn Tool>` values ready for registration in a [`ToolRegistry`](crate::tools::ToolRegistry).
    ///
    /// If a server with the same transport identity — the stdio process
    /// spawned, or the remote endpoint dialled — has already been started,
    /// this is a no-op returning an empty vec (its tools were registered
    /// on the first call). A config declaring no transport at all is
    /// unstartable, and errors rather than deduplicating.
    pub async fn start_server(
        &mut self,
        config: McpServerConfig,
    ) -> Result<Vec<Arc<dyn Tool>>, McpError> {
        let key = SharedServerKey::from_config(&config)?;
        if self.started.contains(&key) {
            debug!(
                server = %config.name,
                target = %key.target(),
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
            // The endpoint or the command: `command` alone is empty for
            // a remote server, so that log line named nothing.
            target = %config.url.as_deref().unwrap_or(&config.command),
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
            // stdio child process: cleared env, pinned PATH, own cwd (#541, `stdio`).
            None => {
                let transport = stdio::spawn_transport(&config, &self.server_root)?;
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
    /// server advertises the resources capability. Shared by initial
    /// startup, [`refresh_tools`](Self::refresh_tools) (Step 7,
    /// `notifications/tools/list_changed`) and
    /// [`McpToolRefresher`](super::McpToolRefresher)'s registry
    /// rebuild. Returns the tool wrappers and their names.
    pub(super) async fn discover_tools(
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
            .is_some_and(|info| info.capabilities.resources.is_some());
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
    /// its [`ToolRegistry`](crate::tools::ToolRegistry) rather than
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
    /// (`logging/setLevel`); only messages at or above `level` reach the
    /// notification sink thereafter. MCP deprecated the request in
    /// SEP-2577 (rmcp 1.8 flags it); allowed until servers drop it.
    #[allow(deprecated)]
    pub async fn set_logging_level(
        &self,
        server: &str,
        level: LoggingLevel,
    ) -> Result<(), McpError> {
        self.client_for(server)?
            .set_level(SetLevelRequestParams::new(level))
            .await
            .map(|_| ())
            .map_err(|err| McpError::LoggingOp {
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

#[cfg(test)]
mod tests;
