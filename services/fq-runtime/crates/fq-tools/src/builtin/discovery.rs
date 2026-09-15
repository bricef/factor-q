//! Read-scoped workspace discovery tools.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use glob::Pattern;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use walkdir::WalkDir;

use crate::tool::{Tool, ToolContext, ToolError, ToolResult};

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 100;
const WALK_BUFFER: usize = 64;

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

impl FileListTool {
    pub fn new() -> Self {
        Self
    }
}

/// Searches text files below a readable directory and returns line-level hits.
#[derive(Debug, Default)]
pub struct FileSearchTool;

impl FileSearchTool {
    pub fn new() -> Self {
        Self
    }
}

/// The `root` a discovery tool walks: inside the read grant, and a
/// directory. Both halves come back from the sandbox, so a root that
/// is a file — or a FIFO — is refused with a message naming what it
/// actually is (#547).
fn validate_root(ctx: &ToolContext<'_>, root: &str) -> Result<PathBuf, ToolError> {
    Ok(ctx.sandbox.check_read_dir(Path::new(root))?)
}

enum WalkItem {
    Path(PathBuf),
    Unreadable,
}

fn walk_files(
    ctx: &ToolContext<'_>,
    root: PathBuf,
    pattern: &str,
) -> Result<(mpsc::Receiver<WalkItem>, tokio::task::JoinHandle<()>), ToolError> {
    if Path::new(pattern).is_absolute() {
        return Err(ToolError::InvalidParameters(
            "glob must be relative to root".to_string(),
        ));
    }
    let pattern = Pattern::new(&root.join(pattern).to_string_lossy())
        .map_err(|err| ToolError::InvalidParameters(err.to_string()))?;
    let sandbox = ctx.sandbox.clone();
    let (sender, receiver) = mpsc::channel(WALK_BUFFER);
    let worker = tokio::task::spawn_blocking(move || {
        for entry in WalkDir::new(root).follow_links(false).sort_by_file_name() {
            let item = match entry {
                Ok(entry) if pattern.matches_path(entry.path()) => {
                    let Ok(path) = sandbox.check_read(entry.path()) else {
                        continue;
                    };
                    WalkItem::Path(path)
                }
                Ok(_) => continue,
                Err(_) => WalkItem::Unreadable,
            };
            if sender.blocking_send(item).is_err() {
                break;
            }
        }
    });
    Ok((receiver, worker))
}

async fn finish_walk(worker: tokio::task::JoinHandle<()>) -> Result<(), ToolError> {
    worker
        .await
        .map_err(|err| ToolError::ExecutionFailed(format!("discovery walker failed: {err}")))
}

#[async_trait]
impl Tool for FileListTool {
    fn name(&self) -> &str {
        "file_list"
    }
    fn description(&self) -> &str {
        "List files matching a glob below a readable directory. Only regular files \
         are listed — directories a glob matches are not returned. Results are capped."
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
        let (mut receiver, worker) = walk_files(ctx, root, &params.glob)?;
        let mut paths = Vec::with_capacity(limit);
        let mut unreadable = 0;
        let mut truncated = false;
        while let Some(item) = receiver.recv().await {
            match item {
                WalkItem::Path(_) if paths.len() == limit => {
                    truncated = true;
                    break;
                }
                WalkItem::Path(path) => paths.push(path.display().to_string()),
                WalkItem::Unreadable => unreadable += 1,
            }
        }
        drop(receiver);
        finish_walk(worker).await?;
        Ok(ToolResult::ok(
            json!({"paths": paths, "truncated": truncated, "unreadable": unreadable}).to_string(),
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
        let (mut receiver, worker) = walk_files(ctx, root, &params.glob)?;
        let mut hits = Vec::with_capacity(limit);
        let mut unreadable = 0;
        let mut truncated = false;
        'files: while let Some(item) = receiver.recv().await {
            let path = match item {
                WalkItem::Path(path) => path,
                WalkItem::Unreadable => {
                    unreadable += 1;
                    continue;
                }
            };
            let Ok(contents) = tokio::fs::read_to_string(&path).await else {
                continue;
            };
            for (index, line) in contents.lines().enumerate() {
                if matcher.is_match(line) {
                    if hits.len() == limit {
                        truncated = true;
                        break 'files;
                    }
                    hits.push(json!({"path": path.display().to_string(), "line": index + 1, "excerpt": line}));
                }
            }
        }
        drop(receiver);
        finish_walk(worker).await?;
        Ok(ToolResult::ok(
            json!({"hits": hits, "truncated": truncated, "unreadable": unreadable}).to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::ToolSandbox;
    use std::fs;
    use std::time::Duration;
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

    async fn list(root: &Path, limit: usize) -> Value {
        let sandbox = ToolSandbox::new().allow_read(root);
        let ctx = ToolContext::new(&sandbox);
        let output = FileListTool
            .execute(&ctx, json!({"root": root, "limit": limit}))
            .await
            .unwrap();
        serde_json::from_str(&output.output).unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_loops_terminate_without_descending() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        symlink(".", dir.path().join("a")).unwrap();
        symlink(".", dir.path().join("b")).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), list(dir.path(), 100))
            .await
            .expect("symlink loop must not extend the walk");
        let paths = result["paths"].as_array().unwrap();
        assert!(
            paths
                .iter()
                .all(|path| !path.as_str().unwrap().contains("/a/"))
        );
        assert!(
            paths
                .iter()
                .all(|path| !path.as_str().unwrap().contains("/b/"))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_directory_is_not_descended() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        symlink(outside.path(), root.path().join("link")).unwrap();
        let result = list(root.path(), 100).await;
        assert!(result["paths"].as_array().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_file_is_still_listed() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let target = root.path().join("target.txt");
        fs::write(&target, "target").unwrap();
        symlink("target.txt", root.path().join("alias")).unwrap();
        let result = list(root.path(), 100).await;
        let paths = result["paths"].as_array().unwrap();
        assert_eq!(
            paths
                .iter()
                .filter(|path| path.as_str() == Some(target.to_str().unwrap()))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn cap_stops_an_adversarial_walk_promptly() {
        let root = tempdir().unwrap();
        for index in 0..5_000 {
            fs::write(root.path().join(format!("{index:04}.txt")), "x").unwrap();
        }
        let result = tokio::time::timeout(Duration::from_secs(2), list(root.path(), 1))
            .await
            .expect("limit must stop the producer during the walk");
        assert_eq!(result["paths"].as_array().unwrap().len(), 1);
        assert_eq!(result["truncated"], true);
    }

    #[tokio::test]
    async fn truncation_reports_only_an_observed_extra_match() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("one.txt"), "one").unwrap();
        assert_eq!(list(root.path(), 1).await["truncated"], false);
        fs::write(root.path().join("two.txt"), "two").unwrap();
        assert_eq!(list(root.path(), 1).await["truncated"], true);
    }

    #[tokio::test]
    async fn sandbox_denials_are_not_unreadable_walk_entries() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("directory")).unwrap();
        fs::write(root.path().join("directory/file.txt"), "x").unwrap();
        assert_eq!(list(root.path(), 100).await["unreadable"], 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_directory_is_reported_when_permissions_apply() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        let blocked = root.path().join("blocked");
        fs::create_dir(&blocked).unwrap();
        fs::write(blocked.join("file.txt"), "x").unwrap();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&blocked).is_ok() {
            fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }
        let result = list(root.path(), 100).await;
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result["unreadable"].as_u64().unwrap() > 0);
    }
}
