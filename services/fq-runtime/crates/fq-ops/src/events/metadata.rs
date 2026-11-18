//! Stable event metadata, co-located so a payload variant has one
//! exhaustive definition for its subject and schema. The projection
//! event type is derived from the schema id rather than maintained as a
//! fourth mapping.

use super::{EventPayload, subjects, transient};

impl EventPayload {
    pub fn subject(&self, agent: &str) -> String {
        match self {
            Self::Triggered(_) => subjects::agent_triggered(agent),
            Self::LlmRequest(_) => subjects::agent_llm_request(agent),
            Self::LlmResponse(_) => subjects::agent_llm_response(agent),
            Self::LlmFailure(_) => subjects::agent_llm_failure(agent),
            Self::ToolCall(_) => subjects::agent_tool_call(agent),
            Self::ToolDispatched(_) => subjects::agent_tool_dispatched(agent),
            Self::ToolResult(_) => subjects::agent_tool_result(agent),
            Self::LlmDispatched(_) => subjects::agent_llm_dispatched(agent),
            Self::InvocationAmbiguous(_) => subjects::agent_invocation_ambiguous(agent),
            Self::InvocationStuck(_) => subjects::agent_invocation_stuck(agent),
            Self::InvocationDeferred(_) => subjects::agent_invocation_deferred(agent),
            Self::InvocationArchived(_) => subjects::agent_invocation_archived(agent),
            Self::InvocationOperatorRecovered(_) => {
                subjects::agent_invocation_operator_recovered(agent)
            }
            Self::InvocationOperatorResumed(_) => {
                subjects::agent_invocation_operator_resumed(agent)
            }
            Self::Completed(_) => subjects::agent_completed(agent),
            Self::Failed(_) => subjects::agent_failed(agent),
            Self::HostNotice(_) => subjects::agent_host_notice(agent),
            Self::InvocationSummary(_) => subjects::agent_invocation_summary(agent),
            Self::SystemStartup(_) => subjects::SYSTEM_STARTUP.to_string(),
            Self::SystemShutdown(_) => subjects::SYSTEM_SHUTDOWN.to_string(),
            Self::SystemTaskFailed(_) => subjects::SYSTEM_TASK_FAILED.to_string(),
            Self::SystemRecovery(_) => subjects::SYSTEM_RECOVERY.to_string(),
            Self::McpServerLog(_) => subjects::SYSTEM_MCP_LOG.to_string(),
            Self::MaintenanceRun(_) => subjects::SYSTEM_MAINTENANCE.to_string(),
            Self::OperatorSignal(_) => subjects::SYSTEM_OPERATOR_SIGNAL.to_string(),
            Self::WorkerHeartbeat(p) => subjects::worker_heartbeat(p.worker_id.as_str()),
            Self::WorkerOrphaned(p) => subjects::worker_orphaned(p.worker_id.as_str()),
            Self::InvocationArchiveAcked(p) => {
                subjects::worker_invocation_archive_acked(p.worker_id.as_str())
            }
            Self::Unknown => subjects::SYSTEM_UNKNOWN.to_string(),
        }
    }

    pub fn schema_id(&self) -> &'static str {
        match self {
            Self::Triggered(_) => "factor-q/triggered@1",
            Self::LlmRequest(_) => "factor-q/llm_request@1",
            Self::LlmDispatched(_) => "factor-q/llm_dispatched@1",
            Self::LlmResponse(_) => "factor-q/llm_response@1",
            Self::LlmFailure(_) => "factor-q/llm_failure@1",
            Self::ToolCall(_) => "factor-q/tool_call@1",
            Self::ToolDispatched(_) => "factor-q/tool_dispatched@1",
            Self::ToolResult(_) => "factor-q/tool_result@1",
            Self::InvocationSummary(_) => "factor-q/invocation_summary@1",
            Self::Completed(_) => "factor-q/completed@1",
            Self::Failed(_) => "factor-q/failed@1",
            Self::HostNotice(_) => "factor-q/host_notice@1",
            Self::InvocationAmbiguous(_) => "factor-q/invocation_ambiguous@1",
            Self::InvocationStuck(_) => "factor-q/invocation_stuck@1",
            Self::InvocationDeferred(_) => "factor-q/invocation_deferred@1",
            Self::InvocationArchived(_) => "factor-q/invocation_archived@1",
            Self::InvocationArchiveAcked(_) => "factor-q/invocation_archive_acked@1",
            Self::InvocationOperatorRecovered(_) => "factor-q/invocation_operator_recovered@1",
            Self::InvocationOperatorResumed(_) => "factor-q/invocation_operator_resumed@1",
            Self::SystemStartup(_) => "factor-q/system_startup@1",
            Self::SystemShutdown(_) => "factor-q/system_shutdown@1",
            Self::SystemTaskFailed(_) => "factor-q/system_task_failed@1",
            Self::SystemRecovery(_) => "factor-q/system_recovery@1",
            Self::WorkerHeartbeat(_) => "factor-q/worker_heartbeat@1",
            Self::WorkerOrphaned(_) => "factor-q/worker_orphaned@1",
            Self::McpServerLog(_) => "factor-q/mcp_server_log@1",
            Self::MaintenanceRun(_) => "factor-q/maintenance_run@1",
            Self::OperatorSignal(_) => "factor-q/operator_signal@1",
            Self::Unknown => "factor-q/unknown@1",
        }
    }

    pub fn event_type(&self) -> &'static str {
        self.schema_id()
            .strip_prefix("factor-q/")
            .unwrap()
            .strip_suffix("@1")
            .unwrap()
    }

    /// Whether this event is **transient** — operational signal the
    /// operator surface does not serve. A property of the type, not
    /// of the instance: see [`transient`] for what that means and why
    /// the set lives in one place.
    pub fn is_transient(&self) -> bool {
        transient::includes(self.event_type())
    }
}
