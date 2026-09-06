//! Where each MCP server is in its life, and how it gets there.
//!
//! The manager's *request* surface — call a tool, read a resource,
//! fetch a prompt — is about a server that is already connected. This
//! module is about the other question: is it connected at all, and if
//! not, what happened and when will we try again.
//!
//! Boot used to have no answer, because it had no state to be in. The
//! daemon started shared servers one after another with no deadline
//! around the `initialize` handshake, so one remote server that
//! accepted TCP and never answered held `fqd` at startup with every
//! agent down (review finding B3,
//! <https://github.com/bricef/factor-q/issues/548>). What replaces it:
//!
//! * **Boot never blocks.** Every shared server starts at once, each
//!   under its own start-up and discovery deadline, so the wait is the
//!   slowest server rather than the sum of all of them.
//! * **A server that fails is `unavailable`, not fatal.** The daemon
//!   finishes booting; the agents that wanted that server are refused
//!   at dispatch, by name, and the ones that did not run normally.
//! * **Unavailable is not permanent.** [`retry_unavailable`] dials each
//!   one again on a doubling backoff, so a server whose package was
//!   still installing, or whose host was rebooting, is picked up
//!   without `fq reload`.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use fq_tools::Tool;
use rmcp::ServiceExt;
use rmcp::model::Root;
use rmcp::transport::StreamableHttpClientTransport;
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::discovery::discover_tools;
use super::limits::McpLimits;
use super::manager::RunningServer;
use super::progress::ProgressRegistry;
use super::{
    AdvertisedCapabilities, FactorQClientHandler, McpClientManager, McpError, McpServerConfig,
    RootsHandle, ServerNotification, ServerRequest, stdio,
};

/// Where one MCP server is in its life, as every health surface reads
/// it.
///
/// Three states and no fourth: a server is being dialled, is answering,
/// or is not — and the third carries why, because "unavailable" with no
/// reason is a line an operator cannot act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerState {
    /// The handshake is in flight. Only visible during boot and during
    /// a retry attempt; a server does not linger here, because both
    /// paths run under a deadline.
    Starting,
    /// Connected, with its tools discovered and registered.
    Ready {
        /// How many tools it advertised, including the synthesized
        /// resource tools.
        tools: u32,
    },
    /// The server did not start, or its discovery was refused. Its
    /// tools are absent from the registry and any agent that declares
    /// it is refused at dispatch.
    Unavailable {
        /// Why, verbatim from the error — a timeout, a cap, a spawn
        /// failure.
        reason: String,
        /// How many times it has been tried, the boot attempt
        /// included.
        attempts: u32,
        /// When the next retry is due, epoch milliseconds. `None` when
        /// retrying is disabled (`[mcp] retry_initial_secs = 0`), which
        /// is the one case where unavailable really is until restart.
        next_retry_at_ms: Option<i64>,
    },
}

/// The `server → state` table, shared by clone.
///
/// The manager holds one and writes it; the daemon's health surfaces
/// and the runner's dispatch check read it. A plain `Mutex` rather than
/// a channel because every reader wants the answer *now* — `fq doctor`
/// is a request/response, and refusing an invocation cannot wait on a
/// broadcast.
#[derive(Clone, Default)]
pub struct McpServerStates {
    inner: Arc<Mutex<BTreeMap<String, McpServerState>>>,
}

impl McpServerStates {
    fn set(&self, server: &str, state: McpServerState) {
        self.inner
            .lock()
            .expect("MCP server states poisoned")
            .insert(server.to_string(), state);
    }

    /// The server is being dialled.
    ///
    /// The three writers are `pub(crate)` rather than `pub(super)`: the
    /// manager is the only production writer, and a test elsewhere in
    /// the crate needs to *seed* a table (the runner's refusal check
    /// reads one it never fills). Not `pub`: nothing outside the
    /// runtime may assert a server's state.
    pub(crate) fn starting(&self, server: &str) {
        self.set(server, McpServerState::Starting);
    }

    /// The server answered and its tools are registered.
    pub(crate) fn ready(&self, server: &str, tools: u32) {
        self.set(server, McpServerState::Ready { tools });
    }

    /// The server did not start. `attempts` counts from the boot
    /// attempt, and `next_retry_at_ms` is when it will be tried again.
    pub(crate) fn unavailable(
        &self,
        server: &str,
        reason: String,
        attempts: u32,
        next_retry_at_ms: Option<i64>,
    ) {
        self.set(
            server,
            McpServerState::Unavailable {
                reason,
                attempts,
                next_retry_at_ms,
            },
        );
    }

    /// One server's state, or `None` for a name this daemon never
    /// declared.
    pub fn state(&self, server: &str) -> Option<McpServerState> {
        self.inner
            .lock()
            .expect("MCP server states poisoned")
            .get(server)
            .cloned()
    }

    /// Every declared server and its state, by name. Ordered, so a
    /// health report and a golden read the same way twice.
    pub fn snapshot(&self) -> Vec<(String, McpServerState)> {
        self.inner
            .lock()
            .expect("MCP server states poisoned")
            .iter()
            .map(|(name, state)| (name.clone(), state.clone()))
            .collect()
    }

    /// Whether this daemon knows about any server at all — the "no
    /// shared servers declared" case every surface has to distinguish
    /// from "all of them are fine".
    pub fn is_empty(&self) -> bool {
        self.inner
            .lock()
            .expect("MCP server states poisoned")
            .is_empty()
    }
}

/// What one shared server's start came to, for the caller that has to
/// register the tools and log the failure.
pub struct SharedServerStart {
    /// The server's declared name.
    pub server: String,
    /// Its tools, or why it has none. A duplicate declaration — the
    /// same transport already started under another agent's name — is
    /// `Ok` with an empty vec, exactly as the single-server path has
    /// always answered it.
    pub outcome: Result<Vec<Arc<dyn Tool>>, McpError>,
}

/// Everything a start needs that is not the config: where stdio
/// children live, which tables to record into, and what the attempt is
/// allowed to cost.
///
/// One value because these four travel together through every start
/// path — boot, retry, and the runner's per-invocation grant servers —
/// and passing them separately made a five-argument function that grew
/// a sixth every time the manager learned something.
pub(super) struct StartContext<'a> {
    pub(super) server_root: &'a Path,
    pub(super) progress: &'a ProgressRegistry,
    pub(super) limits: &'a McpLimits,
}

/// A connected server, before it has been registered with a manager.
pub(super) struct Connected {
    pub(super) server: RunningServer,
    pub(super) tools: Vec<Arc<dyn Tool>>,
    pub(super) roots: RootsHandle,
}

/// Dial one server, run the `initialize` handshake under the start-up
/// deadline, and discover its tools under the discovery deadline.
///
/// Takes no manager, which is what makes the concurrent boot possible:
/// the `&mut` the manager needs is taken once at the end, to register
/// the servers that came up, rather than held across every handshake.
///
/// A handshake that times out drops the transport, and with it the
/// stdio child (`kill_on_drop`) or the HTTP session — an abandoned
/// start leaves nothing running.
pub(super) async fn connect(
    config: &McpServerConfig,
    ctx: &StartContext<'_>,
    server_request_tx: Option<mpsc::UnboundedSender<ServerRequest>>,
    roots: Vec<Root>,
    capabilities: AdvertisedCapabilities,
) -> Result<Connected, McpError> {
    info!(
        server = %config.name,
        // The endpoint or the command: `command` alone is empty for a
        // remote server, so that log line named nothing.
        target = %config.url.as_deref().unwrap_or(&config.command),
        args = ?config.args,
        "starting MCP server"
    );

    // The handler advertises factor-q's client capabilities
    // (roots/sampling/elicitation), forwards resource notifications to
    // `notif_rx`, and — on the per-invocation path — bridges
    // server-initiated requests. It is then served over whichever
    // transport the config selects; the MCP initialize handshake and
    // every subsequent operation are transport-agnostic.
    let (notif_tx, notif_rx) = mpsc::unbounded_channel();
    let roots_cell = Arc::new(tokio::sync::Mutex::new(roots));
    let mut handler = FactorQClientHandler::with_notifications(notif_tx)
        .with_roots(Arc::clone(&roots_cell))
        .with_capabilities(capabilities)
        .with_progress(config.name.clone(), ctx.progress.clone());
    if let Some(req_tx) = server_request_tx {
        handler = handler.with_server_requests(req_tx);
    }
    let target = config.url.clone().unwrap_or_else(|| config.command.clone());
    let client = match &config.url {
        // Streamable HTTP (remote) transport — the 2025-11-25 spec
        // transport.
        Some(url) => {
            let dial = handler.serve(StreamableHttpClientTransport::from_uri(url.clone()));
            handshake(dial, ctx.limits, &target).await?
        }
        // stdio child process: cleared env, pinned PATH, own cwd,
        // bounded line length (#541, #548, `stdio`).
        None => {
            let transport =
                stdio::spawn_transport(config, ctx.server_root, ctx.limits.max_line_bytes)?;
            handshake(handler.serve(transport), ctx.limits, &target).await?
        }
    };

    let client = Arc::new(client);
    let roots = RootsHandle {
        server: config.name.clone(),
        roots: roots_cell,
        client: Arc::clone(&client),
    };

    // Discovery has its own deadline and its own caps, applied inside
    // `discover_tools` so `tools/list_changed` re-discovery is bounded
    // too. A server that fails here is disconnected rather than left
    // half-registered: it holds a child process and a task, and the
    // caller is about to record it unavailable.
    let discovered = discover_tools(&client, &config.name, ctx.progress, ctx.limits).await;
    let (tools, tool_names) = match discovered {
        Ok(discovered) => discovered,
        Err(err) => {
            client.cancellation_token().cancel();
            return Err(err);
        }
    };

    Ok(Connected {
        server: RunningServer {
            name: config.name.clone(),
            client,
            tool_names,
            notifications: tokio::sync::Mutex::new(notif_rx),
        },
        tools,
        roots,
    })
}

/// Run one `initialize` handshake under the start-up deadline.
///
/// Its own function because the two transports produce different error
/// types and different futures, so the deadline cannot be wrapped
/// around a single `match` without boxing one of them. Both failures —
/// the deadline and the handshake's own error — land as
/// [`McpError::ServerStart`] naming the endpoint or the command, which
/// is what the operator has in the definition.
async fn handshake<F, T, E>(dial: F, limits: &McpLimits, target: &str) -> Result<T, McpError>
where
    F: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    tokio::time::timeout(limits.startup_timeout, dial)
        .await
        .map_err(|_| McpError::ServerStart {
            command: target.to_string(),
            reason: format!(
                "no initialize response within the {}s start-up deadline ([mcp] \
                 startup_timeout_secs)",
                limits.startup_timeout.as_secs()
            ),
        })?
        .map_err(|err| McpError::ServerStart {
            command: target.to_string(),
            reason: err.to_string(),
        })
}

/// How often a connected server is checked for having gone away.
///
/// Polled rather than awaited because rmcp exposes no signal for it:
/// `is_transport_closed` reads the peer's own sender, and the service's
/// cancellation token is cancelled by an explicit shutdown, never by
/// the transport ending. A second is the right order — the fact this
/// bounds is an agent being dispatched at a server that is not there,
/// and dispatch is not sub-second work.
const CLOSURE_POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// Watch one connection for the moment it ends, and say so.
///
/// **Nothing else can see it.** rmcp's `RunningService` owns its
/// handler behind an `Arc`, so the notification channel a server feeds
/// stays open for as long as the host holds the client — a server whose
/// child exited, whose endpoint closed, or whose line broke the length
/// bound leaves a stream that is open and permanently silent. Without
/// this the table said `Ready` for ever: every health surface green,
/// every agent that declared it dispatched, every call through it
/// failing with a closed transport, and no retry.
///
/// The state is flipped **here**, at detection, rather than left to the
/// supervisor: the supervisor holds the manager's lock for the length
/// of a retry round, and a server dying while others are being redialled
/// would otherwise stay `Ready` — and keep being dispatched at — for
/// the length of a round. Forgetting the server and dialling it again
/// stay the supervisor's, which is why `gone` carries the name on.
///
/// Holds a `Weak`, never an `Arc`: a watcher that kept the connection
/// alive would change what shutdown can reclaim, and a client the
/// manager has already torn down needs no announcement.
pub(super) async fn watch_connection(
    client: std::sync::Weak<super::McpClient>,
    server: String,
    states: McpServerStates,
    gone: mpsc::UnboundedSender<String>,
) {
    loop {
        tokio::time::sleep(CLOSURE_POLL).await;
        let Some(client) = client.upgrade() else {
            return; // the manager tore it down: an orderly end, not a death
        };
        if !client.is_transport_closed() {
            continue;
        }
        warn!(
            server = %server,
            "MCP server's connection ended; its tools are unavailable until it is dialled \
             again, and agents that declare it are refused meanwhile"
        );
        states.unavailable(
            &server,
            "the connection to this server ended".to_string(),
            1,
            None,
        );
        let _ = gone.send(server);
        return;
    }
}

/// Retry every server in `unavailable` until it comes up, for the life
/// of the daemon.
///
/// One task for all of them, and one *round* for all of them: a round
/// dials the whole pending set concurrently through
/// [`retry_shared_servers`](McpClientManager::retry_shared_servers), so
/// a permanently hung server costs a recovering one nothing. Dialling
/// them one after another put a full start-up deadline between a
/// server that was ready to answer and the moment anyone asked it.
///
/// The backoff is shared because the attempt counts are: every pending
/// server failed at boot and they are tried together, so they escalate
/// in lockstep and one timer answers for all of them.
///
/// A server that comes up is registered with `manager`, its tools are
/// announced through `came_up` — the same channel a `tools/list_changed`
/// rebuild travels on — and it leaves this loop for good.
///
/// `stop` ends the loop, and it is raced against the round as well as
/// against the sleep: a shutdown arriving mid-round must not wait out
/// the deadlines of servers that are never going to answer. Dropping
/// the round drops every transport with it, and a stdio child spawned
/// with `kill_on_drop` goes with its transport, so abandoning a dial
/// leaves nothing running.
pub async fn retry_unavailable(
    manager: Arc<tokio::sync::Mutex<McpClientManager>>,
    unavailable: Vec<McpServerConfig>,
    declared: Vec<McpServerConfig>,
    came_up: mpsc::UnboundedSender<(String, mpsc::UnboundedReceiver<ServerNotification>)>,
    mut went_down: mpsc::UnboundedReceiver<String>,
    stop: Arc<tokio::sync::Notify>,
) {
    let mut pending = unavailable;
    // The boot attempt already happened, and is attempt 1.
    let mut attempts = 1u32;
    loop {
        // Nothing pending is a state to wait in, not a reason to stop:
        // a server that is ready now can still lose its connection, and
        // this task is the only thing watching for that.
        // Nothing to dial, or retrying disabled, both mean "no timer":
        // servers are still marked unavailable when they die, they
        // simply stay that way until something changes.
        let delay = if pending.is_empty() {
            None
        } else {
            due_in(&manager, &pending, attempts).await
        };
        tokio::select! {
            _ = stop.notified() => return,
            gone = went_down.recv() => {
                let Some(server) = gone else {
                    return; // the drain is gone: the daemon is stopping
                };
                if let Some(config) = mark_gone(&manager, &declared, &server).await {
                    pending.push(config);
                    // A server that was answering a moment ago is a
                    // fresh failure, not the next step of an old one.
                    attempts = 1;
                }
                continue;
            }
            _ = tokio::time::sleep(delay.unwrap_or_default()), if delay.is_some() => {}
        }
        attempts += 1;
        let (still_pending, came_up_now) = tokio::select! {
            _ = stop.notified() => return,
            round = dial_round(&manager, pending, attempts) => round,
        };
        for (server, notifications) in came_up_now {
            info!(
                server = %server,
                attempts,
                "MCP server came up on retry; its tools are available again"
            );
            if came_up.send((server, notifications)).is_err() {
                return; // the drain is gone: the daemon is stopping
            }
        }
        pending = still_pending;
    }
}

/// A server whose connection ended: drop it from the manager and hand
/// back the config to dial it again with. It is already marked
/// unavailable — [`watch_connection`] did that at detection, which is
/// the point of doing it there.
///
/// `None` for a name this daemon does not declare as a shared server,
/// or one the manager was no longer holding — either way there is
/// nothing to retry.
async fn mark_gone(
    manager: &Arc<tokio::sync::Mutex<McpClientManager>>,
    declared: &[McpServerConfig],
    server: &str,
) -> Option<McpServerConfig> {
    let config = declared.iter().find(|c| c.name == server)?.clone();
    manager.lock().await.forget(server).then_some(config)
}

/// Dial one round: every pending server at once, as attempt number
/// `attempt`. Returns the ones still down and the notification stream
/// of each one that came up.
///
/// A server whose start succeeded but which has no stream of its own
/// was deduplicated onto a transport already running under another
/// name; its tools are registered and it leaves the pending set either
/// way.
async fn dial_round(
    manager: &Arc<tokio::sync::Mutex<McpClientManager>>,
    pending: Vec<McpServerConfig>,
    attempt: u32,
) -> (
    Vec<McpServerConfig>,
    Vec<(String, mpsc::UnboundedReceiver<ServerNotification>)>,
) {
    let mut guard = manager.lock().await;
    let outcomes = guard.retry_shared_servers(pending.clone(), attempt).await;
    let mut still_pending = Vec::new();
    let mut came_up = Vec::new();
    for outcome in outcomes {
        match outcome.outcome {
            Ok(_) => {
                if let Some(rx) = guard.take_notifications_for(&outcome.server).await {
                    came_up.push((outcome.server, rx));
                }
            }
            Err(err) => {
                warn!(
                    server = %outcome.server,
                    attempts = attempt,
                    error = %err,
                    "MCP server is still unavailable"
                );
                if let Some(config) = pending.iter().find(|c| c.name == outcome.server) {
                    still_pending.push(config.clone());
                }
            }
        }
    }
    (still_pending, came_up)
}

/// How long until the next round is due, stamping each pending
/// server's attempt count and next retry time into the state table on
/// the way past so `fq doctor` can report both. `None` disables
/// retrying.
async fn due_in(
    manager: &Arc<tokio::sync::Mutex<McpClientManager>>,
    pending: &[McpServerConfig],
    attempts: u32,
) -> Option<std::time::Duration> {
    let guard = manager.lock().await;
    let backoff = guard.limits().retry_backoff(attempts)?;
    let states = guard.states();
    drop(guard);
    let due_ms = chrono::Utc::now().timestamp_millis() + backoff.as_millis() as i64;
    for config in pending {
        if let Some(McpServerState::Unavailable { reason, .. }) = states.state(&config.name) {
            states.unavailable(&config.name, reason, attempts, Some(due_ms));
        }
    }
    Some(backoff)
}

#[cfg(test)]
mod tests;
