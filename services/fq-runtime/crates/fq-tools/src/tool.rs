use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::sandbox::{SandboxError, ToolSandbox};

/// Which tool call this is: the invocation it belongs to and the
/// model-issued id of the call within it.
///
/// Carried so a tool that hands work to something outside this process
/// can label it — the MCP adapter records it against the progress token
/// rmcp mints, which is what lets an inbound `notifications/progress`
/// be attributed back to the invocation and call that caused it
/// (<https://github.com/bricef/factor-q/issues/605>). Strings, not
/// typed ids: `fq-tools` is the primitive layer and does not know the
/// runtime's event vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallIdentity {
    pub invocation_id: String,
    pub call_id: String,
}

/// Context passed to a tool for each invocation. Carries the agent's
/// sandbox plus anything else the tool needs that is scoped to a
/// particular agent run.
pub struct ToolContext<'a> {
    pub sandbox: &'a ToolSandbox,
    /// How long the host will wait for this call before it gives up
    /// and reports a timeout to the model. `None` in the bare
    /// constructor — the tool then runs to completion on its own terms.
    ///
    /// A tool that can *act* on the deadline should: the MCP adapter
    /// uses it to cancel the outbound request at the deadline, so the
    /// server is told to stop rather than left working on a result
    /// nobody will read. The host arms its own timer as well, a little
    /// later, as the backstop for tools that cannot (see
    /// [`Tool::requested_deadline`]).
    pub deadline: Option<Duration>,
    /// Which call this is. `None` outside a real invocation (tests,
    /// direct tool use).
    pub call: Option<ToolCallIdentity>,
}

impl<'a> ToolContext<'a> {
    pub fn new(sandbox: &'a ToolSandbox) -> Self {
        Self {
            sandbox,
            deadline: None,
            call: None,
        }
    }

    /// Tell the tool how long the host will wait for it.
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Label this call with the invocation and call id it belongs to.
    pub fn with_call(
        mut self,
        invocation_id: impl Into<String>,
        call_id: impl Into<String>,
    ) -> Self {
        self.call = Some(ToolCallIdentity {
            invocation_id: invocation_id.into(),
            call_id: call_id.into(),
        });
        self
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

    /// The deadline this tool asks for on *this* call, when it manages
    /// one of its own.
    ///
    /// `None` — every tool but `exec` — means "the host decides", and
    /// the host applies its configured default. `Some(d)` is a
    /// *request*: the host clamps it to its ceiling and never grants
    /// more. Returning `Some` also tells the host this tool will report
    /// its own timeout, so the host arms its timer slightly later and
    /// stays a backstop rather than racing the tool's own error, which
    /// is the more useful one (it names the command and keeps the
    /// output captured so far).
    ///
    /// Answered from the parameters because a per-call `timeout_secs`
    /// is part of the request, not of the tool.
    fn requested_deadline(&self, _params: &Value) -> Option<Duration> {
        None
    }

    /// Execute the tool with the given parameters.
    async fn execute(&self, ctx: &ToolContext<'_>, params: Value) -> Result<ToolResult, ToolError>;
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

    /// The call did not finish inside the deadline the host allowed it
    /// (`[tools] default_timeout_secs`, or the tool's own request
    /// clamped to `[tools] max_timeout_secs`).
    ///
    /// Its own variant because a deadline is not an execution failure:
    /// the host learns nothing about whether the work happened, the
    /// model is told to try something else, and the runtime counts
    /// consecutive ones so a permanently unresponsive tool ends the
    /// invocation instead of burning its budget one error at a time
    /// (<https://github.com/bricef/factor-q/issues/547>).
    ///
    /// **Every** deadline arrives here, the ones a tool enforces on
    /// itself included. `exec` used to answer its own timeout as an
    /// ordinary `is_error` result, which read to the host as the tool
    /// having *answered* — so a command that hung on every attempt
    /// never advanced the consecutive-timeout count and the one tool
    /// with a real chance of hanging for fifteen minutes at a time was
    /// the one the limit did not protect.
    ///
    /// `output` is what the tool captured before it stopped, when it
    /// stopped the work itself. `None` means nothing stopped but the
    /// waiting — the host's backstop, or a cancelled MCP request the
    /// server may still be working on — and the two cases give the
    /// model opposite advice about whether the effect happened.
    #[error("timed out after {}s", .after.as_secs())]
    TimedOut {
        after: Duration,
        output: Option<String>,
    },
}

impl From<SandboxError> for ToolError {
    fn from(err: SandboxError) -> Self {
        match err {
            SandboxError::PermissionDenied { reason, .. } => ToolError::PermissionDenied(reason),
            SandboxError::NotFound(path) => ToolError::NotFound(path),
            SandboxError::InvalidPath { target, reason } => {
                ToolError::InvalidParameters(format!("{reason} ({})", target.display()))
            }
            // The grant held; the path is simply the wrong shape. That
            // is the caller's argument problem, so it reads as invalid
            // parameters (with the full "is X, needs Y" message) and
            // the model can pick a different path rather than
            // concluding it lacks permission.
            err @ SandboxError::NotRegularFile { .. } => {
                ToolError::InvalidParameters(err.to_string())
            }
            SandboxError::Io { path, source } => {
                ToolError::Io(format!("{}: {source}", path.display()))
            }
            // Both mangled-path diagnoses are the *caller's* argument
            // problem, not a filesystem fact — surface them as invalid
            // parameters (with the full self-diagnosing message) so a
            // model corrects its tool call instead of concluding the
            // file or its workspace is gone.
            err @ (SandboxError::MisquotedPath { .. }
            | SandboxError::UnsubstitutedPlaceholder { .. }) => {
                ToolError::InvalidParameters(err.to_string())
            }
        }
    }
}
