//! The shared MCP servers: starting them at boot, and draining what
//! they say afterwards (ADR-0018, ADR-0020).
//!
//! Split out of `daemon.rs` for room rather than for purity — but the
//! seam is a real one: both halves are about the *other* processes the
//! daemon supervises, and neither touches the runtime being assembled
//! around them.

use std::sync::Arc;

use fq_runtime::events::{Event, EventPayload};
use fq_runtime::{AgentRegistry, EventBus, McpClientManager, McpServerConfig, ToolRegistry};
use uuid::Uuid;

/// Build the tool registry: built-ins, plus the tools of every shared
/// MCP server named by a loaded agent.
///
/// Grant-bearing servers are skipped: they run per-invocation, wired by
/// the runner (ADR-0018), never shared at daemon boot.
///
/// A server that will not start is a warning, not a failure. Its tools
/// are unavailable and the agents that wanted them fail at invocation
/// time with a missing tool, which is a smaller blast radius than a
/// daemon that will not boot because one remote server is down.
pub(crate) async fn start_shared_servers(
    registry: &AgentRegistry,
    mcp_manager: &mut McpClientManager,
    exec: fq_tools::builtin::ExecConfig,
) -> ToolRegistry {
    let mut tools = ToolRegistry::with_builtins_exec(exec);
    for loaded in registry.iter() {
        for decl in loaded.agent.mcp_servers() {
            if loaded.agent.grants_inbound_capability(&decl.server) {
                continue;
            }
            let config = McpServerConfig {
                name: decl.server.clone(),
                command: decl.command.clone().unwrap_or_default(),
                args: decl.args.clone(),
                env: decl.env.clone(),
                url: decl.url.clone(),
            };
            match mcp_manager.start_server(config).await {
                Ok(mcp_tools) => {
                    for tool in mcp_tools {
                        if let Err(error) = tools.register(tool) {
                            tracing::warn!(server = %decl.server, %error, "refusing MCP tool registration");
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        server = %decl.server,
                        agent = %loaded.agent.id(),
                        error = %err,
                        "failed to start MCP server, its tools will be unavailable"
                    );
                }
            }
        }
    }
    tools
}

/// Drain the shared servers' notification streams for the life of the
/// daemon (ADR-0020): logs and progress fold into tracing, and a
/// `tools/list_changed` installs a rebuilt registry into the shared
/// context so the *next* invocation picks it up.
///
/// Publishing a server's log record onto the bus is fire-and-forget: a
/// failed publish is logged and never blocks the drain.
pub(crate) fn drain_notifications(
    mcp_manager: &mut McpClientManager,
    context: Arc<fq_runtime::ReducerContext>,
    bus: EventBus,
    runtime_id: Uuid,
    exec: fq_tools::builtin::ExecConfig,
    channels: Vec<(
        String,
        tokio::sync::mpsc::UnboundedReceiver<fq_runtime::mcp::ServerNotification>,
    )>,
) {
    let refresher = mcp_manager.tool_refresher(exec);
    tokio::spawn(fq_runtime::mcp::drain_server_notifications(
        channels,
        refresher,
        move |registry| context.install_tools(Arc::new(registry)),
        move |server, level, logger, data| {
            let bus = bus.clone();
            let event = Event::system(
                runtime_id,
                EventPayload::McpServerLog(fq_runtime::events::McpServerLogPayload {
                    server,
                    level,
                    logger,
                    data,
                }),
            );
            tokio::spawn(async move {
                if let Err(err) = bus.publish(&event).await {
                    tracing::warn!(error = %err, "failed to publish MCP server log event");
                }
            });
        },
    ));
}
