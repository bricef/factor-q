//! `file_write` built-in tool: write text content to a file at a
//! given path, subject to the agent's sandbox.
//!
//! Creates the file if it does not exist, truncates if it does. The
//! parent directory must already exist — the tool does not create
//! intermediate directories, because creating directories implicitly
//! would let an agent side-step any future directory-level
//! restrictions. Creating directories is a separate concern that
//! will get its own tool if and when it's needed.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::tool::{Tool, ToolContext, ToolError, ToolResult};

#[derive(Debug, Deserialize)]
struct FileWriteParams {
    path: String,
    content: String,
}

#[derive(Debug, Default)]
pub struct FileWriteTool;

impl FileWriteTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for FileWriteTool {
    fn name(&self) -> &str {
        "file_write"
    }

    fn description(&self) -> &str {
        "Write text content to a file at the given path, creating or \
         truncating it. The parent directory must already exist, and \
         the target path must be within the agent's declared write \
         sandbox."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Filesystem path to write to."
                },
                "content": {
                    "type": "string",
                    "description": "Text content to write to the file."
                }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        ctx: &ToolContext<'_>,
        params: Value,
    ) -> Result<ToolResult, ToolError> {
        let params: FileWriteParams = serde_json::from_value(params)
            .map_err(|err| ToolError::InvalidParameters(err.to_string()))?;
        let target = PathBuf::from(&params.path);

        let canonical = ctx.sandbox.check_write(&target)?;

        tokio::fs::write(&canonical, &params.content)
            .await
            .map_err(|err| ToolError::Io(format!("{}: {err}", canonical.display())))?;

        Ok(ToolResult::ok(format!(
            "Wrote {} bytes to {}",
            params.content.len(),
            canonical.display()
        )))
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
    async fn writes_new_file_within_sandbox() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("new.txt");

        let sandbox = ToolSandbox::new().allow_write(dir.path());
        let ctx = make_tool_ctx(&sandbox);
        let tool = FileWriteTool::new();

        let result = tool
            .execute(
                &ctx,
                json!({
                    "path": target.to_string_lossy(),
                    "content": "hello"
                }),
            )
            .await
            .unwrap();
        assert!(!result.is_error);

        let written = fs::read_to_string(&target).unwrap();
        assert_eq!(written, "hello");
    }

    #[tokio::test]
    async fn overwrites_existing_file_within_sandbox() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("existing.txt");
        fs::write(&target, "old").unwrap();

        let sandbox = ToolSandbox::new().allow_write(dir.path());
        let ctx = make_tool_ctx(&sandbox);
        let tool = FileWriteTool::new();

        tool.execute(
            &ctx,
            json!({
                "path": target.to_string_lossy(),
                "content": "new"
            }),
        )
        .await
        .unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
    }

    #[tokio::test]
    async fn rejects_new_file_outside_sandbox() {
        let allowed = tempdir().unwrap();
        let other = tempdir().unwrap();
        let target = other.path().join("evil.txt");

        let sandbox = ToolSandbox::new().allow_write(allowed.path());
        let ctx = make_tool_ctx(&sandbox);
        let tool = FileWriteTool::new();

        let err = tool
            .execute(
                &ctx,
                json!({
                    "path": target.to_string_lossy(),
                    "content": "payload"
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied(_)));
        assert!(!target.exists(), "file should not have been written");
    }

    #[tokio::test]
    async fn rejects_existing_file_outside_sandbox() {
        let allowed = tempdir().unwrap();
        let other = tempdir().unwrap();
        let target = other.path().join("existing.txt");
        fs::write(&target, "old").unwrap();

        let sandbox = ToolSandbox::new().allow_write(allowed.path());
        let ctx = make_tool_ctx(&sandbox);
        let tool = FileWriteTool::new();

        let err = tool
            .execute(
                &ctx,
                json!({
                    "path": target.to_string_lossy(),
                    "content": "hacked"
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied(_)));
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "old",
            "file should be unchanged"
        );
    }

    #[tokio::test]
    async fn rejects_traversal_escape() {
        let allowed = tempdir().unwrap();
        let other = tempdir().unwrap();
        let escape = allowed
            .path()
            .join("../")
            .join(other.path().file_name().unwrap())
            .join("new.txt");

        let sandbox = ToolSandbox::new().allow_write(allowed.path());
        let ctx = make_tool_ctx(&sandbox);
        let tool = FileWriteTool::new();

        let err = tool
            .execute(
                &ctx,
                json!({
                    "path": escape.to_string_lossy(),
                    "content": "hacked"
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let allowed = tempdir().unwrap();
        let other = tempdir().unwrap();
        let target = other.path().join("existing.txt");
        fs::write(&target, "old").unwrap();
        let link = allowed.path().join("escape");
        symlink(&target, &link).unwrap();

        let sandbox = ToolSandbox::new().allow_write(allowed.path());
        let ctx = make_tool_ctx(&sandbox);
        let tool = FileWriteTool::new();

        let err = tool
            .execute(
                &ctx,
                json!({
                    "path": link.to_string_lossy(),
                    "content": "hacked"
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied(_)));
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "old",
            "target file should be unchanged"
        );
    }

    #[tokio::test]
    async fn reports_missing_parent_as_not_found() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("nonexistent/sub/new.txt");

        let sandbox = ToolSandbox::new().allow_write(dir.path());
        let ctx = make_tool_ctx(&sandbox);
        let tool = FileWriteTool::new();

        let err = tool
            .execute(
                &ctx,
                json!({
                    "path": target.to_string_lossy(),
                    "content": "x"
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn rejects_invalid_parameters() {
        let sandbox = ToolSandbox::new();
        let ctx = make_tool_ctx(&sandbox);
        let tool = FileWriteTool::new();

        // Missing "content" field
        let err = tool
            .execute(&ctx, json!({ "path": "/tmp/x" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));

        // Wrong type
        let err = tool
            .execute(&ctx, json!({ "path": "/tmp/x", "content": 42 }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
    }

    #[tokio::test]
    async fn write_does_not_grant_read() {
        // A write-only sandbox must not be usable by a read tool path.
        // Not a file_write test per se, but included as a
        // cross-tool smoke check that the sandbox methods are
        // distinct.
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_write(dir.path());
        let err = sandbox.check_read(&dir.path().join("anything")).unwrap_err();
        assert!(matches!(
            err,
            crate::sandbox::SandboxError::PermissionDenied { .. }
        ));
    }
}
