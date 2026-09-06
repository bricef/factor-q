//! Progress tokens, and the call each one belongs to.
//!
//! A server may only report progress against a request whose `_meta`
//! carried a `progressToken`, and rmcp's peer layer mints one for every
//! outbound request — overwriting any the host sets. So every call
//! factor-q makes is already progress-capable, the host mints none of
//! its own, and correlation has to run on rmcp's token rather than
//! against it (<https://github.com/bricef/factor-q/issues/605>).
//!
//! [`ProgressRegistry`] is that correlation. The call site records the
//! minted token (`RequestHandle::progress_token`, the token actually on
//! the wire) against the invocation and tool call that issued it; the
//! handler looks an inbound `notifications/progress` up by the same
//! key; the call site drops the entry when the call completes or is
//! cancelled, so the table is empty between calls and cannot grow
//! across a long invocation.
//!
//! The key is `(server, token)`, not the token alone. rmcp numbers its
//! tokens from zero **per peer**, so two connected servers both issue a
//! token `0` within seconds of each other and a token-only table would
//! attribute one server's progress to the other's call.
//!
//! Progress surfaces two ways, both cheap and neither a new event type:
//! a rate-limited `INFO` line carrying the invocation, the call and the
//! numbers, and a per-call *last progress at* timestamp the manager
//! exposes — the fact a stuck-call detector needs (#37) and the one
//! thing a bare log line cannot answer.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rmcp::model::{NumberOrString, ProgressToken};
use tracing::info;

/// Render a progress token to a string (rmcp's are numeric, but a
/// server may echo a string token).
pub(super) fn progress_token_string(token: &ProgressToken) -> String {
    match &token.0 {
        NumberOrString::Number(n) => n.to_string(),
        NumberOrString::String(s) => s.to_string(),
    }
}

/// One in-flight call, as progress correlation needs it.
#[derive(Debug, Clone)]
pub struct InFlightCall {
    /// The invocation the call belongs to.
    pub invocation_id: String,
    /// The model-issued id of the tool call within it.
    pub call_id: String,
    /// The canonical tool name, for the log line.
    pub tool_name: String,
    /// When the last `notifications/progress` for this call arrived —
    /// `None` if the server has reported none yet. A stuck-call
    /// detector reads this beside the call's start.
    pub last_progress_at: Option<Instant>,
    /// When the call was issued.
    pub started_at: Instant,
    /// When the last progress line was *logged*, for the rate limit.
    logged_at: Option<Instant>,
}

impl InFlightCall {
    /// How long since the last sign of life: the most recent progress
    /// report, or the call's start when there has been none.
    pub fn silent_for(&self) -> Duration {
        self.last_progress_at.unwrap_or(self.started_at).elapsed()
    }
}

/// The `(server, token) → call` table.
///
/// Shared by clone: the manager holds one, hands the same one to every
/// server's handler (which routes inbound progress) and to every
/// [`McpTool`](super::McpTool) it builds (which registers and clears
/// entries).
#[derive(Clone, Default)]
pub struct ProgressRegistry {
    calls: Arc<Mutex<HashMap<(String, String), InFlightCall>>>,
}

impl ProgressRegistry {
    /// At most one progress line per call per interval. A server is
    /// free to report progress per byte; the host's job is to make the
    /// call's liveness visible, not to relay a firehose. The timestamp
    /// is updated on *every* notification regardless — rate limiting
    /// the log must not rate-limit the fact.
    const LOG_INTERVAL: Duration = Duration::from_secs(10);

    /// Record a call as in flight under the token rmcp minted for it,
    /// and return the guard that clears it again.
    ///
    /// A guard rather than a matching `finished` call because the
    /// interesting exits are the ones nobody writes: the host's
    /// backstop timer drops the whole call future mid-await, and a
    /// transport error returns early. Both must leave the table empty,
    /// so cleanup rides on the scope rather than on remembering.
    pub(super) fn issue(
        &self,
        server: &str,
        token: &ProgressToken,
        invocation_id: String,
        call_id: String,
        tool_name: String,
    ) -> InFlightGuard {
        let now = Instant::now();
        let key = (server.to_string(), progress_token_string(token));
        self.calls
            .lock()
            .expect("progress registry poisoned")
            .insert(
                key.clone(),
                InFlightCall {
                    invocation_id,
                    call_id,
                    tool_name,
                    last_progress_at: None,
                    started_at: now,
                    logged_at: None,
                },
            );
        InFlightGuard {
            registry: self.clone(),
            key,
        }
    }

    /// Whether the table currently holds anything.
    pub fn is_empty(&self) -> bool {
        self.calls
            .lock()
            .expect("progress registry poisoned")
            .is_empty()
    }

    /// Attribute one inbound `notifications/progress` to the call that
    /// caused it, stamp the call's liveness, and log at most one line
    /// per call per [`LOG_INTERVAL`](Self::LOG_INTERVAL).
    ///
    /// Returns the call it was attributed to, or `None` for a token
    /// the host has no record of — a server reporting progress against
    /// a request that already finished, or one it invented.
    pub(super) fn record_progress(
        &self,
        server: &str,
        token: &str,
        progress: f64,
        total: Option<f64>,
    ) -> Option<InFlightCall> {
        let mut calls = self.calls.lock().expect("progress registry poisoned");
        let call = calls.get_mut(&(server.to_string(), token.to_string()))?;
        let now = Instant::now();
        call.last_progress_at = Some(now);
        let due = call
            .logged_at
            .is_none_or(|last| now.duration_since(last) >= Self::LOG_INTERVAL);
        if due {
            call.logged_at = Some(now);
            info!(
                target: "mcp.server.progress",
                server = %server,
                invocation_id = %call.invocation_id,
                call_id = %call.call_id,
                tool = %call.tool_name,
                progress,
                total = ?total,
                "MCP tool call reported progress"
            );
        }
        Some(call.clone())
    }

    /// Every call currently in flight, for a stuck-call detector (#37)
    /// and for the tests that assert the table empties.
    pub fn in_flight(&self) -> Vec<InFlightCall> {
        self.calls
            .lock()
            .expect("progress registry poisoned")
            .values()
            .cloned()
            .collect()
    }
}

/// Clears one in-flight entry when the call's scope ends, however it
/// ends: returned, errored, or dropped by the host's backstop timer.
pub(super) struct InFlightGuard {
    registry: ProgressRegistry,
    key: (String, String),
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.registry
            .calls
            .lock()
            .expect("progress registry poisoned")
            .remove(&self.key);
    }
}

#[cfg(test)]
mod tests;
