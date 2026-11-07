//! rmcp's wire types ↔ factor-q's server-initiated values.
//!
//! Each conversion reproduces exactly what the runtime built from, or
//! returned as, the rmcp type before factor-q owned the value: the
//! messages a server receives, and the strings an evaluator judges, do
//! not change by passing through here.

use rmcp::model::{
    CreateMessageRequestParams, CreateMessageResult, ElicitRequestParams, ElicitResult,
    ElicitationAction, LoggingLevel, Role, Root, SamplingContent, SamplingMessage,
    SamplingMessageContentBlock,
};

use crate::mcp::{
    ElicitationOutcome, ElicitationReply, ElicitationRequest, McpLogLevel, SamplingReply,
    SamplingRequest, SamplingRole, SamplingStopReason, SamplingTurn, WorkspaceRoot,
};

/// An inbound `sampling/createMessage` as a [`SamplingRequest`].
/// `as_sent` is the params serialized before anything else touches
/// them — the string an input evaluator judges.
pub(super) fn sampling_request(params: CreateMessageRequestParams) -> SamplingRequest {
    let as_sent = serde_json::to_string(&params).unwrap_or_default();
    SamplingRequest {
        turns: params.messages.iter().map(sampling_turn).collect(),
        system_prompt: params.system_prompt,
        temperature: params.temperature,
        max_tokens: params.max_tokens,
        as_sent,
    }
}

/// The wire-shaped JSON a server would send, run through the same
/// conversion the handler applies — so a test outside the boundary can
/// build a [`SamplingRequest`] without naming rmcp's types.
#[cfg(test)]
pub(crate) fn sampling_request_from_wire(
    params: serde_json::Value,
) -> Result<SamplingRequest, serde_json::Error> {
    serde_json::from_value::<CreateMessageRequestParams>(params).map(sampling_request)
}

fn sampling_turn(message: &SamplingMessage) -> SamplingTurn {
    SamplingTurn {
        role: match message.role {
            Role::User => SamplingRole::User,
            Role::Assistant => SamplingRole::Assistant,
        },
        text: sampling_message_text(&message.content),
    }
}

/// Flatten a sampling message's content (single or multiple) into a
/// plain string for the agent model. Non-text content is represented
/// by a placeholder so conversation structure is preserved without
/// claiming to faithfully inject images/audio (a later capability).
fn sampling_message_text(content: &SamplingContent<SamplingMessageContentBlock>) -> String {
    match content {
        SamplingContent::Single(item) => sampling_item_text(item),
        SamplingContent::Multiple(items) => items
            .iter()
            .map(sampling_item_text)
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn sampling_item_text(item: &SamplingMessageContentBlock) -> String {
    match item {
        SamplingMessageContentBlock::Text(text) => text.text.clone(),
        _ => "[non-text sampling content omitted]".to_string(),
    }
}

/// A [`SamplingReply`] in the `CreateMessageResult` shape the protocol
/// returns to the server: one assistant text message, the model, and
/// the stop reason.
impl From<SamplingReply> for CreateMessageResult {
    fn from(reply: SamplingReply) -> Self {
        CreateMessageResult::new(SamplingMessage::assistant_text(reply.text), reply.model)
            .with_stop_reason(stop_reason_to_mcp(reply.stop_reason))
    }
}

fn stop_reason_to_mcp(stop_reason: SamplingStopReason) -> &'static str {
    match stop_reason {
        SamplingStopReason::EndTurn => CreateMessageResult::STOP_REASON_END_TURN,
        SamplingStopReason::MaxTokens => CreateMessageResult::STOP_REASON_END_MAX_TOKEN,
        SamplingStopReason::StopSequence => CreateMessageResult::STOP_REASON_END_SEQUENCE,
        SamplingStopReason::ToolUse => CreateMessageResult::STOP_REASON_TOOL_USE,
    }
}

/// An inbound `elicitation/create` as an [`ElicitationRequest`], one
/// variant per mode. `as_sent` is the params serialized as they
/// arrived.
pub(super) fn elicitation_request(params: ElicitRequestParams) -> ElicitationRequest {
    let as_sent = serde_json::to_string(&params).unwrap_or_default();
    match params {
        ElicitRequestParams::FormElicitationParams {
            message,
            requested_schema,
            ..
        } => ElicitationRequest::Form {
            message,
            requested_schema,
            as_sent,
        },
        ElicitRequestParams::UrlElicitationParams { message, url, .. } => ElicitationRequest::Url {
            message,
            url,
            as_sent,
        },
        _ => ElicitationRequest::Unknown { as_sent },
    }
}

/// An [`ElicitationReply`] as the protocol's result. factor-q attaches
/// no `_meta`.
impl From<ElicitationReply> for ElicitResult {
    fn from(reply: ElicitationReply) -> Self {
        let mut result = ElicitResult::new(match reply.action {
            ElicitationOutcome::Accept => ElicitationAction::Accept,
            ElicitationOutcome::Decline => ElicitationAction::Decline,
            ElicitationOutcome::Cancel => ElicitationAction::Cancel,
        });
        result.content = reply.content;
        result
    }
}

/// A [`WorkspaceRoot`] as the `Root` a `roots/list` answer carries.
impl From<WorkspaceRoot> for Root {
    fn from(root: WorkspaceRoot) -> Self {
        let wire = Root::new(root.uri);
        match root.name {
            Some(name) => wire.with_name(name),
            None => wire,
        }
    }
}

impl From<McpLogLevel> for LoggingLevel {
    fn from(level: McpLogLevel) -> Self {
        match level {
            McpLogLevel::Debug => LoggingLevel::Debug,
            McpLogLevel::Info => LoggingLevel::Info,
            McpLogLevel::Notice => LoggingLevel::Notice,
            McpLogLevel::Warning => LoggingLevel::Warning,
            McpLogLevel::Error => LoggingLevel::Error,
            McpLogLevel::Critical => LoggingLevel::Critical,
            McpLogLevel::Alert => LoggingLevel::Alert,
            McpLogLevel::Emergency => LoggingLevel::Emergency,
        }
    }
}

impl From<LoggingLevel> for McpLogLevel {
    fn from(level: LoggingLevel) -> Self {
        match level {
            LoggingLevel::Debug => McpLogLevel::Debug,
            LoggingLevel::Info => McpLogLevel::Info,
            LoggingLevel::Notice => McpLogLevel::Notice,
            LoggingLevel::Warning => McpLogLevel::Warning,
            LoggingLevel::Error => McpLogLevel::Error,
            LoggingLevel::Critical => McpLogLevel::Critical,
            LoggingLevel::Alert => McpLogLevel::Alert,
            LoggingLevel::Emergency => McpLogLevel::Emergency,
        }
    }
}

#[cfg(test)]
mod tests;
