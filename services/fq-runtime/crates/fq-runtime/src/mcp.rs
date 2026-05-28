//! MCP (Model Context Protocol) client support.
//!
//! Provides [`McpTool`], which adapts a tool from an external MCP server
//! into the [`fq_tools::Tool`] trait so it can be registered in the
//! [`ToolRegistry`](crate::tools::ToolRegistry) alongside built-in tools.
//!
//! [`McpClientManager`] owns the lifecycle of MCP server child processes:
//! starting them, discovering their tools, and shutting them down.

use std::collections::HashSet;
use std::sync::Arc;

use fq_tools::{Tool, ToolContext, ToolError, ToolResult};
use rmcp::ClientHandler;
use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientInfo, ElicitationCapability,
    FormElicitationCapability, ReadResourceRequestParams, ReadResourceResult, Resource,
    ResourceContents, ResourceTemplate, RootsCapabilities, SamplingCapability, ServerCapabilities,
};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde_json::Value;
use tokio::process::Command;
use tracing::{debug, info, warn};

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
}

// Type alias for the concrete client handle we store.
type McpClient = RunningService<RoleClient, FactorQClientHandler>;

/// factor-q's MCP client handler.
///
/// Advertises the client-side capabilities factor-q intends to honour
/// (roots, sampling, elicitation) during the initialize handshake.
/// Server-initiated requests (sampling, elicitation, roots) still use
/// rmcp's default handlers — declining or returning empty — until
/// Steps 5–6 of the full-spec plan implement them under ADR-0017's
/// autonomous-resolution policy.
pub struct FactorQClientHandler;

impl FactorQClientHandler {
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

/// Whether a synthesized resource tool lists or reads.
#[derive(Clone, Copy)]
enum ResourceOp {
    List,
    Read,
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
        // factor-q's client capabilities (roots/sampling/elicitation) and
        // will answer server-initiated requests in later steps.
        let client =
            FactorQClientHandler
                .serve(transport)
                .await
                .map_err(|err| McpError::ServerStart {
                    command: config.command.clone(),
                    reason: err.to_string(),
                })?;

        let client = Arc::new(client);

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

        self.started.insert(key);
        self.servers.push(RunningServer {
            name: config.name,
            client,
            tool_names,
        });

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
        let info = FactorQClientHandler.get_info();
        assert!(info.capabilities.roots.is_some());
        assert!(info.capabilities.sampling.is_some());
        assert!(info.capabilities.elicitation.is_some());
    }
}
