//! How an MCP server is described, and the identity that description
//! carries: [`McpServerConfig`] is what a caller hands
//! [`McpClientManager`](super::McpClientManager) to start a server, and
//! [`SharedServerKey`] is the part of it that says which server it *is*
//! — what a shared server is deduplicated on.

use super::McpError;

/// Configuration for an MCP server: a stdio child process (`command`),
/// or — when `url` is set — a remote server reached over the Streamable
/// HTTP transport (the 2025-11-25 spec remote transport).
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    /// Human-readable name for logging.
    pub name: String,
    /// Executable to spawn (stdio transport).
    pub command: String,
    /// Command-line arguments (stdio transport).
    pub args: Vec<String>,
    /// Environment variables to set on the child process (stdio
    /// transport).
    pub env: Vec<(String, String)>,
    /// When set, the server is reached over the Streamable HTTP remote
    /// transport at this URL instead of a stdio child process;
    /// `command` / `args` / `env` are then unused.
    pub url: Option<String>,
}

/// What a shared MCP server is deduplicated on: the transport target it
/// would actually connect to.
///
/// Sharing exists so one server declared by several agents costs one
/// connection, so the identity has to be the thing that makes two
/// declarations *the same server* — the process a stdio config would
/// spawn, or the endpoint a remote config would dial.
///
/// Deliberately not the declared `name`, which is per-agent vocabulary:
/// two agents may name one endpoint differently and should still share
/// it, and two different endpoints may share a name and must not be
/// merged. Nor `(command, args)` alone, which is blind to the remote
/// transport — `command` is empty for every `url:` server, so they all
/// collided on one key and only the first started (#512).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum SharedServerKey {
    /// A stdio child process, identified by what gets spawned.
    Stdio { command: String, args: Vec<String> },
    /// A remote server, identified by its Streamable HTTP endpoint.
    Remote { url: String },
}

impl SharedServerKey {
    /// Derive the key from a config, mirroring the transport selection
    /// in `McpClientManager::start_inner` exactly — `url` wins,
    /// otherwise a non-empty `command` is the stdio transport — so the
    /// key can never call two configs the same server when they would
    /// start different things.
    ///
    /// A config naming neither has no transport and so no identity;
    /// that is an error, not a shared bucket every such config silently
    /// falls into. Definitions cannot express it (the parser requires
    /// exactly one of `command`/`url`), but [`McpServerConfig`] is
    /// public and constructible directly.
    pub(super) fn from_config(config: &McpServerConfig) -> Result<Self, McpError> {
        match (&config.url, config.command.as_str()) {
            (Some(url), _) => Ok(Self::Remote { url: url.clone() }),
            (None, "") => Err(McpError::UndeclaredTransport {
                name: config.name.clone(),
            }),
            (None, command) => Ok(Self::Stdio {
                command: command.to_string(),
                args: config.args.clone(),
            }),
        }
    }

    /// The transport target, for logs — the endpoint or the command.
    pub(super) fn target(&self) -> &str {
        match self {
            Self::Stdio { command, .. } => command,
            Self::Remote { url } => url,
        }
    }
}
