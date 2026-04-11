//! Event schema for factor-q.
//!
//! See `docs/design/event-schema.md` for the full specification.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub const SCHEMA_VERSION: u32 = 1;

/// Subject hierarchy for factor-q events.
pub mod subjects {
    pub const SYSTEM_STARTUP: &str = "fq.system.startup";
    pub const SYSTEM_SHUTDOWN: &str = "fq.system.shutdown";

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

    pub fn agent_tool_result(agent_id: &str) -> String {
        format!("fq.agent.{agent_id}.tool.result")
    }

    pub fn agent_cost(agent_id: &str) -> String {
        format!("fq.agent.{agent_id}.cost")
    }

    pub fn agent_completed(agent_id: &str) -> String {
        format!("fq.agent.{agent_id}.completed")
    }

    pub fn agent_failed(agent_id: &str) -> String {
        format!("fq.agent.{agent_id}.failed")
    }
}

/// Envelope wrapping every factor-q event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub schema_version: u32,
    pub event_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub agent_id: String,
    pub invocation_id: Uuid,
    #[serde(flatten)]
    pub payload: EventPayload,
}

impl Event {
    /// Construct a new event for the given agent and invocation.
    pub fn new(agent_id: impl Into<String>, invocation_id: Uuid, payload: EventPayload) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            event_id: Uuid::now_v7(),
            timestamp: Utc::now(),
            agent_id: agent_id.into(),
            invocation_id,
            payload,
        }
    }

    /// Return the NATS subject this event should be published on.
    pub fn subject(&self) -> String {
        match &self.payload {
            EventPayload::Triggered(_) => subjects::agent_triggered(&self.agent_id),
            EventPayload::LlmRequest(_) => subjects::agent_llm_request(&self.agent_id),
            EventPayload::LlmResponse(_) => subjects::agent_llm_response(&self.agent_id),
            EventPayload::ToolCall(_) => subjects::agent_tool_call(&self.agent_id),
            EventPayload::ToolResult(_) => subjects::agent_tool_result(&self.agent_id),
            EventPayload::Cost(_) => subjects::agent_cost(&self.agent_id),
            EventPayload::Completed(_) => subjects::agent_completed(&self.agent_id),
            EventPayload::Failed(_) => subjects::agent_failed(&self.agent_id),
        }
    }
}

/// Per-type event payloads, tagged by `event_type`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event_type", content = "payload", rename_all = "snake_case")]
pub enum EventPayload {
    Triggered(TriggeredPayload),
    LlmRequest(LlmRequestPayload),
    LlmResponse(LlmResponsePayload),
    ToolCall(ToolCallPayload),
    ToolResult(ToolResultPayload),
    Cost(CostPayload),
    Completed(CompletedPayload),
    Failed(FailedPayload),
}

/// Published when an agent invocation begins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggeredPayload {
    pub trigger_source: TriggerSource,
    pub trigger_subject: Option<String>,
    pub trigger_payload: Value,
    pub config_snapshot: ConfigSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerSource {
    Manual,
    Subject,
    Schedule,
}

/// Snapshot of the agent's configuration at trigger time.
///
/// Captured on `triggered` so that replay is meaningful even if the agent
/// definition is later modified.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    pub name: String,
    pub model: String,
    pub system_prompt: String,
    pub tools: Vec<String>,
    pub sandbox: SandboxSnapshot,
    pub budget: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxSnapshot {
    #[serde(default)]
    pub fs_read: Vec<String>,
    #[serde(default)]
    pub fs_write: Vec<String>,
    #[serde(default)]
    pub network: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
}

/// Published immediately before an LLM call is made.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequestPayload {
    pub call_id: Uuid,
    pub model: String,
    pub messages: Vec<Message>,
    pub tools_available: Vec<ToolSchema>,
    pub request_params: RequestParams,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<MessageToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageToolCall {
    pub tool_call_id: Uuid,
    pub tool_name: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

/// Published when an LLM call returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponsePayload {
    pub call_id: Uuid,
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<MessageToolCall>,
    pub stop_reason: StopReason,
    pub usage: TokenUsage,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    ToolUse,
    EndTurn,
    MaxTokens,
    StopSequence,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)]
    pub cache_read_tokens: u32,
    #[serde(default)]
    pub cache_write_tokens: u32,
}

/// Published when the agent invokes a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallPayload {
    pub tool_call_id: Uuid,
    pub tool_name: String,
    pub parameters: Value,
}

/// Published when a tool invocation completes (success or failure).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultPayload {
    pub tool_call_id: Uuid,
    pub output: String,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<ToolErrorKind>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorKind {
    SandboxViolation,
    InvalidParameters,
    ExecutionFailed,
    Timeout,
    PermissionDenied,
}

/// Published after each LLM response with cost attribution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostPayload {
    pub call_id: Uuid,
    pub model: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)]
    pub cache_read_tokens: u32,
    #[serde(default)]
    pub cache_write_tokens: u32,
    pub input_cost: f64,
    pub output_cost: f64,
    pub total_cost: f64,
    pub cumulative_invocation_cost: f64,
    pub cumulative_agent_cost: f64,
}

/// Published when an invocation finishes successfully.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletedPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<String>,
    pub total_llm_calls: u32,
    pub total_tool_calls: u32,
    pub total_cost: f64,
    pub total_duration_ms: u64,
}

/// Published when an invocation terminates with an error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailedPayload {
    pub error_kind: FailureKind,
    pub error_message: String,
    pub phase: FailurePhase,
    pub partial_totals: InvocationTotals,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    BudgetExceeded,
    LlmError,
    ToolError,
    SandboxViolation,
    RuntimeError,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailurePhase {
    Setup,
    LlmRequest,
    LlmResponse,
    ToolCall,
    ToolResult,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct InvocationTotals {
    pub total_llm_calls: u32,
    pub total_tool_calls: u32,
    pub total_cost: f64,
    pub total_duration_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trip_triggered_event() {
        let invocation_id = Uuid::now_v7();
        let event = Event::new(
            "researcher",
            invocation_id,
            EventPayload::Triggered(TriggeredPayload {
                trigger_source: TriggerSource::Manual,
                trigger_subject: None,
                trigger_payload: json!({"topic": "rust async"}),
                config_snapshot: ConfigSnapshot {
                    name: "researcher".to_string(),
                    model: "claude-haiku".to_string(),
                    system_prompt: "You are a research agent.".to_string(),
                    tools: vec!["read".to_string(), "web_search".to_string()],
                    sandbox: SandboxSnapshot {
                        fs_read: vec!["/docs".to_string()],
                        fs_write: vec![],
                        network: vec![],
                        env: vec![],
                    },
                    budget: Some(0.50),
                },
            }),
        );

        assert_eq!(event.subject(), "fq.agent.researcher.triggered");
        assert_eq!(event.schema_version, SCHEMA_VERSION);
        assert_eq!(event.agent_id, "researcher");

        let json = serde_json::to_string(&event).unwrap();
        let round_tripped: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped.agent_id, event.agent_id);
        assert_eq!(round_tripped.invocation_id, event.invocation_id);
        match round_tripped.payload {
            EventPayload::Triggered(p) => {
                assert!(matches!(p.trigger_source, TriggerSource::Manual));
                assert_eq!(p.config_snapshot.name, "researcher");
            }
            _ => panic!("wrong payload type"),
        }
    }

    #[test]
    fn subjects_for_all_event_types() {
        let agent = "test-agent";
        assert_eq!(subjects::agent_triggered(agent), "fq.agent.test-agent.triggered");
        assert_eq!(
            subjects::agent_llm_request(agent),
            "fq.agent.test-agent.llm.request"
        );
        assert_eq!(
            subjects::agent_llm_response(agent),
            "fq.agent.test-agent.llm.response"
        );
        assert_eq!(subjects::agent_tool_call(agent), "fq.agent.test-agent.tool.call");
        assert_eq!(
            subjects::agent_tool_result(agent),
            "fq.agent.test-agent.tool.result"
        );
        assert_eq!(subjects::agent_cost(agent), "fq.agent.test-agent.cost");
        assert_eq!(
            subjects::agent_completed(agent),
            "fq.agent.test-agent.completed"
        );
        assert_eq!(subjects::agent_failed(agent), "fq.agent.test-agent.failed");
    }

    #[test]
    fn tool_result_error_kind_serialises() {
        let payload = ToolResultPayload {
            tool_call_id: Uuid::now_v7(),
            output: "Path /etc/passwd is outside allowed scope".to_string(),
            is_error: true,
            error_kind: Some(ToolErrorKind::SandboxViolation),
            duration_ms: 1,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["error_kind"], "sandbox_violation");
        assert_eq!(json["is_error"], true);
    }

    #[test]
    fn tool_result_success_omits_error_kind() {
        let payload = ToolResultPayload {
            tool_call_id: Uuid::now_v7(),
            output: "file contents".to_string(),
            is_error: false,
            error_kind: None,
            duration_ms: 12,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json.get("error_kind"), None);
        assert_eq!(json["is_error"], false);
    }
}
