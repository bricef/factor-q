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
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
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
}

// Type alias for the concrete client handle we store.
type McpClient = RunningService<RoleClient, ()>;

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

        let request =
            CallToolRequestParams::new(self.tool_name.clone()).with_arguments(arguments);

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
        let transport = TokioChildProcess::new(
            Command::new(&config.command).configure(|cmd| {
                cmd.args(&args);
                for (k, v) in &env_vars {
                    cmd.env(k, v);
                }
            }),
        )
        .map_err(|err| McpError::ServerStart {
            command: config.command.clone(),
            reason: err.to_string(),
        })?;

        // Perform the MCP initialize handshake.
        let client = ()
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
            let description = mcp_tool
                .description
                .as_deref()
                .unwrap_or("")
                .to_string();

            // Convert the Arc<JsonObject> input_schema to a serde_json::Value.
            let input_schema =
                serde_json::to_value(&*mcp_tool.input_schema).unwrap_or(Value::Object(
                    serde_json::Map::new(),
                ));

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

        self.started.insert(key);
        self.servers.push(RunningServer {
            name: config.name,
            client,
            tool_names,
        });

        Ok(tools)
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
