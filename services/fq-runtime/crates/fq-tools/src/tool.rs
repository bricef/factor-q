use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::sandbox::{SandboxError, ToolSandbox};

/// Context passed to a tool for each invocation. Carries the agent's
/// sandbox plus anything else the tool needs that is scoped to a
/// particular agent run.
pub struct ToolContext<'a> {
    pub sandbox: &'a ToolSandbox,
}

impl<'a> ToolContext<'a> {
    pub fn new(sandbox: &'a ToolSandbox) -> Self {
        Self { sandbox }
    }
}

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
    async fn execute(
        &self,
        ctx: &ToolContext<'_>,
        params: Value,
    ) -> Result<ToolResult, ToolError>;
}

/// Result of a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub output: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            is_error: false,
        }
    }
}

/// Error from a tool execution.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    #[error("path not found: {0:?}")]
    NotFound(PathBuf),

    #[error("invalid parameters: {0}")]
    InvalidParameters(String),

    #[error("io error: {0}")]
    Io(String),

    #[error("execution failed: {0}")]
    ExecutionFailed(String),
}

impl From<SandboxError> for ToolError {
    fn from(err: SandboxError) -> Self {
        match err {
            SandboxError::PermissionDenied { target, reason } => ToolError::PermissionDenied(
                format!("{reason} ({})", target.display()),
            ),
            SandboxError::NotFound(path) => ToolError::NotFound(path),
            SandboxError::InvalidPath { target, reason } => ToolError::InvalidParameters(
                format!("{reason} ({})", target.display()),
            ),
            SandboxError::Io { path, source } => {
                ToolError::Io(format!("{}: {source}", path.display()))
            }
        }
    }
}
