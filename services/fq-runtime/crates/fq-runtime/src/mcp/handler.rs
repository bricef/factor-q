//! factor-q's rmcp client handler — the inbound edge of a connection.
//!
//! One [`FactorQClientHandler`] is built per started server. It
//! advertises exactly the client capabilities that server was granted
//! ([`AdvertisedCapabilities`], nothing by default), answers
//! `roots/list`, bridges server-initiated requests ([`ServerRequest`])
//! to the runner, and forwards every out-of-band notification onto the
//! per-server sink drained by
//! [`drain_server_notifications`](super::drain_server_notifications).
//!
//! **This module is the boundary for MCP's server-initiated surface.**
//! It is the only production code that names rmcp's sampling,
//! elicitation-envelope, roots and logging types, or calls the peer
//! methods SEP-2577 deprecates (`logging/setLevel`, and
//! `resources/subscribe` in later rmcp): the handler converts between
//! them and factor-q's own values in `server_initiated` (the `convert`
//! child), and `set_level` / `subscribe` wrap the peer calls for the
//! request surface. Keeping them here is what lets one module-level lint
//! expectation cover rmcp's deprecation of them. The elicitation schema
//! types are not deprecated and are not mirrored; they are re-exported
//! from here so the runtime names them in one place.

use std::sync::Arc;

use rmcp::ClientHandler;
use rmcp::model::{
    ClientCapabilities, ClientInfo, CreateElicitationRequestParams, CreateElicitationResult,
    CreateMessageRequestMethod, CreateMessageRequestParams, CreateMessageResult,
    ElicitationCapability, FormElicitationCapability, ListRootsResult,
    LoggingMessageNotificationParam, ProgressNotificationParam, ResourceUpdatedNotificationParam,
    Root, RootsCapabilities, SamplingCapability, SetLevelRequestParams, SubscribeRequestParams,
};
use rmcp::service::{
    MaybeSendFuture, NotificationContext, Peer, RequestContext, RoleClient, ServiceError,
};
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::{debug, info, warn};

use super::progress::{ProgressRegistry, progress_token_string};
use super::{McpLogLevel, ServerNotification, ServerRequest, WorkspaceRoot, elicitation_decline};

mod convert;

#[cfg(test)]
pub(crate) use convert::sampling_request_from_wire;

/// Which inbound (server-initiated) capabilities factor-q advertises to
/// a given server during the initialize handshake (ADR-0017,
/// nothing-by-default). Derived per-server from the agent's grants:
/// a server not granted a capability is not told the client supports
/// it, so a well-behaved server won't even register the corresponding
/// tool (e.g. the everything server gates `trigger-sampling-request` on
/// the client advertising `sampling`). Resources/prompts are *server*
/// capabilities and unaffected by this.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AdvertisedCapabilities {
    pub sampling: bool,
    pub elicitation: bool,
    pub roots: bool,
}

impl AdvertisedCapabilities {
    /// Advertise nothing inbound (a shared, tool-only server).
    pub fn none() -> Self {
        Self::default()
    }

    /// Advertise all three (used by tests that exercise the full
    /// server-initiated surface).
    pub fn all() -> Self {
        Self {
            sampling: true,
            elicitation: true,
            roots: true,
        }
    }
}

/// factor-q's MCP client handler.
///
/// Advertises the client-side capabilities the agent granted this
/// server (roots, sampling, elicitation) during the initialize
/// handshake, forwards out-of-band notifications to a sink, and — on
/// the per-invocation path — bridges server-initiated requests
/// (sampling, elicitation) to the runner and answers `roots/list`.
#[derive(Default)]
pub struct FactorQClientHandler {
    /// Inbound capabilities advertised to this server (per-server
    /// grant). Default: nothing (tool-only).
    capabilities: AdvertisedCapabilities,
    /// Sink for resource notifications forwarded from the connected
    /// server (`resources/updated`, `resources/list_changed`).
    notifications: Option<mpsc::UnboundedSender<ServerNotification>>,
    /// Sink for server-initiated requests (sampling, and later
    /// elicitation) bridged to the runner. `None` for shared,
    /// tool-only servers, which decline inbound requests per the rmcp
    /// default (ADR-0018: only grant-bearing servers run
    /// per-invocation with a wired channel).
    server_requests: Option<mpsc::UnboundedSender<ServerRequest>>,
    /// Workspace roots advertised to the server on `roots/list`
    /// (ADR-0018). Shared (interior-mutable) with the
    /// [`RootsHandle`](super::RootsHandle) so the host can update them
    /// and fire `roots/list_changed`. Empty by default — roots are
    /// nothing-by-default and derived from the agent's sandbox grant.
    roots: Arc<Mutex<Vec<WorkspaceRoot>>>,
    /// This server's name and the shared `(server, token) → call`
    /// table, so an inbound `notifications/progress` is attributed to
    /// the invocation and tool call that issued the request (#605).
    /// `None` for a handler built outside a manager (tests): progress
    /// is then still traced and forwarded, just uncorrelated.
    progress: Option<(String, ProgressRegistry)>,
}

impl FactorQClientHandler {
    /// Build a handler that forwards resource notifications to `tx`.
    pub(super) fn with_notifications(tx: mpsc::UnboundedSender<ServerNotification>) -> Self {
        Self {
            notifications: Some(tx),
            ..Default::default()
        }
    }

    /// Set the inbound capabilities advertised to this server (derived
    /// from the agent's per-server grants). Default is none.
    pub(super) fn with_capabilities(mut self, capabilities: AdvertisedCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Wire a sink for server-initiated requests (sampling /
    /// elicitation). Used on the per-invocation start path; absent
    /// for shared tool-only servers.
    pub(super) fn with_server_requests(mut self, tx: mpsc::UnboundedSender<ServerRequest>) -> Self {
        self.server_requests = Some(tx);
        self
    }

    /// Share the advertised-roots cell with this handler so
    /// `roots/list` reflects host updates. Used on the per-invocation
    /// start path; the same `Arc` is held by the
    /// [`RootsHandle`](super::RootsHandle).
    pub(super) fn with_roots(mut self, roots: Arc<Mutex<Vec<WorkspaceRoot>>>) -> Self {
        self.roots = roots;
        self
    }

    /// Wire the manager's progress-correlation table and tell the
    /// handler which server it speaks for (#605). Without it, inbound
    /// progress is traced and forwarded but attributed to nothing.
    pub(super) fn with_progress(mut self, server: String, registry: ProgressRegistry) -> Self {
        self.progress = Some((server, registry));
        self
    }

    /// Build the `ClientCapabilities` advertised during initialize from
    /// the per-server grant: each of roots (+`list_changed`), sampling,
    /// and form-mode elicitation is advertised only if granted
    /// (ADR-0017, nothing-by-default). An ungranted capability is left
    /// `None`, so the server is never told the client supports it.
    pub fn advertised_capabilities(granted: AdvertisedCapabilities) -> ClientCapabilities {
        let mut capabilities = ClientCapabilities::default();
        if granted.roots {
            capabilities.roots = Some(RootsCapabilities {
                list_changed: Some(true),
            });
        }
        if granted.sampling {
            capabilities.sampling = Some(SamplingCapability::default());
        }
        if granted.elicitation {
            capabilities.elicitation = Some(ElicitationCapability {
                form: Some(FormElicitationCapability {
                    schema_validation: Some(true),
                }),
                url: None,
            });
        }
        capabilities
    }
}

impl ClientHandler for FactorQClientHandler {
    fn get_info(&self) -> ClientInfo {
        let mut info = ClientInfo::default();
        info.capabilities = Self::advertised_capabilities(self.capabilities);
        info
    }

    /// Bridge a `sampling/createMessage` request to the runner.
    ///
    /// The handler does no policy and no LLM call (ADR-0018 §2): it
    /// forwards the params, converted to a
    /// [`SamplingRequest`](super::SamplingRequest), on the
    /// per-invocation channel and awaits the runner's reply. With no
    /// channel wired (a shared tool-only server) or no runner
    /// listening, it declines with `method_not_found` — the rmcp
    /// default.
    fn create_message(
        &self,
        params: CreateMessageRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> impl std::future::Future<Output = Result<CreateMessageResult, rmcp::ErrorData>>
    + MaybeSendFuture
    + '_ {
        let sink = self.server_requests.clone();
        async move {
            let decline = || {
                Err(rmcp::ErrorData::method_not_found::<
                    CreateMessageRequestMethod,
                >())
            };
            let Some(tx) = sink else {
                return decline();
            };
            let (reply_tx, reply_rx) = oneshot::channel();
            if tx
                .send(ServerRequest::Sampling {
                    request: convert::sampling_request(params),
                    reply: reply_tx,
                })
                .is_err()
            {
                // Runner gone — no one will service this request.
                return decline();
            }
            match reply_rx.await {
                Ok(result) => result.map(CreateMessageResult::from),
                // Reply sender dropped without answering → decline.
                Err(_) => decline(),
            }
        }
    }

    /// Bridge an `elicitation/create` request to the runner (ADR-0018).
    /// Like [`create_message`](Self::create_message), the handler does
    /// no policy and no LLM call: it forwards the params and awaits the
    /// runner's reply. With no channel wired or no runner listening it
    /// declines (an ordinary `action: decline` result — the rmcp
    /// default).
    fn create_elicitation(
        &self,
        params: CreateElicitationRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> impl std::future::Future<Output = Result<CreateElicitationResult, rmcp::ErrorData>>
    + MaybeSendFuture
    + '_ {
        let sink = self.server_requests.clone();
        async move {
            let decline = || Ok(CreateElicitationResult::from(elicitation_decline()));
            let Some(tx) = sink else {
                return decline();
            };
            let (reply_tx, reply_rx) = oneshot::channel();
            if tx
                .send(ServerRequest::Elicitation {
                    request: convert::elicitation_request(params),
                    reply: reply_tx,
                })
                .is_err()
            {
                return decline();
            }
            match reply_rx.await {
                Ok(result) => result.map(CreateElicitationResult::from),
                // Reply sender dropped without answering → decline.
                Err(_) => decline(),
            }
        }
    }

    /// Answer `roots/list` with the workspace roots advertised to this
    /// server (ADR-0018). Handler-only: no LLM, no budget — roots are
    /// invocation-scoped config. Empty when the agent granted no roots
    /// to this server.
    fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> impl std::future::Future<Output = Result<ListRootsResult, rmcp::ErrorData>> + MaybeSendFuture + '_
    {
        let roots = Arc::clone(&self.roots);
        async move {
            let roots = roots.lock().await.iter().cloned().map(Root::from).collect();
            Ok(ListRootsResult::new(roots))
        }
    }

    fn on_resource_updated(
        &self,
        params: ResourceUpdatedNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + MaybeSendFuture + '_ {
        if let Some(tx) = &self.notifications {
            let _ = tx.send(ServerNotification::ResourceUpdated { uri: params.uri });
        }
        std::future::ready(())
    }

    fn on_resource_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + MaybeSendFuture + '_ {
        if let Some(tx) = &self.notifications {
            let _ = tx.send(ServerNotification::ResourceListChanged);
        }
        std::future::ready(())
    }

    /// The server's tool list changed (`notifications/tools/list_changed`).
    /// Forward it so the host can re-discover via
    /// [`McpClientManager::refresh_tools`](super::McpClientManager::refresh_tools)
    /// rather than serving the startup-time set.
    fn on_tool_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + MaybeSendFuture + '_ {
        if let Some(tx) = &self.notifications {
            let _ = tx.send(ServerNotification::ToolListChanged);
        }
        std::future::ready(())
    }

    /// The server's prompt list changed
    /// (`notifications/prompts/list_changed`). Prompts are fetched
    /// on-demand, so this is informational — forward it for observers.
    fn on_prompt_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + MaybeSendFuture + '_ {
        if let Some(tx) = &self.notifications {
            let _ = tx.send(ServerNotification::PromptListChanged);
        }
        std::future::ready(())
    }

    /// Fold a server log record (`notifications/message`) into the
    /// host's `tracing` output at the mapped level, and forward it on
    /// the notification sink so consumers (tests, a future event-bus
    /// bridge) can observe it. The server respects the client's
    /// `logging/setLevel` choice, so filtering happens server-side.
    fn on_logging_message(
        &self,
        params: LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + MaybeSendFuture + '_ {
        let severity = McpLogLevel::from(params.level);
        let level = severity.as_str();
        let logger = params.logger.as_deref().unwrap_or("mcp-server");
        // Dynamic level → a static-level dispatch (tracing levels are
        // const). MCP's eight levels collapse onto tracing's five.
        match severity {
            McpLogLevel::Debug => {
                debug!(target: "mcp.server.log", %level, logger, data = %params.data)
            }
            McpLogLevel::Info | McpLogLevel::Notice => {
                info!(target: "mcp.server.log", %level, logger, data = %params.data)
            }
            McpLogLevel::Warning => {
                warn!(target: "mcp.server.log", %level, logger, data = %params.data)
            }
            McpLogLevel::Error
            | McpLogLevel::Critical
            | McpLogLevel::Alert
            | McpLogLevel::Emergency => {
                tracing::error!(target: "mcp.server.log", %level, logger, data = %params.data)
            }
        }
        if let Some(tx) = &self.notifications {
            let _ = tx.send(ServerNotification::Log {
                level: level.to_string(),
                logger: params.logger,
                data: params.data,
            });
        }
        std::future::ready(())
    }

    /// Handle `notifications/progress` for an in-flight request.
    ///
    /// Attribution happens here rather than in the notification drain
    /// so it holds whether or not anyone is draining: the correlation
    /// table is consulted at the point of arrival, which is also where
    /// the call's *last progress at* stamp has to land for a stuck-call
    /// detector to trust it (#605). A token with no entry — progress
    /// against a call that already finished — is traced and forwarded
    /// like any other, just unattributed.
    fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + MaybeSendFuture + '_ {
        let token = progress_token_string(&params.progress_token);
        let attributed = self.progress.as_ref().and_then(|(server, registry)| {
            registry.record_progress(server, &token, params.progress, params.total)
        });
        debug!(
            target: "mcp.server.progress",
            token = %token,
            invocation_id = attributed.as_ref().map(|call| call.invocation_id.as_str()),
            call_id = attributed.as_ref().map(|call| call.call_id.as_str()),
            progress = params.progress,
            total = ?params.total,
            "progress"
        );
        if let Some(tx) = &self.notifications {
            let _ = tx.send(ServerNotification::Progress {
                token,
                progress: params.progress,
                total: params.total,
                message: params.message,
            });
        }
        std::future::ready(())
    }
}

/// Ask a connected server for log records at or above `level`
/// (`logging/setLevel`). MCP deprecated the request in SEP-2577 (rmcp
/// 1.8 flags it); allowed until servers drop it.
#[allow(deprecated)]
pub(super) async fn set_level(
    peer: &Peer<RoleClient>,
    level: McpLogLevel,
) -> Result<(), ServiceError> {
    peer.set_level(SetLevelRequestParams::new(level.into()))
        .await
}

/// Subscribe to update notifications for one resource on a connected
/// server (`resources/subscribe`).
pub(super) async fn subscribe(peer: &Peer<RoleClient>, uri: &str) -> Result<(), ServiceError> {
    peer.subscribe(SubscribeRequestParams::new(uri)).await
}

#[cfg(test)]
mod tests;
