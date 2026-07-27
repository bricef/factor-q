//! Construction of the turn-bearing events the reducer emits
//! (Phase 3d): one site per event shape, stamped with the Round.
//! Split from `runner.rs` to keep that file inside its size budget.

use uuid::Uuid;

use super::types::ToolCallRequest;
use crate::agent::AgentId;
use crate::events::{
    self, Event, EventPayload, LlmResponsePayload, ToolErrorKind, ToolResultPayload,
};

/// One tool-result event: the current Round, the restated tool name,
/// and the outcome fields.
#[allow(clippy::too_many_arguments)]
pub(crate) fn tool_result_event(
    round: u64,
    agent_id: &AgentId,
    invocation_id: Uuid,
    req: &ToolCallRequest,
    output: String,
    is_error: bool,
    error_kind: Option<ToolErrorKind>,
    duration_ms: u64,
) -> Event {
    Event::new(
        agent_id.clone(),
        invocation_id,
        EventPayload::ToolResult(ToolResultPayload {
            round,
            tool_name: req.tool_name.clone(),
            tool_call_id: req.tool_call_id.clone(),
            output,
            is_error,
            error_kind,
            duration_ms,
        }),
    )
}

/// One assistant-response event with its cost metadata attached.
#[allow(clippy::too_many_arguments)]
pub(crate) fn llm_response_event(
    round: u64,
    agent_id: &AgentId,
    invocation_id: Uuid,
    call_id: Uuid,
    response: &crate::llm::ChatResponse,
    origin: events::LlmCallOrigin,
    model: String,
    input_cost: f64,
    output_cost: f64,
    total_cost: f64,
    cumulative_cost: f64,
) -> Event {
    Event::new(
        agent_id.clone(),
        invocation_id,
        EventPayload::LlmResponse(LlmResponsePayload {
            round,
            call_id,
            content: response.content.clone(),
            tool_calls: response.tool_calls.clone(),
            stop_reason: response.stop_reason,
            usage: response.usage,
            origin: origin.clone(),
        }),
    )
    .with_cost(events::CostMetadata {
        call_id,
        model,
        input_tokens: response.usage.input_tokens,
        output_tokens: response.usage.output_tokens,
        cache_read_tokens: response.usage.cache_read_tokens,
        cache_write_tokens: response.usage.cache_write_tokens,
        input_cost,
        output_cost,
        total_cost,
        cumulative_invocation_cost: cumulative_cost,
        cumulative_agent_cost: cumulative_cost,
        origin,
    })
}
