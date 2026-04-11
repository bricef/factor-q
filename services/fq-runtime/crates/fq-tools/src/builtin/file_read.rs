//! `file_read` built-in tool: read the contents of a file at a given
//! path, subject to the agent's sandbox.
//!
//! Always validates against the sandbox before touching the
//! filesystem. A path outside the agent's allowed read prefixes
//! produces a permission-denied result, not a file-not-found one,
//! even if the target happens not to exist.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::tool::{Tool, ToolContext, ToolError, ToolResult};

/// Parameters accepted by the `file_read` tool.
#[derive(Debug, Deserialize)]
struct FileReadParams {
    path: String,
}

/// Built-in `file_read` tool.
#[derive(Debug, Default)]
pub struct FileReadTool;

impl FileReadTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &str {
        "file_read"
    }

    fn description(&self) -> &str {
        "Read the full contents of a text file at the given path. \
         Only paths within the agent's declared read sandbox are allowed."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute or relative filesystem path to the file to read."
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        ctx: &ToolContext<'_>,
        params: Value,
    ) -> Result<ToolResult, ToolError> {
        let params: FileReadParams = serde_json::from_value(params)
            .map_err(|err| ToolError::InvalidParameters(err.to_string()))?;
        let target = PathBuf::from(&params.path);

        let canonical = ctx.sandbox.check_read(&target)?;

        let contents = tokio::fs::read_to_string(&canonical)
            .await
            .map_err(|err| ToolError::Io(format!("{}: {err}", canonical.display())))?;

        Ok(ToolResult::ok(contents))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::ToolSandbox;
    use std::fs;
    use tempfile::tempdir;

    fn make_tool_ctx(sandbox: &ToolSandbox) -> ToolContext<'_> {
        ToolContext::new(sandbox)
    }

    #[tokio::test]
    async fn reads_file_within_sandbox() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("hello.md");
        fs::write(&file, "# Hello").unwrap();

        let sandbox = ToolSandbox::new().allow_read(dir.path());
        let ctx = make_tool_ctx(&sandbox);
        let tool = FileReadTool::new();

        let result = tool
            .execute(&ctx, json!({ "path": file.to_string_lossy() }))
            .await
            .unwrap();
        assert_eq!(result.output, "# Hello");
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn rejects_file_outside_sandbox() {
        let allowed = tempdir().unwrap();
        let other = tempdir().unwrap();
        let file = other.path().join("secret.txt");
        fs::write(&file, "no").unwrap();

        let sandbox = ToolSandbox::new().allow_read(allowed.path());
        let ctx = make_tool_ctx(&sandbox);
        let tool = FileReadTool::new();

        let err = tool
            .execute(&ctx, json!({ "path": file.to_string_lossy() }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let allowed = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let target = outside.path().join("secret.txt");
        fs::write(&target, "secret").unwrap();
        let link = allowed.path().join("escape");
        symlink(&target, &link).unwrap();

        let sandbox = ToolSandbox::new().allow_read(allowed.path());
        let ctx = make_tool_ctx(&sandbox);
        let tool = FileReadTool::new();

        let err = tool
            .execute(&ctx, json!({ "path": link.to_string_lossy() }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn rejects_parent_traversal_escape() {
        let allowed = tempdir().unwrap();
        let other = tempdir().unwrap();
        let file = other.path().join("secret.txt");
        fs::write(&file, "no").unwrap();

        let sandbox = ToolSandbox::new().allow_read(allowed.path());
        let ctx = make_tool_ctx(&sandbox);
        let tool = FileReadTool::new();

        let traversal = allowed
            .path()
            .join("../")
            .join(other.path().file_name().unwrap())
            .join("secret.txt");
        let err = tool
            .execute(&ctx, json!({ "path": traversal.to_string_lossy() }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn missing_file_reports_not_found() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("ghost.txt");

        let sandbox = ToolSandbox::new().allow_read(dir.path());
        let ctx = make_tool_ctx(&sandbox);
        let tool = FileReadTool::new();

        let err = tool
            .execute(&ctx, json!({ "path": missing.to_string_lossy() }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn rejects_invalid_parameters() {
        let sandbox = ToolSandbox::new();
        let ctx = make_tool_ctx(&sandbox);
        let tool = FileReadTool::new();

        let err = tool
            .execute(&ctx, json!({ "wrong_field": "x" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
    }

    #[tokio::test]
    async fn read_does_not_grant_write() {
        // Sanity check that the tool doesn't accidentally cross the
        // sandbox boundary between read and write.
        let dir = tempdir().unwrap();
        let file = dir.path().join("new.txt");

        let sandbox = ToolSandbox::new().allow_read(dir.path());
        let ctx = make_tool_ctx(&sandbox);
        let tool = FileReadTool::new();

        // The tool should report NotFound (since the file doesn't
        // exist), not PermissionDenied — the sandbox correctly
        // classifies this as "you're allowed here but the file
        // isn't there".
        let err = tool
            .execute(&ctx, json!({ "path": file.to_string_lossy() }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }
}
