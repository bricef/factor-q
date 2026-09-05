//! The tool adapter: one tool advertised by an MCP server, presented to
//! the reducer as an ordinary [`fq_tools::Tool`] so it registers in the
//! [`ToolRegistry`](crate::tools::ToolRegistry) beside the built-ins.

use std::sync::Arc;

use fq_tools::{Tool, ToolContext, ToolError, ToolResult};
use rmcp::model::CallToolRequestParams;
use serde_json::Value;

use super::McpClient;

/// A single tool from an MCP server, adapted to the fq-tools [`Tool`] trait.
///
/// Holds an `Arc` to the shared client handle so multiple tools from the
/// same server share one connection.
pub struct McpTool {
    pub(super) tool_name: String,
    pub(super) remote_tool_name: String,
    pub(super) tool_description: String,
    pub(super) tool_input_schema: Value,
    pub(super) client: Arc<McpClient>,
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

        // No `_meta` progress token: rmcp's peer layer mints one for
        // every outbound request and overwrites any the host sets (#605).
        let request =
            CallToolRequestParams::new(self.remote_tool_name.clone()).with_arguments(arguments);

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
