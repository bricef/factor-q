//! The Turn atom: one action in an invocation's conversation — the
//! opening prompt, an assistant output, or a tool result — as an
//! immutable, event-log-backed fact.
//!
//! The atom and its rendering bridge. Folding events into turns needs
//! the event vocabulary and stays in `fq-runtime`; the transcript is a
//! rendering composed over turns, never the reverse.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::transcript::{AssistantToolCall, TranscriptEntry, TurnReasoning};

/// One turn, addressed by its event-log sequence — the universal
/// cursor (P5): the same number that cursors `turn.stream`, feeds
/// `min_seq` gates, and appears in command receipts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TurnState {
    pub seq: u64,
    pub invocation_id: String,
    pub agent_id: String,
    /// The Round grouping key: one Round is an assistant action plus
    /// the tool results it initiated (the model-turn count
    /// `max_iterations` gates). 0 on the opening prompt, which
    /// precedes every Round, and on turns derived from events
    /// predating the field.
    pub round: u64,
    pub timestamp_ms: i64,
    /// For a tool-result turn: the sequence of the assistant turn
    /// whose call it answers — tracing in log coordinates. Absent
    /// when the initiating turn predates the stream window, and on
    /// turns no other turn initiated (assistant turns, the prompt).
    pub initiating_turn: Option<u64>,
    /// The action itself — the fact this atom records.
    pub action: TurnAction,
}

/// What happened: the turn's content, full payload by default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnAction {
    /// The invocation's opening prompt: the system prompt the agent
    /// was configured with and the user message its trigger became.
    /// It precedes every Round, and there is exactly one per
    /// invocation.
    Prompt {
        system: Option<String>,
        user: Option<String>,
    },
    Assistant {
        model: String,
        content: Option<String>,
        /// The model's own working, reduced to what the operator domain
        /// can say about it. Absent when the turn produced none — which
        /// is not the same as present-but-unreadable, see
        /// [`TurnReasoning`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<TurnReasoning>,
        tool_calls: Vec<AssistantToolCall>,
        cost_usd: Option<f64>,
        is_error: Option<bool>,
    },
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        /// Joined from the initiating call when it is in the fold's
        /// window; `null` for a lone turn fetched by key.
        parameters: Value,
        output: Option<String>,
        is_error: Option<bool>,
    },
}

impl TurnState {
    /// The rendering bridge: a turn as the transcript entry it
    /// displays as. `render_pretty` over mapped turns is
    /// byte-identical to the WAL-backed transcript for the same
    /// actions — the flip's contract.
    pub fn transcript_entry(&self) -> TranscriptEntry {
        match &self.action {
            TurnAction::Prompt { system, user } => TranscriptEntry::Prompt {
                timestamp_ms: self.timestamp_ms,
                system: system.clone(),
                user: user.clone(),
            },
            TurnAction::Assistant {
                model,
                content,
                reasoning,
                tool_calls,
                cost_usd,
                is_error,
            } => TranscriptEntry::Assistant {
                timestamp_ms: self.timestamp_ms,
                model: model.clone(),
                content: content.clone(),
                reasoning: reasoning.clone(),
                tool_calls: tool_calls.clone(),
                cost_usd: *cost_usd,
                is_error: *is_error,
            },
            TurnAction::ToolResult {
                tool_call_id,
                tool_name,
                parameters,
                output,
                is_error,
            } => TranscriptEntry::ToolResult {
                timestamp_ms: self.timestamp_ms,
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                parameters: parameters.clone(),
                output: output.clone(),
                is_error: *is_error,
            },
        }
    }
}
