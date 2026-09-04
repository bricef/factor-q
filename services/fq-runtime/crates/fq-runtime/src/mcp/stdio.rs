//! How a stdio MCP server's child process is started (#541).
//!
//! The child gets a *constructed* environment, never the daemon's. The
//! daemon's environment holds provider keys, `GH_TOKEN`, the broker
//! token and whatever else the host exports, and a `command:` in an
//! agent definition must not inherit any of it. This mirrors the `exec`
//! built-in ([`fq_tools::builtin::exec`]), which has cleared its
//! environment since it existed; the two share one `PATH` baseline.
//!
//! What the child sees:
//!
//! - **`PATH`**: the pinned baseline `exec` uses, with the directory the
//!   command resolved to prepended. The command itself is resolved on
//!   the *daemon's* PATH, as it always was — so `command: npx` keeps
//!   working from an nvm, mise or CI-installed Node, whose `node` sits
//!   beside `npx` — but only that one directory is admitted, not the
//!   rest of the daemon's PATH.
//! - **exactly the declaration's `env:`** on top, which may override
//!   `PATH`. Nothing else: a server that needs `HOME` or a credential is
//!   given it in the definition, where a reader can see it.
//! - **a working directory of its own**, `<root>/<server>`, created on
//!   demand. Never the daemon's cwd, where a relative path in a server's
//!   arguments would otherwise land in the operator's project.
//! - **stderr piped into tracing** rather than inherited, so a server's
//!   diagnostics reach the daemon log carrying the server's name.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use fq_tools::builtin::DEFAULT_CHILD_PATH;
use rmcp::transport::TokioChildProcess;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::info;

use super::{McpError, McpServerConfig};

/// The root under which servers get their working directories when the
/// embedder names none — the daemon names `<state dir>/mcp`. Under the
/// temp dir rather than the process cwd, so even an unconfigured manager
/// never starts a server where the daemon happens to run.
pub(crate) fn default_server_root() -> PathBuf {
    std::env::temp_dir().join("factor-q-mcp")
}

/// The configured but unspawned command: program, arguments, environment
/// and working directory, exactly as the server will see them. Kept
/// apart from spawning so a test can run it and read back what it sees.
///
/// Fails if `command` is not on the daemon's PATH or `server_dir` cannot
/// be created.
pub(super) fn stdio_command(config: &McpServerConfig, server_dir: &Path) -> io::Result<Command> {
    let program = resolve_program(&config.command)?;
    std::fs::create_dir_all(server_dir)?;
    let mut cmd = Command::new(&program);
    cmd.args(&config.args)
        .env_clear()
        .env("PATH", child_path(&program))
        .envs(config.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .current_dir(server_dir);
    Ok(cmd)
}

/// Spawn the stdio transport for `config` with its working directory
/// under `root`, forwarding the child's stderr into tracing for as long
/// as the child lives.
pub(super) fn spawn_transport(
    config: &McpServerConfig,
    root: &Path,
) -> Result<TokioChildProcess, McpError> {
    let start_error = |reason: String| McpError::ServerStart {
        command: config.command.clone(),
        reason,
    };
    let cmd = stdio_command(config, &root.join(&config.name))
        .map_err(|err| start_error(err.to_string()))?;
    let (transport, stderr) = TokioChildProcess::builder(cmd)
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| start_error(err.to_string()))?;
    if let Some(stderr) = stderr {
        let server = config.name.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                info!(target: "mcp.server.stderr", %server, "{line}");
            }
        });
    }
    Ok(transport)
}

/// `PATH` for the child: the shared baseline, with the resolved
/// program's directory in front when it lies outside the baseline.
fn child_path(program: &Path) -> String {
    match program.parent() {
        Some(dir) if !DEFAULT_CHILD_PATH.split(':').any(|p| Path::new(p) == dir) => {
            format!("{}:{DEFAULT_CHILD_PATH}", dir.display())
        }
        _ => DEFAULT_CHILD_PATH.to_string(),
    }
}

/// Resolve `command` as a shell would, against the *daemon's* PATH: a
/// name containing `/` is a path and is made absolute (the child runs
/// in a different directory, so a relative one must be pinned now); a
/// bare name is searched for on PATH. The daemon's PATH is consulted
/// rather than the child's because the child's is derived from the
/// answer.
fn resolve_program(command: &str) -> io::Result<PathBuf> {
    if command.contains('/') {
        let path = std::path::absolute(command)?;
        return if is_executable(&path) {
            Ok(path)
        } else {
            Err(not_found(command))
        };
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(command))
        .find(|candidate| is_executable(candidate))
        .ok_or_else(|| not_found(command))
}

fn not_found(command: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("`{command}` was not found on the daemon's PATH"),
    )
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests;
