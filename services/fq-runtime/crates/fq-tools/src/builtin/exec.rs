//! `exec` built-in tool: run a single program as a child process with
//! the agent's sandbox as the enforcement boundary.
//!
//! Named `exec` (not `shell`) on purpose: it runs one program directly,
//! it does not interpret a shell. The name matches its sandbox
//! dimension, `exec_cwd`.
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
//!   clamped to the runtime-configured maximum, as is the configured
//!   default. On timeout the child's whole process group is killed and
//!   the tool returns [`ToolError::TimedOut`] carrying whatever output
//!   was captured up to that point. A `TimedOut` and not an
//!   `is_error: true` result: the host counts consecutive deadlines to
//!   end an invocation whose tools have stopped answering, and a
//!   timeout dressed as an answer left the one tool that can hang for
//!   fifteen minutes outside that protection (#547).
//! - **Process-group teardown** — the child leads its own process
//!   group, and a timed-out or dropped call ends the *group*, not just
//!   the direct child (A9, #552). The `reap` submodule carries the
//!   escalation, and why a cleanly exited command's descendants are
//!   left alone.
//! - **Bounded output drain** — once the child is gone (exited or
//!   killed), legitimate leftover output is only what sits in the
//!   kernel pipe buffer, so capture continues for a short grace window
//!   (`drain_grace`) and is then cut. Without the bound, a descendant
//!   that inherited the stdout/stderr pipe (anything daemonized — a
//!   `nohup`-style spawn, a test runner leaving a child) holds EOF
//!   hostage and the tool hangs forever *despite the timeout having
//!   fired* (#176). Cut output carries an explicit note. The group kill
//!   ends that standoff for the common case, but not for a descendant
//!   that left the group (`setsid`), so the bound stays.
//! - **Output cap & line limits** — stdout and stderr are each bounded
//!   by a configurable byte cap (a safety backstop). When bytes are
//!   dropped the returned text carries a marker showing how much (kept
//!   vs produced). Callers can also bound output by lines — `max_lines`
//!   (first N, like `head`) or `tail_lines` (last N, like `tail`) — the
//!   argv-native replacement for `| head` / `| tail`.
//! - **Environment is a fresh map** — the child does NOT inherit the
//!   parent's environment. A small safe baseline is set (most
//!   importantly a pinned `PATH`), then the agent's declared env
//!   allowlist is copied from the parent on top. An agent that
//!   doesn't list `HOME` in its sandbox will have no `HOME` set in
//!   the child.
//! - **Non-zero exit codes are errors** — the tool reports
//!   `is_error: true` when the exit code is non-zero, but still
//!   returns stdout/stderr so the LLM can understand what happened.
//! - **Shell operators are refused, not run** — a standalone `|`, `||`,
//!   `&&`, `;`, `<`, `>`, or `>>` element in argv is a transplanted
//!   shell idiom, but there is no shell to interpret it. Rather than
//!   hand the operator to the program verbatim (a baffling failure),
//!   the tool rejects the call with a teaching error pointing back at
//!   the safe-by-construction path: plain argv, separate calls to
//!   combine, `file_write` for `>` (#92). Operator characters *inside*
//!   an argument (a `grep` pattern `a|b`) are untouched.
//!
//! ## Known gaps
//!
//! These are real limitations of process-level sandboxing that can
//! only be closed with OS-level isolation (see ADR-0010):
//!
//! - **No PATH restriction on binaries**. Any executable reachable by
//!   the default PATH can be called — `curl`, `wget`, system tools.
//! - **No network isolation**. Commands can open network connections to
//!   any host. An agent definition's `sandbox.network` allowlist is
//!   parsed but never consulted here, so declaring it restricts nothing
//!   — the load path warns about this rather than let it pass silently
//!   (#35; enforcement tracked by #208 and #209).
//! - **No cgroup / rlimit enforcement**. The child can consume CPU,
//!   memory, and open files up to the process-level limits.
//! - **No syscall filtering (seccomp)**. Anything the binary can do,
//!   it can do.
//!
//! Container-level isolation (ADR-0010) is the path to closing these.
//! Until then, the exec tool should be granted only to agents you
//! trust with these capabilities. The tool's own battery runs in
//! `just runtime-ci`; it spawns real children, so it wants a machine
//! you are happy to have `sleep` and `bash` on.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::watch;
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::tool::{Tool, ToolContext, ToolError, ToolResult};

mod output;
mod reap;

use output::{LineLimit, capture_stream, format_output};
// Re-exported at the path it has always had: `fq-cli`'s status view
// renders byte counts with it.
pub use output::human_bytes;

/// The `PATH` every child the runtime starts with a cleared environment
/// gets by default: the `exec` tool's children, and the stdio MCP
/// servers the runtime spawns (which prepend the directory their
/// command resolved to). One definition, so the two can never drift.
pub const DEFAULT_CHILD_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// Runtime-configurable parameters for the exec tool.
#[derive(Debug, Clone)]
pub struct ExecConfig {
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
    /// How long output capture may continue after the child is gone
    /// (exited or killed). Bounds the drain so a descendant holding
    /// the inherited pipe open cannot hang the tool forever (#176);
    /// the kernel pipe buffer flushes in far less than this.
    pub drain_grace: Duration,
    /// How long a timed-out process group gets to exit on `SIGTERM`
    /// before it is `SIGKILL`ed (#552). The window ends early once the
    /// group is empty, so this is the price of a tree that ignores
    /// `SIGTERM`, not of every timeout.
    pub kill_grace: Duration,
}

impl Default for ExecConfig {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(30),
            max_timeout: Duration::from_secs(300),
            max_output_bytes: 100 * 1024,
            default_path: DEFAULT_CHILD_PATH.to_string(),
            drain_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(2),
        }
    }
}

/// Built-in `exec` tool.
#[derive(Debug, Clone)]
pub struct ExecTool {
    config: ExecConfig,
}

impl ExecTool {
    /// Construct an exec tool with default configuration.
    pub fn new() -> Self {
        Self {
            config: ExecConfig::default(),
        }
    }

    /// Construct an exec tool with an explicit configuration.
    pub fn with_config(config: ExecConfig) -> Self {
        Self { config }
    }

    /// This call's wall-clock timeout: what the caller asked for,
    /// clamped down to [`ExecConfig::max_timeout`], or the configured
    /// default when the caller asked for nothing. Clamped rather than
    /// rejected so an over-ambitious `timeout_secs` does not trap the
    /// agent in a retry loop.
    ///
    /// One function so `execute` and
    /// [`requested_deadline`](Tool::requested_deadline) can never
    /// disagree — the host arms its backstop off the second and the
    /// child dies by the first.
    ///
    /// `max_timeout` bounds **both** arms, the configured default
    /// included. A config with `default_timeout > max_timeout` is an
    /// operator mistake the runtime also refuses at load, but the
    /// primitive must not depend on that: an unclamped default would
    /// run the child past the ceiling the host clamped its own backstop
    /// to, inverting the ordering the backstop grace exists to
    /// guarantee, and the child would then die by future-drop with its
    /// captured output discarded.
    fn call_timeout(&self, requested_secs: Option<u64>) -> Duration {
        let requested = match requested_secs {
            // Zero is rejected in `execute`; here it is simply not a
            // request for a longer deadline.
            None | Some(0) => self.config.default_timeout,
            Some(secs) => Duration::from_secs(secs),
        };
        requested.min(self.config.max_timeout)
    }
}

impl Default for ExecTool {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize)]
struct ExecParams {
    command: Vec<String>,
    cwd: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
    /// Keep only the first N lines of each stream (like `head`).
    #[serde(default)]
    max_lines: Option<u64>,
    /// Keep only the last N lines of each stream (like `tail`).
    #[serde(default)]
    tail_lines: Option<u64>,
}

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> &str {
        "exec"
    }

    fn description(&self) -> &str {
        "Run a single program as a child process. Takes an argv array \
         (NOT a shell string) plus a working directory that must be \
         within the agent's exec_cwd sandbox. Every call has a timeout \
         (on expiry the command and every process it started are \
         killed) and an output byte cap; set max_lines or tail_lines to keep \
         only the first or last N lines instead of piping to head/tail. \
         Non-zero exit codes are returned as errors but still include \
         stdout/stderr."
    }

    /// `exec` is the one built-in that times itself: it kills the child
    /// and returns the output captured so far, which is far more useful
    /// than the host's bare "timed out". Declaring the deadline here
    /// lets the host clamp it to `[tools] max_timeout_secs` and arm its
    /// own timer as a backstop rather than a competitor.
    fn requested_deadline(&self, params: &Value) -> Option<Duration> {
        let requested = params
            .get("timeout_secs")
            .and_then(serde_json::Value::as_u64);
        Some(self.call_timeout(requested))
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
                    "format": "path",
                    "description": "Working directory for the child process. Must be within the agent's exec_cwd sandbox."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Optional timeout in seconds. Clamped to the runtime's configured maximum. On expiry the command and every process it started are killed."
                },
                "max_lines": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional. Return only the first N lines of stdout/stderr — the argv-native replacement for piping to `head`. Mutually exclusive with tail_lines."
                },
                "tail_lines": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional. Return only the last N lines of stdout/stderr — the argv-native replacement for piping to `tail`. Mutually exclusive with max_lines."
                }
            },
            "required": ["command", "cwd"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext<'_>, params: Value) -> Result<ToolResult, ToolError> {
        let params: ExecParams = serde_json::from_value(params)
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

        // Reject transplanted shell idioms before spawning. There is no
        // shell here, so a standalone `|`/`>`/`&&`/… element would be
        // handed to the program verbatim (or fail to spawn as the
        // program) with a baffling error. The error message is the
        // agent's next prompt, so it teaches instead (#92).
        if let Some(op) = standalone_shell_operator(&params.command) {
            return Err(ToolError::InvalidParameters(shell_operator_help(op)));
        }

        // Resolve the optional output line-limit — the argv-native
        // replacement for `| head` / `| tail`. At most one may be set.
        let limit = match (params.max_lines, params.tail_lines) {
            (Some(_), Some(_)) => {
                return Err(ToolError::InvalidParameters(
                    "set at most one of `max_lines` (first N lines) or \
                     `tail_lines` (last N lines), not both"
                        .to_string(),
                ));
            }
            (Some(0), _) | (_, Some(0)) => {
                return Err(ToolError::InvalidParameters(
                    "`max_lines` / `tail_lines` must be greater than 0".to_string(),
                ));
            }
            (Some(n), None) => LineLimit::Head(n as usize),
            (None, Some(n)) => LineLimit::Tail(n as usize),
            (None, None) => LineLimit::None,
        };

        // Enforce cwd sandbox.
        let cwd_path = PathBuf::from(&params.cwd);
        let canonical_cwd = ctx.sandbox.check_exec_cwd(&cwd_path)?;

        // Clamp timeout.
        if params.timeout_secs == Some(0) {
            return Err(ToolError::InvalidParameters(
                "timeout_secs must be > 0".to_string(),
            ));
        }
        let timeout_duration = self.call_timeout(params.timeout_secs);

        // Build the child's environment. Start from a small fixed
        // baseline (just PATH), then copy each variable the agent
        // explicitly allowlisted (`sandbox.env`, carried on the
        // ToolSandbox — issue #34) from the parent's env, then the
        // runtime's ambient identity variables (`FQ_*`, issue #162).
        let env = build_child_env(
            &self.config.default_path,
            std::env::vars(),
            ctx.sandbox.env_allowlist(),
            ctx.sandbox.ambient_env(),
        );

        debug!(
            program = %program,
            cwd = %canonical_cwd.display(),
            timeout_ms = timeout_duration.as_millis() as u64,
            "spawning child process"
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
        // Its own process group, so a timeout or a dropped future can
        // end the whole tree rather than just the direct child (#552).
        reap::lead_own_group(&mut cmd);

        let mut child = cmd
            .spawn()
            .map_err(|err| classify_spawn_error(program, err))?;

        let pgid = reap::group_id(&child);
        // Declared after `child` so it drops first: the group is killed
        // while the leader is still unreaped and its id still ours.
        let mut group = reap::GroupGuard::new(pgid);

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let max_output_bytes = self.config.max_output_bytes;
        // A tail limit needs the end of the stream, so capture the tail
        // window; every other mode keeps the head up to the byte cap.
        let capture_tail = matches!(limit, LineLimit::Tail(_));

        // The drain-cut signal (#176): flipped to `true` a grace period
        // after the child is gone, so a descendant holding the
        // inherited pipe open cannot stall the capture tasks forever.
        let (stop_tx, stop_rx) = watch::channel(false);

        let stdout_stop = stop_rx.clone();
        let stdout_task = tokio::spawn(async move {
            match stdout {
                Some(stream) => {
                    capture_stream(stream, max_output_bytes, capture_tail, stdout_stop).await
                }
                None => (Vec::new(), 0, false),
            }
        });
        let stderr_task = tokio::spawn(async move {
            match stderr {
                Some(stream) => {
                    capture_stream(stream, max_output_bytes, capture_tail, stop_rx).await
                }
                None => (Vec::new(), 0, false),
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
                // captured output tasks finish promptly — and kill the
                // whole group, or a `<payload> & sleep 999` outlives
                // the call as an unwatched process (#552).
                warn!(
                    timeout_ms = timeout_duration.as_millis() as u64,
                    "exec timeout fired — killing the child's process group"
                );
                reap::kill_group(&mut child, pgid, self.config.kill_grace).await;
                (None, true)
            }
        };
        // The child is reaped on both branches: nothing left to guard.
        group.disarm();

        // The child is gone on both branches above. Whatever output is
        // still legitimately owed sits in the kernel pipe buffer and
        // flushes near-instantly; a stream still open past the grace is
        // a descendant that inherited the pipe. Arm the cut.
        let drain_grace = self.config.drain_grace;
        tokio::spawn(async move {
            tokio::time::sleep(drain_grace).await;
            let _ = stop_tx.send(true);
        });

        let (stdout_bytes, stdout_total, stdout_cut) = stdout_task
            .await
            .map_err(|err| ToolError::ExecutionFailed(format!("stdout task panicked: {err}")))?;
        let (stderr_bytes, stderr_total, stderr_cut) = stderr_task
            .await
            .map_err(|err| ToolError::ExecutionFailed(format!("stderr task panicked: {err}")))?;

        let mut body = format_output(
            &stdout_bytes,
            stdout_total,
            &stderr_bytes,
            stderr_total,
            limit,
        );
        if stdout_cut || stderr_cut {
            warn!(
                grace_ms = drain_grace.as_millis() as u64,
                "exec output drain cut — a descendant process still held the pipe"
            );
            body.push_str(&format!(
                "(output capture ended after a {}s grace: the command's process \
                 exited but a background/descendant process it started still \
                 holds the output pipe open — any later output from it is not \
                 captured)\n",
                drain_grace.as_secs_f64(),
            ));
        }

        // A deadline, not an ordinary error result. This used to be
        // `Ok(ToolResult { is_error: true })`, which reads to the host
        // as the tool having *answered* — so a command that hung on
        // every attempt never advanced the consecutive-timeout count,
        // and the one tool that can legitimately sit for fifteen
        // minutes was the one that limit did not protect (#547). The
        // captured output rides along on the error so nothing the
        // command did say is lost.
        if timed_out {
            return Err(ToolError::TimedOut {
                after: timeout_duration,
                // `output: Some(..)` already tells the model the tool
                // stopped the work itself; for `exec` that now covers
                // everything the command started, not just the direct
                // child (#552), which is worth saying in as many words.
                output: Some(format!(
                    "{body}(the command and every process it started were killed)\n"
                )),
            });
        }

        let status = exit_status.expect("handled timeout above");
        let exit_code = status.code();
        let is_error = !status.success();

        let header = match exit_code {
            Some(code) => format!("Exit code: {code}"),
            None => "Command was terminated by a signal.".to_string(),
        };

        Ok(ToolResult {
            output: format!("{header}\n\n{body}"),
            is_error,
        })
    }
}

/// Shell operators that, as a *standalone* argv element, are almost
/// always a transplanted shell idiom rather than a real argument. The
/// tool runs one program with no shell, so these are never interpreted.
/// Matched by whole-element equality only — an operator character
/// *inside* an argument (a `grep` pattern `a|b`) is a legitimate value
/// and is left alone.
const SHELL_OPERATORS: &[&str] = &["|", "||", "&&", ";", "<", ">", ">>"];

/// The first standalone shell-operator element in `argv`, if any.
fn standalone_shell_operator(argv: &[String]) -> Option<&'static str> {
    argv.iter().find_map(|arg| {
        SHELL_OPERATORS
            .iter()
            .copied()
            .find(|&op| op == arg.as_str())
    })
}

/// Teaching error shown when argv carries a standalone shell operator.
/// This text is agent-facing prompt copy (#92): it explains there is no
/// shell and points at the safe path — plain argv, separate calls, the
/// `max_lines`/`tail_lines` params in place of `| head`/`| tail`, and
/// `file_write` in place of `>`.
fn shell_operator_help(op: &str) -> String {
    format!(
        "`{op}` is a shell operator, but the `exec` tool does not run a \
         shell — it executes one program directly from the argv array, so \
         `|`, `>`, `&&`, and similar operators are not interpreted. Pass a \
         plain argv array (e.g. [\"grep\", \"-n\", \"foo\", \"file.txt\"]). \
         To chain or pipe commands, make separate `exec` calls and combine \
         the results yourself. To limit output, use the `max_lines` (first \
         N lines, like `head`) or `tail_lines` (last N lines) parameters \
         instead of piping. To write output to a file, use the `file_write` \
         tool instead of `>`."
    )
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

/// Assemble the child's environment: a fixed `PATH` baseline, plus each
/// allowlisted variable copied from the parent process if it is set
/// there, plus the runtime's ambient identity variables. `PATH` in the
/// allowlist overrides the baseline with the parent's value — an agent
/// that grants `PATH` opts into the daemon's fuller path (e.g. a
/// toolchain on it). Ambient variables are applied last: they are
/// runtime-owned facts about the invocation (`FQ_*`, issue #162), so a
/// same-named variable from the host must not shadow them. Generic over
/// the name type so both a runtime `&[String]` (the sandbox allowlist)
/// and `&[&str]` (tests) work.
fn build_child_env<I, S>(
    default_path: &str,
    parent_env: I,
    allowlist: &[S],
    ambient: &[(String, String)],
) -> HashMap<String, String>
where
    I: IntoIterator<Item = (String, String)>,
    S: AsRef<str>,
{
    let mut env = HashMap::new();
    env.insert("PATH".to_string(), default_path.to_string());

    if !allowlist.is_empty() {
        let parent: HashMap<String, String> = parent_env.into_iter().collect();
        for name in allowlist {
            let name = name.as_ref();
            if let Some(value) = parent.get(name) {
                env.insert(name.to_string(), value.clone());
            }
        }
    }
    for (name, value) in ambient {
        env.insert(name.clone(), value.clone());
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

    /// Every grace shortened so the timing tests cost milliseconds.
    fn fast_config() -> ExecConfig {
        ExecConfig {
            default_timeout: Duration::from_secs(5),
            max_timeout: Duration::from_secs(10),
            max_output_bytes: 4 * 1024, // Small to make truncation tests fast
            default_path: "/usr/local/bin:/usr/bin:/bin".to_string(),
            drain_grace: Duration::from_millis(500),
            kill_grace: Duration::from_millis(300),
        }
    }

    fn make_tool_fast() -> ExecTool {
        ExecTool::with_config(fast_config())
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
        // Normal EOF path: the drain-cut note must not appear.
        assert!(!result.output.contains("output capture ended"));
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
        let tool = ExecTool::with_config(ExecConfig {
            default_timeout: Duration::from_secs(1),
            ..fast_config()
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
            .expect_err("a deadline is a TimedOut error, not an ok result (#547)");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "should have been killed well before sleep finished, took {elapsed:?}"
        );
        let ToolError::TimedOut { after, output } = result else {
            panic!("expected TimedOut, got: {result}");
        };
        assert_eq!(after, Duration::from_secs(1));
        assert!(output.is_some(), "the captured output rides along");
    }

    #[tokio::test]
    async fn user_timeout_clamped_to_max() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        // User asks for 9999s, max is 2s. Sleep 5s should get killed.
        let tool = ExecTool::with_config(ExecConfig {
            default_timeout: Duration::from_secs(30),
            max_timeout: Duration::from_secs(2),
            ..fast_config()
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
            .expect_err("a deadline is a TimedOut error, not an ok result (#547)");
        assert!(start.elapsed() < Duration::from_secs(4));
        let ToolError::TimedOut { after, .. } = result else {
            panic!("expected TimedOut, got: {result}");
        };
        assert_eq!(after, Duration::from_secs(2), "clamped to max_timeout");
    }

    /// The deadline `exec` declares to the host is exactly the one it
    /// will enforce on the child (#547) — clamped, defaulted, and
    /// unmoved by a zero it is about to reject.
    #[test]
    fn declared_deadline_matches_the_one_exec_enforces() {
        let tool = make_tool_fast(); // default 5s, max 10s
        let deadline = |params: Value| tool.requested_deadline(&params).unwrap();
        assert_eq!(deadline(json!({})), Duration::from_secs(5));
        assert_eq!(deadline(json!({"timeout_secs": 3})), Duration::from_secs(3));
        assert_eq!(
            deadline(json!({"timeout_secs": 9999})),
            Duration::from_secs(10)
        );
        assert_eq!(deadline(json!({"timeout_secs": 0})), Duration::from_secs(5));
    }

    /// #176 regression: a grandchild that inherited the output pipe
    /// must not hold the tool hostage after the timeout kill — the
    /// drain is bounded, and what was captured before the cut is
    /// still returned with an honest note.
    ///
    /// The fault is injected with `setsid` since #552: the timeout now
    /// kills the child's whole process group, so a plain `sleep 60 &`
    /// dies with its parent and never reaches the pipe standoff. A
    /// descendant that gave itself a new session is outside the group
    /// and still can, which is exactly why the drain stays bounded.
    #[tokio::test]
    async fn timeout_with_pipe_holding_grandchild_returns_within_drain_grace() {
        let Some(setsid) = setsid_path() else {
            eprintln!("skipping: setsid not found on the child PATH");
            return;
        };
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        let tool = ExecTool::with_config(ExecConfig {
            default_timeout: Duration::from_secs(1),
            drain_grace: Duration::from_secs(1),
            ..fast_config()
        });

        let start = std::time::Instant::now();
        // The `setsid`ed `sleep 60` inherits the stdout/stderr pipes
        // and leaves the process group; the foreground `sleep 60` keeps
        // the direct child alive past the timeout so the kill path
        // fires. Without the bounded drain this call hangs ~60s.
        // (Operator characters inside ONE argv element are legitimate
        // values, not standalone operators — the #92 guard allows it.)
        let result = tool
            .execute(
                &ctx,
                json!({
                    "command": ["sh", "-c", format!("echo hi; {setsid} sleep 10 & sleep 60")],
                    "cwd": dir.path().to_string_lossy(),
                }),
            )
            .await
            .expect_err("a deadline is a TimedOut error, not an ok result (#547)");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "must return within timeout + drain grace, took {elapsed:?}"
        );
        let ToolError::TimedOut { output, .. } = result else {
            panic!("expected TimedOut, got: {result}");
        };
        let output = output.expect("the captured output rides along on the error");
        // Output captured before the cut is present…
        assert!(output.contains("hi"), "{output}");
        // …and the cut is reported honestly.
        assert!(output.contains("output capture ended"), "{output}");
    }

    /// The same defect one branch over (#176): a child that exits
    /// cleanly but leaves a daemonized descendant holding the pipe
    /// must not hang the tool either. The command itself succeeded,
    /// so the result is NOT an error — just cut, with the note.
    #[tokio::test]
    async fn clean_exit_with_pipe_holding_grandchild_is_not_hung() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        let tool = make_tool_fast(); // 500ms drain grace

        let start = std::time::Instant::now();
        let result = tool
            .execute(
                &ctx,
                json!({
                    "command": ["sh", "-c", "echo done; sleep 60 &"],
                    "cwd": dir.path().to_string_lossy(),
                }),
            )
            .await
            .unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(4),
            "must return within the drain grace, took {elapsed:?}"
        );
        assert!(!result.is_error, "{}", result.output);
        assert!(result.output.contains("Exit code: 0"), "{}", result.output);
        assert!(result.output.contains("done"), "{}", result.output);
        assert!(
            result.output.contains("output capture ended"),
            "{}",
            result.output
        );
    }

    #[tokio::test]
    async fn output_is_truncated_at_cap_with_honest_report() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        // 4 KB cap, print ~100 KB (20k lines).
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
        // The marker names the byte cap AND how much was kept vs produced,
        // and points at the line-limit params — not a bare "(truncated)".
        assert!(
            result.output.contains("truncated at the byte cap"),
            "output:\n{}",
            result.output
        );
        assert!(
            result.output.contains("kept 4.0 KiB of"),
            "output:\n{}",
            result.output
        );
        assert!(
            result.output.contains("max_lines"),
            "output:\n{}",
            result.output
        );
    }

    #[tokio::test]
    async fn max_lines_keeps_only_the_first_n_lines() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        let tool = make_tool_fast();
        let result = tool
            .execute(
                &ctx,
                json!({
                    "command": ["seq", "1", "100"],
                    "cwd": dir.path().to_string_lossy(),
                    "max_lines": 5,
                }),
            )
            .await
            .unwrap();
        assert!(!result.is_error, "output:\n{}", result.output);
        assert!(
            result.output.contains("1\n2\n3\n4\n5"),
            "output:\n{}",
            result.output
        );
        assert!(
            !result.output.contains("\n6\n"),
            "line 6 should be dropped:\n{}",
            result.output
        );
        assert!(
            result.output.contains("showing the first 5 line(s)"),
            "output:\n{}",
            result.output
        );
    }

    #[tokio::test]
    async fn tail_lines_keeps_only_the_last_n_lines() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        let tool = make_tool_fast();
        let result = tool
            .execute(
                &ctx,
                json!({
                    "command": ["seq", "1", "100"],
                    "cwd": dir.path().to_string_lossy(),
                    "tail_lines": 3,
                }),
            )
            .await
            .unwrap();
        assert!(!result.is_error, "output:\n{}", result.output);
        assert!(
            result.output.contains("98\n99\n100"),
            "output:\n{}",
            result.output
        );
        assert!(
            !result.output.contains("\n1\n"),
            "line 1 should be dropped:\n{}",
            result.output
        );
        assert!(
            result.output.contains("showing the last 3 line(s)"),
            "output:\n{}",
            result.output
        );
    }

    #[tokio::test]
    async fn tail_lines_returns_true_tail_even_past_the_byte_cap() {
        // The tail must be the real end of the stream, not the last lines
        // of a byte-capped head: with a 4 KB cap and ~100 KB of output, a
        // head capture could never reach the final lines.
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        let tool = make_tool_fast(); // 4 KB cap
        let result = tool
            .execute(
                &ctx,
                json!({
                    "command": ["seq", "1", "20000"],
                    "cwd": dir.path().to_string_lossy(),
                    "tail_lines": 2,
                }),
            )
            .await
            .unwrap();
        assert!(!result.is_error, "output:\n{}", result.output);
        assert!(
            result.output.contains("19999\n20000"),
            "tail must reach the true final lines:\n{}",
            result.output
        );
    }

    #[tokio::test]
    async fn max_lines_and_tail_lines_together_is_rejected() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        let tool = make_tool_fast();
        let err = tool
            .execute(
                &ctx,
                json!({
                    "command": ["seq", "1", "10"],
                    "cwd": dir.path().to_string_lossy(),
                    "max_lines": 3,
                    "tail_lines": 3,
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
    }

    #[tokio::test]
    async fn zero_line_limit_is_rejected() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        let tool = make_tool_fast();
        let err = tool
            .execute(
                &ctx,
                json!({
                    "command": ["seq", "1", "10"],
                    "cwd": dir.path().to_string_lossy(),
                    "max_lines": 0,
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
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
            &[] as &[&str],
            &[],
        );
        assert_eq!(env.get("PATH"), Some(&"/usr/bin".to_string()));
        assert!(!env.contains_key("HOME"));
        assert!(!env.contains_key("SECRET"));
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
            &[],
        );
        assert_eq!(env.get("PATH"), Some(&"/usr/bin".to_string()));
        assert_eq!(env.get("HOME"), Some(&"/home/user".to_string()));
        assert!(!env.contains_key("SECRET"));
    }

    #[test]
    fn build_child_env_allowlisted_path_overrides_the_baseline() {
        // Granting PATH opts the child into the parent's fuller PATH
        // (e.g. a toolchain on it) instead of the fixed baseline.
        let env = build_child_env(
            "/usr/bin",
            vec![("PATH".to_string(), "/opt/tools/bin:/usr/bin".to_string())],
            &["PATH"],
            &[],
        );
        assert_eq!(
            env.get("PATH"),
            Some(&"/opt/tools/bin:/usr/bin".to_string())
        );
    }

    #[test]
    fn build_child_env_accepts_a_runtime_string_allowlist() {
        // The real call site passes a &[String] (the sandbox
        // allowlist), not a &[&str] literal — prove the generic binds.
        let allow = vec!["HOME".to_string()];
        let env = build_child_env(
            "/usr/bin",
            vec![("HOME".to_string(), "/home/user".to_string())],
            &allow,
            &[],
        );
        assert_eq!(env.get("HOME"), Some(&"/home/user".to_string()));
    }

    #[test]
    fn build_child_env_injects_ambient_vars_without_any_allowlist() {
        // Ambient identity vars (issue #162) need no sandbox.env
        // opt-in — they are runtime-owned invocation facts.
        let ambient = vec![
            ("FQ_INVOCATION_ID".to_string(), "inv-1".to_string()),
            ("FQ_AGENT_ID".to_string(), "agent-a".to_string()),
        ];
        let env = build_child_env(
            "/usr/bin",
            vec![("HOME".to_string(), "/home/user".to_string())],
            &[] as &[&str],
            &ambient,
        );
        assert_eq!(env.get("FQ_INVOCATION_ID"), Some(&"inv-1".to_string()));
        assert_eq!(env.get("FQ_AGENT_ID"), Some(&"agent-a".to_string()));
        assert!(!env.contains_key("HOME"));
    }

    #[test]
    fn build_child_env_ambient_wins_over_allowlisted_parent_var() {
        // A host variable that happens to share an FQ_* name must not
        // shadow the runtime's value, even when allowlisted.
        let ambient = vec![("FQ_AGENT_ID".to_string(), "real-agent".to_string())];
        let env = build_child_env(
            "/usr/bin",
            vec![("FQ_AGENT_ID".to_string(), "spoofed".to_string())],
            &["FQ_AGENT_ID"],
            &ambient,
        );
        assert_eq!(env.get("FQ_AGENT_ID"), Some(&"real-agent".to_string()));
    }

    /// End-to-end (issue #34): a var named in `sandbox.env` reaches the
    /// spawned child; a var not named does not. Uses a uniquely-named
    /// parent var so no sibling test reads it.
    #[tokio::test]
    async fn allowlisted_env_var_reaches_the_child_process() {
        let dir = tempdir().unwrap();
        // SAFETY: unique name, single write; mirrors the set_var
        // precedent in llm/genai.rs's tests.
        unsafe {
            std::env::set_var("FQ_TEST_ALLOWED_34", "granted");
            std::env::set_var("FQ_TEST_DENIED_34", "secret");
        }

        // Allowlisted → present in the child.
        let sandbox = ToolSandbox::new()
            .allow_exec_cwd(dir.path())
            .allow_env("FQ_TEST_ALLOWED_34");
        let ctx = make_exec_ctx(&sandbox);
        let tool = make_tool_fast();
        let result = tool
            .execute(
                &ctx,
                json!({
                    "command": ["printenv", "FQ_TEST_ALLOWED_34"],
                    "cwd": dir.path().to_string_lossy(),
                }),
            )
            .await
            .unwrap();
        assert!(
            !result.is_error,
            "printenv of an allowlisted var: {result:?}"
        );
        assert!(
            result.output.contains("granted"),
            "allowlisted var must reach the child: {}",
            result.output
        );

        // Not allowlisted → absent (printenv exits non-zero).
        let denied = tool
            .execute(
                &ctx,
                json!({
                    "command": ["printenv", "FQ_TEST_DENIED_34"],
                    "cwd": dir.path().to_string_lossy(),
                }),
            )
            .await
            .unwrap();
        assert!(
            !denied.output.contains("secret"),
            "a var not in sandbox.env must not reach the child: {}",
            denied.output
        );

        unsafe {
            std::env::remove_var("FQ_TEST_ALLOWED_34");
            std::env::remove_var("FQ_TEST_DENIED_34");
        }
    }

    /// End-to-end (issue #162): the runtime's ambient identity vars
    /// reach the spawned child with no `sandbox.env` opt-in.
    #[tokio::test]
    async fn ambient_identity_vars_reach_the_child_process() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new()
            .allow_exec_cwd(dir.path())
            .ambient_var("FQ_INVOCATION_ID", "inv-e2e-162");
        let ctx = make_exec_ctx(&sandbox);
        let tool = make_tool_fast();
        let result = tool
            .execute(
                &ctx,
                json!({
                    "command": ["printenv", "FQ_INVOCATION_ID"],
                    "cwd": dir.path().to_string_lossy(),
                }),
            )
            .await
            .unwrap();
        assert!(!result.is_error, "printenv of an ambient var: {result:?}");
        assert!(
            result.output.contains("inv-e2e-162"),
            "ambient identity var must reach the child: {}",
            result.output
        );
    }

    // --- standalone shell-operator detection (#92) -------------------

    #[tokio::test]
    async fn standalone_pipe_is_rejected_with_teaching_error() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        let tool = make_tool_fast();
        let err = tool
            .execute(
                &ctx,
                json!({
                    "command": ["grep", "-r", "foo", ".", "|", "head"],
                    "cwd": dir.path().to_string_lossy(),
                }),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidParameters(msg) => {
                assert!(msg.contains("does not run a shell"), "copy: {msg}");
                assert!(
                    msg.contains("file_write"),
                    "copy should point at file_write: {msg}"
                );
            }
            other => panic!("expected InvalidParameters, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn standalone_redirect_is_rejected_before_spawning() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        let tool = make_tool_fast();
        let err = tool
            .execute(
                &ctx,
                json!({
                    "command": ["echo", "hi", ">", "out.txt"],
                    "cwd": dir.path().to_string_lossy(),
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
        // Rejected before any process ran: nothing was created.
        assert!(!dir.path().join("out.txt").exists());
    }

    #[tokio::test]
    async fn standalone_and_chain_is_rejected() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        let tool = make_tool_fast();
        let err = tool
            .execute(
                &ctx,
                json!({
                    "command": ["make", "&&", "make", "test"],
                    "cwd": dir.path().to_string_lossy(),
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
    }

    #[tokio::test]
    async fn operator_char_inside_argument_is_allowed() {
        // Operator *characters* embedded in real arguments are
        // legitimate values, not standalone operators: echo must run
        // and print them unchanged.
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        let tool = make_tool_fast();
        let result = tool
            .execute(
                &ctx,
                json!({
                    "command": ["echo", ">out", "a|b", "x&&y"],
                    "cwd": dir.path().to_string_lossy(),
                }),
            )
            .await
            .unwrap();
        assert!(!result.is_error, "output: {}", result.output);
        assert!(result.output.contains(">out"));
        assert!(result.output.contains("a|b"));
        assert!(result.output.contains("x&&y"));
    }

    #[test]
    fn standalone_shell_operator_matches_whole_elements_only() {
        // Standalone operators are detected, anywhere in argv...
        assert_eq!(
            standalone_shell_operator(&["grep".into(), "|".into(), "wc".into()]),
            Some("|")
        );
        assert_eq!(
            standalone_shell_operator(&["a".into(), ">>".into()]),
            Some(">>")
        );
        assert_eq!(
            standalone_shell_operator(&["|".into()]),
            Some("|"),
            "an operator as the program itself is still caught"
        );
        // ...but operator characters inside an argument are not.
        assert_eq!(
            standalone_shell_operator(&["echo".into(), "a|b".into()]),
            None
        );
        assert_eq!(
            standalone_shell_operator(&["echo".into(), ">out".into()]),
            None
        );
        assert_eq!(
            standalone_shell_operator(&["echo".into(), "hello".into()]),
            None
        );
    }

    // ---- process-group teardown (#552, review finding A9) ----------

    /// `setsid` if it is on the child's PATH — the only way a test
    /// payload can put a descendant *outside* the child's process
    /// group now that the group is killed as one.
    fn setsid_path() -> Option<String> {
        DEFAULT_CHILD_PATH
            .split(':')
            .map(|dir| format!("{dir}/setsid"))
            .find(|path| std::path::Path::new(path).is_file())
    }

    /// A payload that publishes `<leader-pid> <background-pid>` to
    /// `pids` in the cwd, backgrounds a process, and then blocks. One
    /// `printf` so the file is never read half-written.
    const GROUP_PAYLOAD: &str = "sleep 999 & printf '%s %s\\n' \"$$\" \"$!\" > pids; sleep 999";

    /// The process group `pid` belongs to, or `None` if there is no
    /// such process. Answers for zombies too, so a `Some` here is not
    /// on its own proof that anything is running.
    fn pgid_of(pid: i32) -> Option<i32> {
        // SAFETY: `getpgid` reads scheduler bookkeeping for one pid and
        // reports failure through errno.
        let rc = unsafe { libc::getpgid(pid) };
        (rc >= 0).then_some(rc)
    }

    /// Wait for [`GROUP_PAYLOAD`] to publish its pids, and assert the
    /// child really leads a group of its own.
    ///
    /// This is the half that fails if `process_group(0)` is dropped:
    /// without it the child and its descendant sit in the *test
    /// process's* group, so every "is the group gone?" assertion below
    /// would pass for the wrong reason.
    async fn probe_group(pid_file: &std::path::Path) -> (i32, i32) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(text) = fs::read_to_string(pid_file) {
                let mut fields = text.split_whitespace();
                if let (Some(Ok(leader)), Some(Ok(background))) = (
                    fields.next().map(str::parse::<i32>),
                    fields.next().map(str::parse::<i32>),
                ) {
                    assert_eq!(
                        pgid_of(leader),
                        Some(leader),
                        "the exec child must lead its own process group"
                    );
                    assert_eq!(
                        pgid_of(background),
                        Some(leader),
                        "a process the command started must inherit that group"
                    );
                    return (leader, background);
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the payload never published its pids"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Poll until nothing — running or zombie — is left in the group.
    async fn assert_group_reaped(pgid: i32) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while reap::group_alive(pgid) {
            assert!(
                std::time::Instant::now() < deadline,
                "process group {pgid} outlived the exec call"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// #552 / A9: a timed-out call ends the whole process group. Before
    /// the fix, `bash -c '<payload> & sleep 999'` left `<payload>`
    /// running as the daemon's user indefinitely, invisible to the
    /// runtime, because only the direct child was killed.
    #[tokio::test]
    async fn timeout_kills_the_whole_process_group() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        let pid_file = dir.path().join("pids");
        let tool = ExecTool::with_config(ExecConfig {
            default_timeout: Duration::from_secs(1),
            ..fast_config()
        });

        let start = std::time::Instant::now();
        let (result, (leader, _background)) = tokio::join!(
            tool.execute(
                &ctx,
                json!({
                    "command": ["bash", "-c", GROUP_PAYLOAD],
                    "cwd": dir.path().to_string_lossy(),
                }),
            ),
            probe_group(&pid_file),
        );

        let result = result.expect_err("a deadline is a TimedOut error (#547)");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(4),
            "the timeout, the kill grace and the drain are all bounded, took {elapsed:?}"
        );
        let ToolError::TimedOut { output, .. } = &result else {
            panic!("expected TimedOut, got: {result}");
        };
        let output = output.as_deref().unwrap_or_default();
        assert!(
            output.contains("every process it started were killed"),
            "the model is told the whole tree went: {output}"
        );
        assert_group_reaped(leader).await;
    }

    /// The same kill has to happen when the tool's future is dropped
    /// from outside — the runner is gaining a host-side deadline
    /// (<https://github.com/bricef/factor-q/issues/547>), and
    /// `kill_on_drop` alone would reap only the direct child.
    #[tokio::test]
    async fn dropping_the_call_kills_the_whole_process_group() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        // Far longer than the outer deadline: the tool's own timeout
        // must not be what ends this call.
        let tool = ExecTool::with_config(ExecConfig {
            default_timeout: Duration::from_secs(300),
            max_timeout: Duration::from_secs(300),
            ..fast_config()
        });

        let pid_file = dir.path().join("pids");
        // `join!` drops the futures it owns as it returns, so the exec
        // future — and with it the group guard — is gone by the time
        // the assertions below run.
        let (outcome, (leader, _background)) = tokio::join!(
            tokio::time::timeout(
                Duration::from_millis(750),
                tool.execute(
                    &ctx,
                    json!({
                        "command": ["bash", "-c", GROUP_PAYLOAD],
                        "cwd": dir.path().to_string_lossy(),
                    }),
                )
            ),
            probe_group(&pid_file),
        );

        assert!(outcome.is_err(), "the outer deadline must fire first");
        assert_group_reaped(leader).await;
    }

    /// A tree that ignores `SIGTERM` is escalated to `SIGKILL` once the
    /// kill grace runs out, rather than outliving the call.
    #[tokio::test]
    async fn a_group_ignoring_sigterm_is_killed_after_the_grace() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        let tool = ExecTool::with_config(ExecConfig {
            default_timeout: Duration::from_secs(1),
            ..fast_config()
        });
        let pid_file = dir.path().join("pids");

        // `trap '' TERM` is inherited as SIG_IGN across the fork *and*
        // the exec, so nothing in this group answers SIGTERM. Only the
        // escalation to SIGKILL can end it.
        let payload = format!("trap '' TERM; {GROUP_PAYLOAD}");
        let start = std::time::Instant::now();
        let (result, (leader, _background)) = tokio::join!(
            tool.execute(
                &ctx,
                json!({
                    "command": ["bash", "-c", payload],
                    "cwd": dir.path().to_string_lossy(),
                }),
            ),
            probe_group(&pid_file),
        );

        let result = result.expect_err("a deadline is a TimedOut error (#547)");
        assert!(
            matches!(result, ToolError::TimedOut { .. }),
            "expected TimedOut, got: {result}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "the escalation is bounded by the kill grace"
        );
        assert_group_reaped(leader).await;
    }

    /// A command that exits on its own leaves the guard with nothing to
    /// kill: the guard disarms on the reap, so a descendant the agent
    /// deliberately daemonized keeps running and the result carries no
    /// teardown error.
    ///
    /// Asserted by outcome, not by liveness: the descendant only
    /// touches `alive` *after* the call has returned, so a group kill
    /// on the clean path would leave the file missing rather than
    /// racing a signal against a `getpgid`.
    #[tokio::test]
    async fn clean_exit_leaves_its_own_descendants_alone() {
        let dir = tempdir().unwrap();
        let sandbox = ToolSandbox::new().allow_exec_cwd(dir.path());
        let ctx = make_exec_ctx(&sandbox);
        let tool = make_tool_fast();

        let result = tool
            .execute(
                &ctx,
                json!({
                    "command": [
                        "bash",
                        "-c",
                        "sh -c 'sleep 1; touch alive' & printf '%s %s\\n' \"$$\" \"$!\" > pids",
                    ],
                    "cwd": dir.path().to_string_lossy(),
                }),
            )
            .await
            .unwrap();
        assert!(!result.is_error, "{}", result.output);
        assert!(result.output.contains("Exit code: 0"), "{}", result.output);

        let text = fs::read_to_string(dir.path().join("pids")).unwrap();
        let mut fields = text.split_whitespace();
        let leader: i32 = fields.next().unwrap().parse().unwrap();
        let background: i32 = fields.next().unwrap().parse().unwrap();
        assert_eq!(
            pgid_of(background),
            Some(leader),
            "the backgrounded process must be in the child's group — otherwise \
             surviving the call proves nothing"
        );

        let alive = dir.path().join("alive");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !alive.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "a descendant of a cleanly exited command must outlive the call"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        reap::signal_group(leader, libc::SIGKILL);
    }
}
