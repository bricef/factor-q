//! `[mcp]` — what an MCP server is allowed to cost the daemon.
//!
//! Its own module for the same reason `[tools]` has one: the keys are a
//! policy rather than a bag of numbers. Together they say that no
//! server, however badly behaved, can stop the daemon booting or make
//! it allocate without bound — deadlines on the two calls a start makes,
//! caps on what discovery may return, a bound on one line of stdio, and
//! how patiently a server that is down is tried again.
//!
//! Every number is here rather than in code because an operator is the
//! one who knows their servers: a locally-installed stdio server
//! answers in milliseconds, and a remote one behind a cold serverless
//! endpoint may legitimately want a minute (Design Principle 8 —
//! tunable parameters are configuration, not code).

use std::time::Duration;

use serde::Deserialize;

use super::ConfigError;
use crate::mcp::McpLimits;

/// MCP configuration — `[mcp]` in `fqd.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct McpConfig {
    /// How long the `initialize` handshake may take, in seconds.
    /// Default 30. A server that has not answered by then is marked
    /// unavailable and boot carries on without it; before this existed
    /// a server that accepted the connection and never answered held
    /// the daemon at startup with every agent down.
    #[serde(default = "default_startup_timeout_secs")]
    pub startup_timeout_secs: u64,
    /// How long discovering a server's tools may take, in seconds,
    /// across every page of `tools/list`. Default 30. The whole walk,
    /// not one request: a server with unboundedly many quick pages is
    /// as effective a wedge as one slow answer.
    #[serde(default = "default_discovery_timeout_secs")]
    pub discovery_timeout_secs: u64,
    /// The most `tools/list` pages one discovery may follow. Default
    /// 100. A server that keeps returning a `next_cursor` — by design
    /// or through a cursor bug that never advances — is cut off here.
    #[serde(default = "default_max_discovery_pages")]
    pub max_discovery_pages: u32,
    /// The most tools one server may advertise, counted across pages.
    /// Default 1000. Every tool becomes a schema in every prompt the
    /// agents using that server send, so a runaway list costs tokens on
    /// every turn as well as memory.
    #[serde(default = "default_max_tools")]
    pub max_tools: u32,
    /// The longest line the stdio transport reads from a server before
    /// refusing the connection, in bytes. Default 1048576 (1 MiB). One
    /// JSON-RPC message is one line, so this bounds what a single
    /// message may make the daemon buffer.
    #[serde(default = "default_max_line_bytes")]
    pub max_line_bytes: usize,
    /// How long to wait before the first retry of an unavailable
    /// server, in seconds, doubling per attempt. Default 30. `0`
    /// disables retrying, which is the only way to ask for
    /// "unavailable until the daemon restarts".
    #[serde(default = "default_retry_initial_secs")]
    pub retry_initial_secs: u64,
    /// The ceiling the doubling saturates at, in seconds. Default 600.
    /// A server that is down for an afternoon is then dialled once every
    /// ten minutes rather than once every thirty seconds.
    #[serde(default = "default_retry_max_secs")]
    pub retry_max_secs: u64,
}

fn default_startup_timeout_secs() -> u64 {
    30
}

fn default_discovery_timeout_secs() -> u64 {
    30
}

fn default_max_discovery_pages() -> u32 {
    100
}

fn default_max_tools() -> u32 {
    1_000
}

fn default_max_line_bytes() -> usize {
    1024 * 1024
}

fn default_retry_initial_secs() -> u64 {
    30
}

fn default_retry_max_secs() -> u64 {
    600
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            startup_timeout_secs: default_startup_timeout_secs(),
            discovery_timeout_secs: default_discovery_timeout_secs(),
            max_discovery_pages: default_max_discovery_pages(),
            max_tools: default_max_tools(),
            max_line_bytes: default_max_line_bytes(),
            retry_initial_secs: default_retry_initial_secs(),
            retry_max_secs: default_retry_max_secs(),
        }
    }
}

impl McpConfig {
    /// No bound may be zero.
    ///
    /// Checked rather than clamped for the reason `[tools]`' own
    /// reconciliation is: the consequence of a zero here is invisible
    /// at the setting — the daemon boots, and every MCP server is
    /// simply unavailable for as long as it runs — so the operator
    /// would read a number that was never in force. `retry_initial_secs`
    /// is exempt: zero there is the documented way to ask for
    /// "unavailable until restart".
    pub(super) fn validate(&self) -> Result<(), ConfigError> {
        let zero = |key| Err(ConfigError::McpZeroBound { key });
        if self.startup_timeout_secs == 0 {
            return zero("startup_timeout_secs");
        }
        if self.discovery_timeout_secs == 0 {
            return zero("discovery_timeout_secs");
        }
        if self.max_discovery_pages == 0 {
            return zero("max_discovery_pages");
        }
        if self.max_tools == 0 {
            return zero("max_tools");
        }
        if self.max_line_bytes == 0 {
            return zero("max_line_bytes");
        }
        Ok(())
    }

    /// The bounds as the MCP manager takes them.
    pub fn to_limits(&self) -> McpLimits {
        McpLimits {
            startup_timeout: Duration::from_secs(self.startup_timeout_secs),
            discovery_timeout: Duration::from_secs(self.discovery_timeout_secs),
            max_discovery_pages: self.max_discovery_pages,
            max_tools: self.max_tools,
            max_line_bytes: self.max_line_bytes,
            retry_initial: Duration::from_secs(self.retry_initial_secs),
            retry_max: Duration::from_secs(self.retry_max_secs),
        }
    }
}
