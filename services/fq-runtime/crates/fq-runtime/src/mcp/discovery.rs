//! What a server advertises, and what it is allowed to cost to ask.
//!
//! Discovery is `tools/list` followed page by page, plus the
//! host-synthesized resource tools. It is not a one-shot read, which is
//! why it has its own module and its own bounds: the server chooses how
//! many pages there are, how many tools are on each, and how long to
//! take answering, and every one of those is unbounded in the protocol.
//! A server that returns a `next_cursor` for ever spun the loop with
//! the tool vector growing, and `notifications/tools/list_changed` let
//! it start the spin again at any time (review finding B3,
//! <https://github.com/bricef/factor-q/issues/548>).
//!
//! Three bounds, all `[mcp]` in `fqd.toml`: a page cap, a tool-count
//! cap, and one deadline over the whole walk. Exceeding any of them is
//! a [`McpError::ToolDiscovery`] naming the cap and the key that sets
//! it, so the operator reads what to change rather than "discovery
//! failed".

use std::sync::Arc;

use fq_tools::Tool;
use rmcp::model::PaginatedRequestParams;
use serde_json::Value;
use tracing::{debug, info};

use super::limits::McpLimits;
use super::naming::{namespaced_tool_name, validate_server_name};
use super::progress::ProgressRegistry;
use super::{McpClient, McpError, McpResourceTool, McpTool};

/// Discover a server's current tools: the regular MCP tools plus the
/// synthesized host-fulfilled resource tools when the server advertises
/// the resources capability. Shared by initial startup,
/// [`refresh_tools`](super::McpClientManager::refresh_tools)
/// (`notifications/tools/list_changed`) and
/// [`McpToolRefresher`](super::McpToolRefresher)'s registry rebuild —
/// the same bounds apply on every one of those paths, because a server
/// that can spin discovery at boot can spin it again by notification.
/// Returns the tool wrappers and their names.
pub(super) async fn discover_tools(
    client: &Arc<McpClient>,
    server_name: &str,
    progress: &ProgressRegistry,
    limits: &McpLimits,
) -> Result<(Vec<Arc<dyn Tool>>, Vec<String>), McpError> {
    validate_server_name(server_name)?;
    let discovery_error = |reason: String| McpError::ToolDiscovery {
        command: server_name.to_string(),
        reason,
    };
    let mcp_tools = tokio::time::timeout(
        limits.discovery_timeout,
        list_tools_bounded(client, server_name, limits),
    )
    .await
    .map_err(|_| {
        discovery_error(format!(
            "no complete tool list within the {}s discovery deadline ([mcp] \
             discovery_timeout_secs)",
            limits.discovery_timeout.as_secs()
        ))
    })??;

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
            server_name: server_name.to_string(),
            remote_tool_name: remote_name,
            tool_description: description,
            tool_input_schema: input_schema,
            client: Arc::clone(client),
            progress: progress.clone(),
        }));
    }

    // Synthesize host-fulfilled resource tools when the server
    // advertises the resources capability, so the agent's LLM can
    // list/read its resources on demand.
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

/// Follow `tools/list` to the end of its cursor chain, refusing past
/// the page and tool-count caps.
///
/// Both caps are checked *before* the next request rather than after
/// the answer, so the memory the walk can hold is bounded by what the
/// caps allow and not by one more page beyond them.
async fn list_tools_bounded(
    client: &Arc<McpClient>,
    server_name: &str,
    limits: &McpLimits,
) -> Result<Vec<rmcp::model::Tool>, McpError> {
    let discovery_error = |reason: String| McpError::ToolDiscovery {
        command: server_name.to_string(),
        reason,
    };
    let mut tools: Vec<rmcp::model::Tool> = Vec::new();
    let mut cursor = None;
    for page in 1..=limits.max_discovery_pages {
        let result = client
            .list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor)))
            .await
            .map_err(|err| discovery_error(err.to_string()))?;
        if tools.len() + result.tools.len() > limits.max_tools as usize {
            return Err(discovery_error(format!(
                "declares more than {} tools ([mcp] max_tools); discovery stopped on page \
                 {page}",
                limits.max_tools
            )));
        }
        tools.extend(result.tools);
        cursor = result.next_cursor;
        if cursor.is_none() {
            return Ok(tools);
        }
    }
    Err(discovery_error(format!(
        "still returning a next_cursor after {} pages of tools/list ([mcp] \
         max_discovery_pages); discovery stopped with {} tools collected",
        limits.max_discovery_pages,
        tools.len()
    )))
}

#[cfg(test)]
mod tests;
