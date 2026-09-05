//! Progress tokens: minted per outbound request, echoed back by the
//! server on `notifications/progress`.
//!
//! A server may only report progress against a request that carried a
//! `progressToken` in its `_meta`, so every outbound tool call attaches
//! one — both the plain call ([`McpTool::execute`](super::McpTool)) and
//! the cancellable one
//! ([`McpClientManager::call_tool_cancellable`](super::McpClientManager)).

use std::sync::atomic::{AtomicI64, Ordering};

use rmcp::model::{NumberOrString, ProgressToken};

/// Monotonic source of per-request progress tokens (Step 7). Each
/// outbound tool call gets a fresh token so a server that supports
/// progress can report against it via `notifications/progress`.
static PROGRESS_TOKEN_SEQ: AtomicI64 = AtomicI64::new(1);

pub(super) fn next_progress_token() -> ProgressToken {
    ProgressToken(NumberOrString::Number(
        PROGRESS_TOKEN_SEQ.fetch_add(1, Ordering::Relaxed),
    ))
}

/// Render a progress token to a string for the neutral
/// [`ServerNotification::Progress`](super::ServerNotification) (tokens
/// are numeric here, but a server may echo a string token).
pub(super) fn progress_token_string(token: &ProgressToken) -> String {
    match &token.0 {
        NumberOrString::Number(n) => n.to_string(),
        NumberOrString::String(s) => s.to_string(),
    }
}
