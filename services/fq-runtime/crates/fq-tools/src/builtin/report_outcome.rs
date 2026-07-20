//! Built-in `report_outcome` tool — schema only (#125).
//!
//! The agent's explicit, status-bearing terminal: calling it declares
//! how the *task* went (`success | failed | blocked | partial`) and
//! ends the invocation. The declaration is intercepted in the reducer
//! harness (`fq-runtime`'s `worker/reducer/harness.rs`) as a pure
//! mapping to the terminal transition — it is never dispatched through
//! [`Tool::execute`]; the implementation here exists to advertise the
//! schema and to fail loudly if a host ever forgets the interception.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::tool::{Tool, ToolContext, ToolError, ToolResult};

/// Canonical (bare) tool name. Pinned in a const so the fq-runtime
/// harness interception and this schema refer to the same string.
pub const REPORT_OUTCOME_TOOL_NAME: &str = "report_outcome";

/// The task-status values the harness accepts. Must stay in lockstep
/// with fq-runtime's `TaskStatus` serde spellings — the harness parses
/// these strings.
pub const TASK_STATUS_VALUES: &[&str] = &["success", "failed", "blocked", "partial"];

/// Built-in `report_outcome` tool. Schema-only; the harness intercepts
/// the call (see module docs).
#[derive(Debug, Default)]
pub struct ReportOutcomeTool;

impl ReportOutcomeTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for ReportOutcomeTool {
    fn name(&self) -> &str {
        REPORT_OUTCOME_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Declare the outcome of your task and END this invocation \
         immediately — nothing runs after this call, so finish all \
         other work first. `status` is how the TASK went, independent \
         of whether the runtime worked: `success` (goal achieved), \
         `failed` (goal not achieved), `blocked` (could not proceed — \
         say what blocked you), `partial` (some of the goal delivered). \
         This call is the only way to end your run: a turn with no \
         tool calls does not finish anything — the harness asks you \
         to continue. Every run ends with this call, whatever the \
         status, with a summary a human can act on."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": TASK_STATUS_VALUES,
                    "description": "How the task went (not the runtime)."
                },
                "summary": {
                    "type": "string",
                    "description": "One short paragraph: what was delivered or what blocked you, actionable for a human."
                }
            },
            "required": ["status", "summary"],
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        _ctx: &ToolContext<'_>,
        _params: Value,
    ) -> Result<ToolResult, ToolError> {
        // Reached only when the harness failed to intercept — e.g. the
        // arguments did not parse as a valid declaration (unknown
        // status). The error teaches the model to correct the call
        // rather than mis-stamping a terminal status.
        Err(ToolError::ExecutionFailed(format!(
            "report_outcome was not accepted as a terminal declaration — \
             check the arguments: `status` must be one of {TASK_STATUS_VALUES:?} \
             and `summary` a string. Correct the call and declare again — \
             the run does not end until a valid declaration is made."
        )))
    }
}
