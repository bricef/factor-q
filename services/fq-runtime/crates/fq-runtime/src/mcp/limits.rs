//! What one MCP server is allowed to cost the daemon: the deadlines
//! start-up and discovery run under, the caps discovery and the stdio
//! transport refuse past, and how often an unavailable server is tried
//! again.
//!
//! One value rather than seven arguments, because every one of these is
//! `[mcp]` in `fqd.toml` (Design Principle 8 — tunable parameters are
//! configuration, not code) and they travel together: the manager
//! applies the deadlines, discovery applies the caps, the stdio
//! transport applies the line bound, and the retry loop applies the
//! backoff.
//!
//! Before they existed, boot blocked on the first unresponsive server:
//! no deadline around the `initialize` handshake, `list_all_tools`
//! following `next_cursor` without bound, and a stdio codec that read a
//! line of any length (review finding B3,
//! <https://github.com/bricef/factor-q/issues/548>).

use std::time::Duration;

/// The bounds every MCP server runs under.
///
/// [`Default`] is the shipped `[mcp]` section: the numbers below are
/// the documented defaults, and the config type produces this exact
/// value when the table is absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpLimits {
    /// How long the `initialize` handshake may take before the server
    /// is abandoned and marked unavailable.
    pub startup_timeout: Duration,
    /// How long discovering a server's tools may take, across every
    /// page of `tools/list`.
    pub discovery_timeout: Duration,
    /// The most `tools/list` pages one discovery may follow. A server
    /// that keeps returning a `next_cursor` is cut off here.
    pub max_discovery_pages: u32,
    /// The most tools one server may advertise, counted across pages.
    pub max_tools: u32,
    /// The longest line the stdio transport will read from a server
    /// before refusing the connection. A JSON-RPC message is one line,
    /// so this bounds what a single message may make the daemon
    /// buffer.
    pub max_line_bytes: usize,
    /// The wait before the first retry of an unavailable server,
    /// doubling per attempt. `Duration::ZERO` disables retrying.
    pub retry_initial: Duration,
    /// The ceiling the doubling saturates at.
    pub retry_max: Duration,
}

impl Default for McpLimits {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(30),
            discovery_timeout: Duration::from_secs(30),
            max_discovery_pages: 100,
            max_tools: 1_000,
            max_line_bytes: 1024 * 1024,
            retry_initial: Duration::from_secs(30),
            retry_max: Duration::from_secs(600),
        }
    }
}

impl McpLimits {
    /// The wait before retry number `attempt` (1 is the first retry):
    /// [`retry_initial`](Self::retry_initial) doubled once per earlier
    /// attempt, capped at [`retry_max`](Self::retry_max). `None` when
    /// retrying is disabled.
    ///
    /// Doubling rather than a fixed interval because the two cases this
    /// serves are different lengths: a server whose package is being
    /// installed comes back in a minute, and a remote endpoint that is
    /// down for the afternoon should not be dialled every thirty
    /// seconds until it is.
    pub fn retry_backoff(&self, attempt: u32) -> Option<Duration> {
        if self.retry_initial.is_zero() {
            return None;
        }
        let shift = attempt.saturating_sub(1).min(32);
        let scaled = self
            .retry_initial
            .saturating_mul(2u32.saturating_pow(shift));
        Some(scaled.min(self.retry_max.max(self.retry_initial)))
    }
}
