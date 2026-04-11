//! `shell` built-in tool: run a command as a child process with the
//! agent's sandbox as the enforcement boundary.
//!
//! This tool takes an **argv array**, not a shell string. No shell is
//! invoked anywhere in this module — we use
//! [`tokio::process::Command`] directly — so there is no opportunity
//! for shell injection. The LLM calls:
//!
//! ```json
//! { "command": ["grep", "-r", "pattern", "./src"], "cwd": "/data/project" }
//! ```
//!
//! not a string. If an agent needs pipelines or redirects, that's a
//! distinct tool we have not built yet.
//!
//! ## Safeguards
//!
//! - **`cwd` must be within the agent's `exec_cwd` sandbox**. An
//!   agent's read/write access to a directory does NOT imply exec
//!   permission. `exec_cwd` is a separate sandbox dimension that must
//!   be granted explicitly.
//! - **Timeout** — every call has a wall-clock timeout. The agent may
//!   request a shorter timeout via `timeout_secs`; anything longer is
//!   clamped to the runtime-configured maximum. On timeout the child
//!   is killed and the tool returns with `is_error: true` plus
//!   whatever output was captured up to that point.
//! - **Output cap** — stdout and stderr are each truncated to a
//!   configurable byte limit. Beyond the cap the captured streams
//!   carry a marker showing how much was dropped.
//! - **Environment is a fresh map** — the child does NOT inherit the
//!   parent's environment. A small safe baseline is set (most
//!   importantly a pinned `PATH`), then the agent's declared env
//!   allowlist is copied from the parent on top. An agent that
//!   doesn't list `HOME` in its sandbox will have no `HOME` set in
//!   the child.
//! - **Non-zero exit codes are errors** — the tool reports
//!   `is_error: true` when the exit code is non-zero, but still
//!   returns stdout/stderr so the LLM can understand what happened.
//!
//! ## Known gaps
//!
//! These are real limitations of process-level sandboxing that can
//! only be closed with OS-level isolation (see ADR-0010):
//!
//! - **No PATH restriction on binaries**. Any executable reachable by
//!   the default PATH can be called — `curl`, `wget`, system tools.
//! - **No network isolation**. Commands can open network connections.
//! - **No cgroup / rlimit enforcement**. The child can consume CPU,
//!   memory, and open files up to the process-level limits.
//! - **No syscall filtering (seccomp)**. Anything the binary can do,
//!   it can do.
//!
//! Container-level isolation (ADR-0010) is the path to closing these.
//! Until then, the shell tool should be granted only to agents you
//! trust with these capabilities, and tests for the tool itself
//! should be run in a disposable container
//! (see `services/fq-runtime/Dockerfile.shell-test` and
//! `just test-shell-sandbox`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::tool::{Tool, ToolContext, ToolError, ToolResult};

/// Runtime-configurable parameters for the shell tool.
#[derive(Debug, Clone)]
pub struct ShellConfig {
    /// Timeout applied when the caller does not specify one.
    pub default_timeout: Duration,
    /// Hard upper bound on any single call's timeout. Caller-supplied
    /// timeouts above this are clamped down, not rejected, to avoid
    /// trapping an agent in a retry loop over a misconfigured value.
    pub max_timeout: Duration,
    /// Maximum bytes captured from stdout OR stderr. Each stream is
    /// bounded independently.
    pub max_output_bytes: usize,
    /// Baseline `PATH` passed to every child process. Set to a small
    /// fixed value so children do not inherit whatever weird PATH the
    /// parent had.
    pub default_path: String,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(30),
            max_timeout: Duration::from_secs(300),
            max_output_bytes: 100 * 1024,
            default_path: "/usr/local/bin:/usr/bin:/bin".to_string(),
        }
    }
}

/// Built-in `shell` tool.
#[derive(Debug, Clone)]
pub struct ShellTool {
    config: ShellConfig,
}

impl ShellTool {
    /// Construct a shell tool with default configuration.
    pub fn new() -> Self {
        Self {
            config: ShellConfig::default(),
        }
    }

    /// Construct a shell tool with an explicit configuration.
    pub fn with_config(config: ShellConfig) -> Self {
        Self { config }
    }
}

impl Default for ShellTool {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize)]
struct ShellParams {
    command: Vec<String>,
    cwd: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Run a command as a child process. Takes an argv array (NOT a \
         shell string) plus a working directory that must be within \
         the agent's exec_cwd sandbox. Every call has a timeout and \
         output size cap. Non-zero exit codes are returned as errors \
         but still include stdout/stderr."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "description": "Argv array. First element is the executable; no shell interpretation is performed."
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory for the child process. Must be within the agent's exec_cwd sandbox."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Optional timeout in seconds. Clamped to the runtime's configured maximum."
                }
            },
            "required": ["command", "cwd"],
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        ctx: &ToolContext<'_>,
        params: Value,
    ) -> Result<ToolResult, ToolError> {
        let params: ShellParams = serde_json::from_value(params)
            .map_err(|err| ToolError::InvalidParameters(err.to_string()))?;

        if params.command.is_empty() {
            return Err(ToolError::InvalidParameters(
                "command must not be empty".to_string(),
            ));
        }
        let program = &params.command[0];
        if program.is_empty() {
            return Err(ToolError::InvalidParameters(
                "command[0] (the program) must not be empty".to_string(),
            ));
        }

        // Enforce cwd sandbox.
        let cwd_path = PathBuf::from(&params.cwd);
        let canonical_cwd = ctx.sandbox.check_exec_cwd(&cwd_path)?;

        // Clamp timeout.
        let timeout_duration = match params.timeout_secs {
            Some(secs) if secs == 0 => {
                return Err(ToolError::InvalidParameters(
                    "timeout_secs must be > 0".to_string(),
                ));
            }
            Some(secs) => {
                let requested = Duration::from_secs(secs);
                if requested > self.config.max_timeout {
                    self.config.max_timeout
                } else {
                    requested
                }
            }
            None => self.config.default_timeout,
        };

        // Build the child's environment. Start from a small fixed
        // baseline (just PATH), then copy each variable the agent
        // explicitly allowlisted from the parent's env.
        let env = build_child_env(
            &self.config.default_path,
            // We treat the allowlist as "every env var the agent
            // declared in `sandbox.env`". The executor converts
            // Agent::Sandbox into ToolSandbox; the env allowlist
            // isn't currently carried on ToolSandbox, so for phase 1
            // we expose it through a dedicated helper below.
            std::env::vars(),
            allowed_env_vars(),
        );

        debug!(
            program = %program,
            cwd = %canonical_cwd.display(),
            timeout_ms = timeout_duration.as_millis() as u64,
            "spawning shell command"
        );

        let mut cmd = Command::new(program);
        cmd.args(&params.command[1..])
            .current_dir(&canonical_cwd)
            .env_clear()
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|err| classify_spawn_error(program, err))?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let max_output_bytes = self.config.max_output_bytes;

        let stdout_task = tokio::spawn(async move {
            match stdout {
                Some(stream) => read_capped(stream, max_output_bytes).await,
                None => (Vec::new(), false),
            }
        });
        let stderr_task = tokio::spawn(async move {
            match stderr {
                Some(stream) => read_capped(stream, max_output_bytes).await,
                None => (Vec::new(), false),
            }
        });

        let wait_result = timeout(timeout_duration, child.wait()).await;

        let (exit_status, timed_out) = match wait_result {
            Ok(Ok(status)) => (Some(status), false),
            Ok(Err(err)) => {
                return Err(ToolError::ExecutionFailed(format!(
                    "failed to wait for child: {err}"
                )));
            }
            Err(_) => {
                // Killing on drop is set, but be explicit so the
                // captured output tasks finish promptly.
                warn!(timeout_ms = timeout_duration.as_millis() as u64, "shell timeout fired — killing child");
                if let Err(err) = child.start_kill() {
                    warn!(error = %err, "failed to start_kill after timeout");
                }
                let _ = child.wait().await;
                (None, true)
            }
        };

        let (stdout_bytes, stdout_truncated) = stdout_task
            .await
            .map_err(|err| ToolError::ExecutionFailed(format!("stdout task panicked: {err}")))?;
        let (stderr_bytes, stderr_truncated) = stderr_task
            .await
            .map_err(|err| ToolError::ExecutionFailed(format!("stderr task panicked: {err}")))?;

        let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
        let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();

        if timed_out {
            let body = format_output(&stdout, stdout_truncated, &stderr, stderr_truncated);
            return Ok(ToolResult {
                output: format!(
                    "Command timed out after {}s.\n\n{body}",
                    timeout_duration.as_secs()
                ),
                is_error: true,
            });
        }

        let status = exit_status.expect("handled timeout above");
        let exit_code = status.code();
        let is_error = !status.success();

        let header = match exit_code {
            Some(code) => format!("Exit code: {code}"),
            None => "Command was terminated by a signal.".to_string(),
        };
        let body = format_output(&stdout, stdout_truncated, &stderr, stderr_truncated);

        Ok(ToolResult {
            output: format!("{header}\n\n{body}"),
            is_error,
        })
    }
}

fn classify_spawn_error(program: &str, err: std::io::Error) -> ToolError {
    match err.kind() {
        std::io::ErrorKind::NotFound => {
            ToolError::ExecutionFailed(format!("program not found: {program}"))
        }
        std::io::ErrorKind::PermissionDenied => {
            ToolError::PermissionDenied(format!("permission denied executing {program}: {err}"))
        }
        _ => ToolError::Io(format!("failed to spawn {program}: {err}")),
    }
}

/// Read at most `max_bytes` from a stream. Returns the captured
/// bytes plus a flag indicating whether the stream was truncated.
async fn read_capped<R>(stream: R, max_bytes: usize) -> (Vec<u8>, bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::with_capacity(max_bytes.min(8 * 1024));
    let mut scratch = [0u8; 8 * 1024];
    let mut truncated = false;
    loop {
        match reader.read(&mut scratch).await {
            Ok(0) => break,
            Ok(n) => {
                let remaining = max_bytes.saturating_sub(buf.len());
                if remaining == 0 {
                    // Drain the rest of the stream so the child
                    // doesn't deadlock on a full pipe, then return
                    // the capped buffer with the truncated flag set.
                    loop {
                        match reader.read(&mut scratch).await {
                            Ok(0) | Err(_) => return (buf, true),
                            Ok(_) => continue,
                        }
                    }
                }
                let take = remaining.min(n);
                buf.extend_from_slice(&scratch[..take]);
                if take < n {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (buf, truncated)
}

fn format_output(
    stdout: &str,
    stdout_truncated: bool,
    stderr: &str,
    stderr_truncated: bool,
) -> String {
    let mut out = String::new();
    out.push_str("--- stdout ---\n");
    if stdout.is_empty() {
        out.push_str("(empty)\n");
    } else {
        out.push_str(stdout);
        if !stdout.ends_with('\n') {
            out.push('\n');
        }
    }
    if stdout_truncated {
        out.push_str("(stdout truncated)\n");
    }
    out.push_str("\n--- stderr ---\n");
    if stderr.is_empty() {
        out.push_str("(empty)\n");
    } else {
        out.push_str(stderr);
        if !stderr.ends_with('\n') {
            out.push('\n');
        }
    }
    if stderr_truncated {
        out.push_str("(stderr truncated)\n");
    }
    out
}

/// Env vars the agent has allowlisted. This is a module-local
/// constant for phase 1 — in a later slice this will be plumbed
/// through `ToolSandbox` properly and read from the agent definition.
/// For now, only PATH is injected from the default, and nothing else
/// is passed through unless the test infrastructure overrides this.
fn allowed_env_vars() -> &'static [&'static str] {
    // Phase 1 default: no additional env vars pass through. Tests
    // that need specific variables set them via std::env::set_var
    // before running the tool and include them in the allowlist via
    // the ShellConfig-driven path below (not yet implemented).
    //
    // This deliberately limits what the shell tool exposes until we
    // plumb an explicit allowlist through ToolSandbox.
    &[]
}

fn build_child_env<I>(
    default_path: &str,
    parent_env: I,
    allowlist: &[&'static str],
) -> HashMap<String, String>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut env = HashMap::new();
    env.insert("PATH".to_string(), default_path.to_string());

    if allowlist.is_empty() {
        return env;
    }
    let parent: HashMap<String, String> = parent_env.into_iter().collect();
    for name in allowlist {
        if let Some(value) = parent.get(*name) {
            env.insert((*name).to_string(), value.clone());
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::ToolSandbox;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    fn make_exec_ctx<'a>(sandbox: &'a ToolSandbox) -> ToolContext<'a> {
        ToolContext::new(sandbox)
    }

    fn make_tool_fast() -> ShellTool {
        ShellTool::with_config(ShellConfig {
            default_timeout: Duration::from_secs(5),
            max_timeout: Duration::from_secs(10),
            max_output_bytes: 4 * 1024, // Small to make truncation tests fast
            default_path: "/usr/local/bin:/usr/bin:/bin".to_string(),
        })
    }

    #[tokio::test]
    async fn runs_echo_and_captures_stdout() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        let tool = make_tool_fast();

        let result = tool
            .execute(
                &ctx,
                json!({
                    "command": ["echo", "hello world"],
                    "cwd": dir.path().to_string_lossy(),
                }),
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.output.contains("hello world"));
        assert!(result.output.contains("Exit code: 0"));
    }

    #[tokio::test]
    async fn non_zero_exit_is_error_but_output_is_returned() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        let tool = make_tool_fast();

        let result = tool
            .execute(
                &ctx,
                json!({
                    "command": ["false"],
                    "cwd": dir.path().to_string_lossy(),
                }),
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.output.contains("Exit code: 1"));
    }

    #[tokio::test]
    async fn cwd_outside_sandbox_is_denied_and_does_not_execute() {
        let allowed = tempdir().unwrap();
        let other = tempdir().unwrap();
        // Create a "marker" file in allowed/ so we can prove `echo`
        // was never invoked: if the tool ran the command, the test
        // would print something, but we're not asserting on that.
        // The security claim is that check_exec_cwd rejects before
        // spawning.
        let sandbox = ToolSandbox::new().allow_exec_cwd(allowed.path());
        let ctx = make_exec_ctx(&sandbox);
        let tool = make_tool_fast();

        let err = tool
            .execute(
                &ctx,
                json!({
                    "command": ["echo", "pwned"],
                    "cwd": other.path().to_string_lossy(),
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn cwd_via_symlink_escape_is_denied() {
        use std::os::unix::fs::symlink;
        let allowed = tempdir().unwrap();
        let other = tempdir().unwrap();
        let link = allowed.path().join("escape");
        symlink(other.path(), &link).unwrap();

        let sandbox = ToolSandbox::new().allow_exec_cwd(allowed.path());
        let ctx = make_exec_ctx(&sandbox);
        let tool = make_tool_fast();

        let err = tool
            .execute(
                &ctx,
                json!({
                    "command": ["echo", "pwned"],
                    "cwd": link.to_string_lossy(),
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn empty_sandbox_denies_any_execution() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new();
        let ctx = make_exec_ctx(&sandbox);
        let tool = make_tool_fast();
        let err = tool
            .execute(
                &ctx,
                json!({
                    "command": ["echo", "pwned"],
                    "cwd": dir.path().to_string_lossy(),
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn read_access_does_not_grant_exec() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_read(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        let tool = make_tool_fast();
        let err = tool
            .execute(
                &ctx,
                json!({
                    "command": ["echo", "nope"],
                    "cwd": dir.path().to_string_lossy(),
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn write_access_does_not_grant_exec() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_write(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        let tool = make_tool_fast();
        let err = tool
            .execute(
                &ctx,
                json!({
                    "command": ["echo", "nope"],
                    "cwd": dir.path().to_string_lossy(),
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn empty_argv_is_rejected() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        let tool = make_tool_fast();
        let err = tool
            .execute(
                &ctx,
                json!({
                    "command": [],
                    "cwd": dir.path().to_string_lossy(),
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
    }

    #[tokio::test]
    async fn unknown_program_surfaces_as_execution_failed() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        let tool = make_tool_fast();
        let err = tool
            .execute(
                &ctx,
                json!({
                    "command": ["definitely-not-a-real-binary-xyz12345"],
                    "cwd": dir.path().to_string_lossy(),
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed(_)));
    }

    #[tokio::test]
    async fn timeout_kills_long_running_command() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        // Sleep 30s, timeout after 1s.
        let tool = ShellTool::with_config(ShellConfig {
            default_timeout: Duration::from_secs(1),
            max_timeout: Duration::from_secs(10),
            max_output_bytes: 4 * 1024,
            default_path: "/usr/local/bin:/usr/bin:/bin".to_string(),
        });

        let start = std::time::Instant::now();
        let result = tool
            .execute(
                &ctx,
                json!({
                    "command": ["sleep", "30"],
                    "cwd": dir.path().to_string_lossy(),
                }),
            )
            .await
            .unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "should have been killed well before sleep finished, took {elapsed:?}"
        );
        assert!(result.is_error);
        assert!(result.output.contains("timed out"));
    }

    #[tokio::test]
    async fn user_timeout_clamped_to_max() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        // User asks for 9999s, max is 2s. Sleep 5s should get killed.
        let tool = ShellTool::with_config(ShellConfig {
            default_timeout: Duration::from_secs(30),
            max_timeout: Duration::from_secs(2),
            max_output_bytes: 4 * 1024,
            default_path: "/usr/local/bin:/usr/bin:/bin".to_string(),
        });

        let start = std::time::Instant::now();
        let result = tool
            .execute(
                &ctx,
                json!({
                    "command": ["sleep", "5"],
                    "cwd": dir.path().to_string_lossy(),
                    "timeout_secs": 9999,
                }),
            )
            .await
            .unwrap();
        assert!(start.elapsed() < Duration::from_secs(4));
        assert!(result.is_error);
        assert!(result.output.contains("timed out"));
    }

    #[tokio::test]
    async fn output_is_truncated_at_cap() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        // 4 KB cap, print ~80 KB (20k lines of 4 chars).
        let tool = make_tool_fast();
        let result = tool
            .execute(
                &ctx,
                json!({
                    "command": ["seq", "1", "20000"],
                    "cwd": dir.path().to_string_lossy(),
                }),
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(
            result.output.contains("(stdout truncated)"),
            "expected truncation marker, got output:\n{}",
            result.output
        );
    }

    #[tokio::test]
    async fn cwd_is_respected_by_child() {
        let dir = tempdir().unwrap();
        let sub = dir.path().join("work");
        fs::create_dir(&sub).unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        let tool = make_tool_fast();
        let result = tool
            .execute(
                &ctx,
                json!({
                    "command": ["pwd"],
                    "cwd": sub.to_string_lossy(),
                }),
            )
            .await
            .unwrap();
        let canonical = fs::canonicalize(&sub).unwrap();
        assert!(
            result.output.contains(canonical.to_str().unwrap()),
            "pwd output {} did not mention canonical cwd {}",
            result.output,
            canonical.display()
        );
    }

    #[tokio::test]
    async fn child_env_defaults_to_fixed_path_only() {
        // Run `env` and verify that the output contains PATH but
        // nothing like SHELL or HOME (which the parent process
        // almost certainly has).
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        let tool = make_tool_fast();
        let result = tool
            .execute(
                &ctx,
                json!({
                    "command": ["env"],
                    "cwd": dir.path().to_string_lossy(),
                }),
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.output.contains("PATH=/usr/local/bin:/usr/bin:/bin"));
        assert!(!result.output.contains("SHELL="));
        // HOME might legitimately not exist, but we shouldn't have
        // inherited it from the parent.
        assert!(!result.output.contains("HOME="));
    }

    // --- unit tests for the helpers ----------------------------------

    #[test]
    fn build_child_env_with_empty_allowlist_only_sets_path() {
        let env = build_child_env(
            "/usr/bin",
            vec![
                ("HOME".to_string(), "/home/user".to_string()),
                ("SECRET".to_string(), "no".to_string()),
            ],
            &[],
        );
        assert_eq!(env.get("PATH"), Some(&"/usr/bin".to_string()));
        assert!(env.get("HOME").is_none());
        assert!(env.get("SECRET").is_none());
        assert_eq!(env.len(), 1);
    }

    #[test]
    fn build_child_env_copies_allowlisted_vars_from_parent() {
        let env = build_child_env(
            "/usr/bin",
            vec![
                ("HOME".to_string(), "/home/user".to_string()),
                ("SECRET".to_string(), "no".to_string()),
            ],
            &["HOME"],
        );
        assert_eq!(env.get("PATH"), Some(&"/usr/bin".to_string()));
        assert_eq!(env.get("HOME"), Some(&"/home/user".to_string()));
        assert!(env.get("SECRET").is_none());
    }
}
