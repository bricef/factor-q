//! The shared MCP servers: starting them at boot, draining what they
//! say afterwards, and dialling again the ones that did not answer
//! (ADR-0018, ADR-0020, #548).
//!
//! Split out of `daemon.rs` for room rather than for purity — but the
//! seam is a real one: every half is about the *other* processes the
//! daemon supervises, and none of it touches the runtime being
//! assembled around them.
//!
//! **Boot never blocks here.** The servers start concurrently, each
//! under its own start-up and discovery deadline, and one that fails is
//! recorded `unavailable` rather than holding the daemon. What that
//! costs is stated where it is paid: an agent that needs an unavailable
//! server is refused at dispatch by name (the runner's check), and
//! `fq doctor` names it with the reason and the next retry time.

use std::sync::Arc;

use fq_runtime::events::{Event, EventPayload};
use fq_runtime::{
    AgentRegistry, Config, EventBus, McpClientManager, McpServerConfig, McpServerStates,
    ProgressRegistry, ToolRegistry,
};
use uuid::Uuid;

/// The daemon's handle on its shared MCP servers.
///
/// The manager sits behind a mutex because two owners need it: the
/// daemon, for the handles it takes at boot and for shutdown, and the
/// retry loop, which starts servers for as long as the daemon runs. It
/// is locked briefly and rarely — a retry every backoff interval, and
/// once at shutdown — so there is nothing to contend over.
pub(crate) struct SharedServers {
    manager: Arc<tokio::sync::Mutex<McpClientManager>>,
    states: McpServerStates,
    /// Ends the retry loop. Signalled by [`shutdown`](Self::shutdown)
    /// before the manager is torn down, so a retry cannot start a child
    /// the shutdown has already walked past. `notify_one` rather than
    /// `notify_waiters`: the permit persists, so a retry that is
    /// mid-handshake still sees it on its next turn.
    stop: Arc<tokio::sync::Notify>,
    /// The servers that did not answer at boot, held until
    /// [`supervise`](Self::supervise) hands them to the retry loop.
    /// Kept here rather than passed around so `assemble` does not have
    /// to carry a second value between the two calls.
    pending: Vec<McpServerConfig>,
    /// Every shared server a loaded agent declares, ready or not. The
    /// supervisor needs the whole list, not just the failures: a server
    /// that is ready now can lose its connection later, and dialling it
    /// again means having its config.
    declared: Vec<McpServerConfig>,
    retry: Option<tokio::task::JoinHandle<()>>,
}

/// Build the tool registry and start every shared MCP server a loaded
/// agent names — all of them at once.
///
/// Grant-bearing servers are skipped: they run per-invocation, wired by
/// the runner (ADR-0018), never shared at daemon boot.
pub(crate) async fn start_shared_servers(
    registry: &AgentRegistry,
    config: &Config,
) -> (SharedServers, ToolRegistry) {
    let exec = config.tools.exec.to_exec_config();
    let mut manager = McpClientManager::with_server_root(config.state.directory.join("mcp"))
        .with_limits(config.mcp.to_limits());
    let declared = declared_shared_servers(registry);
    let outcomes = manager.start_shared_servers(declared.clone()).await;

    let mut tools = ToolRegistry::with_builtins_exec(exec);
    let mut unavailable = Vec::new();
    for outcome in outcomes {
        match outcome.outcome {
            Ok(mcp_tools) => {
                for tool in mcp_tools {
                    if let Err(error) = tools.register(tool) {
                        tracing::warn!(server = %outcome.server, %error, "refusing MCP tool registration");
                    }
                }
            }
            Err(error) => {
                tracing::warn!(
                    server = %outcome.server,
                    %error,
                    "MCP server unavailable; its tools are absent and the agents that \
                     declare it will be refused at dispatch until it comes up"
                );
                if let Some(config) = declared.iter().find(|c| c.name == outcome.server) {
                    unavailable.push(config.clone());
                }
            }
        }
    }

    let states = manager.states();
    (
        SharedServers {
            manager: Arc::new(tokio::sync::Mutex::new(manager)),
            states,
            stop: Arc::new(tokio::sync::Notify::new()),
            pending: unavailable,
            declared,
            retry: None,
        },
        tools,
    )
}

/// Every shared server a loaded agent declares, in registry order.
///
/// Deduplication is the manager's, on transport identity: two agents
/// naming one endpoint differently still share a connection, and two
/// endpoints sharing a name are not merged.
fn declared_shared_servers(registry: &AgentRegistry) -> Vec<McpServerConfig> {
    let mut declared = Vec::new();
    for loaded in registry.iter() {
        for decl in loaded.agent.mcp_servers() {
            if loaded.agent.grants_inbound_capability(&decl.server) {
                continue;
            }
            declared.push(McpServerConfig {
                name: decl.server.clone(),
                command: decl.command.clone().unwrap_or_default(),
                args: decl.args.clone(),
                env: decl.env.clone(),
                url: decl.url.clone(),
            });
        }
    }
    declared
}

impl SharedServers {
    /// The `server → state` table every health surface reads, and the
    /// one the runner consults before starting an invocation.
    pub(crate) fn states(&self) -> McpServerStates {
        self.states.clone()
    }

    /// The progress-correlation table (#605), shared into the runner's
    /// per-invocation manager so one table answers "is anything still
    /// moving?".
    pub(crate) async fn progress(&self) -> ProgressRegistry {
        self.manager.lock().await.progress()
    }

    /// A live read-only handle over the connected servers, for the
    /// runner's `static_resources` pins.
    pub(crate) async fn resource_reader(&self) -> fq_runtime::McpResourceReader {
        self.manager.lock().await.resource_reader()
    }

    /// Drain the shared servers' notification streams for the life of
    /// the daemon (ADR-0020), and dial the unavailable ones again until
    /// they answer (#548).
    ///
    /// Logs and progress fold into tracing; a `tools/list_changed`, and
    /// a server coming up on retry, install a rebuilt registry into the
    /// shared context so the *next* invocation picks it up. Publishing
    /// a server's log record onto the bus is fire-and-forget: a failed
    /// publish is logged and never blocks the drain.
    pub(crate) async fn supervise(
        &mut self,
        context: Arc<fq_runtime::ReducerContext>,
        bus: EventBus,
        runtime_id: Uuid,
        exec: fq_tools::builtin::ExecConfig,
    ) {
        let (channels, refresher) = {
            let mut manager = self.manager.lock().await;
            let channels = manager.take_notifications().await;
            (channels, manager.tool_refresher(exec))
        };
        let declared = std::mem::take(&mut self.declared);
        if declared.is_empty() {
            return; // no agent names a shared server: nothing to supervise
        }
        let (came_up_tx, came_up_rx) = tokio::sync::mpsc::unbounded_channel();
        let (gone_tx, gone_rx) = tokio::sync::mpsc::unbounded_channel();
        // Always spawned, even with every server ready: it is the only
        // thing watching for one of them losing its connection.
        self.retry = Some(tokio::spawn(fq_runtime::mcp::retry_unavailable(
            Arc::clone(&self.manager),
            std::mem::take(&mut self.pending),
            declared,
            came_up_tx,
            gone_rx,
            Arc::clone(&self.stop),
        )));
        tokio::spawn(fq_runtime::mcp::drain_server_notifications(
            channels,
            came_up_rx,
            gone_tx,
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

    /// Stop the retry loop, then shut every running server down.
    ///
    /// In that order: a retry that started a child after the manager
    /// had been walked would leave a process nothing owns.
    pub(crate) async fn shutdown(self) {
        self.stop.notify_one();
        if let Some(retry) = self.retry
            && let Err(err) = retry.await
        {
            tracing::warn!(error = %err, "MCP retry task did not stop cleanly");
        }
        self.manager.lock().await.shutdown().await;
    }
}
