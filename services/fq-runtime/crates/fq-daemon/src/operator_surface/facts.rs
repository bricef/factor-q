//! What this daemon knows about itself that a caller cannot derive.
//!
//! Its own module because it is a *value*, and the file it came out of
//! is registration — declarations bound to handlers. Splitting them
//! also pays the file gate honestly: the facts grew a fifth and sixth
//! field when the surface learned to report a stuck invocation (#37)
//! and an unavailable MCP server (#548), and a data bundle is the part
//! of a registration file that should move.

/// The daemon's own facts, as `control.status` and `control.doctor`
/// answer them.
///
/// Grouped because they are one concept — where this process keeps its
/// state, what it calls stuck, and how long it will take to stop —
/// and because a reader must get every one of them from the daemon
/// rather than deriving any from a config it may not share.
pub struct DaemonFacts {
    pub db_paths: std::sync::Arc<fq_runtime::RuntimeDbPaths>,
    /// A pre-split `events.db`, if one is still on disk.
    pub legacy_events_db: std::sync::Arc<std::path::PathBuf>,
    pub drain_deadline_ms: u64,
    /// The stuck threshold this daemon derived from its call deadlines
    /// (`Config::stuck_after`) — the one number every liveness verdict
    /// this surface serves is judged against (#37).
    ///
    /// `control.doctor`'s executions block, the Invocation view's
    /// detail (`fq invocation show`, the dashboard's detail page),
    /// `invocation.active` (the dashboard's active table; no `fq` verb
    /// serves it yet) and the control plane's stuck sweep are all
    /// handed this, and
    /// `control.status` reports it. Anything given a different number
    /// would call the same invocation something else.
    pub stuck_after_ms: i64,
    /// Whether `[summary]` names a model. Health expects the summary
    /// durable only when one is configured — a daemon without a
    /// summariser has no such consumer, and reporting it missing would
    /// be a permanent red nobody can clear (#549).
    pub summary_enabled: bool,
    /// The live `server → starting | ready | unavailable` table for the
    /// shared MCP servers (#548). Both health reports name the
    /// unavailable ones, because an unavailable server is a standing
    /// degradation nothing else reports: boot carried on without it,
    /// and the only other place it surfaces is the terminal refusal of
    /// an agent that needed it.
    pub mcp_servers: fq_runtime::McpServerStates,
}
