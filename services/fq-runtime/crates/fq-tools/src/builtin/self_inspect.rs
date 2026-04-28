//! `self_inspect` built-in tool: lets an agent ask the runtime
//! about its own invocation-scoped state (budget, iterations,
//! configured model, available tools).
//!
//! Unlike the other built-ins, `self_inspect` is a
//! **host-fulfilled tool**: the runtime's invocation state
//! (cost so far, iteration counter, configured budget) is not
//! reachable from a normal [`ToolContext`], which carries only
//! the sandbox. Instead, both the legacy executor and the
//! reducer runner intercept tool calls whose name matches
//! [`SELF_INSPECT_TOOL_NAME`] before they reach the registry,
//! synthesise the result from their own tracked state, and
//! emit the same canonical events as a regular tool dispatch.
//!
//! This crate still owns the **schema** so the LLM advertising
//! list (`build_schemas`) and the human-facing tool catalogue
//! agree on what `self_inspect` looks like. The `execute()`
//! method here is a tripwire: it should never run, and if it
//! does, that signals a runtime that hasn't been taught about
//! the host-fulfilled-tool pattern.
//!
//! Why expose this at all: design principle #2 — *no
//! confabulation where data exists*. Agents will be asked
//! about budgets, iteration counts, and the model they're
//! running on. The runtime knows all of it. Without
//! `self_inspect`, the agent would invent answers rather than
//! say "I don't know." With it, the agent has an authoritative
//! tool to consult instead.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::tool::{Tool, ToolContext, ToolError, ToolResult};

/// Canonical tool name. Pinned in a const so both crates that
/// need to special-case it (`fq-runtime` executor and reducer
/// runner) refer to the same string.
pub const SELF_INSPECT_TOOL_NAME: &str = "self_inspect";

/// All known include-filter values for `self_inspect`'s
/// `include` parameter. The host honours these strings.
pub const SELF_INSPECT_SECTIONS: &[&str] = &["budget", "iterations", "model", "tools"];

/// Built-in `self_inspect` tool. Schema-only; the actual data
/// fulfilment happens in the host (see module docs).
#[derive(Debug, Default)]
pub struct SelfInspectTool;

impl SelfInspectTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for SelfInspectTool {
    fn name(&self) -> &str {
        SELF_INSPECT_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Inspect this agent's own invocation-scoped runtime state. \
         Returns budget usage and remaining headroom, iteration count, \
         the configured model, and the list of available tools. Use \
         this when asked about your own runtime state instead of \
         guessing — the runtime tracks the authoritative values."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "include": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": SELF_INSPECT_SECTIONS
                    },
                    "description": "Optional subset of sections to return. If omitted, all sections are returned."
                }
            },
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        _ctx: &ToolContext<'_>,
        _params: Value,
    ) -> Result<ToolResult, ToolError> {
        // This path indicates a runtime that didn't intercept
        // self_inspect at the host layer. That's a programming
        // error in the host, not a user-facing failure.
        Err(ToolError::ExecutionFailed(
            "self_inspect must be dispatched by the host runtime; \
             reaching the registry execute() path means the host \
             forgot to intercept the tool name. See \
             docs/guide/reducer-harness.md."
                .to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::ToolSandbox;

    #[test]
    fn schema_advertises_optional_include() {
        let tool = SelfInspectTool::new();
        let schema = tool.parameters_schema();
        let props = schema
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties");
        assert!(props.contains_key("include"));
        // No required fields.
        assert!(schema.get("required").is_none());
    }

    #[tokio::test]
    async fn execute_in_registry_path_returns_marker_error() {
        // If the registry path runs (e.g. host hasn't been taught
        // to intercept), the tool surfaces a clear error rather
        // than silently returning empty data.
        let tool = SelfInspectTool::new();
        let sandbox = ToolSandbox::new();
        let ctx = ToolContext::new(&sandbox);
        let err = tool.execute(&ctx, json!({})).await.unwrap_err();
        match err {
            ToolError::ExecutionFailed(msg) => {
                assert!(msg.contains("host runtime"));
            }
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
    }
}
