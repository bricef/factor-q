//! `[edge]` — the authenticated operator edge's own configuration.
//!
//! Its own module because the section grew a connection budget: the
//! bind address and the read gate were two fields, and the caps that
//! bound what an unauthenticated peer can make the daemon allocate are
//! four more, each of which has to explain itself. The parent stays a
//! table of contents.

use serde::Deserialize;

/// The authenticated operator edge (ADR-0006 + ADR-0031): the tarpc
/// `invoke`/`next_batch` surface operator clients speak. Born
/// authenticated — every connection presents a capability token — so
/// a non-loopback bind is the operator's choice, not a refusal.
///
/// **Not optional.** The edge is the only way into a running daemon:
/// `fq` has no NATS path of its own, so a daemon without one cannot be
/// inspected, reloaded or stopped by anything but a signal. There is
/// no `enabled` key, and the bound listener is also the daemon's
/// single-instance lock — it is taken before the worker registers, so
/// a second daemon on the same address loses at `bind(2)` having
/// caused no side effect.
#[derive(Debug, Clone, Deserialize)]
pub struct EdgeConfig {
    /// Bind address for the TLS listener.
    #[serde(default = "default_edge_bind")]
    pub bind: String,
    /// Upper bound, in milliseconds, on how long a `min_seq`-gated
    /// read waits for the projection fold to catch up before
    /// answering `Lagging`. Config, not code (Design Principle 8).
    #[serde(default = "default_edge_min_seq_wait_ms")]
    pub min_seq_wait_ms: u64,
    /// Ceiling on connections the edge holds at once, authenticated or
    /// not. Past it the accept loop stops taking sockets off the
    /// backlog, so further peers queue and are then refused by the
    /// kernel — backpressure an operator can see, rather than an fd
    /// table the daemon runs out of.
    #[serde(default = "default_edge_max_connections")]
    pub max_connections: usize,
    /// Ceiling on connections in the pre-auth phase (TLS handshake and
    /// token read). Tighter than `max_connections` because a handshake
    /// costs CPU where an established connection costs an fd, and
    /// because everything in this phase is by definition anonymous.
    #[serde(default = "default_edge_max_pre_auth_connections")]
    pub max_pre_auth_connections: usize,
    /// Ceiling on in-flight requests per authenticated connection, so
    /// one client cannot queue unbounded work on the daemon.
    #[serde(default = "default_edge_max_concurrent_requests")]
    pub max_concurrent_requests: usize,
    /// How long the accept loop pauses after a failed `accept`, in
    /// milliseconds. Tokio does not clear a listener's readiness on
    /// `EMFILE`, so retrying immediately turns file-descriptor
    /// exhaustion into a spinning worker thread that starves the very
    /// tasks holding those descriptors.
    #[serde(default = "default_edge_accept_error_backoff_ms")]
    pub accept_error_backoff_ms: u64,
}

impl Default for EdgeConfig {
    fn default() -> Self {
        Self {
            bind: default_edge_bind(),
            min_seq_wait_ms: default_edge_min_seq_wait_ms(),
            max_connections: default_edge_max_connections(),
            max_pre_auth_connections: default_edge_max_pre_auth_connections(),
            max_concurrent_requests: default_edge_max_concurrent_requests(),
            accept_error_backoff_ms: default_edge_accept_error_backoff_ms(),
        }
    }
}

fn default_edge_min_seq_wait_ms() -> u64 {
    2_000
}

fn default_edge_max_connections() -> usize {
    256
}

fn default_edge_max_pre_auth_connections() -> usize {
    64
}

fn default_edge_max_concurrent_requests() -> usize {
    32
}

fn default_edge_accept_error_backoff_ms() -> u64 {
    100
}

fn default_edge_bind() -> String {
    "127.0.0.1:9472".to_string()
}
