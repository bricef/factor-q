//! Issuing one `tools/call`, cancellably.
//!
//! One implementation, two callers: [`McpTool`](super::McpTool), which
//! is how the agent's tool calls reach a server, and
//! [`McpClientManager::call_tool_cancellable`](super::McpClientManager::call_tool_cancellable),
//! which is how the host cancels one for a reason of its own. Before
//! this module the cancellable path existed with no production caller
//! and `McpTool` used a bare `call_tool`, so an MCP server that
//! accepted a request and never answered held the invocation forever
//! (review finding B2, <https://github.com/bricef/factor-q/issues/547>).
//!
//! Cancellation is the host's `cancel` future rather than rmcp's
//! `PeerRequestOptions::with_timeout`. rmcp applies its own timeout
//! inside `RequestHandle::await_response`, which this path deliberately
//! does not use: the host cancels for reasons wider than a deadline —
//! shutdown, budget, a superseded step — and one mechanism that covers
//! all of them beats two that overlap. What rmcp's timeout *does* on
//! expiry is exactly what happens here anyway: send
//! `notifications/cancelled`, which asks the server to stop and, on the
//! way out, drops the request from rmcp's own local responder pool.

use std::sync::Arc;

use fq_tools::ToolCallIdentity;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, CancelledNotificationParam,
    ClientRequest, JsonObject, ServerResult,
};
use rmcp::service::PeerRequestOptions;

use super::progress::ProgressRegistry;
use super::{McpClient, McpError};

/// One outbound `tools/call`: who to send it to, what to send, and
/// which of the host's calls it belongs to.
pub(super) struct OutboundCall<'a> {
    pub(super) client: &'a Arc<McpClient>,
    /// The server's factor-q name — half the progress-correlation key,
    /// because rmcp numbers its tokens per peer.
    pub(super) server: &'a str,
    /// The name as the *server* knows it (no `<server>__` prefix).
    pub(super) remote_tool_name: &'a str,
    /// The canonical name, for error messages the model reads.
    pub(super) tool_name: &'a str,
    pub(super) arguments: JsonObject,
    pub(super) progress: &'a ProgressRegistry,
    /// The invocation and tool call this belongs to, when the caller
    /// has one. `None` leaves the call uncorrelated: progress from it
    /// is still traced, just attributed to nothing.
    pub(super) call: Option<&'a ToolCallIdentity>,
}

/// Send the request and race it against `cancel`.
///
/// Returns `Some(result)` when the server answered first. When `cancel`
/// fires, sends `notifications/cancelled` — best effort; the host stops
/// awaiting either way — and returns `None`.
///
/// No `_meta` progress token is attached: rmcp's peer layer mints one
/// for every outbound request and overwrites any the host sets (#605).
/// The minted token is read back off the request handle and recorded
/// against this call, so the progress the server reports under it can
/// be attributed. The entry is cleared by a guard, so it goes whether
/// the call returns, errors, is cancelled, or has its future dropped
/// outright by the host's backstop timer.
pub(super) async fn call_tool_cancellable<F>(
    call: OutboundCall<'_>,
    cancel: F,
) -> Result<Option<CallToolResult>, McpError>
where
    F: std::future::Future<Output = ()>,
{
    let params = CallToolRequestParams::new(call.remote_tool_name.to_string())
        .with_arguments(call.arguments);
    let tool_call_error = |reason: String| McpError::ToolCall {
        tool_name: call.tool_name.to_string(),
        reason,
    };
    let mut handle = call
        .client
        .peer()
        .send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(params)),
            PeerRequestOptions::no_options(),
        )
        .await
        .map_err(|err| tool_call_error(err.to_string()))?;

    let _in_flight = call.call.map(|identity| {
        call.progress.issue(
            call.server,
            &handle.progress_token,
            identity.invocation_id.clone(),
            identity.call_id.clone(),
            call.tool_name.to_string(),
        )
    });

    // Clone what's needed to cancel without consuming the handle
    // (the `select!` borrows `handle.rx`).
    let request_id = handle.id.clone();
    let peer = handle.peer.clone();

    tokio::pin!(cancel);
    tokio::select! {
        result = &mut handle.rx => match result {
            Ok(Ok(ServerResult::CallToolResult(result))) => Ok(Some(result)),
            Ok(Ok(_)) => Err(tool_call_error("unexpected response type".to_string())),
            Ok(Err(err)) => Err(tool_call_error(err.to_string())),
            Err(_) => Err(tool_call_error("transport closed".to_string())),
        },
        _ = &mut cancel => {
            // Best-effort: tell the server to abort. We stop
            // awaiting the response regardless.
            let _ = peer
                .notify_cancelled(CancelledNotificationParam {
                    request_id,
                    reason: Some("cancelled by host".to_string()),
                })
                .await;
            Ok(None)
        }
    }
}
