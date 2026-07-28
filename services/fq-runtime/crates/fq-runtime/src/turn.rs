//! The Turn atom: one action within a Round — an assistant output or
//! a tool result — as an immutable, event-log-backed fact
//! (`docs/design/committed/operator-surface-domain-model.md`). The
//! transcript is a *rendering* composed over turns (plus the
//! invocation's prompt and outcome); the dependency runs that way,
//! never the reverse.

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::events::{Event, EventPayload};
use crate::transcript::{AssistantToolCall, TranscriptEntry};

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
    /// `max_iterations` gates). 0 on turns derived from events
    /// predating the field.
    pub round: u64,
    pub timestamp_ms: i64,
    /// For a tool-result turn: the sequence of the assistant turn
    /// whose call it answers — tracing in log coordinates. Absent
    /// when the initiating turn predates the stream window.
    pub initiating_turn: Option<u64>,
    /// The action itself — the fact this atom records.
    pub action: TurnAction,
}

/// What happened: the turn's content, full payload by default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnAction {
    Assistant {
        model: String,
        content: Option<String>,
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
            TurnAction::Assistant {
                model,
                content,
                tool_calls,
                cost_usd,
                is_error,
            } => TranscriptEntry::Assistant {
                timestamp_ms: self.timestamp_ms,
                model: model.clone(),
                content: content.clone(),
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

/// The fold from events to turns. Stateful across a window so a tool
/// result joins its initiating call (name, parameters, and the
/// assistant turn's sequence); the initiating events always precede
/// the result in the log, so folding forward suffices.
#[derive(Default)]
pub struct TurnFold {
    calls: HashMap<String, PendingCall>,
}

struct PendingCall {
    tool_name: String,
    parameters: Value,
    assistant_seq: Option<u64>,
}

impl TurnFold {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one event at `seq`. Returns the turn it yields, if any —
    /// most events (dispatch middle states, lifecycle, heartbeats)
    /// yield none.
    pub fn apply(&mut self, seq: u64, event: &Event) -> Option<TurnState> {
        let envelope = &event.envelope;
        let base = |round, initiating_turn, action| TurnState {
            seq,
            invocation_id: envelope.invocation_id.to_string(),
            agent_id: envelope.agent_id.to_string(),
            round,
            timestamp_ms: envelope.timestamp.timestamp_millis(),
            initiating_turn,
            action,
        };
        match &event.payload {
            EventPayload::LlmResponse(p) => {
                for call in &p.tool_calls {
                    self.calls.insert(
                        call.tool_call_id.to_string(),
                        PendingCall {
                            tool_name: call.tool_name.clone(),
                            parameters: call.parameters.clone(),
                            assistant_seq: Some(seq),
                        },
                    );
                }
                let cost = envelope.cost.as_ref();
                Some(base(
                    p.round,
                    None,
                    TurnAction::Assistant {
                        model: cost
                            .map(|c| c.model.clone())
                            .unwrap_or_else(|| "?".to_string()),
                        content: p.content.clone(),
                        tool_calls: p
                            .tool_calls
                            .iter()
                            .map(|c| AssistantToolCall {
                                tool_call_id: c.tool_call_id.to_string(),
                                tool_name: c.tool_name.clone(),
                                parameters: c.parameters.clone(),
                            })
                            .collect(),
                        cost_usd: cost.map(|c| c.total_cost),
                        // Not unknown — false. An `llm.response` event
                        // exists only for a call that returned a
                        // response: when the provider errors the runner
                        // closes the WAL row `is_error = true` and
                        // returns without publishing one. That is the
                        // same occasion on which the WAL-backed
                        // transcript records `is_error = false`, so the
                        // bridge's byte-identity contract needs the
                        // same value, not a null standing in for it.
                        is_error: Some(false),
                    },
                ))
            }
            EventPayload::ToolCall(p) => {
                // Enrich (or establish) the pending-call record; the
                // event itself is part of the assistant turn, not a
                // turn of its own.
                self.calls
                    .entry(p.tool_call_id.to_string())
                    .or_insert(PendingCall {
                        tool_name: p.tool_name.clone(),
                        parameters: p.parameters.clone(),
                        assistant_seq: None,
                    });
                None
            }
            EventPayload::ToolResult(p) => {
                let pending = self.calls.remove(&p.tool_call_id.to_string());
                let tool_name = pending
                    .as_ref()
                    .map(|c| c.tool_name.clone())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| p.tool_name.clone());
                Some(base(
                    p.round,
                    pending.as_ref().and_then(|c| c.assistant_seq),
                    TurnAction::ToolResult {
                        tool_call_id: p.tool_call_id.to_string(),
                        tool_name,
                        parameters: pending.map(|c| c.parameters).unwrap_or(Value::Null),
                        output: Some(p.output.clone()),
                        is_error: Some(p.is_error),
                    },
                ))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentId;
    use crate::events::{
        CostMetadata, LlmCallOrigin, LlmResponsePayload, MessageToolCall, StopReason, TokenUsage,
        ToolCallId, ToolResultPayload,
    };
    use uuid::Uuid;

    fn assistant_event(round: u64, with_call: Option<&str>) -> Event {
        Event::new(
            AgentId::new("fold-probe").unwrap(),
            Uuid::now_v7(),
            EventPayload::LlmResponse(LlmResponsePayload {
                round,
                call_id: Uuid::now_v7(),
                content: Some("thinking".into()),
                tool_calls: with_call
                    .map(|id| {
                        vec![MessageToolCall {
                            tool_call_id: ToolCallId::new(id).unwrap(),
                            tool_name: "read_file".into(),
                            parameters: serde_json::json!({"path": "x"}),
                        }]
                    })
                    .unwrap_or_default(),
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                origin: LlmCallOrigin::default(),
            }),
        )
        .with_cost(CostMetadata {
            call_id: Uuid::now_v7(),
            model: "claude-haiku".into(),
            input_tokens: 1,
            output_tokens: 1,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            input_cost: 0.0,
            output_cost: 0.0,
            total_cost: 0.01,
            cumulative_invocation_cost: 0.01,
            cumulative_agent_cost: 0.01,
            origin: Default::default(),
        })
    }

    fn result_event(round: u64, call_id: &str, tool_name: &str) -> Event {
        Event::new(
            AgentId::new("fold-probe").unwrap(),
            Uuid::now_v7(),
            EventPayload::ToolResult(ToolResultPayload {
                round,
                tool_name: tool_name.into(),
                tool_call_id: ToolCallId::new(call_id).unwrap(),
                output: "ok".into(),
                is_error: false,
                error_kind: None,
                duration_ms: 5,
            }),
        )
    }

    /// The join: a result folded after its assistant turn carries the
    /// call's name and parameters, and names the assistant turn as
    /// its initiator in log coordinates.
    #[test]
    fn result_joins_its_initiating_turn() {
        let mut fold = TurnFold::new();
        let assistant = fold.apply(41, &assistant_event(3, Some("tc-1"))).unwrap();
        assert_eq!(assistant.round, 3);
        assert!(matches!(assistant.action, TurnAction::Assistant { .. }));

        let result = fold.apply(45, &result_event(3, "tc-1", "")).unwrap();
        assert_eq!(result.round, 3);
        assert_eq!(result.initiating_turn, Some(41));
        let TurnAction::ToolResult {
            tool_name,
            parameters,
            ..
        } = &result.action
        else {
            panic!("expected a tool-result turn");
        };
        assert_eq!(tool_name, "read_file");
        assert_eq!(parameters, &serde_json::json!({"path": "x"}));
    }

    /// A lone result (window starts after its call) renders from the
    /// event's restated tool_name, parameters null, no initiator.
    #[test]
    fn lone_result_stands_on_its_restated_name() {
        let mut fold = TurnFold::new();
        let result = fold.apply(90, &result_event(2, "tc-9", "exec")).unwrap();
        assert_eq!(result.initiating_turn, None);
        let TurnAction::ToolResult {
            tool_name,
            parameters,
            ..
        } = &result.action
        else {
            panic!("expected a tool-result turn");
        };
        assert_eq!(tool_name, "exec");
        assert_eq!(parameters, &serde_json::Value::Null);
    }

    /// The rendering bridge maps turns onto the exact transcript
    /// entries the WAL path produces — the flip's byte contract.
    #[test]
    fn transcript_entry_bridge_matches_shapes() {
        let mut fold = TurnFold::new();
        let turn = fold.apply(7, &assistant_event(1, None)).unwrap();
        match turn.transcript_entry() {
            TranscriptEntry::Assistant {
                model,
                cost_usd,
                is_error,
                ..
            } => {
                assert_eq!(model, "claude-haiku");
                assert_eq!(cost_usd, Some(0.01));
                // The WAL path renders a completed call as
                // `is_error: false`; a response event means exactly
                // that, so the bridge must not weaken it to `null`.
                assert_eq!(is_error, Some(false));
            }
            other => panic!("expected an assistant entry, got {other:?}"),
        }
    }
}
