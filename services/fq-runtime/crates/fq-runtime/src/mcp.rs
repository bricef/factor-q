//! MCP (Model Context Protocol) client support.
//!
//! Provides [`McpTool`], which adapts a tool from an external MCP server
//! into the [`fq_tools::Tool`] trait so it can be registered in the
//! [`ToolRegistry`](crate::tools::ToolRegistry) alongside built-in
//! tools.
//!
//! [`McpClientManager`] owns the lifecycle of MCP server child processes:
//! starting them, discovering their tools, and shutting them down.
//!
//! The module is one connection's worth of machinery split by
//! responsibility (#191); this root is the public face, and everything
//! it re-exports keeps the path it always had:
//!
//! | module | what lives there |
//! | --- | --- |
//! | [`handler`] | the rmcp client handler and the capabilities it advertises |
//! | [`manager`] | server lifecycle: start, discover, call, shut down |
//! | [`tools`] | the tool adapter, [`McpTool`] |
//! | [`resources`] | synthesized resource tools and their rendering |
//! | [`roots`] | workspace roots (ADR-0018) and the handle that updates them |
//! | [`notifications`] | the out-of-band notification type and the drain loop |
//! | [`handles`] | cloneable reader / refresher handles over running servers |
//! | [`prompt_convert`] | the rmcp → factor-q prompt boundary |
//! | [`naming`] | the `<server>__<tool>` identifier rules |
//! | [`progress`] | per-request progress tokens |
//! | [`server_config`] | how a server is described, and what it is deduplicated on |
//! | [`stdio`] | how a stdio server's child process is started |

use rmcp::service::{RoleClient, RunningService};

mod handler;
mod handles;
mod manager;
mod naming;
mod notifications;
mod progress;
mod prompt_convert;
mod resources;
mod roots;
mod server_config;
mod stdio;
mod tools;

#[cfg(test)]
mod mock;

pub use handler::{AdvertisedCapabilities, FactorQClientHandler, ServerRequest};
pub use handles::{McpResourceReader, McpToolRefresher};
pub use manager::McpClientManager;
pub use notifications::{ServerNotification, drain_server_notifications};
pub use resources::{McpResourceTool, render_resource_contents};
pub use roots::{RootsHandle, advertised_roots_from_tool_sandbox, roots_from_tool_sandbox};
pub use server_config::McpServerConfig;
pub use tools::McpTool;

pub(crate) use handler::elicitation_decline;
pub(crate) use stdio::default_server_root;

/// Errors from MCP server lifecycle and tool calls.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("failed to start MCP server '{command}': {reason}")]
    ServerStart { command: String, reason: String },

    #[error("tool discovery failed for '{command}': {reason}")]
    ToolDiscovery { command: String, reason: String },

    #[error("tool call to '{tool_name}' failed: {reason}")]
    ToolCall { tool_name: String, reason: String },

    #[error("no MCP server named '{name}' is running")]
    UnknownServer { name: String },

    #[error(
        "MCP server '{name}' declares no transport: set `command` (stdio) or `url` (Streamable HTTP)"
    )]
    UndeclaredTransport { name: String },

    #[error("resource operation on '{server}' failed: {reason}")]
    ResourceOp { server: String, reason: String },

    #[error("prompt operation on '{server}' failed: {reason}")]
    PromptOp { server: String, reason: String },

    #[error("roots operation on '{server}' failed: {reason}")]
    RootsOp { server: String, reason: String },
}

// Type alias for the concrete client handle we store.
type McpClient = RunningService<RoleClient, FactorQClientHandler>;
