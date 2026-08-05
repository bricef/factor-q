//! Subject hierarchy for factor-q events — split from
//! `events.rs` to keep that file inside its size budget.
pub const SYSTEM_STARTUP: &str = "fq.system.startup";
pub const SYSTEM_SHUTDOWN: &str = "fq.system.shutdown";
pub const SYSTEM_TASK_FAILED: &str = "fq.system.task_failed";
pub const SYSTEM_RECOVERY: &str = "fq.system.recovery";
/// Daemon-scoped log records forwarded from connected MCP servers
/// (ADR-0020).
pub const SYSTEM_MCP_LOG: &str = "fq.system.mcp.log";
/// Where an [`crate::events::EventPayload::Unknown`] would route if it
/// were ever published. It never is — the variant only exists so a
/// newer daemon's event type deserialises in an older binary — but
/// `subject()` is total, so the case needs an answer.
pub const SYSTEM_UNKNOWN: &str = "fq.system.unknown";

/// Validate that `s` is safe to use as a single NATS subject
/// token. NATS subjects are dot-separated tokens; a token
/// must be non-empty and must not contain `.`, `*`, `>`, or
/// whitespace. This is the shared predicate used by every
/// id-newtype whose value lands in a subject string
/// (currently [`crate::agent::AgentId`] and
/// [`crate::worker::WorkerId`]). Wrapping the constraint in
/// a single function means the validation can't drift
/// between sites.
pub fn validate_token(s: &str) -> Result<(), SubjectTokenError> {
    if s.is_empty() {
        return Err(SubjectTokenError::Empty);
    }
    for ch in s.chars() {
        if ch == '.' || ch == '*' || ch == '>' || ch.is_whitespace() {
            return Err(SubjectTokenError::InvalidChar(s.to_string()));
        }
    }
    Ok(())
}

/// Failure mode from [`validate_token`].
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum SubjectTokenError {
    #[error("must not be empty")]
    Empty,
    #[error("must not contain '.', '*', '>', or whitespace: {0:?}")]
    InvalidChar(String),
}

pub fn agent_triggered(agent_id: &str) -> String {
    format!("fq.agent.{agent_id}.triggered")
}

pub fn agent_llm_request(agent_id: &str) -> String {
    format!("fq.agent.{agent_id}.llm.request")
}

pub fn agent_llm_response(agent_id: &str) -> String {
    format!("fq.agent.{agent_id}.llm.response")
}

pub fn agent_tool_call(agent_id: &str) -> String {
    format!("fq.agent.{agent_id}.tool.call")
}

pub fn agent_tool_dispatched(agent_id: &str) -> String {
    format!("fq.agent.{agent_id}.tool.dispatched")
}

pub fn agent_tool_result(agent_id: &str) -> String {
    format!("fq.agent.{agent_id}.tool.result")
}

pub fn agent_llm_dispatched(agent_id: &str) -> String {
    format!("fq.agent.{agent_id}.llm.dispatched")
}

/// An invocation cannot be auto-recovered (see
/// data-architecture.md §3.4). The worker publishes this
/// on startup when its WAL categorisation finds a
/// `dispatched`-without-`completed` row.
pub fn agent_invocation_ambiguous(agent_id: &str) -> String {
    format!("fq.agent.{agent_id}.invocation.ambiguous")
}

/// Worker → control-plane archive hand-off (step 8 of
/// data-architecture.md). Emitted by the worker after an
/// invocation reaches terminal state, carrying the final
/// state blob the control-plane writes into
/// `invocation_archive`.
pub fn agent_invocation_archived(agent_id: &str) -> String {
    format!("fq.agent.{agent_id}.invocation.archived")
}

/// Operator → control-plane (step 9). Emitted by an
/// operator-issued `fq invocation drop` (or future
/// recovery actions) so audit can distinguish operator-
/// triggered terminal transitions from worker-triggered
/// ones. The coordination consumer's existing
/// `fq.agent.*.invocation.*` filter picks it up.
pub fn agent_invocation_operator_recovered(agent_id: &str) -> String {
    format!("fq.agent.{agent_id}.invocation.operator_recovered")
}

pub fn agent_invocation_operator_resumed(agent_id: &str) -> String {
    format!("fq.agent.{agent_id}.invocation.operator_resumed")
}

/// A durable host notice injected into the invocation's
/// conversation at a step boundary (#155). Deliberately outside
/// the coordination consumer's `fq.agent.*.invocation.*` filter —
/// notices are conversation-plane observability, not coordination.
pub fn agent_host_notice(agent_id: &str) -> String {
    format!("fq.agent.{agent_id}.host_notice")
}

pub fn agent_invocation_summary(agent_id: &str) -> String {
    format!("fq.agent.{agent_id}.invocation_summary")
}

pub fn agent_completed(agent_id: &str) -> String {
    format!("fq.agent.{agent_id}.completed")
}

pub fn agent_failed(agent_id: &str) -> String {
    format!("fq.agent.{agent_id}.failed")
}

/// Every agent's `failed` subject — the dead-letter scan's filter.
pub const ALL_AGENTS_FAILED: &str = "fq.agent.*.failed";

/// Worker liveness signal. Emitted periodically by each
/// worker; the control-plane's heartbeat consumer updates
/// `coordination_worker.last_heartbeat` on receipt. The
/// stale-worker sweep in the coordination consumer marks a
/// worker stale when this signal falls behind its threshold.
pub fn worker_heartbeat(worker_id: &str) -> String {
    format!("fq.worker.{worker_id}.heartbeat")
}

/// Worker liveness transition emitted once when a heartbeat lapses.
pub fn worker_orphaned(worker_id: &str) -> String {
    format!("fq.worker.{worker_id}.orphaned")
}

/// Control-plane → worker acknowledgement of an
/// `invocation.archived` hand-off. Worker-scoped so a worker
/// can subscribe to its own acks with a single filter
/// (mirrors the heartbeat naming). The invocation_id lives
/// on the envelope.
pub fn worker_invocation_archive_acked(worker_id: &str) -> String {
    format!("fq.worker.{worker_id}.invocation.archive_acked")
}
