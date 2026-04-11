use serde::{Deserialize, Serialize};

/// Subject hierarchy for factor-q events.
pub mod subjects {
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

    pub fn agent_completed(agent_id: &str) -> String {
        format!("fq.agent.{agent_id}.completed")
    }

    pub fn agent_failed(agent_id: &str) -> String {
        format!("fq.agent.{agent_id}.failed")
    }

    pub fn agent_cost(agent_id: &str) -> String {
        format!("fq.agent.{agent_id}.cost")
    }
}

/// Envelope for all factor-q events.
#[derive(Debug, Serialize, Deserialize)]
pub struct Event {
    pub schema_version: u32,
    pub timestamp: String,
    pub agent_id: String,
    pub event_type: EventType,
    pub payload: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    Triggered,
    LlmRequest,
    LlmResponse,
    ToolCall,
    ToolResult,
    Completed,
    Failed,
    Cost,
}
