//! Adapter that implements [`LlmClient`](super::LlmClient) on top of the
//! `genai` crate.
//!
//! The adapter owns the conversion between factor-q's internal types
//! and `genai`'s types in one place. Nothing outside this module
//! depends on `::genai` at all — the event schema and the executor
//! stay free of the underlying library.
//!
//! Auth is handled by `genai` itself via environment variables
//! (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, etc). We don't override the
//! resolver in this adapter; operators configure which env var to use
//! per provider in `fq.toml` and ensure it's set in the runtime
//! environment.

use async_trait::async_trait;
use serde_json::Value;

use crate::events::{
    Message, MessageRole, MessageToolCall, RequestParams, StopReason, TokenUsage, ToolSchema,
};

use super::{ChatRequest, ChatResponse, LlmClient, LlmError};

// Use the crate via its fully qualified name to avoid confusion with
// our parent module name.
use ::genai as provider;

/// Production LLM client backed by the `genai` crate.
#[derive(Clone)]
pub struct GenAiClient {
    client: provider::Client,
}

impl GenAiClient {
    /// Construct a client using `genai`'s default configuration, which
    /// resolves API keys from provider-specific environment variables.
    pub fn new() -> Self {
        Self {
            client: provider::Client::default(),
        }
    }
}

impl Default for GenAiClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LlmClient for GenAiClient {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        let (model, chat_req, options) = into_provider_request(request)?;
        let response = self
            .client
            .exec_chat(&model, chat_req, Some(&options))
            .await
            .map_err(map_error)?;
        from_provider_response(response)
    }
}

/// Convert an internal `ChatRequest` into the `(model, ChatRequest, ChatOptions)`
/// tuple that `genai::Client::exec_chat` expects.
fn into_provider_request(
    request: ChatRequest,
) -> Result<(String, provider::chat::ChatRequest, provider::chat::ChatOptions), LlmError> {
    let ChatRequest {
        model,
        messages,
        tools,
        params,
    } = request;

    let mut chat_messages = Vec::with_capacity(messages.len());
    for msg in messages {
        chat_messages.push(convert_message(msg)?);
    }

    let mut chat_req = provider::chat::ChatRequest::new(chat_messages);
    if !tools.is_empty() {
        let converted_tools: Vec<provider::chat::Tool> =
            tools.into_iter().map(convert_tool_schema).collect();
        chat_req = chat_req.with_tools(converted_tools);
    }

    let options = convert_params(params);

    Ok((model, chat_req, options))
}

fn convert_message(msg: Message) -> Result<provider::chat::ChatMessage, LlmError> {
    let Message {
        role,
        content,
        tool_calls,
        tool_call_id,
    } = msg;

    // A `tool` role message carries a tool response and MUST have a
    // matching tool_call_id from the earlier assistant message.
    if matches!(role, MessageRole::Tool) {
        let call_id = tool_call_id.ok_or_else(|| {
            LlmError::InvalidResponse(
                "tool role message is missing tool_call_id".to_string(),
            )
        })?;
        let content = content.unwrap_or_default();
        let tool_response = provider::chat::ToolResponse::new(call_id, content);
        return Ok(provider::chat::ChatMessage {
            role: provider::chat::ChatRole::Tool,
            content: provider::chat::MessageContent::from_parts(vec![
                provider::chat::ContentPart::ToolResponse(tool_response),
            ]),
            options: None,
        });
    }

    // Assistant messages with tool calls: carry the tool calls as
    // content parts alongside any text.
    if matches!(role, MessageRole::Assistant) && !tool_calls.is_empty() {
        let mut parts: Vec<provider::chat::ContentPart> = Vec::new();
        if let Some(text) = content {
            if !text.is_empty() {
                parts.push(provider::chat::ContentPart::Text(text));
            }
        }
        for call in tool_calls {
            parts.push(provider::chat::ContentPart::ToolCall(
                provider::chat::ToolCall {
                    call_id: call.tool_call_id,
                    fn_name: call.tool_name,
                    fn_arguments: call.parameters,
                },
            ));
        }
        return Ok(provider::chat::ChatMessage {
            role: provider::chat::ChatRole::Assistant,
            content: provider::chat::MessageContent::from_parts(parts),
            options: None,
        });
    }

    // Ordinary text-only messages.
    let text = content.unwrap_or_default();
    let chat_msg = match role {
        MessageRole::System => provider::chat::ChatMessage::system(text),
        MessageRole::User => provider::chat::ChatMessage::user(text),
        MessageRole::Assistant => provider::chat::ChatMessage::assistant(text),
        MessageRole::Tool => unreachable!("handled above"),
    };
    Ok(chat_msg)
}

fn convert_tool_schema(tool: ToolSchema) -> provider::chat::Tool {
    let ToolSchema {
        name,
        description,
        parameters_schema,
    } = tool;
    let mut out = provider::chat::Tool::new(name);
    if !description.is_empty() {
        out = out.with_description(description);
    }
    if parameters_schema != Value::Null {
        out = out.with_schema(parameters_schema);
    }
    out
}

fn convert_params(params: RequestParams) -> provider::chat::ChatOptions {
    let mut options = provider::chat::ChatOptions::default();
    options.temperature = params.temperature;
    options.max_tokens = params.max_tokens;
    options
}

/// Convert a genai `ChatResponse` into our internal shape.
fn from_provider_response(
    response: provider::chat::ChatResponse,
) -> Result<ChatResponse, LlmError> {
    let content_text = response.first_text().map(|s| s.to_string());
    let usage = convert_usage(&response.usage);

    // Clone tool calls out of the content before consuming it. The
    // response's own `into_tool_calls` consumes the whole response, so
    // we collect both text and calls via separate accessors.
    let tool_calls: Vec<MessageToolCall> = response
        .tool_calls()
        .into_iter()
        .map(|tc| MessageToolCall {
            tool_call_id: tc.call_id.clone(),
            tool_name: tc.fn_name.clone(),
            parameters: tc.fn_arguments.clone(),
        })
        .collect();

    let stop_reason = if !tool_calls.is_empty() {
        StopReason::ToolUse
    } else {
        StopReason::EndTurn
    };

    Ok(ChatResponse {
        content: content_text,
        tool_calls,
        stop_reason,
        usage,
    })
}

fn convert_usage(usage: &provider::chat::Usage) -> TokenUsage {
    let input_tokens = usage.prompt_tokens.unwrap_or(0).max(0) as u32;
    let output_tokens = usage.completion_tokens.unwrap_or(0).max(0) as u32;

    let (cache_read, cache_write) = match &usage.prompt_tokens_details {
        Some(d) => (
            d.cached_tokens.unwrap_or(0).max(0) as u32,
            d.cache_creation_tokens.unwrap_or(0).max(0) as u32,
        ),
        None => (0, 0),
    };

    TokenUsage {
        input_tokens,
        output_tokens,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
    }
}

/// Map a `genai::Error` to our `LlmError` variants. Specific auth
/// failures become [`LlmError::Auth`]; everything else is reported as
/// [`LlmError::RequestFailed`] with the underlying message.
///
/// `genai::Error::Resolver` wraps the auth resolver's own error type —
/// when the resolver fails it is almost always an auth problem (for
/// example `ApiKeyEnvNotFound`), so we treat it as `Auth` too.
fn map_error(err: provider::Error) -> LlmError {
    let message = err.to_string();
    match err {
        provider::Error::RequiresApiKey { .. }
        | provider::Error::NoAuthResolver { .. }
        | provider::Error::NoAuthData { .. }
        | provider::Error::Resolver { .. } => LlmError::Auth(message),
        _ => LlmError::RequestFailed(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{MessageRole, RequestParams};

    fn request_with_system_and_user(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.to_string(),
            messages: vec![
                Message {
                    role: MessageRole::System,
                    content: Some("You are a helper.".to_string()),
                    tool_calls: vec![],
                    tool_call_id: None,
                },
                Message {
                    role: MessageRole::User,
                    content: Some("Say hello.".to_string()),
                    tool_calls: vec![],
                    tool_call_id: None,
                },
            ],
            tools: vec![],
            params: RequestParams {
                temperature: Some(0.2),
                max_tokens: Some(64),
            },
        }
    }

    #[test]
    fn converts_basic_request() {
        let (model, req, opts) =
            into_provider_request(request_with_system_and_user("gpt-4o-mini")).unwrap();
        assert_eq!(model, "gpt-4o-mini");
        assert_eq!(req.messages.len(), 2);
        assert_eq!(opts.temperature, Some(0.2));
        assert_eq!(opts.max_tokens, Some(64));
    }

    #[test]
    fn converts_tool_schema() {
        let tool = convert_tool_schema(ToolSchema {
            name: "read_file".to_string(),
            description: "Read a file from disk.".to_string(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                }
            }),
        });
        assert_eq!(tool.name, "read_file");
        assert!(tool.description.is_some());
        assert!(tool.schema.is_some());
    }

    #[test]
    fn converts_tool_message_with_id() {
        let msg = convert_message(Message {
            role: MessageRole::Tool,
            content: Some("file contents".to_string()),
            tool_calls: vec![],
            tool_call_id: Some("toolu_01ABC".to_string()),
        })
        .unwrap();
        assert!(matches!(msg.role, provider::chat::ChatRole::Tool));
    }

    #[test]
    fn tool_message_without_id_is_error() {
        let err = convert_message(Message {
            role: MessageRole::Tool,
            content: Some("nothing".to_string()),
            tool_calls: vec![],
            tool_call_id: None,
        })
        .unwrap_err();
        assert!(matches!(err, LlmError::InvalidResponse(_)));
    }

    #[test]
    fn converts_assistant_message_with_tool_calls() {
        let msg = convert_message(Message {
            role: MessageRole::Assistant,
            content: Some("I'll read that file.".to_string()),
            tool_calls: vec![MessageToolCall {
                tool_call_id: "toolu_01ABC".to_string(),
                tool_name: "read_file".to_string(),
                parameters: serde_json::json!({"path": "/tmp/x"}),
            }],
            tool_call_id: None,
        })
        .unwrap();
        assert!(matches!(msg.role, provider::chat::ChatRole::Assistant));
    }

    /// Live network test. Requires FQ_NETWORK_TESTS=1 and a real API
    /// key for Anthropic. Skipped in normal test runs.
    #[tokio::test]
    async fn live_anthropic_round_trip() {
        if std::env::var("FQ_NETWORK_TESTS").is_err() {
            eprintln!("skipping: set FQ_NETWORK_TESTS=1");
            return;
        }
        if std::env::var("ANTHROPIC_API_KEY").is_err() {
            eprintln!("skipping: ANTHROPIC_API_KEY not set");
            return;
        }

        let client = GenAiClient::new();
        let request = ChatRequest {
            model: "claude-haiku-4-5".to_string(),
            messages: vec![
                Message {
                    role: MessageRole::System,
                    content: Some(
                        "You are a test. Reply in exactly one word: OK".to_string(),
                    ),
                    tool_calls: vec![],
                    tool_call_id: None,
                },
                Message {
                    role: MessageRole::User,
                    content: Some("Say OK.".to_string()),
                    tool_calls: vec![],
                    tool_call_id: None,
                },
            ],
            tools: vec![],
            params: RequestParams {
                temperature: Some(0.0),
                max_tokens: Some(16),
            },
        };

        let response = client.chat(request).await.expect("chat");
        assert!(response.content.is_some(), "expected some content");
        assert!(
            response.usage.input_tokens > 0,
            "expected positive input tokens, got {}",
            response.usage.input_tokens
        );
    }
}
