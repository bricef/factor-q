//! The LLM-call payload cluster: `llm.request`, `llm.dispatched` and
//! `llm.response`, plus the message, tool-schema, request-parameter
//! and token-usage types they share.
//!
//! Split out of `events.rs` to keep that file inside its size budget.
//! Every type here is re-exported from [`crate::events`], which stays
//! the import path — this is a file boundary, not an API one.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::ToolCallId;

/// What prompted an LLM call, for cost attribution (ADR-0004 /
/// ADR-0018). Agent turns are the reducer's own reasoning steps;
/// sampling calls are server-initiated (`sampling/createMessage`) and
/// tagged with the originating MCP server so their spend is distinct
/// from the agent's own turns while still bounded by the same
/// invocation budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LlmCallOrigin {
    /// A reducer-driven agent reasoning turn (the default).
    #[default]
    AgentTurn,
    /// A server-initiated sampling completion, attributed to the
    /// requesting MCP server.
    Sampling { server: String },
    /// A server-initiated elicitation completion (structured input),
    /// attributed to the requesting MCP server.
    Elicitation { server: String },
}

/// Published immediately before an LLM call is made.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequestPayload {
    pub call_id: Uuid,
    pub model: String,
    pub messages: Vec<Message>,
    pub tools_available: Vec<ToolSchema>,
    pub request_params: RequestParams,
    /// What prompted this call — agent turn vs server-initiated sampling
    /// / elicitation (ADR-0018) — mirroring the cost event's attribution
    /// so the request/response trace is self-describing. `default` =
    /// `AgentTurn` for events persisted before this field existed.
    #[serde(default)]
    pub origin: LlmCallOrigin,
}

/// WAL middle-state event for LLM dispatch. Emitted between
/// [`LlmRequestPayload`] and [`LlmResponsePayload`] once the
/// LLM call has returned control to the runtime — before the
/// response is durably written.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmDispatchedPayload {
    pub call_id: Uuid,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<MessageToolCall>,
    /// ID correlating a `tool` role message with a prior assistant tool
    /// call. Assigned by the LLM provider and carried through as-is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<ToolCallId>,
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
    /// ID assigned by the LLM provider. Carried through unchanged so
    /// that `tool.call` and `tool.result` events can be correlated with
    /// the raw provider response.
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters_schema: Value,
}

/// Per-request model reasoning effort. `None` leaves the provider default.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    /// Disables (nearly) all reasoning — required for gpt-5-family
    /// models on short mechanical tasks: their default reasoning
    /// scales to fill `max_tokens`, returning empty content (found
    /// live: the #216 summariser produced nothing on gpt-5-nano).
    Minimal,
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

/// Published when an LLM call returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponsePayload {
    /// The Round grouping key (`fq_runtime::turn`); 1-based, 0 on
    /// pre-field events.
    #[serde(default)]
    pub round: u64,
    pub call_id: Uuid,
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<MessageToolCall>,
    pub stop_reason: StopReason,
    pub usage: TokenUsage,
    /// What prompted this call (see [`LlmRequestPayload::origin`]).
    #[serde(default)]
    pub origin: LlmCallOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    ToolUse,
    EndTurn,
    MaxTokens,
    StopSequence,
}

/// Token usage for one LLM call.
///
/// Invariant: `input_tokens` is the **total** prompt size;
/// `cache_read_tokens` and `cache_write_tokens` are subsets of it
/// (the uncached portion is `input - read - write`). The genai
/// adapter normalises every provider to this shape — Anthropic
/// reports the three parts separately and they are summed; OpenAI
/// and Gemini already report totals with cached counts as details.
/// Pricing depends on this (see [`crate::pricing::ModelPricing`]).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)]
    pub cache_read_tokens: u32,
    #[serde(default)]
    pub cache_write_tokens: u32,
}
