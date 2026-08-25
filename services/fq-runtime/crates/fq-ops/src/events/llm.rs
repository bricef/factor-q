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
/// Pricing depends on this (see the runtime's `ModelPricing`).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)]
    pub cache_read_tokens: u32,
    #[serde(default)]
    pub cache_write_tokens: u32,
}

/// Why an LLM call failed — a serialisable projection of the
/// runtime's `LlmError`, not a new taxonomy.
///
/// The error enum already has the right joints, and `is_transient`
/// already partitions them the way an operator cares about, but it
/// cannot go on the wire: it is a `thiserror` type carrying the
/// provider strings that belong in `error_message`. So this mirrors
/// its variants as `Copy` units and a single [`From`] does the
/// conversion, which is the only place the two can drift. That
/// conversion stays in `fq-runtime` beside the error it reads; only
/// the wire shape lives here.
///
/// One variant has no `LlmError` counterpart. `EmptyResponse` is the
/// provider returning 200 with no content and no tool calls; the
/// runner synthesises a `RequestFailed` for it today, which makes it
/// indistinguishable from a transport error. It is not: it is the one
/// failure that *bills*, because the provider did the prefill.
///
/// A 429 currently arrives as `RequestFailed` with the status buried
/// in `error_message` — under-classified, and where
/// [#278](https://github.com/bricef/factor-q/issues/278)'s
/// `Retry-After` rework lands, at which point `rate_limited` becomes
/// queryable with no schema change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmErrorKind {
    Auth,
    RateLimited,
    InvalidResponse,
    RequestFailed,
    UnpricedModel,
    /// A 200 with nothing in it. Synthesised by the runner, never by
    /// a provider, and the only kind that can carry usage.
    EmptyResponse,
}

/// Published when an LLM call ends without a response — the sibling
/// of [`LlmResponsePayload`], sharing its `call_id`.
///
/// A separate variant rather than nullable fields on the response:
/// every consumer that reads `stop_reason` and `usage` today reads
/// them unconditionally, and making them optional would push a "did
/// this actually happen?" branch into all of them while the type
/// stopped saying which case you are in. As a variant, the compiler
/// asks the question at the match site.
///
/// Terminality is per `call_id`, not per invocation: provider retries
/// happen inside a single `LlmClient::chat` call and are not
/// event-visible, so one request yields one outcome — but a failed
/// *sampling* or *elicitation* call declines the server's request
/// without ending the agent's invocation, so one invocation may carry
/// several of these.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmFailurePayload {
    /// The Round grouping key; 0 on pre-field events. A failed call
    /// consumes one, exactly as a successful one does.
    #[serde(default)]
    pub round: u64,
    /// Correlates with the `llm.request` that opened this call.
    pub call_id: Uuid,
    /// Restated because it is not derivable from the envelope when
    /// the call carried no cost.
    pub model: String,
    pub error_kind: LlmErrorKind,
    /// The provider's text. Often the operator's only handle on a 429.
    pub error_message: String,
    /// Wall time for the call *including* the hidden retry attempts
    /// inside `RetryingLlmClient` — which is the tell for a rate
    /// limit that eventually gave up.
    pub duration_ms: u64,
    /// What the provider billed, when we know. **`None` is not zero**:
    /// `Some(0)` says the provider billed nothing, `None` says we
    /// cannot see what it billed — a transport failure yields no
    /// parsed body, while an empty completion yields real counts.
    /// Writing the second as the first would be exactly the cost loss
    /// this event exists to stop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    /// What prompted this call (see [`LlmRequestPayload::origin`]).
    #[serde(default)]
    pub origin: LlmCallOrigin,
}
