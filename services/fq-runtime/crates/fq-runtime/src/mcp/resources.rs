//! Model-controlled access to a server's MCP resources.
//!
//! A server that advertises the resources capability gets a synthesized
//! trio of host tools ([`McpResourceTool`]) — list, read, and list
//! templates — so the agent's LLM can discover and pull resources on
//! demand. [`render_resource_contents`] is the shared rendering used by
//! both that read tool and the runner's `static_resources` injection.

use std::sync::Arc;

use fq_tools::{Tool, ToolContext, ToolError, ToolResult};
use rmcp::model::{ReadResourceRequestParams, ReadResourceResult, ResourceContents};
use serde_json::Value;

use super::McpClient;

/// Which resource operation a synthesized tool performs.
#[derive(Clone, Copy)]
enum ResourceOp {
    List,
    Read,
    ListTemplates,
}

/// A host-synthesized tool exposing a server's MCP resources to the
/// agent's LLM (model-controlled access). One pair per server that
/// advertises the resources capability — `<server>__list_resources`
/// and `<server>__read_resource`. Mirrors [`McpTool`](super::McpTool):
/// it holds the shared client handle and registers in the
/// [`ToolRegistry`](crate::tools::ToolRegistry) like any other tool, so
/// no reducer-runner changes are needed. (Host-curated injection of
/// declared resources is a separate path — see the plan's step 3d.)
pub struct McpResourceTool {
    name: String,
    description: String,
    schema: Value,
    op: ResourceOp,
    client: Arc<McpClient>,
}

impl McpResourceTool {
    pub(super) fn list(server: &str, client: Arc<McpClient>) -> Self {
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

    pub(super) fn read(server: &str, client: Arc<McpClient>) -> Self {
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

    pub(super) fn list_templates(server: &str, client: Arc<McpClient>) -> Self {
        Self {
            name: format!("{server}__list_resource_templates"),
            description: format!(
                "List the resource templates the '{server}' MCP server exposes \
                 (URI templates like scheme://path/{{param}}); fill in the params \
                 and read the concrete URI with {server}__read_resource."
            ),
            schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            op: ResourceOp::ListTemplates,
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
                Ok(ToolResult {
                    output: render_resource_contents(&result),
                    is_error: false,
                })
            }
            ResourceOp::ListTemplates => {
                let templates = self
                    .client
                    .list_all_resource_templates()
                    .await
                    .map_err(|err| ToolError::ExecutionFailed(err.to_string()))?;
                let mut output = String::new();
                for template in &templates {
                    let raw = &template.raw;
                    output.push_str(&raw.uri_template);
                    output.push_str(" — ");
                    output.push_str(&raw.name);
                    if let Some(description) = raw.description.as_deref() {
                        output.push_str(": ");
                        output.push_str(description);
                    }
                    output.push('\n');
                }
                if output.is_empty() {
                    output.push_str("(no resource templates)");
                }
                Ok(ToolResult {
                    output,
                    is_error: false,
                })
            }
        }
    }
}

/// Render a [`ReadResourceResult`]'s contents into a plain-text
/// block. Text contents are concatenated verbatim; binary (blob)
/// contents are summarised with their size and mime type, since
/// they are not meaningful as model-visible text. Shared by the
/// model-controlled read tool ([`McpResourceTool`]) and the
/// runner's `static_resources` injection so both render identically.
pub fn render_resource_contents(result: &ReadResourceResult) -> String {
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
    output
}
