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

/// One turn in a conversation.
///
/// **The turn kind is the variant, so the role is carried by the type**
/// rather than by a `role` field that a separate content list can
/// disagree with ([ADR-0034](../../adrs/accepted/0034-reasoning-as-a-content-part.md)
/// D1). Four states the previous `{role, content, tool_calls,
/// tool_call_id}` shape left representable are now unrepresentable: a
/// tool message with no correlation id, reasoning in a user turn, a tool
/// call in a user turn, and a tool result in an assistant turn.
///
/// Each variant carries the parts its kind can hold rather than sharing
/// one flat part type, which is what keeps those states gone. Upstream
/// shows the cost of the alternative: genai carries the comment *"ToolCall
/// is not valid in user content for Anthropic; skip gracefully"* — an
/// invalid combination handled by dropping it silently.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Message {
    /// The system prompt. Seeded once, first, per conversation.
    System { text: String },
    /// A user turn: the trigger's rendered prompt, host notices, or
    /// pinned static-resource context.
    ///
    /// Plain text today because every user message the runtime builds is
    /// plain text, and MCP sampling already flattens multimodal content
    /// upstream of this type. It becomes a part list when binary content
    /// arrives — a variant is cheap to widen, and a one-variant part enum
    /// today would be structure nothing branches on.
    User { text: String },
    /// An assistant turn: text, reasoning, and tool calls, in the order
    /// the provider returned them.
    Assistant { parts: Vec<AssistantPart> },
    /// Every result answering a single assistant turn's tool calls.
    ///
    /// One message, N results, in the order of the calls — which matches
    /// the Anthropic wire shape, where `tool_result` blocks are batched
    /// into one turn, and unfolds into one `tool` message per result on
    /// the OpenAI-compatible wire. The harness emits this form for a
    /// parallel round since
    /// [#511](https://github.com/bricef/factor-q/issues/511) closed.
    ToolResults { results: Vec<ToolResult> },
}

impl Message {
    /// The system prompt.
    pub fn system(text: impl Into<String>) -> Self {
        Self::System { text: text.into() }
    }

    /// A user turn.
    pub fn user(text: impl Into<String>) -> Self {
        Self::User { text: text.into() }
    }

    /// An assistant turn carrying a single text part — scripted context,
    /// or a model turn that said something and did nothing else.
    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self::Assistant {
            parts: vec![AssistantPart::Text { text: text.into() }],
        }
    }

    /// One tool result as its own turn: the single-call round, and the
    /// convenience scripted conversations lean on. A parallel round is
    /// `Message::ToolResults` with several results, in call order
    /// ([#511](https://github.com/bricef/factor-q/issues/511)).
    pub fn tool_result(tool_call_id: ToolCallId, output: impl Into<String>) -> Self {
        Self::ToolResults {
            results: vec![ToolResult {
                tool_call_id,
                output: output.into(),
                is_error: false,
            }],
        }
    }

    /// The text this turn carries, whatever kind it is — `None` only when
    /// it genuinely carries none. Lets a reader ask "what is in this
    /// message?" without matching on the kind first.
    ///
    /// A tool-results turn answers with its outputs joined, for the same
    /// reason an assistant turn joins its text parts: the caller asked
    /// what the message contains, and the outputs are what it contains.
    /// Returning `None` there would be a semantic distinction ("data, not
    /// speech") that no caller wants and that reads as a bug at every
    /// call site.
    ///
    /// Callers that care *which* kind they have should match on the
    /// variant instead — that is what the enum is for.
    pub fn text(&self) -> Option<String> {
        match self {
            Self::System { text } | Self::User { text } => Some(text.clone()),
            Self::Assistant { parts } => assistant_text(parts),
            Self::ToolResults { results } => {
                let outputs: Vec<&str> = results.iter().map(|r| r.output.as_str()).collect();
                (!outputs.is_empty()).then(|| outputs.join("\n"))
            }
        }
    }
}

/// The visible text of an assistant turn, joined when the provider split
/// it across parts. `None` when the turn carried none — a tool-only turn,
/// or one that was pure reasoning.
///
/// Defined once so every consumer answers "what did this turn say?" the
/// same way; the payload and the runtime's response type both delegate here.
pub fn assistant_text(parts: &[AssistantPart]) -> Option<String> {
    let texts: Vec<&str> = parts
        .iter()
        .filter_map(|part| match part {
            AssistantPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    (!texts.is_empty()).then(|| texts.join("\n"))
}

/// Reduce an assistant turn's reasoning to what the operator surface can
/// say about it: readable text, and whatever is carried but not readable.
///
/// **This is where parts stop** ([ADR-0034] D3). Provider vocabulary —
/// the three-way shape, the model tie, the signature — is internal; the
/// operator domain gets the two facts it can act on. Several reasoning
/// parts in one turn join, for the same reason text parts do.
///
/// `None` when the turn produced no reasoning at all, which is a
/// different fact from producing some we cannot read (D4) — that returns
/// `Some` with no `text`.
///
/// **Text is readable text.** A part whose text is empty after trimming
/// contributes none: Anthropic returns `thinking` blocks with an empty
/// string and a signature ([#537]), and those are opaque, not "reasoned
/// about nothing". Presence is decided by the part existing, not by
/// what it holds, so a part with neither text nor token still reduces
/// to `Some` — with both fields `None`, the *empty* state — rather than
/// to absence (I7).
///
/// [ADR-0034]: https://github.com/bricef/factor-q/blob/main/docs/adrs/accepted/0034-reasoning-as-a-content-part.md
/// [#537]: https://github.com/bricef/factor-q/issues/537
pub fn reduce_reasoning(parts: &[AssistantPart]) -> Option<crate::transcript::TurnReasoning> {
    let mut texts: Vec<&str> = Vec::new();
    let mut opaque: Vec<Value> = Vec::new();
    let mut present = false;
    for part in parts {
        let AssistantPart::Reasoning(reasoning) = part else {
            continue;
        };
        present = true;
        match &reasoning.content {
            ReasoningContent::Plain { text } => texts.extend(readable(text)),
            ReasoningContent::Signed { text, token } => {
                texts.extend(readable(text));
                opaque.push(token.clone());
            }
            ReasoningContent::Opaque { token } => opaque.push(token.clone()),
        }
    }
    if !present {
        return None;
    }
    Some(crate::transcript::TurnReasoning {
        text: (!texts.is_empty()).then(|| texts.join("\n")),
        opaque: match opaque.len() {
            0 => None,
            1 => Some(opaque.remove(0)),
            _ => Some(Value::Array(opaque)),
        },
    })
}

/// What a reasoning part's text contributes to the operator surface: the
/// text verbatim, unless it is empty after trimming — then nothing.
fn readable(text: &str) -> Option<&str> {
    (!text.trim().is_empty()).then_some(text)
}

/// Build an assistant turn's parts from text and tool calls — the
/// inverse of [`assistant_text`] and [`assistant_tool_calls`].
///
/// Text first, then calls, which is the order every provider returns and
/// the order Anthropic requires. Reasoning is not expressible here by
/// design: it comes from a provider response, never from a caller
/// assembling a turn by hand.
pub fn assistant_parts(
    text: Option<String>,
    tool_calls: Vec<MessageToolCall>,
) -> Vec<AssistantPart> {
    let mut parts = Vec::with_capacity(tool_calls.len() + 1);
    if let Some(text) = text.filter(|t| !t.is_empty()) {
        parts.push(AssistantPart::Text { text });
    }
    parts.extend(tool_calls.into_iter().map(AssistantPart::ToolCall));
    parts
}

/// The tool calls an assistant turn requested, in order.
pub fn assistant_tool_calls(parts: &[AssistantPart]) -> impl Iterator<Item = &MessageToolCall> {
    parts.iter().filter_map(|part| match part {
        AssistantPart::ToolCall(call) => Some(call),
        _ => None,
    })
}

/// A part of an assistant turn. The three things such a turn can hold —
/// and nothing else, which is the point.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AssistantPart {
    Text { text: String },
    Reasoning(Reasoning),
    ToolCall(MessageToolCall),
}

/// The model's own working for one turn.
///
/// **Reasoning is model-tied.** Providers verify or reject it against the
/// model that produced it, so `model` is not annotation — the cross-model
/// strip keys on it (ADR-0034 D5).
///
/// The three shapes are the ones providers actually return, and code
/// branches on the difference: the transcript can render `Plain` and
/// `Signed`'s text but not `Opaque`'s, and each adapter encodes them
/// differently on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reasoning {
    /// The model that produced this. Replaying reasoning to a different
    /// model is at best wasted input tokens and at worst a protocol
    /// violation.
    pub model: String,
    pub content: ReasoningContent,
}

/// What a reasoning part actually carries.
///
/// **`Opaque` is not "empty".** A provider block we cannot read still
/// carries the turn's reasoning — Anthropic's `signature` encrypts the
/// full chain of thought — so it is recorded as present-and-opaque rather
/// than omitted. Absence and opacity are different facts (ADR-0034 D4).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReasoningContent {
    /// Readable working, no continuity token. Kimi, DeepSeek.
    Plain { text: String },
    /// Readable working plus a token that must be echoed verbatim.
    /// Anthropic `thinking` — where the token, not the text, is the
    /// payload.
    Signed { text: String, token: Value },
    /// No readable content: the token *is* the content. Anthropic
    /// `redacted_thinking`, Gemini thought signatures.
    Opaque { token: Value },
}

// A note on `token: Value` rather than `String`.
//
// It is the provider's opaque payload in whatever shape that provider
// uses, and for Anthropic that is the **whole block** — not the bare
// signature. Anthropic verifies a thinking block *against* its signature,
// so a signature without the block it signs cannot be replayed, and
// reconstructing the block from text + signature would silently drop any
// field the API adds later (I5). Keeping the block verbatim is the only
// lossless option, and it is what the adapter echoes back.
//
// Gemini's thought signature is a bare string, which `Value` also holds.
// ADR-0034 wrote this field as `String`; the shape is wider than the ADR
// anticipated for exactly the reason above.

/// One tool's result, answering the call with the matching id.
///
/// The id lives on the result rather than beside it, which is what
/// deletes the "tool message is missing tool_call_id" failure the old
/// shape could reach at runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_call_id: ToolCallId,
    pub output: String,
    #[serde(default)]
    pub is_error: bool,
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
    /// The turn's content, ordered as the provider returned it.
    ///
    /// **A response is an assistant turn**, so it carries that turn kind's
    /// parts rather than a flat part type — which makes a tool result in a
    /// response unrepresentable (ADR-0034 D3). Replaced the flat
    /// `content: Option<String>` + `tool_calls` pair in schema v3.
    #[serde(default)]
    pub parts: Vec<AssistantPart>,
    pub stop_reason: StopReason,
    pub usage: TokenUsage,
    /// What prompted this call (see [`LlmRequestPayload::origin`]).
    #[serde(default)]
    pub origin: LlmCallOrigin,
}

impl LlmResponsePayload {
    /// What this turn said. See [`assistant_text`].
    pub fn text(&self) -> Option<String> {
        assistant_text(&self.parts)
    }

    /// What this turn called. See [`assistant_tool_calls`].
    pub fn tool_calls(&self) -> impl Iterator<Item = &MessageToolCall> {
        assistant_tool_calls(&self.parts)
    }
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
    /// The share of `output_tokens` the model spent thinking rather than
    /// speaking. **A decomposition, not an addition** — providers already
    /// fold reasoning into the completion count, so this never changes
    /// what a call costs. `0` where the provider does not report it,
    /// which is most of them.
    ///
    /// It exists because for a reasoning-first model that split is most
    /// of the bill, and it was invisible everywhere in the cost data
    /// (#437). Additive on the wire, so it needs no schema bump.
    #[serde(default)]
    pub reasoning_tokens: u32,
}

impl TokenUsage {
    /// The output tokens that were *spoken* rather than thought.
    ///
    /// Saturating because the split is the provider's arithmetic, not
    /// ours: a provider reporting more reasoning than completion tokens
    /// is a provider bug (genai already corrects one such case for xAI),
    /// and this should read 0 rather than panic or wrap.
    pub fn spoken_tokens(&self) -> u32 {
        self.output_tokens.saturating_sub(self.reasoning_tokens)
    }
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
