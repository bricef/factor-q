//! Read-scoped workspace discovery tools.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use glob::glob;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tool::{Tool, ToolContext, ToolError, ToolResult};

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 100;

#[derive(Debug, Deserialize)]
struct ListParams {
    root: String,
    #[serde(default = "default_glob")]
    glob: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

#[derive(Debug, Deserialize)]
struct SearchParams {
    root: String,
    query: String,
    #[serde(default)]
    regex: bool,
    #[serde(default = "default_glob")]
    glob: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_glob() -> String {
    "**/*".to_string()
}
fn default_limit() -> usize {
    DEFAULT_LIMIT
}

/// Lists files matching a glob below a readable directory.
#[derive(Debug, Default)]
pub struct FileListTool;

/// Searches text files below a readable directory and returns line-level hits.
#[derive(Debug, Default)]
pub struct FileSearchTool;

fn validate_root(ctx: &ToolContext<'_>, root: &str) -> Result<PathBuf, ToolError> {
    let root = ctx.sandbox.check_read(Path::new(root))?;
    if !root.is_dir() {
        return Err(ToolError::InvalidParameters(
            "root must be a directory".to_string(),
        ));
    }
    Ok(root)
}

fn files(root: &Path, pattern: &str) -> Result<Vec<PathBuf>, ToolError> {
    if Path::new(pattern).is_absolute() {
        return Err(ToolError::InvalidParameters(
            "glob must be relative to root".to_string(),
        ));
    }
    let pattern = root.join(pattern).to_string_lossy().into_owned();
    glob(&pattern)
        .map_err(|err| ToolError::InvalidParameters(err.to_string()))?
        .filter_map(Result::ok)
        .collect::<Vec<_>>()
        .pipe(Ok)
}

// Keeps collection setup readable without exposing a public helper.
trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

#[async_trait]
impl Tool for FileListTool {
    fn name(&self) -> &str {
        "file_list"
    }
    fn description(&self) -> &str {
        "List files matching a glob below a readable directory. Results are capped."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "root":{"type":"string","format":"path","description":"Readable directory to search."},
            "glob":{"type":"string","description":"Relative glob pattern (default **/*)."},
            "limit":{"type":"integer","minimum":1,"maximum":100,"description":"Maximum results (default 100)."}
        },"required":["root"],"additionalProperties":false})
    }
    async fn execute(&self, ctx: &ToolContext<'_>, params: Value) -> Result<ToolResult, ToolError> {
        let params: ListParams = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidParameters(e.to_string()))?;
        let root = validate_root(ctx, &params.root)?;
        let limit = params.limit.min(MAX_LIMIT);
        let mut paths = Vec::new();
        for path in files(&root, &params.glob)? {
            // Re-check every hit so symlinks and glob traversal cannot escape the grant.
            if let Ok(path) = ctx.sandbox.check_read(&path) {
                paths.push(path.display().to_string());
                if paths.len() == limit {
                    break;
                }
            }
        }
        Ok(ToolResult::ok(
            json!({"paths": paths, "truncated": paths.len() == limit}).to_string(),
        ))
    }
}

#[async_trait]
impl Tool for FileSearchTool {
    fn name(&self) -> &str {
        "file_search"
    }
    fn description(&self) -> &str {
        "Search text files below a readable directory. Returns capped structured path, line, and excerpt hits."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "root":{"type":"string","format":"path","description":"Readable directory to search."},
            "query":{"type":"string","description":"Literal query, or regular expression when regex is true."},
            "regex":{"type":"boolean","description":"Interpret query as a regular expression (default false)."},
            "glob":{"type":"string","description":"Relative file glob (default **/*)."},
            "limit":{"type":"integer","minimum":1,"maximum":100,"description":"Maximum hits (default 100)."}
        },"required":["root","query"],"additionalProperties":false})
    }
    async fn execute(&self, ctx: &ToolContext<'_>, params: Value) -> Result<ToolResult, ToolError> {
        let params: SearchParams = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidParameters(e.to_string()))?;
        let root = validate_root(ctx, &params.root)?;
        let query = if params.regex {
            params.query
        } else {
            regex::escape(&params.query)
        };
        let matcher =
            Regex::new(&query).map_err(|e| ToolError::InvalidParameters(e.to_string()))?;
        let limit = params.limit.min(MAX_LIMIT);
        let mut hits = Vec::new();
        'files: for path in files(&root, &params.glob)? {
            let Ok(path) = ctx.sandbox.check_read(&path) else {
                continue;
            };
            let Ok(contents) = tokio::fs::read_to_string(&path).await else {
                continue;
            };
            for (index, line) in contents.lines().enumerate() {
                if matcher.is_match(line) {
                    hits.push(json!({"path": path.display().to_string(), "line": index + 1, "excerpt": line}));
                    if hits.len() == limit {
                        break 'files;
                    }
                }
            }
        }
        Ok(ToolResult::ok(
            json!({"hits": hits, "truncated": hits.len() == limit}).to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::ToolSandbox;
    use std::fs;
    use tempfile::tempdir;

    #[tokio::test]
    async fn list_is_capped_and_cannot_escape_root() {
        let allowed = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::write(allowed.path().join("one.txt"), "one").unwrap();
        fs::write(allowed.path().join("two.txt"), "two").unwrap();
        fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        let sandbox = ToolSandbox::new().allow_read(allowed.path());
        let ctx = ToolContext::new(&sandbox);
        let tool = FileListTool;
        let output = tool
            .execute(
                &ctx,
                json!({"root": allowed.path(), "glob": "*.txt", "limit": 1}),
            )
            .await
            .unwrap();
        assert!(output.output.contains("\"truncated\":true"));
        let err = tool
            .execute(&ctx, json!({"root": outside.path()}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn search_returns_structured_literal_hits_and_is_capped() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("notes.txt"), "needle\nneedle\n").unwrap();
        let sandbox = ToolSandbox::new().allow_read(dir.path());
        let ctx = ToolContext::new(&sandbox);
        let output = FileSearchTool
            .execute(
                &ctx,
                json!({"root": dir.path(), "query": "needle", "limit": 1}),
            )
            .await
            .unwrap();
        assert!(output.output.contains("\"line\":1"));
        assert!(output.output.contains("\"truncated\":true"));
    }
}
