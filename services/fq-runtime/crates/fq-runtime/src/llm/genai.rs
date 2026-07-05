//! Adapter that implements [`LlmClient`] on top of the
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

    /// Construct from the parsed `[providers.anthropic]` config. When
    /// `base_url` is set, the client is built with an endpoint
    /// override; otherwise the provider default applies.
    pub fn from_anthropic_config(config: &crate::config::AnthropicConfig) -> Self {
        match &config.base_url {
            Some(url) => Self::with_base_url(url.clone()),
            None => Self::new(),
        }
    }

    /// Construct a client that redirects every request to `base_url`
    /// instead of the provider-default endpoint. Used by tests (the
    /// `MockAnthropicServer`) and for operator overrides via the
    /// `[providers.anthropic]` `base_url` setting in `fq.toml`.
    ///
    /// Auth and model resolution are unchanged — the closure replaces
    /// only the endpoint on whichever `ServiceTarget` genai resolves
    /// for the requested model.
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        use ::std::sync::Arc;
        use provider::ServiceTarget;
        use provider::resolver::{Endpoint, ServiceTargetResolver};

        let url: Arc<str> = Arc::from(base_url.into());
        let resolver = ServiceTargetResolver::from_resolver_fn(
            move |target: ServiceTarget| -> Result<ServiceTarget, provider::resolver::Error> {
                Ok(ServiceTarget {
                    endpoint: Endpoint::from_owned(url.clone()),
                    auth: target.auth,
                    model: target.model,
                })
            },
        );
        let client = provider::Client::builder()
            .with_service_target_resolver(resolver)
            .build();
        Self { client }
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
) -> Result<
    (
        String,
        provider::chat::ChatRequest,
        provider::chat::ChatOptions,
    ),
    LlmError,
> {
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

    // Prompt-caching breakpoints. Two markers per request: the system
    // prompt, whose prefix (tools + system) is byte-identical on every
    // turn of an invocation, and the final message — the moving
    // breakpoint that lets each turn read the previous turn's cache
    // and extend it. The runner rebuilds the conversation append-only
    // from a single registry snapshot (ADR-0020), so the prefix match
    // holds by construction. genai maps the hint to `cache_control`
    // blocks on its Anthropic adapter only; other providers ignore it
    // (OpenAI/Gemini cache automatically, no marker exists to send).
    let last = chat_messages.len().saturating_sub(1);
    for (index, message) in chat_messages.iter_mut().enumerate() {
        let is_system = matches!(message.role, provider::chat::ChatRole::System);
        if is_system || index == last {
            message.options = Some(provider::chat::CacheControl::Ephemeral.into());
        }
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
            LlmError::InvalidResponse("tool role message is missing tool_call_id".to_string())
        })?;
        let content = content.unwrap_or_default();
        let tool_response = provider::chat::ToolResponse::new(call_id.into_inner(), content);
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
        if let Some(text) = content
            && !text.is_empty()
        {
            parts.push(provider::chat::ContentPart::Text(text));
        }
        for call in tool_calls {
            parts.push(provider::chat::ContentPart::ToolCall(
                provider::chat::ToolCall {
                    call_id: call.tool_call_id.into_inner(),
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
    provider::chat::ChatOptions {
        temperature: params.temperature,
        max_tokens: params.max_tokens,
        ..Default::default()
    }
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
    // Wrap tool_call_id at the provider->internal boundary. A
    // provider returning an empty string is a protocol bug we
    // surface immediately rather than letting it propagate.
    let tool_calls: Vec<MessageToolCall> = response
        .tool_calls()
        .into_iter()
        .map(|tc| {
            let tool_call_id = crate::events::ToolCallId::new(tc.call_id.clone())
                .map_err(|err| LlmError::InvalidResponse(err.to_string()))?;
            Ok::<_, LlmError>(MessageToolCall {
                tool_call_id,
                tool_name: tc.fn_name.clone(),
                parameters: tc.fn_arguments.clone(),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

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
    fn marks_system_and_last_message_for_prompt_caching() {
        let (_, req, _) =
            into_provider_request(request_with_system_and_user("claude-sonnet-4-5")).unwrap();
        let marked: Vec<bool> = req
            .messages
            .iter()
            .map(|m| {
                m.options
                    .as_ref()
                    .is_some_and(|o| o.cache_control.is_some())
            })
            .collect();
        // System prompt and the final (user) message carry the
        // breakpoint; nothing else does.
        assert_eq!(marked, vec![true, true]);
    }

    #[test]
    fn marks_only_system_and_final_message_in_longer_conversations() {
        let mut request = request_with_system_and_user("claude-sonnet-4-5");
        request.messages.push(Message {
            role: MessageRole::Assistant,
            content: Some("Hello!".to_string()),
            tool_calls: vec![],
            tool_call_id: None,
        });
        request.messages.push(Message {
            role: MessageRole::User,
            content: Some("And again.".to_string()),
            tool_calls: vec![],
            tool_call_id: None,
        });
        let (_, req, _) = into_provider_request(request).unwrap();
        let marked: Vec<bool> = req
            .messages
            .iter()
            .map(|m| {
                m.options
                    .as_ref()
                    .is_some_and(|o| o.cache_control.is_some())
            })
            .collect();
        assert_eq!(marked, vec![true, false, false, true]);
    }

    /// End-to-end through the mock Anthropic server: the wire request
    /// carries `cache_control` breakpoints where genai is expected to
    /// place them (system block + final message part), and the cache
    /// usage the server reports round-trips into [`TokenUsage`] with
    /// the total-prompt invariant (`input_tokens` = uncached + read +
    /// written).
    #[tokio::test]
    async fn cache_control_reaches_the_wire_and_usage_round_trips() {
        use crate::test_support::mock_anthropic::{MockAnthropicServer, MockResponse};

        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-mock-not-real") };
        let server = MockAnthropicServer::start().await;
        server.push_response(MockResponse::text("hello", 10, 5).with_cache_usage(70, 20));

        let client = GenAiClient::with_base_url(server.base_url());
        let response = client
            .chat(request_with_system_and_user("claude-sonnet-4-5"))
            .await
            .expect("chat via mock");

        // Usage invariant: Anthropic's wire `input_tokens` excludes
        // cache tokens; the adapter reports the total.
        assert_eq!(response.usage.input_tokens, 100);
        assert_eq!(response.usage.cache_read_tokens, 70);
        assert_eq!(response.usage.cache_write_tokens, 20);

        let received = server.received_requests();
        assert_eq!(received.len(), 1);
        let body = &received[0];

        // The system prompt is present. Note: genai 0.4.4 drops the
        // cache marker on a *single* system message (its Anthropic
        // adapter only renders system parts when the marked index is
        // > 0 — an off-by-one). That costs nothing within a run: the
        // final-message breakpoint below covers the whole prefix,
        // system included. If genai fixes the bug, the system block
        // becomes a parts array carrying its own marker — accept both.
        assert!(
            body["system"].is_string() || body["system"].is_array(),
            "system missing from wire request, got {:?}",
            body["system"]
        );

        // The final message's final content part carries the
        // load-bearing breakpoint.
        let messages = body["messages"].as_array().expect("messages array");
        let last_content = messages
            .last()
            .expect("at least one message")
            .get("content")
            .expect("content");
        let has_marker = match last_content {
            Value::Array(parts) => parts
                .iter()
                .any(|part| part["cache_control"]["type"] == "ephemeral"),
            other => other["cache_control"]["type"] == "ephemeral",
        };
        assert!(
            has_marker,
            "final message should carry cache_control, got {last_content:?}"
        );

        server.shutdown().await;
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
            tool_call_id: Some(crate::events::ToolCallId::new("toolu_01ABC").unwrap()),
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
                tool_call_id: crate::events::ToolCallId::new("toolu_01ABC").unwrap(),
                tool_name: "read_file".to_string(),
                parameters: serde_json::json!({"path": "/tmp/x"}),
            }],
            tool_call_id: None,
        })
        .unwrap();
        assert!(matches!(msg.role, provider::chat::ChatRole::Assistant));
    }

    /// Drift detector against the real Anthropic API. Confirms
    /// that our genai-adapter pipeline still successfully sends
    /// a request and parses the response — i.e. that Anthropic
    /// hasn't shifted the wire contract under us in a way the
    /// mock-server tests can't see.
    ///
    /// Marked `#[ignore]` so `cargo test` skips it. Run via
    /// `just acceptance-drift` or
    /// `cargo test -- --ignored anthropic_real_api`. Requires
    /// `ANTHROPIC_API_KEY`; one short Haiku call, ~fractions of
    /// a cent per run.
    #[tokio::test]
    #[ignore = "live Anthropic API; run via `just acceptance-drift`"]
    async fn anthropic_real_api_basic_response_parses() {
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
                    content: Some("You are a test. Reply in exactly one word: OK".to_string()),
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
        assert!(
            response.content.as_deref().is_some_and(|c| !c.is_empty()),
            "expected non-empty content, got {:?}",
            response.content
        );
        assert!(
            response.usage.input_tokens > 0,
            "expected positive input tokens, got {}",
            response.usage.input_tokens
        );
    }

    #[tokio::test]
    async fn with_base_url_overrides_resolved_endpoint() {
        let client = GenAiClient::with_base_url("http://127.0.0.1:9999");
        let target = client
            .client
            .resolve_service_target("claude-haiku-4-5")
            .await
            .expect("resolve service target");
        assert_eq!(target.endpoint.base_url(), "http://127.0.0.1:9999");
    }

    #[tokio::test]
    async fn from_anthropic_config_without_base_url_uses_default_endpoint() {
        let cfg = crate::config::AnthropicConfig::default();
        let client = GenAiClient::from_anthropic_config(&cfg);
        let target = client
            .client
            .resolve_service_target("claude-haiku-4-5")
            .await
            .expect("resolve service target");
        // genai's default Anthropic endpoint is the public API URL.
        assert!(
            target.endpoint.base_url().contains("anthropic.com"),
            "expected default endpoint to point at Anthropic, got {}",
            target.endpoint.base_url()
        );
    }

    #[tokio::test]
    async fn from_anthropic_config_with_base_url_uses_override() {
        let cfg = crate::config::AnthropicConfig {
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
            base_url: Some("http://127.0.0.1:54321".to_string()),
        };
        let client = GenAiClient::from_anthropic_config(&cfg);
        let target = client
            .client
            .resolve_service_target("claude-haiku-4-5")
            .await
            .expect("resolve service target");
        assert_eq!(target.endpoint.base_url(), "http://127.0.0.1:54321");
    }
}
