//! Server lifecycle: starting MCP servers, discovering what they
//! advertise, calling into them, and tearing them down.
//!
//! [`McpClientManager`] is the one owner of every running server. It
//! deduplicates shared servers by transport identity, wraps each
//! discovered tool ([`McpTool`](super::McpTool)) and each synthesized
//! resource tool ([`McpResourceTool`](super::McpResourceTool)), exposes
//! the per-server request surface (resources, prompts, logging,
//! cancellable tool calls), and owns the graceful-shutdown ordering the
//! stdio transport needs (#25).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use fq_tools::Tool;
use fq_tools::builtin::ExecConfig;
use rmcp::model::{Root, ServerCapabilities};
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};

use super::discovery;
use super::lifecycle::{self, Connected, McpServerStates, SharedServerStart, StartContext};
use super::limits::McpLimits;
use super::progress::ProgressRegistry;
use super::server_config::SharedServerKey;
use super::{
    AdvertisedCapabilities, McpClient, McpError, McpResourceReader, McpServerConfig,
    McpToolRefresher, RootsHandle, ServerNotification, ServerRequest, default_server_root,
};

/// The connected clients, by server name, shared with every handle a
/// manager hands out.
///
/// Shared rather than snapshotted because a handle outlives the call
/// that made it: the daemon takes its resource reader and tool
/// refresher at boot, and a server that only comes up later — on the
/// retry loop — has to be visible through both of them without either
/// being rebuilt. A `Vec` rather than a map because it is read in
/// declaration order and never has more entries than a definition
/// directory declares servers.
pub(super) type SharedClients = Arc<RwLock<Vec<(String, Arc<McpClient>)>>>;

/// Tracks a running MCP server and its client handle.
pub(super) struct RunningServer {
    pub(super) name: String,
    pub(super) client: Arc<McpClient>,
    pub(super) tool_names: Vec<String>,
    /// Receiver for resource notifications the handler forwards.
    pub(super) notifications: Mutex<mpsc::UnboundedReceiver<ServerNotification>>,
}

/// Manages the lifecycle of MCP server child processes.
///
/// Starts servers, discovers their tools (wrapping each as an
/// [`McpTool`](super::McpTool)) and provides graceful shutdown.
/// Deduplicates servers by transport
/// identity — the stdio process spawned, or the remote endpoint dialled
/// — so the same server declared by multiple agents starts only once.
pub struct McpClientManager {
    pub(super) servers: Vec<RunningServer>,
    /// Transport identities already started, to deduplicate, each
    /// against the name it was started under. The name is what
    /// [`forget`](Self::forget) removes by: a server whose connection
    /// died has to leave the deduplication set, or the retry that would
    /// bring it back is skipped as a duplicate of itself.
    started: HashMap<SharedServerKey, String>,
    /// Root of the stdio servers' working directories,
    /// `<root>/<server>` (see [`stdio`](super::stdio)).
    server_root: std::path::PathBuf,
    /// `(server, rmcp progress token) → the invocation and tool call
    /// that issued the request` (#605). Handed to every handler (which
    /// routes inbound `notifications/progress` through it) and to every
    /// [`McpTool`] this manager builds (which registers and clears
    /// entries), so both ends of the correlation share one table.
    /// `pub(super)` for the request surface, which attaches every
    /// outbound `tools/call` to it.
    pub(super) progress: ProgressRegistry,
    /// What a start is allowed to cost and what discovery is allowed to
    /// return — `[mcp]` in `fqd.toml` (#548).
    limits: McpLimits,
    /// `server → starting | ready | unavailable`, read by every health
    /// surface and by the dispatch check that refuses an agent whose
    /// server is down (#548).
    states: McpServerStates,
    /// Live view of the connected clients, handed to every
    /// [`McpResourceReader`] and [`McpToolRefresher`] this manager
    /// makes.
    clients: SharedClients,
}

impl Default for McpClientManager {
    fn default() -> Self {
        Self::new()
    }
}

impl McpClientManager {
    /// Stdio servers start under the default root (the temp dir, never the cwd).
    pub fn new() -> Self {
        Self::with_server_root(default_server_root())
    }

    /// Stdio servers start in `<server_root>/<server>`, created on demand (#541).
    pub fn with_server_root(server_root: std::path::PathBuf) -> Self {
        Self {
            servers: Vec::new(),
            started: HashMap::new(),
            server_root,
            progress: ProgressRegistry::default(),
            limits: McpLimits::default(),
            states: McpServerStates::default(),
            clients: SharedClients::default(),
        }
    }

    /// Apply an operator's `[mcp]` bounds instead of the defaults. The
    /// daemon passes `config.mcp.to_limits()`; a manager built without
    /// one runs on the same numbers the shipped config documents, which
    /// is what keeps a test or the sim honest about production
    /// behaviour.
    pub fn with_limits(mut self, limits: McpLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The bounds this manager applies.
    pub fn limits(&self) -> &McpLimits {
        &self.limits
    }

    /// The `server → state` table. Cheap to clone; `fq doctor`,
    /// `control.status` and the runner's dispatch check all read this
    /// one.
    ///
    /// A grant-bearing server is deliberately absent from the daemon's
    /// table: it runs per-invocation (ADR-0018) under a manager of its
    /// own, so "unavailable" there would be a verdict about one
    /// invocation reported as a standing fact about the daemon.
    pub fn states(&self) -> McpServerStates {
        self.states.clone()
    }

    /// The `(server, token) → call` table this manager's servers report
    /// progress through (#605). Cheap to clone; a stuck-call detector
    /// reads [`in_flight`](ProgressRegistry::in_flight) for each call's
    /// last sign of life.
    pub fn progress(&self) -> ProgressRegistry {
        self.progress.clone()
    }

    /// Report progress into an existing table instead of a fresh one.
    ///
    /// The runner builds a second, short-lived manager per invocation
    /// for the grant-bearing servers that run as their own process
    /// (ADR-0018). Left to mint its own table, those servers' calls
    /// would be invisible to the daemon's liveness surface — and they
    /// are precisely the long-running ones, since a server granted
    /// sampling is doing work worth reporting progress on. One table
    /// across both kinds of manager is what makes
    /// [`progress`](Self::progress) an answer to "is anything still
    /// moving?" rather than "is anything shared still moving?".
    pub fn sharing_progress(mut self, progress: ProgressRegistry) -> Self {
        self.progress = progress;
        self
    }

    /// Start an MCP server, discover its tools, and return them as
    /// `Arc<dyn Tool>` values ready for registration in a [`ToolRegistry`](crate::tools::ToolRegistry).
    ///
    /// If a server with the same transport identity — the stdio process
    /// spawned, or the remote endpoint dialled — has already been started,
    /// this is a no-op returning an empty vec (its tools were registered
    /// on the first call). A config declaring no transport at all is
    /// unstartable, and errors rather than deduplicating.
    pub async fn start_server(
        &mut self,
        config: McpServerConfig,
    ) -> Result<Vec<Arc<dyn Tool>>, McpError> {
        let key = SharedServerKey::from_config(&config)?;
        if self.started.contains_key(&key) {
            debug!(
                server = %config.name,
                target = %key.target(),
                "MCP server already started, skipping duplicate"
            );
            return Ok(Vec::new());
        }
        let name = config.name.clone();
        self.states.starting(&name);
        // Shared servers are tool-only: advertise no inbound capabilities
        // (a grant-bearing server runs per-invocation instead — ADR-0018).
        let started = self
            .start_inner(config, None, Vec::new(), AdvertisedCapabilities::none())
            .await;
        match started {
            Ok((tools, _roots)) => {
                self.states.ready(&name, tools.len() as u32);
                self.started.insert(key, name.clone());
                Ok(tools)
            }
            Err(err) => {
                // Not inserted into `started`: an unavailable server has
                // no connection to deduplicate against, and the retry
                // loop has to be able to dial the same transport again.
                self.states.unavailable(&name, err.to_string(), 1, None);
                Err(err)
            }
        }
    }

    /// Start every shared server at once, each under its own start-up
    /// and discovery deadline (#548).
    ///
    /// This is what makes boot bounded by the slowest server rather
    /// than by the sum of all of them, and what keeps one unresponsive
    /// server from holding the daemon at startup with every agent down.
    /// A server that fails is
    /// [`Unavailable`](super::McpServerState::Unavailable)
    /// with its reason recorded; the returned outcome carries the same
    /// error so the caller can log it against the agent that declared
    /// it.
    ///
    /// Deduplication happens up front, on the transport identity, so
    /// two agents naming one endpoint still dial it once — the
    /// concurrent starts cannot race each other into two connections.
    pub async fn start_shared_servers(
        &mut self,
        configs: Vec<McpServerConfig>,
    ) -> Vec<SharedServerStart> {
        self.dial_round(configs, 1).await
    }

    /// Dial a set of servers that failed earlier, concurrently, as
    /// attempt number `attempt` — the boot attempt counting as 1.
    ///
    /// The count is the caller's because only the retry loop knows it,
    /// and every health surface quotes it: a table that said "1
    /// attempt(s)" for ever would tell an operator the daemon had
    /// stopped trying when it had not.
    pub async fn retry_shared_servers(
        &mut self,
        configs: Vec<McpServerConfig>,
        attempt: u32,
    ) -> Vec<SharedServerStart> {
        self.dial_round(configs, attempt).await
    }

    /// One concurrent dial of `configs`, recording each outcome against
    /// `attempt`.
    async fn dial_round(
        &mut self,
        configs: Vec<McpServerConfig>,
        attempt: u32,
    ) -> Vec<SharedServerStart> {
        let mut outcomes = Vec::with_capacity(configs.len());
        let mut unique = Vec::with_capacity(configs.len());
        for config in configs {
            let server = config.name.clone();
            match SharedServerKey::from_config(&config) {
                Err(err) => outcomes.push(SharedServerStart {
                    server,
                    outcome: Err(err),
                }),
                Ok(key) if self.started.insert(key.clone(), server.clone()).is_some() => {
                    debug!(server = %server, "MCP server already started, skipping duplicate");
                    outcomes.push(SharedServerStart {
                        server,
                        outcome: Ok(Vec::new()),
                    });
                }
                Ok(_) => {
                    self.states.starting(&server);
                    unique.push(config);
                }
            }
        }

        let ctx = StartContext {
            server_root: &self.server_root,
            progress: &self.progress,
            limits: &self.limits,
        };
        let started = futures::future::join_all(unique.into_iter().map(|config| async {
            let connected = lifecycle::connect(
                &config,
                &ctx,
                None,
                Vec::new(),
                AdvertisedCapabilities::none(),
            )
            .await;
            (config, connected)
        }))
        .await;

        for (config, connected) in started {
            match connected {
                Ok(Connected { server, tools, .. }) => {
                    self.states.ready(&config.name, tools.len() as u32);
                    outcomes.push(SharedServerStart {
                        server: config.name,
                        outcome: Ok(self.register(server, tools)),
                    });
                }
                Err(err) => {
                    // The identity stays out of `started` so the retry
                    // loop can dial the same transport again.
                    if let Ok(key) = SharedServerKey::from_config(&config) {
                        self.started.remove(&key);
                    }
                    self.states
                        .unavailable(&config.name, err.to_string(), attempt, None);
                    outcomes.push(SharedServerStart {
                        server: config.name,
                        outcome: Err(err),
                    });
                }
            }
        }
        outcomes
    }

    /// Drop a server this manager can no longer talk to: its
    /// connection ended, so its client, its place in the deduplication
    /// set and its `ready` state all have to go.
    ///
    /// Without this a transport that dies *after* boot — a stdio child
    /// that exited, a line past the cap, a remote endpoint that closed
    /// — left the table saying `Ready`, so every health surface stayed
    /// green while every call through it failed and nothing retried
    /// it. Returns whether this manager was holding it.
    ///
    /// The tools it contributed stay in the registry until the next
    /// rebuild. That is deliberate rather than an omission: an agent
    /// declaring this server is refused at dispatch from here on, so
    /// nothing reaches those tools, and a rebuild costs a discovery
    /// round-trip against every *other* server for no gain.
    pub fn forget(&mut self, server: &str) -> bool {
        let held = self.servers.iter().any(|s| s.name == server);
        if !held {
            return false;
        }
        self.servers.retain(|s| s.name != server);
        self.started.retain(|_, name| name != server);
        self.clients
            .write()
            .expect("MCP client view poisoned")
            .retain(|(name, _)| name != server);
        self.states.unavailable(
            server,
            "the connection to this server ended".to_string(),
            1,
            None,
        );
        true
    }

    /// Take ownership of a connected server: record its client in the
    /// live handle view and keep the [`RunningServer`] for shutdown.
    /// Returns `tools` unchanged, so a caller can register and forward
    /// in one expression.
    fn register(&mut self, server: RunningServer, tools: Vec<Arc<dyn Tool>>) -> Vec<Arc<dyn Tool>> {
        self.clients
            .write()
            .expect("MCP client view poisoned")
            .push((server.name.clone(), Arc::clone(&server.client)));
        self.servers.push(server);
        tools
    }

    /// Start a *per-invocation* MCP server instance with a wired
    /// server-initiated request channel and advertised `roots`
    /// (ADR-0018).
    ///
    /// Unlike [`Self::start_server`], this never deduplicates: a server
    /// granted an inbound capability (sampling, elicitation, roots)
    /// runs as its own child process per invocation, so its
    /// server-initiated requests attribute to the right invocation's
    /// budget, grant, and event chain. Returns the discovered tools,
    /// the receiver the runner services in its `select!` loop, and a
    /// [`RootsHandle`] for updating the advertised roots. Pass empty
    /// `roots` when the agent grants none.
    pub async fn start_server_with_requests(
        &mut self,
        config: McpServerConfig,
        roots: Vec<Root>,
        capabilities: AdvertisedCapabilities,
    ) -> Result<
        (
            Vec<Arc<dyn Tool>>,
            mpsc::UnboundedReceiver<ServerRequest>,
            RootsHandle,
        ),
        McpError,
    > {
        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let (tools, roots_handle) = self
            .start_inner(config, Some(req_tx), roots, capabilities)
            .await?;
        Ok((tools, req_rx, roots_handle))
    }

    /// Shared start path: dial the server under the start-up deadline,
    /// discover its tools under the discovery deadline, and register
    /// the [`RunningServer`] ([`lifecycle::connect`] does the first two).
    /// `server_request_tx` wires the per-invocation sampling /
    /// elicitation bridge; `None` leaves the server tool-only (inbound
    /// requests decline). `roots` seeds the advertised workspace.
    /// Deduplication is the caller's concern. Returns the tools and a
    /// [`RootsHandle`] over the (possibly empty) advertised roots.
    async fn start_inner(
        &mut self,
        config: McpServerConfig,
        server_request_tx: Option<mpsc::UnboundedSender<ServerRequest>>,
        roots: Vec<Root>,
        capabilities: AdvertisedCapabilities,
    ) -> Result<(Vec<Arc<dyn Tool>>, RootsHandle), McpError> {
        let ctx = StartContext {
            server_root: &self.server_root,
            progress: &self.progress,
            limits: &self.limits,
        };
        let Connected {
            server,
            tools,
            roots,
        } = lifecycle::connect(&config, &ctx, server_request_tx, roots, capabilities).await?;
        Ok((self.register(server, tools), roots))
    }

    /// Re-discover a server's tools and refresh the cached tool-name
    /// list, reacting to `notifications/tools/list_changed` (Step 7).
    /// Returns the current tool set so the caller can re-register it in
    /// its [`ToolRegistry`](crate::tools::ToolRegistry) rather than
    /// serving the stale set discovered at startup. Resources and
    /// prompts are fetched on-demand (never cached), so they need no
    /// refresh.
    pub async fn refresh_tools(&mut self, server: &str) -> Result<Vec<Arc<dyn Tool>>, McpError> {
        let idx = self
            .servers
            .iter()
            .position(|s| s.name == server)
            .ok_or_else(|| McpError::UnknownServer {
                name: server.to_string(),
            })?;
        let client = Arc::clone(&self.servers[idx].client);
        let (tools, tool_names) =
            discovery::discover_tools(&client, server, &self.progress, &self.limits).await?;
        self.servers[idx].tool_names = tool_names;
        Ok(tools)
    }

    /// The capabilities a started server advertised during the initialize
    /// handshake, looked up by server name. `None` if no server with that
    /// name is running or the handshake produced no peer info.
    pub fn server_capabilities(&self, name: &str) -> Option<ServerCapabilities> {
        self.servers
            .iter()
            .find(|server| server.name == name)
            .and_then(|server| server.client.peer_info())
            .map(|info| info.capabilities.clone())
    }

    /// A cloneable read-only handle for reading resources from the
    /// currently-running servers — used to inject `static_resources`
    /// at invocation start without sharing the manager's lifecycle.
    pub fn resource_reader(&self) -> McpResourceReader {
        McpResourceReader {
            clients: Arc::clone(&self.clients),
        }
    }

    /// A cloneable handle for re-discovering the running servers'
    /// tools — used by the daemon's notification drain to rebuild the
    /// shared registry on `tools/list_changed` (ADR-0020) without
    /// sharing the manager's `&mut` lifecycle (same pattern as
    /// [`resource_reader`](Self::resource_reader)).
    ///
    /// `exec_config` carries the `[tools.exec]` timeouts so a rebuilt
    /// registry keeps the daemon's configured `exec` limits instead of
    /// reverting to the crate defaults on the next `tools/list_changed`.
    pub fn tool_refresher(&self, exec_config: ExecConfig) -> McpToolRefresher {
        McpToolRefresher {
            clients: Arc::clone(&self.clients),
            exec_config,
            progress: self.progress.clone(),
            limits: self.limits.clone(),
        }
    }

    /// Extract every server's notification receiver so a drain task
    /// can own them outright (ADR-0020). Each receiver is replaced
    /// with a closed dummy, so a later
    /// [`recv_notification`](Self::recv_notification) for that server
    /// returns `None` immediately rather than racing the drain.
    pub async fn take_notifications(
        &mut self,
    ) -> Vec<(String, mpsc::UnboundedReceiver<ServerNotification>)> {
        let mut out = Vec::with_capacity(self.servers.len());
        for server in &self.servers {
            let mut guard = server.notifications.lock().await;
            let (_closed_tx, closed_rx) = mpsc::unbounded_channel();
            let rx = std::mem::replace(&mut *guard, closed_rx);
            out.push((server.name.clone(), rx));
        }
        out
    }

    /// One server's notification receiver, for a server that came up
    /// after the drain task was started (#548). Same replacement as
    /// [`take_notifications`](Self::take_notifications): the server
    /// keeps a closed dummy, so nothing else can consume the stream the
    /// drain now owns. `None` for a server this manager does not hold.
    pub async fn take_notifications_for(
        &mut self,
        server: &str,
    ) -> Option<mpsc::UnboundedReceiver<ServerNotification>> {
        let running = self.servers.iter().find(|s| s.name == server)?;
        let mut guard = running.notifications.lock().await;
        let (_closed_tx, closed_rx) = mpsc::unbounded_channel();
        Some(std::mem::replace(&mut *guard, closed_rx))
    }

    /// Await the next out-of-band notification a server's handler
    /// forwarded (resource change, list-changed, log, progress).
    /// `None` if the server is unknown or its channel closed.
    pub async fn recv_notification(&self, server: &str) -> Option<ServerNotification> {
        let server = self.servers.iter().find(|s| s.name == server)?;
        server.notifications.lock().await.recv().await
    }

    /// How long [`shutdown`](Self::shutdown) waits for a stdio child to
    /// exit gracefully after we've cancelled the service (which sends
    /// the child EOF on stdin) but can't `close().await` it directly
    /// because tool `Arc`s are still outstanding. rmcp's child-process
    /// transport itself waits up to 3s for the child before force-killing;
    /// we give it a little more headroom so the *graceful* path (EOF →
    /// the server tears its stdio down and exits) wins the race against
    /// the abrupt drop-guard kill, which is what causes the flaky
    /// teardown `EPIPE` on the Node stdio servers (see issue #25).
    const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(4);

    /// Gracefully shut down all managed MCP server processes.
    ///
    /// Teardown ordering matters for stdio (child-process) servers: an
    /// MCP server mid-write when its stdin/stdout pipe is closed abruptly
    /// hits `EPIPE`, and the `@modelcontextprotocol/sdk` stdio transport
    /// installs no socket `error` handler, so Node throws on the
    /// unhandled `'error'` event and the whole process exits 101 — even
    /// though every request already completed. That reddens CI as pure
    /// teardown noise (issue #25).
    ///
    /// The fix is to always tear the server down *gracefully*: cancel the
    /// service so rmcp closes the transport (which sends the child EOF on
    /// stdin and waits for it to exit) rather than letting the child be
    /// abruptly killed mid-write.
    ///
    /// - When no tool `Arc`s are outstanding we can take `&mut` and
    ///   `close().await`, which cancels *and* awaits the background task's
    ///   graceful transport close to completion — the cleanest path.
    /// - Otherwise (tool wrappers still hold client `Arc`s) we can't get
    ///   `&mut`, so we cancel the service via its cancellation token —
    ///   which drives the same graceful transport close on the background
    ///   task — and then give the child a bounded window to receive EOF
    ///   and exit before we drop our handle. Without this wait, dropping
    ///   the `RunningService` here lets its drop guard cancel and the
    ///   child-process transport kill the child *abruptly*, racing the
    ///   server's final writes → the flaky `EPIPE` crash.
    pub async fn shutdown(&mut self) {
        for server in &mut self.servers {
            info!(
                server = %server.name,
                tools = ?server.tool_names,
                "shutting down MCP server"
            );
            match Arc::get_mut(&mut server.client) {
                // Sole owner: cancel and await the graceful transport
                // close to completion.
                Some(client) => {
                    if let Err(err) = client.close().await {
                        warn!(
                            server = %server.name,
                            error = %err,
                            "error during MCP server shutdown"
                        );
                    }
                }
                // Tool wrappers still hold client Arcs, so we can't take
                // `&mut` to `close().await`. Cancel the service anyway —
                // that drives the same graceful transport close (EOF to
                // the child, wait for it to exit) on the background task —
                // then wait for the child to exit before we drop, so it
                // isn't killed mid-write (issue #25).
                None => {
                    debug!(
                        server = %server.name,
                        "MCP client has outstanding references; cancelling and \
                         awaiting graceful child exit before drop"
                    );
                    server.client.cancellation_token().cancel();
                    Self::await_graceful_close(&server.client, Self::SHUTDOWN_GRACE).await;
                }
            }
        }
        self.servers.clear();
        self.started.clear();
        // The handles this manager handed out are live views, so they
        // have to be emptied too — a rebuilt registry after shutdown
        // would otherwise be built from clients whose transport is gone.
        self.clients
            .write()
            .expect("MCP client view poisoned")
            .clear();
    }

    /// After cancelling a service we can't `close().await` (outstanding
    /// tool `Arc`s), wait — up to `grace` — for its background task to
    /// finish the graceful transport close so the stdio child exits on
    /// EOF instead of being killed mid-write. Polls the service's
    /// closed/transport-closed state, which flips once the background
    /// loop has run its `transport.close()` (the EOF + child-exit path).
    /// Bounded so a wedged child can't hang shutdown — the drop guard
    /// force-kills it after we return.
    async fn await_graceful_close(client: &Arc<McpClient>, grace: std::time::Duration) {
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            if client.is_transport_closed() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                debug!("timed out awaiting graceful MCP child exit; drop guard will force-kill");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }
}

#[cfg(test)]
mod tests;
