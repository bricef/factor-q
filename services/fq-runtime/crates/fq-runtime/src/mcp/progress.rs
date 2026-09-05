//! Progress tokens, as they reach the host.
//!
//! A server may only report progress against a request whose `_meta`
//! carried a `progressToken`, and rmcp's peer layer mints one for every
//! outbound request — so every call factor-q makes is already
//! progress-capable and the host mints none of its own. What arrives
//! back on `notifications/progress` is that token, which the handler
//! renders here for the neutral
//! [`ServerNotification::Progress`](super::ServerNotification).
//!
//! Correlating an inbound token with the call that caused it needs
//! `RequestHandle::progress_token` — the token actually on the wire —
//! and is issue #605.

use rmcp::model::{NumberOrString, ProgressToken};

/// Render a progress token to a string (rmcp's are numeric, but a
/// server may echo a string token).
pub(super) fn progress_token_string(token: &ProgressToken) -> String {
    match &token.0 {
        NumberOrString::Number(n) => n.to_string(),
        NumberOrString::String(s) => s.to_string(),
    }
}
