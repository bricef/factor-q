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
    /// Which of the two optional durables this daemon actually runs:
    /// the summariser (only when `[summary]` names a model) and the
    /// maintenance consumer (`[maintenance] enabled`). Health expects
    /// a durable only when it is configured — reporting one missing
    /// that the daemon was told not to create would be a permanent red
    /// nobody can clear (#549).
    pub enabled_consumers: fq_runtime::health::EnabledConsumers,
    /// The live `server → starting | ready | unavailable` table for the
    /// shared MCP servers (#548). Both health reports name the
    /// unavailable ones, because an unavailable server is a standing
    /// degradation nothing else reports: boot carried on without it,
    /// and the only other place it surfaces is the terminal refusal of
    /// an agent that needed it.
    pub mcp_servers: fq_runtime::McpServerStates,
    /// The worker's provider throttle (#278) — the same value the LLM
    /// stack takes permits from and the dispatcher holds triggers on.
    /// Both health reports list the models it is holding back; an
    /// operator wondering why the fleet went quiet reads the pause here.
    pub throttle: std::sync::Arc<fq_runtime::llm::ModelThrottle>,
    /// The per-agent in-flight count (#718) — the same value the
    /// dispatcher admits triggers against and every resume path counts
    /// into. `control.doctor` names the agents it is holding work back
    /// for, so a queue that is not moving has a reason an operator can
    /// read.
    pub agent_caps: std::sync::Arc<fq_runtime::control_plane::agent_cap::AgentConcurrency>,
    /// The worker id this process claims triggers under — new on every
    /// start. `control.doctor` reads a trigger claim under any other id as
    /// one a process that has exited left behind.
    pub worker_id: String,
    /// When this daemon assembled its operator surface, epoch ms: moments
    /// before its dispatcher starts consuming. `control.doctor` gives a
    /// claim an exited process left the trigger redelivery window from
    /// here before it calls the claim abandoned.
    pub dispatching_since_ms: i64,
    /// `[state] retention_days` in milliseconds: the window after which
    /// the retention sweep ages failures and dead letters out of the
    /// projection. `control.doctor` stops listing a dead trigger claim at
    /// the same age, so a claim's line goes when the dead letter it points
    /// at does. `None` when the sweep is off.
    pub retention_ms: Option<i64>,
}
