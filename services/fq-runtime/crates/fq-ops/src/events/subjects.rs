//! The `fq.*` wire vocabulary — every subject the runtime publishes to
//! or filters on, spelled in one place.
//!
//! Originally split from `events.rs` to keep that file inside its size
//! budget, so the module sits under `events`; its scope is wider than
//! that name suggests and always has been. It already owns the
//! `fq.system.*` and `fq.worker.*` namespaces alongside `fq.agent.*`,
//! and it owns `fq.trigger.*` too (#43).
//!
//! **One namespace, one home.** The trigger subjects lived in `bus.rs`
//! until #43 — a second vocabulary, in the transport module, outside
//! the reach of [`validate_token`] below. That is the shape #453 came
//! from: a consumer filtering `fq.agent.*.llm_response` against a
//! constructor emitting `fq.agent.*.llm.response`, so the rolling
//! summary never fired once. A subject that is spelled in two places
//! is a subject that can be spelled two ways.
pub const SYSTEM_STARTUP: &str = "fq.system.startup";
pub const SYSTEM_SHUTDOWN: &str = "fq.system.shutdown";
pub const SYSTEM_TASK_FAILED: &str = "fq.system.task_failed";
pub const SYSTEM_RECOVERY: &str = "fq.system.recovery";
/// Daemon-scoped log records forwarded from connected MCP servers
/// (ADR-0020).
pub const SYSTEM_MCP_LOG: &str = "fq.system.mcp.log";
/// The outcome of one maintenance task run (#257): what ran, under
/// which run id, and how it ended. One subject for every outcome —
/// success, failure, and the refusal of a task name this build does
/// not know — because they are one fact with three endings, and a
/// reader that wants only the failures reads the payload rather than a
/// second subject that could drift from the first.
pub const SYSTEM_MAINTENANCE: &str = "fq.system.maintenance";
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

/// The other terminal outcome of an LLM call: the provider errored,
/// or returned nothing. Sibling of `llm.response`, so
/// `fq.agent.*.llm.>` already matches it.
pub fn agent_llm_failure(agent_id: &str) -> String {
    format!("fq.agent.{agent_id}.llm.failure")
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

/// An in-flight invocation has stopped crossing step boundaries.
/// Emitted by the control plane's stuck sweep, once per crossing.
/// Agent-scoped, like `invocation.ambiguous`: it is a fact about one
/// invocation, and it rides the same `fq.agent.*.invocation.*`
/// namespace.
pub fn agent_invocation_stuck(agent_id: &str) -> String {
    format!("fq.agent.{agent_id}.invocation.stuck")
}

/// Emitted by the worker when it puts an invocation down for a rate
/// limit (<https://github.com/bricef/factor-q/issues/278>). Inside
/// `fq.agent.*.invocation.*`, like every other per-invocation fact.
pub fn agent_invocation_deferred(agent_id: &str) -> String {
    format!("fq.agent.{agent_id}.invocation.deferred")
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

/// The trigger namespace: one subject per agent, `fq.trigger.<agent_id>`.
///
/// Triggers ride their own JetStream stream rather than the event
/// stream — work-queue-ish delivery, 24h retention, no compression —
/// so `fq.trigger.>` is deliberately disjoint from the event stream's
/// subject set (the runtime's `bus::EVENT_STREAM_SUBJECTS`). NATS
/// forbids two streams claiming overlapping subjects, and that
/// disjointness is what keeps the two streams legal.
///
/// The spelling lives here with the rest of the vocabulary; the
/// domain-typed constructor callers should reach for is the runtime's
/// `trigger::subject`, which takes an [`AgentId`](crate::agent::AgentId)
/// and so cannot be handed a token that would break the subject.
pub const TRIGGER_PREFIX: &str = "fq.trigger.";

/// Every agent's triggers. Two roles, one string: the subject set the
/// trigger stream captures, and the dispatcher's default consumer
/// filter.
pub const ALL_TRIGGERS: &str = "fq.trigger.>";

/// One agent's trigger subject.
///
/// Prefer the runtime's `trigger::subject` — it takes a validated
/// [`AgentId`](crate::agent::AgentId) instead of a bare `&str`. This
/// raw form exists for the transport's own stream wiring and for
/// parsing paths that only ever hold a `&str`.
pub fn trigger(agent_id: &str) -> String {
    format!("{TRIGGER_PREFIX}{agent_id}")
}

/// The maintenance namespace: one subject per task,
/// `fq.maintenance.<task>` (#257).
///
/// This is the **inbound** half of maintenance — a command an external
/// scheduler (fq-cron) publishes, asking the daemon to run a named
/// task. The outcome it produces is an event on
/// [`SYSTEM_MAINTENANCE`], which is a different namespace on a
/// different stream, and deliberately so: the event log is the system
/// of record for *facts* ([ADR-0026]), and a command is not one.
///
/// Like `fq.trigger.>`, this rides its own JetStream stream (the
/// runtime's `bus::MAINTENANCE_STREAM_NAME`) rather than the event
/// stream — short retention, no compression, and no envelope version,
/// since the body is an opaque scheduler payload rather than an
/// [`crate::events::Event`]. NATS forbids two streams claiming
/// overlapping subjects, so `fq.maintenance.>` is disjoint from both
/// the event stream's subject set and `fq.trigger.>`, and that
/// disjointness is what keeps all three streams legal.
///
/// [ADR-0026]: https://github.com/bricef/factor-q/blob/main/docs/adrs/accepted/0026-event-log-system-of-record.md
pub const MAINTENANCE_PREFIX: &str = "fq.maintenance.";

/// Every maintenance task's subject. Two roles, one string: the
/// subject set the maintenance stream captures, and the maintenance
/// consumer's durable filter.
pub const ALL_MAINTENANCE: &str = "fq.maintenance.>";

/// One maintenance task's subject.
///
/// Takes a `&str` because the task name on the wire is whatever the
/// scheduler published — the daemon's own closed registry
/// (`control_plane::maintenance::MaintenanceTask`) is what turns a
/// token into a task, and refuses the ones it does not know.
pub fn maintenance(task: &str) -> String {
    format!("{MAINTENANCE_PREFIX}{task}")
}

/// Recover the task token from a maintenance subject.
///
/// Exactly three dot-separated tokens, and the task is the whole of
/// the third — the same shape as [`agent_id_from_trigger`], so a
/// dotted tail (`fq.maintenance.a.b`) is refused rather than silently
/// truncated to its first token.
pub fn task_from_maintenance(subject: &str) -> Option<&str> {
    let mut parts = subject.splitn(3, '.');
    let (first, second, third) = (parts.next()?, parts.next()?, parts.next()?);
    if first != "fq" || second != "maintenance" || validate_token(third).is_err() {
        return None;
    }
    Some(third)
}

/// Recover the agent id from a trigger subject.
///
/// Agent ids are validated to contain no dots (see
/// [`validate_token`]), so a trigger subject has exactly three
/// dot-separated tokens and the id is the whole of the third.
pub fn agent_id_from_trigger(subject: &str) -> Option<&str> {
    let mut parts = subject.splitn(3, '.');
    let first = parts.next()?;
    let second = parts.next()?;
    let third = parts.next()?;
    if first != "fq" || second != "trigger" || third.is_empty() {
        return None;
    }
    Some(third)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wildcard and the constructor are two literals, and a
    /// namespace spelled twice is a namespace that can drift — which
    /// is the bug in #453, one level up. Assert the relationship
    /// rather than trusting it.
    #[test]
    fn the_trigger_wildcard_and_constructor_share_one_namespace() {
        assert_eq!(ALL_TRIGGERS, format!("{TRIGGER_PREFIX}>"));
        assert!(trigger("researcher").starts_with(TRIGGER_PREFIX));
    }

    /// Same relationship, same reason, one namespace over (#257).
    #[test]
    fn the_maintenance_wildcard_and_constructor_share_one_namespace() {
        assert_eq!(ALL_MAINTENANCE, format!("{MAINTENANCE_PREFIX}>"));
        assert!(maintenance("ping").starts_with(MAINTENANCE_PREFIX));
    }

    #[test]
    fn a_maintenance_subject_round_trips_through_its_task() {
        assert_eq!(task_from_maintenance(&maintenance("ping")), Some("ping"));
    }

    /// A dotted tail is not a task name with a dot in it — it is a
    /// subject this daemon has no task for, and saying so is the whole
    /// point of a closed registry.
    #[test]
    fn only_a_single_token_maintenance_subject_yields_a_task() {
        assert_eq!(task_from_maintenance("fq.maintenance.a.b"), None);
        assert_eq!(task_from_maintenance("fq.maintenance."), None);
        assert_eq!(task_from_maintenance("fq.maintenance"), None);
        assert_eq!(task_from_maintenance("fq.trigger.researcher"), None);
        assert_eq!(task_from_maintenance(""), None);
    }

    /// The three streams' subject sets must stay disjoint — NATS
    /// refuses overlapping claims, so an overlap is a daemon that will
    /// not start rather than a subtle routing bug.
    #[test]
    fn the_maintenance_namespace_overlaps_neither_triggers_nor_events() {
        assert!(!maintenance("ping").starts_with(TRIGGER_PREFIX));
        assert!(!maintenance("ping").starts_with("fq.system."));
        assert!(!maintenance("ping").starts_with("fq.agent."));
        assert!(!maintenance("ping").starts_with("fq.worker."));
        assert!(!SYSTEM_MAINTENANCE.starts_with(MAINTENANCE_PREFIX));
    }

    #[test]
    fn a_trigger_subject_round_trips_through_its_agent_id() {
        assert_eq!(
            agent_id_from_trigger(&trigger("researcher")),
            Some("researcher")
        );
    }

    #[test]
    fn only_a_trigger_subject_yields_an_agent_id() {
        assert_eq!(agent_id_from_trigger("fq.agent.researcher.completed"), None);
        assert_eq!(agent_id_from_trigger("fq.trigger."), None);
        assert_eq!(agent_id_from_trigger("fq.trigger"), None);
        assert_eq!(agent_id_from_trigger(""), None);
    }
}
