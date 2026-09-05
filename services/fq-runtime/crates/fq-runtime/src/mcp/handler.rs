//! factor-q's rmcp client handler — the inbound edge of a connection.
//!
//! One [`FactorQClientHandler`] is built per started server. It
//! advertises exactly the client capabilities that server was granted
//! ([`AdvertisedCapabilities`], nothing by default), answers
//! `roots/list`, bridges server-initiated requests ([`ServerRequest`])
//! to the runner, and forwards every out-of-band notification onto the
//! per-server sink drained by
//! [`drain_server_notifications`](super::drain_server_notifications).

use std::sync::Arc;

use rmcp::ClientHandler;
use rmcp::model::{
    ClientCapabilities, ClientInfo, CreateElicitationRequestParams, CreateElicitationResult,
    CreateMessageRequestMethod, CreateMessageRequestParams, CreateMessageResult, ElicitationAction,
    ElicitationCapability, FormElicitationCapability, ListRootsResult, LoggingLevel,
    LoggingMessageNotificationParam, ProgressNotificationParam, ResourceUpdatedNotificationParam,
    Root, RootsCapabilities, SamplingCapability,
};
use rmcp::service::{MaybeSendFuture, NotificationContext, RequestContext, RoleClient};
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::{debug, info, warn};

use super::ServerNotification;
use super::progress::progress_token_string;

/// A request a connected MCP server initiates back toward the host
/// *mid-invocation* (ADR-0018).
///
/// The handler ([`FactorQClientHandler`]) is a thin bridge: it
/// translates an inbound rmcp request into one of these variants,
/// forwards it on a per-invocation channel, and awaits the host's
/// reply on the embedded oneshot. The runner is the sole arbiter —
/// it gates, runs the LLM call through its single budgeted/WAL'd
/// path, validates the result, and replies. Step 5 wires the
/// sampling arm; Step 6 adds an `Elicitation` variant to the same
/// channel and `select!` arm.
pub enum ServerRequest {
    /// `sampling/createMessage` — the server asks the host to run an
    /// LLM completion and return the result to the *server* (not the
    /// agent's transcript). `reply` carries either the result or a
    /// structured decline (e.g. ungranted / over-budget); dropping
    /// the sender declines with `method_not_found`.
    Sampling {
        params: CreateMessageRequestParams,
        reply: oneshot::Sender<Result<CreateMessageResult, rmcp::ErrorData>>,
    },
    /// `elicitation/create` — the server asks for structured user
    /// input matching a schema. factor-q answers it autonomously on
    /// the agent's model (ADR-0017); `reply` carries the result, whose
    /// `action` is `accept` (with content) or `decline`. A dropped
    /// sender declines.
    Elicitation {
        params: CreateElicitationRequestParams,
        reply: oneshot::Sender<Result<CreateElicitationResult, rmcp::ErrorData>>,
    },
}

/// The structured decline an elicitation request resolves to when
/// refused (ungranted, over-budget, retries exhausted) or when the
/// host cannot service it. Per the protocol this is an ordinary
/// result with `action: decline`, not an error — the server continues
/// without the input.
pub(crate) fn elicitation_decline() -> CreateElicitationResult {
    CreateElicitationResult {
        action: ElicitationAction::Decline,
        content: None,
        meta: None,
    }
}

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
    roots: Arc<Mutex<Vec<Root>>>,
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
    pub(super) fn with_roots(mut self, roots: Arc<Mutex<Vec<Root>>>) -> Self {
        self.roots = roots;
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
    /// forwards the params on the per-invocation channel and awaits
    /// the runner's reply. With no channel wired (a shared tool-only
    /// server) or no runner listening, it declines with
    /// `method_not_found` — the rmcp default.
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
                    params,
                    reply: reply_tx,
                })
                .is_err()
            {
                // Runner gone — no one will service this request.
                return decline();
            }
            match reply_rx.await {
                Ok(result) => result,
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
            let Some(tx) = sink else {
                return Ok(elicitation_decline());
            };
            let (reply_tx, reply_rx) = oneshot::channel();
            if tx
                .send(ServerRequest::Elicitation {
                    params,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Ok(elicitation_decline());
            }
            match reply_rx.await {
                Ok(result) => result,
                // Reply sender dropped without answering → decline.
                Err(_) => Ok(elicitation_decline()),
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
        async move { Ok(ListRootsResult::new(roots.lock().await.clone())) }
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
        let level = logging_level_name(params.level);
        let logger = params.logger.as_deref().unwrap_or("mcp-server");
        // Dynamic level → a static-level dispatch (tracing levels are
        // const). MCP's eight levels collapse onto tracing's five.
        match params.level {
            LoggingLevel::Debug => {
                debug!(target: "mcp.server.log", %level, logger, data = %params.data)
            }
            LoggingLevel::Info | LoggingLevel::Notice => {
                info!(target: "mcp.server.log", %level, logger, data = %params.data)
            }
            LoggingLevel::Warning => {
                warn!(target: "mcp.server.log", %level, logger, data = %params.data)
            }
            LoggingLevel::Error
            | LoggingLevel::Critical
            | LoggingLevel::Alert
            | LoggingLevel::Emergency => {
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

    /// Forward `notifications/progress` for an in-flight request: trace
    /// it and forward a [`ServerNotification::Progress`] on the sink.
    fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + MaybeSendFuture + '_ {
        let token = progress_token_string(&params.progress_token);
        debug!(
            target: "mcp.server.progress",
            token = %token,
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

/// Map an MCP logging level to its canonical lowercase name.
fn logging_level_name(level: LoggingLevel) -> &'static str {
    match level {
        LoggingLevel::Debug => "debug",
        LoggingLevel::Info => "info",
        LoggingLevel::Notice => "notice",
        LoggingLevel::Warning => "warning",
        LoggingLevel::Error => "error",
        LoggingLevel::Critical => "critical",
        LoggingLevel::Alert => "alert",
        LoggingLevel::Emergency => "emergency",
    }
}

#[cfg(test)]
mod tests;
