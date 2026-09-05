//! The identifier rules for the provider-visible tool namespace.
//!
//! Every tool an MCP server advertises reaches the LLM as
//! `<server>__<tool>`, so a server's id and the composed name are both
//! constrained by what providers accept in a tool name. Both rules live
//! here because they are one rule read from two ends: the charset that
//! makes `__` an unambiguous separator, and the length bound the
//! composition has to fit.

use super::McpError;

/// MCP server ids form the namespace in provider-visible tool names
/// (`<server>__<tool>`, #177). The charset excludes `_` so the first
/// `__` in a canonical name unambiguously splits namespace from tool,
/// and the whole name stays inside provider tool-name rules
/// (`[a-zA-Z0-9_-]`, e.g. Anthropic's).
pub(super) fn validate_server_name(name: &str) -> Result<(), McpError> {
    if name == "builtin" {
        return Err(McpError::ToolDiscovery {
            command: name.to_string(),
            reason: "server id 'builtin' is reserved for runtime tools".to_string(),
        });
    }
    if name.is_empty()
        || name.len() > 48
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(McpError::ToolDiscovery {
            command: name.to_string(),
            reason: "server id must match [a-z0-9-]+ and be at most 48 characters".to_string(),
        });
    }
    Ok(())
}

/// Compose the canonical, provider-visible name for a server's tool and
/// enforce the 64-character combined bound (the strictest provider
/// tool-name length, Anthropic's). Failing one tool fails the server's
/// discovery loudly rather than silently offering a partial tool set.
pub(super) fn namespaced_tool_name(
    server_name: &str,
    remote_name: &str,
) -> Result<String, McpError> {
    let name = format!("{server_name}__{remote_name}");
    if name.len() > 64 {
        return Err(McpError::ToolDiscovery {
            command: server_name.to_string(),
            reason: format!("namespaced tool name '{name}' exceeds 64 characters"),
        });
    }
    Ok(name)
}

#[cfg(test)]
mod tests;
