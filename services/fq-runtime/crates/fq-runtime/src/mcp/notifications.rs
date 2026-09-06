//! Out-of-band notifications a connected server pushes at the host,
//! and the daemon-side loop that drains them (ADR-0020).

use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_stream::{StreamExt, StreamMap};
use tracing::{debug, info};

use crate::tools::ToolRegistry;

use super::McpToolRefresher;

/// An out-of-band notification forwarded from a connected MCP server
/// to the host's notification sink: resource changes, capability-list
/// changes, log records, and progress (Step 7). The host drains these
/// from the per-server channel (see
/// [`McpClientManager::recv_notification`](super::McpClientManager::recv_notification))
/// to react — refresh stale caches, fold logs into tracing, surface
/// progress, etc.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerNotification {
    /// A subscribed resource changed (`notifications/resources/updated`).
    ResourceUpdated { uri: String },
    /// The server's resource list changed
    /// (`notifications/resources/list_changed`).
    ResourceListChanged,
    /// The server's tool list changed
    /// (`notifications/tools/list_changed`).
    ToolListChanged,
    /// The server's prompt list changed
    /// (`notifications/prompts/list_changed`).
    PromptListChanged,
    /// A log record from the server (`notifications/message`). `level`
    /// is the MCP level name (`"debug"`..`"emergency"`).
    Log {
        level: String,
        logger: Option<String>,
        data: Value,
    },
    /// Progress on an in-flight request (`notifications/progress`),
    /// keyed by the `token` the host attached when issuing it.
    Progress {
        token: String,
        progress: f64,
        total: Option<f64>,
        message: Option<String>,
    },
}

/// Drain every shared server's notification stream in the daemon
/// (ADR-0020). Logs and progress are already folded into `tracing` by
/// the handler — consuming them here is what stops the unbounded
/// channels growing. `tools/list_changed` re-discovers via the
/// `refresher` and hands the rebuilt registry to `on_tools_changed`
/// (the daemon installs it into the shared `ReducerContext`, so the
/// *next* invocation sees it). Returns when every server's channel has
/// closed (shutdown). Log records are forwarded to `on_log` (the
/// event-bus bridge, plan B2); progress is consumed. Everything is
/// already folded into `tracing` at the handler.
///
/// The merge is a [`StreamMap`], which polls its entries from a random
/// starting index, so no server can starve another by construction. The
/// hand-rolled poll-merge this replaced always started at index 0 and
/// returned the first ready channel, so one chatty server held the loop
/// for as long as it kept a message queued and every server behind it
/// waited (#191).
///
/// `late` carries servers that came up *after* the drain started — the
/// retry loop's successes (#548). Each one joins the same merge and
/// triggers the same rebuild a `tools/list_changed` would, because from
/// the registry's point of view a server appearing is a tool list
/// changing. The loop ends when `late` is closed and every channel has
/// drained, which is shutdown: while a retry loop is still running,
/// zero connected servers is a state to wait in rather than a reason to
/// stop.
///
/// **A server dying is not visible here.** rmcp's `RunningService` owns
/// its handler behind an `Arc`, so the sender feeding these channels
/// lives as long as the host's client handle does: a connection that
/// ends leaves its stream open and silent. Detecting that is the
/// transport's business, and belongs to the per-connection watcher the
/// `lifecycle` module runs.
pub async fn drain_server_notifications<F, G>(
    channels: Vec<(String, mpsc::UnboundedReceiver<ServerNotification>)>,
    mut late: mpsc::UnboundedReceiver<(String, mpsc::UnboundedReceiver<ServerNotification>)>,
    refresher: McpToolRefresher,
    on_tools_changed: F,
    on_log: G,
) where
    F: Fn(ToolRegistry) + Send + Sync + 'static,
    G: Fn(String, String, Option<String>, Value) + Send + Sync + 'static,
{
    let mut channels: StreamMap<String, UnboundedReceiverStream<ServerNotification>> = channels
        .into_iter()
        .map(|(server, rx)| (server, UnboundedReceiverStream::new(rx)))
        .collect();
    let mut late_closed = false;

    loop {
        if late_closed && channels.is_empty() {
            return;
        }
        let next = tokio::select! {
            arrival = late.recv(), if !late_closed => match arrival {
                Some((server, rx)) => {
                    info!(server = %server, "MCP server joined after boot: rebuilding the shared registry");
                    channels.insert(server, UnboundedReceiverStream::new(rx));
                    on_tools_changed(refresher.rebuild_registry().await);
                    continue;
                }
                None => {
                    late_closed = true;
                    continue;
                }
            },
            next = channels.next(), if !channels.is_empty() => next,
        };
        // `StreamMap` drops an exhausted stream and yields `None` once
        // the map is empty; with a retry loop still open that is not the
        // end, so go round and let the guard above decide.
        let Some((server, notification)) = next else {
            continue;
        };
        match notification {
            ServerNotification::ToolListChanged => {
                info!(server = %server, "tools/list_changed: rebuilding the shared registry");
                on_tools_changed(refresher.rebuild_registry().await);
            }
            // Bridge log records onto the event bus (plan B2); they are
            // already traced at the handler.
            ServerNotification::Log {
                level,
                logger,
                data,
            } => on_log(server, level, logger, data),
            // Progress is consumed so the channel drains; surfacing it
            // to an operator is an Observability follow-up.
            ServerNotification::Progress { .. } => {}
            // Future notification->action loops (ADR-0020): resource
            // invalidation, prompt-list refresh.
            ServerNotification::ResourceUpdated { uri } => {
                debug!(server = %server, uri = %uri, "resource updated (no action wired)");
            }
            note @ (ServerNotification::ResourceListChanged
            | ServerNotification::PromptListChanged) => {
                debug!(server = %server, ?note, "list changed (fetched on demand; no cache to refresh)");
            }
        }
    }
}

#[cfg(test)]
mod tests;
