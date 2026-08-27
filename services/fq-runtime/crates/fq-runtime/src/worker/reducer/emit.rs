//! Construction of the turn-bearing events the reducer emits
//! (Phase 3d): one site per event shape, stamped with the Round.
//! Split from `runner.rs` to keep that file inside its size budget.

use uuid::Uuid;

use super::types::ToolCallRequest;
use crate::agent::AgentId;
use crate::events::{
    self, Event, EventPayload, LlmErrorKind, LlmFailurePayload, LlmResponsePayload, TokenUsage,
    ToolErrorKind, ToolResultPayload,
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
            parts: response.parts.clone(),
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

/// One failed LLM call, as the runner knows it at the moment it gives
/// up. The WAL close and the failure event need the same facts, so
/// they are gathered once rather than threaded through two long
/// argument lists.
pub(crate) struct FailedCall<'a> {
    pub(crate) agent_id: &'a AgentId,
    pub(crate) invocation_id: Uuid,
    pub(crate) call_id: Uuid,
    pub(crate) model: &'a str,
    pub(crate) error_kind: LlmErrorKind,
    pub(crate) error_message: String,
    pub(crate) duration_ms: u64,
    /// What the provider billed, when we know. `None` is not zero —
    /// see [`crate::events::LlmFailurePayload::usage`].
    pub(crate) usage: Option<TokenUsage>,
    pub(crate) origin: &'a events::LlmCallOrigin,
}

/// One failure event: the terminal outcome of a call that produced no
/// response.
///
/// `priced` is `Some` only when the provider's usage was recoverable
/// *and* the model has pricing — an empty completion, in practice.
/// When it is `None` the envelope carries no [`events::CostMetadata`]
/// at all, and deliberately not a zeroed one: the projection's
/// retention sweep exempts rows on `total_cost IS NOT NULL`, so a
/// zero-cost failure would be kept forever as a fake cost record,
/// where an absent-cost one is swept with the rest of the trail.
pub(crate) fn llm_failure_event(
    round: u64,
    call: &FailedCall<'_>,
    priced: Option<(f64, f64, f64)>,
    cumulative_cost: f64,
) -> Event {
    let event = Event::new(
        call.agent_id.clone(),
        call.invocation_id,
        EventPayload::LlmFailure(LlmFailurePayload {
            round,
            call_id: call.call_id,
            model: call.model.to_string(),
            error_kind: call.error_kind,
            error_message: call.error_message.clone(),
            duration_ms: call.duration_ms,
            usage: call.usage,
            origin: call.origin.clone(),
        }),
    );
    let (Some(usage), Some((input_cost, output_cost, total_cost))) = (call.usage, priced) else {
        return event;
    };
    event.with_cost(events::CostMetadata {
        call_id: call.call_id,
        model: call.model.to_string(),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cache_write_tokens: usage.cache_write_tokens,
        input_cost,
        output_cost,
        total_cost,
        cumulative_invocation_cost: cumulative_cost,
        cumulative_agent_cost: cumulative_cost,
        origin: call.origin.clone(),
    })
}
