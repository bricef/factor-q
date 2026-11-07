//! What a connected server asks of the host, and what the host answers,
//! as factor-q's own values (ADR-0018).
//!
//! MCP's server-initiated surface — sampling, elicitation, roots and the
//! logging level — crosses the connection as rmcp's wire types. Those
//! types stay at the boundary, the `handler` module, which converts them
//! into the values here on the way in and back on the way out. Everything
//! past it (the runner, the validation policies, the roots plumbing)
//! works on these, so the runtime's own logic does not move when the
//! protocol or rmcp reshapes its envelope: SEP-2577 deprecates sampling,
//! roots and logging, and rmcp marks their types deprecated with it.
//!
//! The values carry what the runtime reads and nothing more. Where the
//! runtime needs the request exactly as it arrived — an evaluator judges
//! the serialized request — the boundary records that serialization in
//! `as_sent` rather than the runtime re-deriving it from a partial copy.
//!
//! The elicitation schema is not mirrored: it is not deprecated, it is
//! what the elicited value is validated against field by field, so it
//! is re-exported from rmcp once, here.
//!
//! A leaf module: it imports nothing else from the crate, so `policy`,
//! the runner and the `mcp` boundary can all depend on it without it
//! pulling any of them into an import cycle.

use serde_json::Value;
use tokio::sync::oneshot;

/// The elicitation schema types, re-exported once so the runtime names
/// them from here rather than from rmcp.
pub(crate) use rmcp::model::{ElicitationSchema, EnumSchema, PrimitiveSchema, StringFormat};

/// A request a connected MCP server initiates back toward the host
/// *mid-invocation* (ADR-0018).
///
/// The handler ([`FactorQClientHandler`](crate::mcp::FactorQClientHandler))
/// is a thin bridge: it translates an inbound rmcp request into one of
/// these variants, forwards it on a per-invocation channel, and awaits
/// the host's reply on the embedded oneshot. The runner is the sole
/// arbiter — it gates, runs the LLM call through its single
/// budgeted/WAL'd path, validates the result, and replies. Step 5 wires
/// the sampling arm; Step 6 adds an `Elicitation` variant to the same
/// channel and `select!` arm.
pub enum ServerRequest {
    /// `sampling/createMessage` — the server asks the host to run an
    /// LLM completion and return the result to the *server* (not the
    /// agent's transcript). `reply` carries either the result or a
    /// structured decline (e.g. ungranted / over-budget); dropping
    /// the sender declines with `method_not_found`.
    Sampling {
        request: SamplingRequest,
        reply: oneshot::Sender<Result<SamplingReply, rmcp::ErrorData>>,
    },
    /// `elicitation/create` — the server asks for structured user
    /// input matching a schema. factor-q answers it autonomously on
    /// the agent's model (ADR-0017); `reply` carries the result, whose
    /// `action` is `accept` (with content) or `decline`. A dropped
    /// sender declines.
    Elicitation {
        request: ElicitationRequest,
        reply: oneshot::Sender<Result<ElicitationReply, rmcp::ErrorData>>,
    },
}

/// A `sampling/createMessage` request: the conversation the server
/// wants completed, and the few knobs the runtime honours.
#[derive(Debug, Clone, PartialEq)]
pub struct SamplingRequest {
    /// The server's scripted conversation, in order.
    pub turns: Vec<SamplingTurn>,
    /// The server's `systemPrompt`, run as a system message.
    pub system_prompt: Option<String>,
    /// The server's `temperature`, as the protocol carries it.
    pub temperature: Option<f32>,
    /// The server's `maxTokens` — required by the protocol.
    pub max_tokens: u32,
    /// The request serialized exactly as the server sent it: what an
    /// input evaluator judges.
    pub as_sent: String,
}

/// One turn of a sampling conversation, flattened to text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamplingTurn {
    pub role: SamplingRole,
    /// The turn's content as one string. Several content blocks are
    /// joined by newlines; a non-text block (image, audio, tool use or
    /// result) is a fixed placeholder, so the conversation keeps its
    /// shape without claiming to inject what it cannot.
    pub text: String,
}

/// Who spoke a [`SamplingTurn`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplingRole {
    User,
    Assistant,
}

/// The host's answer to a sampling request: one assistant text
/// completion, the model that produced it, and why it stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamplingReply {
    pub text: String,
    pub model: String,
    pub stop_reason: SamplingStopReason,
}

/// Why a sampled completion stopped, in the protocol's vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplingStopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
}

/// An `elicitation/create` request.
///
/// Every mode the protocol defines is represented, because every
/// request is gated, validated and judged before its mode is looked at;
/// only [`Self::Form`] is then answered, and any other mode declines.
#[derive(Debug, Clone, PartialEq)]
pub enum ElicitationRequest {
    /// Form mode: fill the requested schema.
    Form {
        /// What the server says it needs, in human words.
        message: String,
        /// The flat object schema the answer must satisfy.
        requested_schema: ElicitationSchema,
        /// The request serialized exactly as the server sent it. An
        /// inbound validator that rewrites `message` or
        /// `requested_schema` does not rewrite this.
        as_sent: String,
    },
    /// URL mode: send the user to a page. factor-q has no user to send,
    /// so this declines once its gates have run.
    Url {
        message: String,
        url: String,
        /// The request serialized exactly as the server sent it.
        as_sent: String,
    },
}

impl ElicitationRequest {
    /// The request serialized exactly as the server sent it, whatever
    /// its mode: what an input evaluator judges.
    pub fn as_sent(&self) -> &str {
        match self {
            Self::Form { as_sent, .. } | Self::Url { as_sent, .. } => as_sent,
        }
    }
}

/// The host's answer to an elicitation request.
#[derive(Debug, Clone, PartialEq)]
pub struct ElicitationReply {
    pub action: ElicitationOutcome,
    /// The elicited value; present only on [`ElicitationOutcome::Accept`].
    pub content: Option<Value>,
}

/// What became of an elicitation request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElicitationOutcome {
    Accept,
    Decline,
    Cancel,
}

/// The structured decline an elicitation request resolves to when
/// refused (ungranted, over-budget, retries exhausted) or when the
/// host cannot service it. Per the protocol this is an ordinary
/// result with `action: decline`, not an error — the server continues
/// without the input.
pub(crate) fn elicitation_decline() -> ElicitationReply {
    ElicitationReply {
        action: ElicitationOutcome::Decline,
        content: None,
    }
}

/// A workspace root advertised to a server on `roots/list` (ADR-0018):
/// a URI the server may reason about, and an optional display name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRoot {
    pub uri: String,
    pub name: Option<String>,
}

impl WorkspaceRoot {
    /// A root at `uri` with no name.
    pub fn new(uri: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            name: None,
        }
    }

    /// Name the root (factor-q names a `file://` root by its path).
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
}

/// An MCP logging level — the eight RFC 5424 severities, least severe
/// first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpLogLevel {
    Debug,
    Info,
    Notice,
    Warning,
    Error,
    Critical,
    Alert,
    Emergency,
}

impl McpLogLevel {
    /// The level's canonical lowercase name, as the protocol spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Notice => "notice",
            Self::Warning => "warning",
            Self::Error => "error",
            Self::Critical => "critical",
            Self::Alert => "alert",
            Self::Emergency => "emergency",
        }
    }
}
