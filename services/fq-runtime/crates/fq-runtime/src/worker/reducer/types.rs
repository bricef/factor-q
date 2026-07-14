//! Boundary types between the reducer harness and its host.
//!
//! These types intentionally mirror the shapes described in
//! `docs/design/committed/wasm-boundary-design.md`. They are JSON-
//! serialisable so they can later be passed across the WASM
//! component-model ABI without restructuring; for the native
//! prototype they cross only ordinary Rust function boundaries.
//!
//! The reducer is **pure**: no I/O, no async, no hidden state.
//! Everything it needs is in [`StepInput`]; everything it
//! produces is in [`StepOutput`]. The opaque `state` blob is the
//! reducer's own conversation memory, which the host ferries
//! between steps without inspecting.
//!
//! Where it would have been redundant to invent fresh types, we
//! reuse the runtime's existing event-schema types
//! ([`Message`], [`ToolSchema`], [`MessageToolCall`],
//! [`StopReason`], [`TokenUsage`], [`RequestParams`],
//! [`ToolErrorKind`]). The reducer's surface and the event
//! schema describe the same conversation; sharing types prevents
//! drift between them.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::AgentId;
use crate::events::{
    Effort, Message, MessageToolCall, RequestParams, StopReason, TokenUsage, ToolCallId,
    ToolErrorKind, ToolSchema,
};

/// Static-for-the-invocation configuration the host hands to the
/// reducer on every step. Cheap to pass repeatedly; passing it
/// every time is what keeps `step` pure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub agent_id: AgentId,
    pub model: String,
    pub system_prompt: String,
    pub tools_available: Vec<ToolSchema>,
    pub allowed_tool_names: Vec<String>,
    /// Maximum LLM turns this invocation may take before the
    /// runtime forces termination with a `MaxIterations` failure.
    /// The value is **literal** — `0` is a valid stop signal
    /// (no turns allowed) and the runtime will terminate
    /// immediately at step 1. Resolve the harness default at the
    /// producer site rather than relying on a sentinel here. See
    /// `worker::reducer::harness::DEFAULT_MAX_ITERATIONS`.
    pub max_iterations: u32,
    /// Optional per-agent reasoning effort; `None` uses the provider default.
    pub effort: Option<Effort>,
}

/// Trigger payload, carried unchanged across the boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerPayload {
    pub source: TriggerSourceKind,
    pub subject: Option<String>,
    pub payload: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TriggerSourceKind {
    Manual,
    Subject,
    Schedule,
}

/// Input to one invocation of the reducer's `step` function.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepInput {
    pub config: AgentConfig,
    pub trigger: TriggerPayload,
    /// Opaque guest state. Empty on step 0; otherwise the bytes
    /// returned in the previous step's [`StepOutput::state`].
    #[serde(default)]
    pub state: Vec<u8>,
    /// Outcome of the previous step's [`NextAction`]. None on
    /// step 0.
    pub last_result: Option<CapabilityResult>,
    /// Wall-clock time at step start, in milliseconds since epoch.
    pub now_ms: u64,
    /// Fresh randomness for this step. The reducer must not read
    /// from any other source.
    pub random_seed: u64,
    /// Monotonic counter starting at 0.
    pub step_index: u32,
    /// Host-curated context assembled from the agent's
    /// `static_resources` pins (MCP resource content the host
    /// read at invocation start). `Some` only on step 0; the
    /// reducer injects it once after the system prompt. `None`
    /// when no pins are declared, on every non-initial step, and
    /// on resume (the content is already in the persisted state).
    /// The reducer does no I/O — the runner reads the pins and
    /// passes the rendered content in here.
    #[serde(default)]
    pub static_resource_context: Option<String>,
    /// Durable host messages injected at this step boundary.
    #[serde(default)]
    pub host_notices: Vec<String>,
}

/// Output of one invocation of `step`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepOutput {
    pub next_action: NextAction,
    /// Updated opaque state. The host stores this verbatim and
    /// returns it on the next call.
    pub state: Vec<u8>,
    /// Fire-and-forget tracing logs collected during this step.
    #[serde(default)]
    pub logs: Vec<LogEntry>,
    /// Fire-and-forget semantic events. Not used by canonical
    /// lifecycle events (which the host emits itself); reserved
    /// for guest-decided emissions like skill-composition or
    /// reasoning-trace records.
    #[serde(default)]
    pub events: Vec<EmittedEvent>,
}

/// What the host should do before calling `step` again.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum NextAction {
    CallModel(ModelRequest),
    CallTool(ToolCallRequest),
    CallToolsParallel(Vec<ToolCallRequest>),
    /// The invocation has completed successfully. The string is
    /// the agent's final output.
    Complete(String),
    /// The invocation has failed terminally.
    Failed(HarnessError),
}

/// The outcome of the previous step's [`NextAction`], handed back
/// to the reducer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum CapabilityResult {
    ModelResult(ModelResponse),
    ToolResult(ToolCallResult),
    ParallelToolResults(Vec<ToolCallResult>),
    /// Host cancelled the action (e.g. shutdown) before it
    /// completed. The reducer typically translates this to a
    /// terminal `Failed`.
    Cancelled,
    /// Host-side error the reducer should surface to the LLM or
    /// fail with. Kept distinct from [`Self::Cancelled`] so the
    /// reducer can distinguish.
    HostError(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub params: RequestParams,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelResponse {
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<MessageToolCall>,
    pub stop_reason: StopReason,
    pub usage: TokenUsage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRequest {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallResult {
    pub tool_call_id: ToolCallId,
    pub output: String,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<ToolErrorKind>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub level: LogLevel,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// A semantic event the reducer wants the host to publish on the
/// event bus. Distinct from canonical lifecycle events
/// ([`crate::events::Event`]) which the host emits as it
/// executes actions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmittedEvent {
    pub kind: String,
    pub payload: Value,
}

/// Terminal failure surfaced from the reducer.
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("{kind:?}: {message}")]
pub struct HarnessError {
    pub kind: HarnessErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HarnessErrorKind {
    /// `MAX_ITERATIONS` exceeded before the LLM declared a final
    /// answer.
    MaxIterations,
    /// Invariant violation inside the reducer (state corruption,
    /// unexpected variant). Should be impossible if the host
    /// honours the protocol; surfaced for debugging.
    InternalError,
}

/// Pure synchronous reducer. The single load-bearing trait of
/// the harness boundary.
pub trait Reducer {
    fn step(&self, input: StepInput) -> Result<StepOutput, HarnessError>;
}
