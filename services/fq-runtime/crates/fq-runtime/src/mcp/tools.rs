//! The tool adapter: one tool advertised by an MCP server, presented to
//! the reducer as an ordinary [`fq_tools::Tool`] so it registers in the
//! [`ToolRegistry`](crate::tools::ToolRegistry) beside the built-ins.

use std::sync::Arc;

use fq_tools::{Tool, ToolContext, ToolError, ToolResult};
use serde_json::Value;

use super::McpClient;
use super::call::{OutboundCall, call_tool_cancellable};
use super::progress::ProgressRegistry;

/// A single tool from an MCP server, adapted to the fq-tools [`Tool`] trait.
///
/// Holds an `Arc` to the shared client handle so multiple tools from the
/// same server share one connection.
pub struct McpTool {
    pub(super) tool_name: String,
    /// The server's factor-q name. Needed apart from `tool_name`
    /// because the progress-correlation key is `(server, token)` —
    /// rmcp numbers its tokens per peer, so two servers both issue a
    /// token `0`.
    pub(super) server_name: String,
    pub(super) remote_tool_name: String,
    pub(super) tool_description: String,
    pub(super) tool_input_schema: Value,
    pub(super) client: Arc<McpClient>,
    pub(super) progress: ProgressRegistry,
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

    /// Send the call and wait — but only as long as the host said, and
    /// cancellably.
    ///
    /// This used to be a bare `call_tool(...).await`, so a server that
    /// accepted the request and never answered parked the invocation
    /// for as long as the daemon lived (review finding B2). The
    /// deadline the host puts on the context is now the cancellation
    /// trigger: at it, the request is abandoned *and the server is told
    /// so* with `notifications/cancelled`, which is the difference
    /// between a deadline and merely looking away — a server left
    /// working produces a result nobody will read, and on a stdio
    /// server that is a child process still burning CPU.
    ///
    /// With no deadline on the context (direct or test use) the call
    /// waits indefinitely, exactly as before.
    async fn execute(&self, ctx: &ToolContext<'_>, params: Value) -> Result<ToolResult, ToolError> {
        let arguments = match params.as_object() {
            Some(obj) => obj.clone(),
            None if params.is_null() => serde_json::Map::new(),
            None => {
                return Err(ToolError::InvalidParameters(
                    "MCP tool parameters must be a JSON object".to_string(),
                ));
            }
        };

        let deadline = ctx.deadline;
        let cancel = async move {
            match deadline {
                Some(after) => tokio::time::sleep(after).await,
                None => std::future::pending().await,
            }
        };

        let outcome = call_tool_cancellable(
            OutboundCall {
                client: &self.client,
                server: &self.server_name,
                remote_tool_name: &self.remote_tool_name,
                tool_name: &self.tool_name,
                arguments,
                progress: &self.progress,
                call: ctx.call.as_ref(),
            },
            cancel,
        )
        .await
        .map_err(|err| ToolError::ExecutionFailed(err.to_string()))?;

        let Some(result) = outcome else {
            return Err(ToolError::TimedOut {
                after: deadline.unwrap_or_default(),
                // The server was asked to stop; whether it did is its
                // business, so nothing here is a record of what
                // happened.
                output: None,
            });
        };

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
