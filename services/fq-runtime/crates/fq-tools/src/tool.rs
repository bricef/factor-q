use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Trait that all tools must implement.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// Unique name of the tool.
    fn name(&self) -> &str;

    /// Description of the tool for the LLM.
    fn description(&self) -> &str;

    /// JSON Schema for the tool's parameters.
    fn parameters_schema(&self) -> Value;

    /// Execute the tool with the given parameters.
    async fn execute(&self, params: Value) -> Result<ToolResult, ToolError>;
}

/// Result of a tool execution.
#[derive(Debug, Serialize, Deserialize)]
pub struct ToolResult {
    pub output: String,
    pub is_error: bool,
}

/// Error from a tool execution.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    #[error("execution failed: {0}")]
    ExecutionFailed(String),

    #[error("invalid parameters: {0}")]
    InvalidParameters(String),
}
