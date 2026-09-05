//! Cheap, cloneable handles over a manager's connected servers.
//!
//! [`McpClientManager`](super::McpClientManager) owns the servers and
//! needs `&mut` to start or stop one, which makes it a poor thing to
//! share. These handles carry only the client `Arc`s, so the runner can
//! read resources and the daemon's notification drain can rebuild the
//! shared registry without holding the manager's lifecycle.

use std::collections::HashMap;
use std::sync::Arc;

use fq_tools::builtin::ExecConfig;
use rmcp::model::{ReadResourceRequestParams, ReadResourceResult};
use tracing::warn;

use crate::tools::ToolRegistry;

use super::{McpClient, McpClientManager, McpError};

/// A cheap, cloneable read-only handle over a manager's connected
/// servers for reading resources, without the manager's `&mut`
/// lifecycle. [`ReducerContext`](crate::ReducerContext) holds one so
/// the runner can read `static_resources` pins at invocation start
/// while `main` keeps the manager for graceful shutdown.
#[derive(Clone, Default)]
pub struct McpResourceReader {
    pub(super) clients: HashMap<String, Arc<McpClient>>,
}

impl McpResourceReader {
    /// Read a resource by `(server, uri)`.
    pub async fn read_resource(
        &self,
        server: &str,
        uri: &str,
    ) -> Result<ReadResourceResult, McpError> {
        let client = self
            .clients
            .get(server)
            .ok_or_else(|| McpError::UnknownServer {
                name: server.to_string(),
            })?;
        client
            .read_resource(ReadResourceRequestParams::new(uri))
            .await
            .map_err(|err| McpError::ResourceOp {
                server: server.to_string(),
                reason: err.to_string(),
            })
    }
}

/// A cheap, cloneable handle for re-discovering connected servers'
/// tools (ADR-0020), without the manager's `&mut` lifecycle. The
/// daemon's notification drain holds one and rebuilds the shared
/// registry when a server signals `tools/list_changed`.
#[derive(Clone, Default)]
pub struct McpToolRefresher {
    pub(super) clients: Vec<(String, Arc<McpClient>)>,
    /// The `[tools.exec]` timeouts to re-apply to the `exec` built-in on
    /// every rebuild, so a refresh never silently reverts to the crate
    /// defaults.
    pub(super) exec_config: ExecConfig,
}

impl McpToolRefresher {
    /// Rebuild the full shared registry: built-ins plus every
    /// connected server's currently-advertised tools. The registry is
    /// append-only, so rebuild-from-scratch *is* the refresh
    /// operation. A server whose re-discovery fails contributes no
    /// tools (its calls would fail anyway) and is logged.
    pub async fn rebuild_registry(&self) -> ToolRegistry {
        let mut registry = ToolRegistry::with_builtins_exec(self.exec_config.clone());
        for (name, client) in &self.clients {
            match McpClientManager::discover_tools(client, name).await {
                Ok((tools, _)) => {
                    for tool in tools {
                        if let Err(error) = registry.register(tool) {
                            warn!(server = %name, %error, "refusing MCP tool registration");
                        }
                    }
                }
                Err(err) => {
                    warn!(
                        server = %name,
                        error = %err,
                        "tool re-discovery failed during refresh; this server's \
                         tools are absent from the rebuilt registry"
                    );
                }
            }
        }
        registry
    }
}
