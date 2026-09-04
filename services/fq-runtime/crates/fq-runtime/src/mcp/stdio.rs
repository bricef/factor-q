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

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use fq_tools::builtin::DEFAULT_CHILD_PATH;
use rmcp::transport::TokioChildProcess;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tracing::info;

use super::{McpError, McpServerConfig, validate_server_name};

/// The root under which servers get their working directories when the
/// embedder names none. The daemon always names `<state dir>/mcp`
/// ([`super::McpClientManager::with_server_root`]); this default exists
/// for tests and in-process embedders. It is per process — under the
/// temp dir, never the process cwd — so two embedders never share a
/// directory and no fixed, predictable path is ever used.
pub(crate) fn default_server_root() -> PathBuf {
    std::env::temp_dir().join(format!("factor-q-mcp-{}", std::process::id()))
}

/// The longest stderr line forwarded whole. The rest of a longer line is
/// dropped and the line marked, so a server that streams a megabyte
/// without a newline cannot grow the daemon's memory by a megabyte.
const STDERR_LINE_CAP: usize = 8 * 1024;

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
    // The server's name is its directory name. Validate it here, before
    // any path is built or created — tool discovery validates it again
    // later, but by then `../x` would already have made a cwd outside
    // `root`. The charset (`[a-z0-9-]+`) admits no separator.
    validate_server_name(&config.name)?;
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
        tokio::spawn(forward_stderr(stderr, move |line| {
            info!(target: "mcp.server.stderr", %server, "{line}");
        }));
    }
    Ok(transport)
}

/// Forward `reader`'s lines to `emit` until EOF or an unrecoverable read
/// error. Bytes, not `str`: a line that is not UTF-8 is rendered lossily
/// and forwarding continues, where a `lines()` loop would have stopped at
/// the first bad byte and dropped the read end — leaving the child to
/// take `EPIPE`/`SIGPIPE` on every later write, with its diagnostics gone
/// for the rest of its life. A line past [`STDERR_LINE_CAP`] is cut there
/// and marked with how much was dropped; the bytes are consumed either
/// way, so the pipe never backs up.
pub(super) async fn forward_stderr<R: AsyncRead + Unpin>(reader: R, mut emit: impl FnMut(String)) {
    let mut reader = BufReader::new(reader);
    let mut line: Vec<u8> = Vec::new();
    let mut dropped = 0usize;
    loop {
        let buf = match reader.fill_buf().await {
            Ok(buf) => buf,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            // The pipe is gone (the child exited, or the handle broke):
            // nothing more will arrive, and the child cannot be hurt by
            // our leaving.
            Err(_) => break,
        };
        if buf.is_empty() {
            break; // EOF
        }
        let (take, complete) = match buf.iter().position(|&b| b == b'\n') {
            Some(at) => (at + 1, true),
            None => (buf.len(), false),
        };
        let content = if complete { take - 1 } else { take };
        let keep = content.min(STDERR_LINE_CAP - line.len());
        line.extend_from_slice(&buf[..keep]);
        dropped += content - keep;
        reader.consume(take);
        if complete {
            emit(render_line(&line, dropped));
            line.clear();
            dropped = 0;
        }
    }
    if !line.is_empty() || dropped > 0 {
        emit(render_line(&line, dropped)); // an unterminated last line
    }
}

fn render_line(line: &[u8], dropped: usize) -> String {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let mut text = String::from_utf8_lossy(line).into_owned();
    if dropped > 0 {
        text.push_str(&format!(" …[{dropped} more bytes dropped]"));
    }
    text
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

/// Resolve `command` as a shell would, against the *daemon's* PATH. The
/// daemon's PATH is consulted rather than the child's because the
/// child's is derived from the answer.
fn resolve_program(command: &str) -> io::Result<PathBuf> {
    resolve_program_in(command, std::env::var_os("PATH").unwrap_or_default())
}

/// [`resolve_program`] against an explicit `PATH` value. A name
/// containing `/` is a path; a bare name is searched for on `path`.
/// Either way the answer is made absolute before use: the child changes
/// directory before it execs, so a relative program — from a relative
/// `command:`, or from a relative or empty `PATH` entry such as `.` —
/// would resolve against the wrong directory there, and [`child_path`]
/// would admit a relative directory.
fn resolve_program_in(command: &str, path: impl AsRef<OsStr>) -> io::Result<PathBuf> {
    let found = if command.contains('/') {
        PathBuf::from(command)
    } else {
        std::env::split_paths(path.as_ref())
            .map(|dir| dir.join(command))
            .find(|candidate| is_executable(candidate))
            .ok_or_else(|| not_found(command))?
    };
    let program = std::path::absolute(&found)?;
    if is_executable(&program) {
        Ok(program)
    } else {
        Err(not_found(command))
    }
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
