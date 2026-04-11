//! LLM client abstraction.
//!
//! factor-q owns its own request/response types so the internal contract
//! and the event schema do not depend on any specific LLM client library.
//! Concrete implementations adapt these types to whatever underlying
//! library they use — see `llm::genai` for the `genai` adapter and
//! `llm::fixture` for the canned-response client used in tests.
//!
//! The call_id is owned by the executor, not the client. Each call the
//! executor makes gets a fresh UUID v7 assigned before the client is
//! invoked; the same id is used to correlate `llm.request`,
//! `llm.response`, and `cost` events for that call.

pub mod fixture;
pub mod genai;

use async_trait::async_trait;

use crate::events::{Message, MessageToolCall, RequestParams, StopReason, TokenUsage, ToolSchema};

/// A request to an LLM, without the call_id (assigned by the executor).
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub params: RequestParams,
}

/// A response from an LLM, without the call_id.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<MessageToolCall>,
    pub stop_reason: StopReason,
    pub usage: TokenUsage,
}

/// Abstraction over any LLM client. Implementations are responsible for
/// converting between factor-q's types and their underlying library's.
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError>;
}

/// Errors from an LLM client call.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("authentication failed: {0}")]
    Auth(String),

    #[error("rate limited")]
    RateLimited,

    #[error("invalid response from provider: {0}")]
    InvalidResponse(String),

    #[error("request failed: {0}")]
    RequestFailed(String),
}
