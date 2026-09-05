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
//! per provider in `fqd.toml` and ensure it's set in the runtime
//! environment.

use async_trait::async_trait;
use serde_json::Value;

use crate::events::{
    AssistantPart, Effort, Message, MessageToolCall, RequestParams, StopReason, TokenUsage,
    ToolSchema,
};

use super::{ChatRequest, ChatResponse, LlmClient, LlmError};

// Use the crate via its fully qualified name to avoid confusion with
// our parent module name.
use ::genai as provider;

/// The wire goldens: what this adapter decodes and what it sends, pinned
/// per scenario so a dependency change cannot move either unnoticed.
#[cfg(test)]
mod wire_goldens;

/// The provider client could not be built.
///
/// genai builds an HTTP client underneath (TLS roots, proxy settings from
/// the environment), and since 0.7.0-beta.21 reports failure instead of
/// panicking. That happens once, at construction — the daemon's startup —
/// and never per request, so it is its own error rather than an
/// [`LlmError`] variant that the event schema would then have to name.
#[derive(Debug, thiserror::Error)]
#[error("could not build the provider client: {cause}")]
pub struct ClientBuildError {
    cause: String,
}

fn client_build_error(err: provider::Error) -> ClientBuildError {
    ClientBuildError {
        cause: err.to_string(),
    }
}

/// Production LLM client backed by the `genai` crate.
#[derive(Clone)]
pub struct GenAiClient {
    client: provider::Client,
}

impl GenAiClient {
    /// Construct a client using `genai`'s default configuration, which
    /// resolves API keys from provider-specific environment variables.
    pub fn new() -> Result<Self, ClientBuildError> {
        let client = provider::Client::new().map_err(client_build_error)?;
        Ok(Self { client })
    }

    /// Construct from the parsed `[providers.anthropic]` config. When
    /// `base_url` is set, the client is built with an endpoint
    /// override; otherwise the provider default applies.
    pub fn from_anthropic_config(
        config: &crate::config::AnthropicConfig,
    ) -> Result<Self, ClientBuildError> {
        match &config.base_url {
            Some(url) => Self::with_base_url(url.clone()),
            None => Self::new(),
        }
    }

    /// Construct a client that redirects every request to `base_url`
    /// instead of the provider-default endpoint. Used by tests (the
    /// `MockAnthropicServer`) and for operator overrides via the
    /// `[providers.anthropic]` `base_url` setting in `fqd.toml`.
    ///
    /// Auth and model resolution are unchanged — the closure replaces
    /// only the endpoint on whichever `ServiceTarget` genai resolves
    /// for the requested model.
    pub fn with_base_url(base_url: impl Into<String>) -> Result<Self, ClientBuildError> {
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
            .build()
            .map_err(client_build_error)?;
        Ok(Self { client })
    }

    /// Build a client from the whole `[providers]` config — a single
    /// genai client whose `ServiceTargetResolver` routes each request to
    /// the provider that declares the requested model.
    ///
    /// Routing is **model-id keyed**: every extra provider lists the ids
    /// it serves (`models = [...]`), and a request for one of those ids
    /// is redirected to that provider's `base_url` (when set) with auth
    /// taken from its `api_key_env` (an env lookup, so the key lives
    /// neither in the config nor anywhere agent-visible — ADR-0028) and
    /// tagged with the adapter kind for its `api_shape`. Requests for
    /// unlisted models fall through to genai's default resolution
    /// (`claude-*`, `gpt-*`, … via their standard env vars), so the
    /// Anthropic path is unchanged.
    ///
    /// `anthropic.base_url`, when set, keeps its historical meaning:
    /// redirect every Anthropic-adapter request to that endpoint (the
    /// mock server, or a Bedrock-style proxy) — no `models` list needed.
    ///
    /// When nothing needs overriding this is exactly [`Self::new`].
    pub fn from_providers(
        config: &crate::config::ProvidersConfig,
    ) -> Result<Self, ClientBuildError> {
        use ::std::collections::HashMap;
        use ::std::sync::Arc;
        use provider::ServiceTarget;
        use provider::adapter::AdapterKind;
        use provider::resolver::{AuthData, Endpoint, ServiceTargetResolver};

        struct Route {
            base_url: Option<Arc<str>>,
            api_key_env: String,
            adapter_kind: AdapterKind,
        }

        // model id -> route, from every extra provider's `models` list.
        let mut by_model: HashMap<String, Route> = HashMap::new();
        for provider_cfg in config.extra.values() {
            for model in &provider_cfg.models {
                by_model.insert(
                    model.clone(),
                    Route {
                        base_url: provider_cfg
                            .base_url
                            .clone()
                            .map(ensure_trailing_slash)
                            .map(Arc::from),
                        api_key_env: provider_cfg.api_key_env.clone(),
                        adapter_kind: adapter_kind_for(provider_cfg.api_shape),
                    },
                );
            }
        }

        // anthropic: adapter-keyed base_url override (mock / proxy).
        let anthropic_base_url: Option<Arc<str>> = config
            .anthropic
            .as_ref()
            .and_then(|a| a.base_url.clone())
            .map(Arc::from);

        // Nothing to override -> genai default (identical to `new()`).
        if by_model.is_empty() && anthropic_base_url.is_none() {
            return Self::new();
        }

        let by_model = Arc::new(by_model);
        let resolver = ServiceTargetResolver::from_resolver_fn(
            move |target: ServiceTarget| -> Result<ServiceTarget, provider::resolver::Error> {
                let model_name = target.model.model_name.to_string();
                if let Some(route) = by_model.get(&model_name) {
                    let endpoint = match &route.base_url {
                        Some(url) => Endpoint::from_owned(url.clone()),
                        None => target.endpoint,
                    };
                    return Ok(ServiceTarget {
                        endpoint,
                        auth: AuthData::from_env(route.api_key_env.clone()),
                        model: provider::ModelIden::new(
                            route.adapter_kind,
                            target.model.model_name,
                        ),
                    });
                }
                if let Some(url) = &anthropic_base_url
                    && target.model.adapter_kind == AdapterKind::Anthropic
                {
                    return Ok(ServiceTarget {
                        endpoint: Endpoint::from_owned(url.clone()),
                        ..target
                    });
                }
                Ok(target)
            },
        );
        let client = provider::Client::builder()
            .with_service_target_resolver(resolver)
            .build()
            .map_err(client_build_error)?;
        Ok(Self { client })
    }
}

/// Map the config's `api_shape` onto a genai adapter kind. Every
/// OpenAI-compatible provider (Groq, Together, OpenRouter, local vLLM,
/// …) uses the OpenAI adapter — genai formats the request identically
/// and only the endpoint + key differ.
fn adapter_kind_for(shape: crate::config::ApiShape) -> provider::adapter::AdapterKind {
    use crate::config::ApiShape;
    use provider::adapter::AdapterKind;
    match shape {
        ApiShape::Anthropic => AdapterKind::Anthropic,
        ApiShape::Openai | ApiShape::OpenaiCompatible => AdapterKind::OpenAI,
        ApiShape::Gemini => AdapterKind::Gemini,
        ApiShape::Ollama => AdapterKind::Ollama,
    }
}

/// genai builds each request URL by `Url::join`-ing the adapter's path
/// (e.g. `chat/completions`) onto the endpoint base — and RFC-3986 join
/// drops the base's final path segment unless it ends in `/` (so
/// `.../v1` + `chat/completions` becomes `.../chat/completions`, losing
/// `v1`, and 404s). Provider docs routinely show base URLs without the
/// trailing slash, so normalise it here: config-first should just work,
/// not fail cryptically on a silently mangled path.
fn ensure_trailing_slash(url: String) -> String {
    if url.ends_with('/') {
        url
    } else {
        format!("{url}/")
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
        chat_messages.push(convert_message(msg, &model)?);
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

/// Convert one of our turns into the provider's message shape.
///
/// `target_model` is what this request will be sent to, and it is what
/// makes the cross-model reasoning strip possible: reasoning is tied to
/// the model that produced it, so a part from a different model is
/// dropped here rather than replayed (ADR-0034 D5). This is the single
/// choke point for that rule — the last place before the wire, and the
/// only one that knows both sides.
fn convert_message(
    msg: Message,
    target_model: &str,
) -> Result<provider::chat::ChatMessage, LlmError> {
    let chat_msg = match msg {
        Message::System { text } => provider::chat::ChatMessage::system(text),
        Message::User { text } => provider::chat::ChatMessage::user(text),

        Message::Assistant { parts } => {
            // Empty text parts are dropped rather than sent: providers
            // reject an empty text block, and the previous shape filtered
            // them the same way.
            let converted: Vec<provider::chat::ContentPart> = parts
                .into_iter()
                .filter_map(|part| match part {
                    crate::events::AssistantPart::Text { text } if text.is_empty() => None,
                    crate::events::AssistantPart::Text { text } => {
                        Some(provider::chat::ContentPart::Text(text))
                    }
                    crate::events::AssistantPart::ToolCall(call) => Some(
                        provider::chat::ContentPart::ToolCall(provider::chat::ToolCall {
                            call_id: call.tool_call_id.into_inner(),
                            fn_name: call.tool_name,
                            fn_arguments: call.parameters,
                            thought_signatures: None,
                        }),
                    ),
                    crate::events::AssistantPart::Reasoning(reasoning) => {
                        encode_reasoning(reasoning, target_model)
                    }
                })
                .collect();

            // A turn with a single text part and nothing else is sent as a
            // plain assistant message, which is the shape the previous code
            // produced and what the provider snapshot pins.
            match converted.as_slice() {
                [provider::chat::ContentPart::Text(text)] => {
                    provider::chat::ChatMessage::assistant(text.clone())
                }
                _ => provider::chat::ChatMessage {
                    role: provider::chat::ChatRole::Assistant,
                    content: provider::chat::MessageContent::from_parts(converted),
                    options: None,
                },
            }
        }

        Message::ToolResults { results } => provider::chat::ChatMessage {
            role: provider::chat::ChatRole::Tool,
            content: provider::chat::MessageContent::from_parts(
                results
                    .into_iter()
                    .map(|result| {
                        provider::chat::ContentPart::ToolResponse(
                            provider::chat::ToolResponse::new(
                                result.tool_call_id.into_inner(),
                                result.output,
                            ),
                        )
                    })
                    .collect::<Vec<provider::chat::ContentPart>>(),
            ),
            options: None,
        },
    };
    Ok(chat_msg)
}

/// Encode one reasoning part for the wire, or drop it and say why.
///
/// Two reasons a part does not go out, and neither is silent:
///
/// **Model mismatch.** Reasoning is verified or ignored against the model
/// that produced it, so replaying it to a different model is at best
/// wasted input tokens and at worst a protocol violation. Multi-agent
/// graphs have cross-model edges by construction (ADR-0003), so this is
/// an expected drop rather than an error — logged at debug, not warn.
///
/// **No readable text.** `Opaque` reasoning carries its content in a
/// provider token, and genai's `ContentPart::ReasoningContent` is a bare
/// string with nowhere to put one. Anthropic's signed and redacted blocks
/// round-trip through `ContentPart::Custom` instead, which genai's
/// Anthropic adapter echoes verbatim.
///
/// Note what this deliberately does *not* do: branch on the provider. We
/// hand genai the semantic part and let its adapter encode it — the
/// OpenAI-compatible adapter hoists `ReasoningContent` into the sibling
/// `reasoning_content` field, which is exactly the Kimi/DeepSeek
/// round-trip, while the Anthropic adapter omits an unpaired one. That
/// omission is correct: a `Plain` block has no signature, and Anthropic
/// rejects a thinking block that lacks one, so it must never reach that
/// wire.
fn encode_reasoning(
    reasoning: crate::events::Reasoning,
    target_model: &str,
) -> Option<provider::chat::ContentPart> {
    if reasoning.model != target_model {
        tracing::debug!(
            produced_by = %reasoning.model,
            target_model = %target_model,
            "dropping reasoning on a cross-model edge; it is tied to the model that produced it"
        );
        return None;
    }
    match reasoning.content {
        // Readable-only reasoning is a semantic part: genai's
        // OpenAI-compatible adapter hoists it into the sibling
        // `reasoning_content` field (the Kimi/DeepSeek round trip), and
        // its Anthropic adapter drops it — correctly, since a block with
        // no signature is one Anthropic would reject.
        crate::events::ReasoningContent::Plain { text } => {
            Some(provider::chat::ContentPart::ReasoningContent(text))
        }
        // Signed reasoning goes back as the provider's own block,
        // verbatim. Anthropic verifies a thinking block *against* its
        // signature, so the block is the unit that can be replayed — not
        // the text, and not the signature alone.
        //
        // `Custom` rather than a provider branch: genai's Anthropic
        // adapter echoes these blocks unchanged and its OpenAI adapter
        // ignores them, so this arm is right for both without asking
        // which one we are talking to — which this layer cannot know.
        // genai also offers a `ThoughtSignature` + `ReasoningContent`
        // pair that its Anthropic adapter rebuilds into a block; sending
        // the block we hold keeps every key the provider put in it.
        crate::events::ReasoningContent::Signed { token, .. } => Some(
            provider::chat::ContentPart::Custom(provider::chat::CustomPart {
                model_iden: None,
                data: token,
            }),
        ),
        // Opaque reasoning is one of two tokens, and the token says which
        // by construction — still not a provider branch, but a branch on
        // what we hold:
        //
        // - A bare continuity token (Gemini's `thoughtSignature`) goes back
        //   as genai's `ThoughtSignature` part, which its Gemini adapter
        //   attaches to the next function call it meets — the part the
        //   token came with, in the order this crate records. A `Custom`
        //   part would be ignored on that wire.
        // - A whole provider block (Anthropic's `redacted_thinking`) goes
        //   back verbatim as `Custom`, like a signed block.
        crate::events::ReasoningContent::Opaque { token } => match bare_signature_of(&token) {
            Some(signature) => Some(provider::chat::ContentPart::ThoughtSignature(
                signature.to_string(),
            )),
            None => Some(provider::chat::ContentPart::Custom(
                provider::chat::CustomPart {
                    model_iden: None,
                    data: token,
                },
            )),
        },
    }
}

/// The token type this crate mints for a continuity token that arrived
/// with no readable text and no surrounding block — Gemini's
/// `thoughtSignature`. The type is ours, so a reader of the event log
/// can tell it from a provider block; the signature is the provider's,
/// verbatim.
const BARE_SIGNATURE_TYPE: &str = "thought_signature";

/// An opaque reasoning part for a bare continuity token.
fn bare_signature(signature: &str) -> crate::events::ReasoningContent {
    crate::events::ReasoningContent::Opaque {
        token: serde_json::json!({
            "type": BARE_SIGNATURE_TYPE,
            "signature": signature,
        }),
    }
}

/// The signature inside an opaque token minted by [`bare_signature`], or
/// `None` for any other opaque token (a provider block, replayed whole).
fn bare_signature_of(token: &Value) -> Option<&str> {
    if token.get("type").and_then(Value::as_str) != Some(BARE_SIGNATURE_TYPE) {
        return None;
    }
    token.get("signature").and_then(Value::as_str)
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
        reasoning_effort: params.effort.map(|effort| match effort {
            Effort::Minimal => provider::chat::ReasoningEffort::Minimal,
            Effort::Low => provider::chat::ReasoningEffort::Low,
            Effort::Medium => provider::chat::ReasoningEffort::Medium,
            Effort::High => provider::chat::ReasoningEffort::High,
            Effort::XHigh => provider::chat::ReasoningEffort::XHigh,
        }),
        ..Default::default()
    }
}

/// Convert a genai `ChatResponse` into our internal shape.
fn from_provider_response(
    response: provider::chat::ChatResponse,
) -> Result<ChatResponse, LlmError> {
    let usage = convert_usage(&response.usage);

    // Build the turn's parts in the order the provider returned them.
    // Ordering is a provider concern (ADR-0034 I6) — Anthropic requires
    // thinking blocks first, OpenAI-compatible providers carry reasoning
    // as a sibling field where position is meaningless — so we preserve
    // what arrived rather than imposing an order of our own.
    //
    // Wrap tool_call_id at the provider->internal boundary. A provider
    // returning an empty string is a protocol bug we surface immediately
    // rather than letting it propagate.
    let mut parts: Vec<AssistantPart> = Vec::new();

    // Reasoning first, which is where Anthropic requires it and where
    // position is meaningless for OpenAI-compatible providers (they
    // carry it as a sibling field, not in the content list) — so leading
    // is correct for both.
    //
    // The model we ASKED for, not the one the provider reported: the
    // strip compares against the next request's target, and that is
    // expressed in the same vocabulary.
    let model = response.model_iden.model_name.to_string();

    // How reasoning arrives depends on the adapter, and every shape is
    // genai's transport rather than ours:
    //
    // - Anthropic: each `thinking` block is a `ThoughtSignature` part
    //   immediately followed by a `ReasoningContent` part, in block order
    //   (genai 0.7.0-beta.21, upstream PR #275). A `redacted_thinking`
    //   block stays a `Custom` part carrying the raw JSON.
    // - Gemini: a `thoughtSignature` is a `ThoughtSignature` part with
    //   *no* reasoning text behind it — the token is the whole content —
    //   and a thought summary, when the API returns one, is not a part
    //   at all but the `reasoning_content` field.
    // - OpenAI-compatible: the sibling `reasoning_content` field, surfaced
    //   as `ChatResponse::reasoning_content` and never as a part.
    //
    // So a signature is paired only with the reasoning text that
    // immediately follows it; a signature that anything else follows
    // stood alone, and is carried as an opaque part in its place
    // (ADR-0034 D2's Gemini row). A signed pair is joined back into the
    // block Anthropic verifies it by, because the block is the only unit
    // that can be replayed — not the text, and not the signature alone
    // (Appendix A). What the reducer records is therefore the same
    // whether the block came as one part or two.
    let mut text_carried = false;
    let mut pending_signature: Option<&str> = None;
    for part in response.content.iter() {
        if !matches!(part, provider::chat::ContentPart::ReasoningContent(_))
            && let Some(signature) = pending_signature.take()
        {
            parts.push(AssistantPart::Reasoning(crate::events::Reasoning {
                model: model.clone(),
                content: bare_signature(signature),
            }));
        }
        let content = match part {
            provider::chat::ContentPart::ThoughtSignature(signature) => {
                pending_signature = Some(signature);
                continue;
            }
            provider::chat::ContentPart::ReasoningContent(text) => match pending_signature.take() {
                Some(signature) => crate::events::ReasoningContent::Signed {
                    text: text.clone(),
                    token: serde_json::json!({
                        "type": "thinking",
                        "thinking": text,
                        "signature": signature,
                    }),
                },
                None => crate::events::ReasoningContent::Plain { text: text.clone() },
            },
            provider::chat::ContentPart::Custom(custom) => match custom.typ() {
                // A raw block from a proxy or an older adapter: carried
                // as-is, signature included.
                Some("thinking") => crate::events::ReasoningContent::Signed {
                    text: custom
                        .data()
                        .get("thinking")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    token: custom.data().clone(),
                },
                Some("redacted_thinking") => crate::events::ReasoningContent::Opaque {
                    token: custom.data().clone(),
                },
                // Some other block type we do not model. Skipped rather
                // than guessed at: inventing a meaning for it would be
                // worse than not carrying it, and it is not reasoning as
                // far as we know.
                _ => continue,
            },
            _ => continue,
        };
        // Signed and plain parts carry the readable text themselves; an
        // opaque one does not, so it says nothing about whether the
        // sibling field below is a duplicate.
        text_carried |= !matches!(content, crate::events::ReasoningContent::Opaque { .. });
        parts.push(AssistantPart::Reasoning(crate::events::Reasoning {
            model: model.clone(),
            content,
        }));
    }
    if let Some(signature) = pending_signature {
        parts.push(AssistantPart::Reasoning(crate::events::Reasoning {
            model: model.clone(),
            content: bare_signature(signature),
        }));
    }

    // Providers that report readable reasoning as a sibling field rather
    // than as parts: Kimi, DeepSeek, anything OpenAI-shaped — and Gemini's
    // thought summary, which sits beside its opaque signature parts. genai
    // populates `reasoning_content` unconditionally on the non-streaming
    // path — no capture flag needed. Skipped when a part already carried
    // the text, or the same reasoning would be recorded twice: once
    // replayable, once not.
    if !text_carried
        && let Some(text) = response
            .reasoning_content
            .as_ref()
            .filter(|t| !t.is_empty())
    {
        parts.push(AssistantPart::Reasoning(crate::events::Reasoning {
            model: model.clone(),
            content: crate::events::ReasoningContent::Plain { text: text.clone() },
        }));
    }

    for part in response.content.iter() {
        match part {
            provider::chat::ContentPart::Text(text) => {
                parts.push(crate::events::AssistantPart::Text { text: text.clone() });
            }
            provider::chat::ContentPart::ToolCall(call) => {
                let tool_call_id = crate::events::ToolCallId::new(call.call_id.clone())
                    .map_err(|err| LlmError::InvalidResponse(err.to_string()))?;
                parts.push(crate::events::AssistantPart::ToolCall(MessageToolCall {
                    tool_call_id,
                    tool_name: call.fn_name.clone(),
                    parameters: call.fn_arguments.clone(),
                }));
            }
            // Reasoning is read in phase 3. Everything else a provider
            // may send is not part of an assistant turn as we model it.
            _ => {}
        }
    }

    let has_tool_calls = parts
        .iter()
        .any(|part| matches!(part, crate::events::AssistantPart::ToolCall(_)));
    let stop_reason = map_stop_reason(response.stop_reason.as_ref(), has_tool_calls);

    Ok(ChatResponse {
        parts,
        stop_reason,
        usage,
    })
}

/// Map genai's reported stop reason onto our [`StopReason`].
///
/// `MaxTokens` and `StopSequence` pass through — the truncation signal
/// must survive, or a cut-off answer records as a complete `EndTurn`.
/// On the completed/tool-call axis the provider label can disagree with
/// what we actually parsed; the reducer dispatches on parsed calls, so
/// disagreements trust `has_tool_calls` and warn. `ContentFilter` and
/// `Other` have no fq variant (adding one is an event-schema contract
/// change) — they fall back to tool-call inference with a warning
/// carrying the raw provider string.
fn map_stop_reason(
    reason: Option<&provider::chat::StopReason>,
    has_tool_calls: bool,
) -> StopReason {
    use provider::chat::StopReason as Reported;

    let inferred = if has_tool_calls {
        StopReason::ToolUse
    } else {
        StopReason::EndTurn
    };

    match reason {
        Some(Reported::ToolCall(raw)) => {
            if !has_tool_calls {
                tracing::warn!(
                    raw = %raw,
                    "provider reported a tool-call stop but no tool calls parsed; trusting parsed content"
                );
            }
            inferred
        }
        Some(Reported::Completed(raw)) => {
            if has_tool_calls {
                tracing::warn!(
                    raw = %raw,
                    "provider reported a completed stop but tool calls were parsed; trusting parsed content"
                );
            }
            inferred
        }
        Some(Reported::MaxTokens(_)) => StopReason::MaxTokens,
        Some(Reported::StopSequence(_)) => StopReason::StopSequence,
        Some(Reported::ContentFilter(raw)) | Some(Reported::Other(raw)) => {
            tracing::warn!(
                raw = %raw,
                "unmapped provider stop reason; falling back to tool-call inference"
            );
            inferred
        }
        None => {
            tracing::warn!("provider reported no stop reason; falling back to tool-call inference");
            inferred
        }
    }
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

    // The thought-vs-spoken split. Providers fold reasoning into
    // `completion_tokens`, so this is a decomposition of a number we
    // already have — reading it cannot change what anything costs, and
    // the pricing table is deliberately not told about it.
    //
    // Most providers omit `completion_tokens_details` entirely, which
    // reads as 0: not "no thinking happened", but "not reported". The
    // two are indistinguishable on this wire and the distinction is not
    // worth an Option here, because nothing branches on it.
    let reasoning_tokens = usage
        .completion_tokens_details
        .as_ref()
        .and_then(|d| d.reasoning_tokens)
        .unwrap_or(0)
        .max(0) as u32;

    TokenUsage {
        input_tokens,
        output_tokens,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        reasoning_tokens,
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
    use crate::events::{Effort, RequestParams};

    fn request_with_system_and_user(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.to_string(),
            messages: vec![
                Message::system("You are a helper.".to_string()),
                Message::user("Say hello.".to_string()),
            ],
            tools: vec![],
            params: RequestParams {
                effort: None,
                temperature: Some(0.2),
                max_tokens: Some(64),
            },
        }
    }

    /// **The wire oracle for #437's shape change.**
    ///
    /// `Message` becomes an enum over turn kinds and the response chain
    /// becomes parts-shaped, which rewrites every construction site in the
    /// tree. That is a *shape* change: the request that reaches the provider
    /// must come out byte-identical on the other side. This snapshot is the
    /// judge — built before the refactor, per the fq-ops review's lesson 10
    /// ("build the oracle before the thing it will judge").
    ///
    /// If this file moves, the refactor changed behaviour. If it does not,
    /// the diff is shape only, which is the phase-2 contract.
    ///
    /// The conversation deliberately exercises every message the reducer can
    /// produce: the seeded system prompt, pinned static-resource context, the
    /// trigger's user message, a bare-text assistant turn, the corrective host
    /// notice that answers one, an assistant turn carrying parallel tool
    /// calls, both tool results, and a trailing notice — plus tools and
    /// per-request params. It also pins the two prompt-caching breakpoints
    /// (system, and the final message), which are easy to lose in a refactor
    /// and cost money rather than failing loudly.
    ///
    /// One thing here moved on purpose, once: the two parallel tool results
    /// go out as **one `Tool`-role message carrying both** — what the
    /// reducer emits since [#511](https://github.com/bricef/factor-q/issues/511)
    /// closed, and Anthropic's documented shape. Until then the snapshot
    /// pinned two separate messages, the known defect phase 2 deliberately
    /// left alone, and it moved when #511 landed exactly as it said it
    /// would. The conversation is built by hand, so it tracks the reducer's
    /// output by construction, not by running it: the reducer's own tests
    /// cover that.
    #[test]
    fn provider_request_shape_is_stable_for_a_full_conversation() {
        let request = ChatRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![
                Message::system("You are a careful assistant."),
                Message::user("Pinned resource: the deploy runbook."),
                Message::user("Investigate the failing deploy."),
                Message::assistant_text("Let me look at the logs."),
                Message::user("No tool calls were made and the run is not over."),
                Message::Assistant {
                    parts: vec![
                        crate::events::AssistantPart::Text {
                            text: "Reading both logs.".to_string(),
                        },
                        crate::events::AssistantPart::ToolCall(MessageToolCall {
                            tool_call_id: crate::events::ToolCallId::new("call_a".to_string())
                                .expect("non-empty"),
                            tool_name: "file_read".to_string(),
                            parameters: serde_json::json!({"path": "/var/log/deploy.log"}),
                        }),
                        crate::events::AssistantPart::ToolCall(MessageToolCall {
                            tool_call_id: crate::events::ToolCallId::new("call_b".to_string())
                                .expect("non-empty"),
                            tool_name: "file_read".to_string(),
                            parameters: serde_json::json!({"path": "/var/log/agent.log"}),
                        }),
                    ],
                },
                Message::ToolResults {
                    results: vec![
                        crate::events::ToolResult {
                            tool_call_id: crate::events::ToolCallId::new("call_a".to_string())
                                .expect("non-empty"),
                            output: "deploy failed at step 3".to_string(),
                            is_error: false,
                        },
                        crate::events::ToolResult {
                            tool_call_id: crate::events::ToolCallId::new("call_b".to_string())
                                .expect("non-empty"),
                            output: "agent restarted".to_string(),
                            is_error: false,
                        },
                    ],
                },
                Message::user("Budget is 80% spent."),
            ],
            tools: vec![ToolSchema {
                name: "file_read".to_string(),
                description: "Read a file from the workspace.".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }),
            }],
            params: RequestParams {
                effort: Some(Effort::High),
                temperature: Some(0.2),
                max_tokens: Some(4096),
            },
        };

        let (model, chat_req, options) =
            into_provider_request(request).expect("conversion must succeed");

        let actual = fq_test_support::canonical_json(&serde_json::json!({
            "model": model,
            "chat_request": chat_req,
            "chat_options": options,
        }));

        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/snapshots/provider_request.json");
        if std::env::var_os("UPDATE_SNAPSHOT").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &actual).unwrap();
            return;
        }
        let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
            panic!(
                "missing snapshot {path:?} — run `UPDATE_SNAPSHOT=1 cargo test -p fq-runtime \
                 provider_request_shape_is_stable` and commit the result"
            )
        });
        assert_eq!(
            actual, expected,
            "the request handed to the provider changed. During #437's phase 2 this means \
             the shape change altered behaviour and is a bug; at any other time, review the \
             diff before regenerating."
        );
    }

    // region: reasoning round-trip (#437 phase 3)

    fn reasoning_part(model: &str, text: &str) -> AssistantPart {
        AssistantPart::Reasoning(crate::events::Reasoning {
            model: model.to_string(),
            content: crate::events::ReasoningContent::Plain {
                text: text.to_string(),
            },
        })
    }

    /// **The test the issue asks for.** A reasoning-first model carries
    /// the substance of a turn in `reasoning_content`; if we do not send
    /// it back, turn N+1 re-derives from a weaker base than the
    /// provider's own contract assumes, and pays for the thinking again.
    ///
    /// So this asserts on the outbound wire shape, not on output quality:
    /// an assistant turn holding a reasoning part must reach genai as a
    /// `ReasoningContent` part, which its OpenAI-compatible adapter hoists
    /// into the sibling `reasoning_content` field — the Kimi/DeepSeek
    /// round-trip.
    #[test]
    fn reasoning_reaches_the_provider_on_the_next_turn() {
        let msg = convert_message(
            Message::Assistant {
                parts: vec![
                    reasoning_part("kimi-k2", "The runbook is the cheapest place to start."),
                    AssistantPart::Text {
                        text: "Reading the runbook.".to_string(),
                    },
                ],
            },
            "kimi-k2",
        )
        .expect("conversion succeeds");

        let reasoning: Vec<&String> = msg
            .content
            .parts()
            .iter()
            .filter_map(|part| match part {
                provider::chat::ContentPart::ReasoningContent(text) => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(
            reasoning.len(),
            1,
            "the turn's reasoning must survive to the provider"
        );
        assert_eq!(reasoning[0], "The runbook is the cheapest place to start.");
    }

    /// Reasoning is tied to the model that produced it, and ADR-0003
    /// guarantees per-agent model selection — so a multi-node graph has
    /// cross-model edges by construction. On each one the reasoning must
    /// go, or we bill input tokens for a block the model will ignore.
    #[test]
    fn reasoning_is_stripped_on_a_cross_model_edge() {
        let msg = convert_message(
            Message::Assistant {
                parts: vec![
                    reasoning_part("kimi-k2", "thought hard about it"),
                    AssistantPart::Text {
                        text: "done".to_string(),
                    },
                ],
            },
            "gpt-4o",
        )
        .expect("conversion succeeds");

        assert!(
            !msg.content
                .parts()
                .iter()
                .any(|p| matches!(p, provider::chat::ContentPart::ReasoningContent(_))),
            "reasoning from another model must not be replayed"
        );
        // …and the rest of the turn is untouched: a strip is not a drop.
        assert!(
            msg.content
                .parts()
                .iter()
                .any(|p| matches!(p, provider::chat::ContentPart::Text(t) if t == "done")),
            "stripping reasoning must not take the turn's text with it"
        );
    }

    /// Opaque reasoning has no readable text, and genai's
    /// `ReasoningContent` is a bare string with nowhere to put a
    /// continuity token. It is dropped rather than encoded as if the
    /// token were prose — sending a signature as text would be rejected
    /// by the provider and is a corruption, not a degradation.
    #[test]
    fn opaque_reasoning_is_not_encoded_as_text() {
        let msg = convert_message(
            Message::Assistant {
                parts: vec![AssistantPart::Reasoning(crate::events::Reasoning {
                    model: "claude-sonnet-4-6".to_string(),
                    content: crate::events::ReasoningContent::Opaque {
                        token: serde_json::json!("encrypted-blob"),
                    },
                })],
            },
            "claude-sonnet-4-6",
        )
        .expect("conversion succeeds");

        assert!(
            !msg.content.parts().iter().any(|p| matches!(
                p,
                provider::chat::ContentPart::ReasoningContent(t) if t.contains("encrypted-blob")
            )),
            "an opaque token must never be sent as if it were reasoning text"
        );
    }

    /// The read side: genai normalises every provider's reasoning into
    /// `ChatResponse.reasoning_content`, and it must become a part tagged
    /// with the model that produced it — the tag is what the strip above
    /// keys on, so an untagged part would silently defeat it.
    #[test]
    fn provider_reasoning_becomes_a_tagged_part() {
        let response = provider::chat::ChatResponse {
            content: provider::chat::MessageContent::from_parts(vec![
                provider::chat::ContentPart::Text("Answer.".to_string()),
            ]),
            reasoning_content: Some("Worked it through.".to_string()),
            model_iden: provider::ModelIden::new(provider::adapter::AdapterKind::OpenAI, "kimi-k2"),
            provider_model_iden: provider::ModelIden::new(
                provider::adapter::AdapterKind::OpenAI,
                "kimi-k2-0905",
            ),
            stop_reason: None,
            usage: provider::chat::Usage::default(),
            captured_raw_body: None,
            response_id: None,
        };

        let parsed = from_provider_response(response).expect("parses");

        let reasoning = parsed
            .parts
            .iter()
            .find_map(|part| match part {
                AssistantPart::Reasoning(r) => Some(r),
                _ => None,
            })
            .expect("reasoning must be captured");
        assert_eq!(
            reasoning.model, "kimi-k2",
            "tagged with the model we asked for, which is what the strip compares against"
        );
        assert!(matches!(
            &reasoning.content,
            crate::events::ReasoningContent::Plain { text } if text == "Worked it through."
        ));
        // Reasoning leads the turn — the order Anthropic requires and one
        // OpenAI-compatible providers are indifferent to.
        assert!(matches!(parsed.parts[0], AssistantPart::Reasoning(_)));
        assert_eq!(parsed.text().as_deref(), Some("Answer."));
    }

    // region: bare continuity tokens (#600, ADR-0034 D2's Gemini row)

    fn gemini_response(
        parts: Vec<provider::chat::ContentPart>,
        reasoning_content: Option<&str>,
    ) -> provider::chat::ChatResponse {
        provider::chat::ChatResponse {
            content: provider::chat::MessageContent::from_parts(parts),
            reasoning_content: reasoning_content.map(str::to_string),
            model_iden: provider::ModelIden::new(
                provider::adapter::AdapterKind::Gemini,
                "gemini-3-pro",
            ),
            provider_model_iden: provider::ModelIden::new(
                provider::adapter::AdapterKind::Gemini,
                "gemini-3-pro-001",
            ),
            stop_reason: None,
            usage: provider::chat::Usage::default(),
            captured_raw_body: None,
            response_id: None,
        }
    }

    fn gemini_call() -> provider::chat::ContentPart {
        provider::chat::ContentPart::ToolCall(provider::chat::ToolCall {
            call_id: "call#read_file#0".to_string(),
            fn_name: "read_file".to_string(),
            fn_arguments: serde_json::json!({"path": "/runbook"}),
            thought_signatures: None,
        })
    }

    fn reasoning_parts(parsed: &ChatResponse) -> Vec<&crate::events::Reasoning> {
        parsed
            .parts
            .iter()
            .filter_map(|part| match part {
                AssistantPart::Reasoning(r) => Some(r),
                _ => None,
            })
            .collect()
    }

    /// Gemini's `thoughtSignature` reaches this adapter as a signature
    /// part with no reasoning text behind it. It is a continuity token
    /// and nothing else — so it is recorded as *opaque* reasoning, tagged
    /// with the model, in the place it arrived. Dropping it (what this
    /// adapter did before #600) recorded the turn as having no reasoning
    /// at all, which misstates absence for opacity (I7).
    #[test]
    fn a_bare_thought_signature_becomes_an_opaque_part() {
        let response = gemini_response(
            vec![
                provider::chat::ContentPart::ThoughtSignature("sig-1".to_string()),
                gemini_call(),
            ],
            Some(""),
        );

        let parsed = from_provider_response(response).expect("parses");

        let reasoning = reasoning_parts(&parsed);
        assert_eq!(reasoning.len(), 1, "one token, one part");
        assert_eq!(reasoning[0].model, "gemini-3-pro");
        let crate::events::ReasoningContent::Opaque { token } = &reasoning[0].content else {
            panic!("a bare signature is opaque, not {:?}", reasoning[0].content);
        };
        assert_eq!(token["type"], "thought_signature");
        assert_eq!(token["signature"], "sig-1");
        assert!(
            matches!(parsed.parts[0], AssistantPart::Reasoning(_)),
            "reasoning leads the turn"
        );
        assert_eq!(parsed.tool_calls().len(), 1, "the call rides along");
    }

    /// With `includeThoughts` on, Gemini also returns a readable thought
    /// summary — which genai hands over as `reasoning_content`, beside the
    /// signature parts. Both are recorded: the summary is not a duplicate
    /// of anything a signature carries.
    #[test]
    fn a_thought_summary_is_recorded_beside_the_signature() {
        let response = gemini_response(
            vec![
                provider::chat::ContentPart::ThoughtSignature("sig-1".to_string()),
                gemini_call(),
            ],
            Some("Consider the runbook first."),
        );

        let parsed = from_provider_response(response).expect("parses");

        let kinds: Vec<&str> = reasoning_parts(&parsed)
            .iter()
            .map(|r| match &r.content {
                crate::events::ReasoningContent::Opaque { .. } => "opaque",
                crate::events::ReasoningContent::Plain { .. } => "plain",
                crate::events::ReasoningContent::Signed { .. } => "signed",
            })
            .collect();
        assert_eq!(
            kinds,
            ["opaque", "plain"],
            "signature first, then the summary"
        );
    }

    /// The write side: the opaque token this adapter minted goes back as
    /// genai's `ThoughtSignature` part, which its Gemini adapter attaches
    /// to the next function call — a `Custom` part would be ignored on
    /// that wire. A provider block (Anthropic's `redacted_thinking`) still
    /// goes back verbatim as `Custom`, and neither crosses a model edge.
    #[test]
    fn an_opaque_signature_replays_as_a_thought_signature_part() {
        let signature = crate::events::Reasoning {
            model: "gemini-3-pro".to_string(),
            content: crate::events::ReasoningContent::Opaque {
                token: serde_json::json!({"type": "thought_signature", "signature": "sig-1"}),
            },
        };
        assert!(
            matches!(
                encode_reasoning(signature.clone(), "gemini-3-pro"),
                Some(provider::chat::ContentPart::ThoughtSignature(s)) if s == "sig-1"
            ),
            "a bare token goes back as the part Gemini's adapter attaches to the call"
        );
        assert!(
            encode_reasoning(signature, "gemini-3-flash").is_none(),
            "and never to a different model"
        );

        let redacted = crate::events::Reasoning {
            model: "claude-opus-5".to_string(),
            content: crate::events::ReasoningContent::Opaque {
                token: serde_json::json!({"type": "redacted_thinking", "data": "EqQB"}),
            },
        };
        assert!(
            matches!(
                encode_reasoning(redacted, "claude-opus-5"),
                Some(provider::chat::ContentPart::Custom(custom)) if custom.typ() == Some("redacted_thinking")
            ),
            "a provider block still goes back whole"
        );
    }

    // endregion: bare continuity tokens

    /// A response with no reasoning must produce no reasoning part —
    /// absence stays absence, rather than becoming an empty one.
    #[test]
    fn a_response_without_reasoning_gains_no_part() {
        let response = provider::chat::ChatResponse {
            content: provider::chat::MessageContent::from_parts(vec![
                provider::chat::ContentPart::Text("Answer.".to_string()),
            ]),
            reasoning_content: None,
            model_iden: provider::ModelIden::new(provider::adapter::AdapterKind::OpenAI, "gpt-4o"),
            provider_model_iden: provider::ModelIden::new(
                provider::adapter::AdapterKind::OpenAI,
                "gpt-4o",
            ),
            stop_reason: None,
            usage: provider::chat::Usage::default(),
            captured_raw_body: None,
            response_id: None,
        };
        let parsed = from_provider_response(response).expect("parses");
        assert!(
            !parsed
                .parts
                .iter()
                .any(|p| matches!(p, AssistantPart::Reasoning(_)))
        );
    }

    /// **The issue's acceptance criterion, at the wire.**
    ///
    /// > *A fixture-backed multi-turn test over an OpenAI-compatible
    /// > provider: model returns `reasoning_content` on turn 1, makes a
    /// > tool call; assert the assistant message factor-q sends on turn 2
    /// > carries that `reasoning_content` back.*
    ///
    /// Every other test here asserts on our own types, which proves the
    /// halves of the chain but not the join. This one runs a real HTTP
    /// server and reads the bytes we actually sent — so it also covers
    /// genai's hoisting, which we depend on and otherwise only verified
    /// by reading its source.
    ///
    /// Turn 1 is deliberately **silent**: no visible text, only reasoning
    /// and a tool call. That is the shape a reasoning-first model
    /// routinely returns, and the one where dropping reasoning drops the
    /// entire substance of the turn rather than a supplement to it.
    #[tokio::test]
    async fn reasoning_content_round_trips_over_the_wire() {
        use crate::test_support::mock_openai::{MockChoice, MockOpenAiServer};

        const MODEL: &str = "kimi-k2";
        const THINKING: &str = "The runbook is the cheapest place to start.";

        let mock = MockOpenAiServer::start().await;
        mock.push(
            MockChoice::silent()
                .with_reasoning(THINKING)
                .with_tool_call(
                    "call_1",
                    "read_file",
                    serde_json::json!({"path": "/runbook"}),
                ),
        );
        mock.push(MockChoice::text("Done."));
        let client = mock.client(MODEL);

        // -- Turn 1
        let first = client
            .chat(ChatRequest {
                model: MODEL.to_string(),
                messages: vec![Message::user("Investigate the deploy.")],
                tools: vec![],
                params: RequestParams {
                    effort: None,
                    temperature: None,
                    max_tokens: Some(1024),
                },
            })
            .await
            .expect("turn 1 succeeds");

        let call_id = first
            .tool_calls()
            .first()
            .expect("turn 1 requested a tool")
            .tool_call_id
            .clone();

        // -- Turn 2: replay the assistant turn and answer the tool, which
        //    is exactly what the reducer does at its own replay point.
        client
            .chat(ChatRequest {
                model: MODEL.to_string(),
                messages: vec![
                    Message::user("Investigate the deploy."),
                    Message::Assistant {
                        parts: first.parts.clone(),
                    },
                    Message::tool_result(call_id, "runbook says: restart"),
                ],
                tools: vec![],
                params: RequestParams {
                    effort: None,
                    temperature: None,
                    max_tokens: Some(1024),
                },
            })
            .await
            .expect("turn 2 succeeds");

        let sent = mock.requests();
        mock.shutdown().await;

        assert_eq!(sent.len(), 2, "two turns were sent");
        let assistant = sent[1]["messages"]
            .as_array()
            .expect("messages array")
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("turn 2 replays the assistant turn");

        assert_eq!(
            assistant["reasoning_content"].as_str(),
            Some(THINKING),
            "the provider must get its own reasoning back on the next turn; \
             sent body was {}",
            sent[1]
        );
        assert_eq!(
            assistant["tool_calls"][0]["id"].as_str(),
            Some("call_1"),
            "the tool call rides along unchanged"
        );
    }

    /// The split is captured when the provider reports it.
    #[test]
    fn reasoning_tokens_are_captured_from_completion_details() {
        let usage = provider::chat::Usage {
            prompt_tokens: Some(1000),
            completion_tokens: Some(500),
            completion_tokens_details: Some(provider::chat::CompletionTokensDetails {
                reasoning_tokens: Some(400),
                ..Default::default()
            }),
            ..Default::default()
        };

        let converted = convert_usage(&usage);

        assert_eq!(converted.output_tokens, 500);
        assert_eq!(converted.reasoning_tokens, 400);
        assert_eq!(
            converted.spoken_tokens(),
            100,
            "the split decomposes output_tokens; it does not add to them"
        );
    }

    /// **I4, the invariant that makes this safe.** `reasoning_tokens` is
    /// a decomposition of a number we already had, so pricing must not
    /// see it and the bill must not move. Asserted by pricing the same
    /// usage twice — once with the split reported, once without — and
    /// requiring bit-identical costs.
    ///
    /// If this ever fails, we started charging twice for thinking.
    #[test]
    fn the_reasoning_split_does_not_change_what_a_call_costs() {
        let pricing = crate::pricing::ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cache_read_per_million: Some(0.3),
            cache_write_per_million: Some(3.75),
        };

        let without = TokenUsage {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 200,
            cache_write_tokens: 100,
            reasoning_tokens: 0,
        };
        let with_split = TokenUsage {
            reasoning_tokens: 400,
            ..without
        };

        let (in_a, out_a, total_a) = pricing.calculate(&without);
        let (in_b, out_b, total_b) = pricing.calculate(&with_split);

        assert_eq!(
            total_a.to_bits(),
            total_b.to_bits(),
            "reporting the thought/spoken split must not change total_cost"
        );
        assert_eq!(in_a.to_bits(), in_b.to_bits());
        assert_eq!(
            out_a.to_bits(),
            out_b.to_bits(),
            "output cost is priced on output_tokens, which already includes reasoning"
        );
    }

    /// A provider that does not report the split reads as 0 rather than
    /// failing the conversion — most of them do not report it, and an
    /// unreported split is not an error.
    #[test]
    fn absent_completion_details_read_as_no_reported_reasoning() {
        let usage = provider::chat::Usage {
            prompt_tokens: Some(10),
            completion_tokens: Some(20),
            completion_tokens_details: None,
            ..Default::default()
        };
        let converted = convert_usage(&usage);
        assert_eq!(converted.reasoning_tokens, 0);
        assert_eq!(converted.spoken_tokens(), 20);
    }

    /// Defence against a provider reporting more reasoning than
    /// completion tokens — genai already corrects one such case for xAI,
    /// so the shape is real. `spoken_tokens` saturates rather than
    /// wrapping to four billion.
    #[test]
    fn a_nonsensical_split_saturates_rather_than_wrapping() {
        let usage = TokenUsage {
            input_tokens: 0,
            output_tokens: 10,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 99,
        };
        assert_eq!(usage.spoken_tokens(), 0);
    }

    /// **What the fork exists for.** Anthropic requires thinking blocks
    /// to be passed back within a tool-use turn and verifies them by
    /// their `signature`, so a block replayed without one is rejected —
    /// and stock genai discards the signature on the way in, which makes
    /// the turn unreplayable.
    ///
    /// Turn 1 returns a signed thinking block and a tool call; turn 2
    /// replays that turn. The assertion is on the bytes we send: the
    /// block must go back **whole**, signature intact, and first — the
    /// order Anthropic's API requires.
    #[tokio::test]
    async fn anthropic_thinking_block_round_trips_over_the_wire() {
        use crate::test_support::mock_anthropic::{
            ContentBlock, MockAnthropicServer, MockResponse,
        };

        const MODEL: &str = "claude-sonnet-4-6";
        const THINKING: &str = "The runbook is the cheapest place to start.";
        const SIGNATURE: &str = "sig-abc123";

        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-mock-not-real") };
        let mock = MockAnthropicServer::start().await;
        mock.push_response(MockResponse {
            content: vec![
                ContentBlock::Thinking {
                    thinking: THINKING.to_string(),
                    signature: SIGNATURE.to_string(),
                },
                ContentBlock::ToolUse {
                    id: "toolu_01".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({"path": "/runbook"}),
                },
            ],
            stop_reason: "tool_use".to_string(),
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        });
        mock.push_response(MockResponse::text("Done.", 120, 5));

        let client = GenAiClient::with_base_url(mock.base_url()).expect("client builds");

        // -- Turn 1
        let first = client
            .chat(ChatRequest {
                model: MODEL.to_string(),
                messages: vec![Message::user("Investigate the deploy.")],
                tools: vec![],
                params: RequestParams {
                    effort: None,
                    temperature: None,
                    max_tokens: Some(1024),
                },
            })
            .await
            .expect("turn 1 succeeds");

        // The signature must survive parsing, or nothing downstream can
        // replay it.
        let reasoning = first
            .parts
            .iter()
            .find_map(|p| match p {
                AssistantPart::Reasoning(r) => Some(r),
                _ => None,
            })
            .expect("the thinking block became a reasoning part");
        let crate::events::ReasoningContent::Signed { text, token } = &reasoning.content else {
            panic!("an Anthropic thinking block is signed, not plain: {reasoning:?}");
        };
        assert_eq!(text, THINKING);
        assert_eq!(
            token["signature"].as_str(),
            Some(SIGNATURE),
            "the signature is what makes the block replayable"
        );

        let call_id = first.tool_calls()[0].tool_call_id.clone();

        // -- Turn 2: replay the assistant turn, as the reducer does.
        client
            .chat(ChatRequest {
                model: MODEL.to_string(),
                messages: vec![
                    Message::user("Investigate the deploy."),
                    Message::Assistant {
                        parts: first.parts.clone(),
                    },
                    Message::tool_result(call_id, "runbook says: restart"),
                ],
                tools: vec![],
                params: RequestParams {
                    effort: None,
                    temperature: None,
                    max_tokens: Some(1024),
                },
            })
            .await
            .expect("turn 2 succeeds");

        let sent = mock.received_requests();
        mock.shutdown().await;

        assert_eq!(sent.len(), 2);
        let assistant = sent[1]["messages"]
            .as_array()
            .expect("messages array")
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("turn 2 replays the assistant turn");
        let blocks = assistant["content"]
            .as_array()
            .expect("a replayed thinking turn is a block array, not a bare string");

        assert_eq!(
            blocks[0]["type"], "thinking",
            "Anthropic requires thinking blocks first in an assistant turn; sent: {}",
            sent[1]
        );
        assert_eq!(
            blocks[0]["signature"], SIGNATURE,
            "the signature must be echoed verbatim, or the API rejects the turn"
        );
        assert_eq!(blocks[0]["thinking"], THINKING);
    }

    // endregion: reasoning round-trip

    #[test]
    fn maps_reasoning_effort_to_provider_options() {
        let options = convert_params(RequestParams {
            effort: Some(Effort::XHigh),
            temperature: None,
            max_tokens: None,
        });
        assert!(matches!(
            options.reasoning_effort,
            Some(provider::chat::ReasoningEffort::XHigh)
        ));

        let options = convert_params(RequestParams {
            effort: None,
            temperature: None,
            max_tokens: None,
        });
        assert!(options.reasoning_effort.is_none());
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
        request.messages.push(Message::assistant_text("Hello!"));
        request.messages.push(Message::user("And again."));
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

        let client = GenAiClient::with_base_url(server.base_url()).expect("client builds");
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

        // genai 0.6 renders a cache-marked single system message as
        // a content-parts array, preserving its cache breakpoint.
        let system_parts = body["system"]
            .as_array()
            .expect("system prompt should be a cache-marked parts array");
        assert!(
            system_parts
                .iter()
                .any(|part| part["cache_control"]["type"] == "ephemeral"),
            "system prompt should carry cache_control, got {system_parts:?}"
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

    /// Every genai stop-reason variant maps per the table in #178:
    /// `MaxTokens` and `StopSequence` pass through, `Completed` /
    /// `ToolCall` follow the parsed content, and `ContentFilter` /
    /// `Other` / absent fall back to tool-call inference.
    #[test]
    fn maps_every_provider_stop_reason_variant() {
        use provider::chat::StopReason as Reported;

        let raw = |s: &str| s.to_string();
        let cases: Vec<(Option<Reported>, bool, StopReason)> = vec![
            (
                Some(Reported::Completed(raw("end_turn"))),
                false,
                StopReason::EndTurn,
            ),
            (
                Some(Reported::ToolCall(raw("tool_use"))),
                true,
                StopReason::ToolUse,
            ),
            (
                Some(Reported::MaxTokens(raw("max_tokens"))),
                false,
                StopReason::MaxTokens,
            ),
            // Truncation mid-tool-use still surfaces as MaxTokens —
            // the reducer dispatches on parsed calls, not the label.
            (
                Some(Reported::MaxTokens(raw("max_tokens"))),
                true,
                StopReason::MaxTokens,
            ),
            (
                Some(Reported::StopSequence(raw("stop_sequence"))),
                false,
                StopReason::StopSequence,
            ),
            (
                Some(Reported::ContentFilter(raw("SAFETY"))),
                false,
                StopReason::EndTurn,
            ),
            (
                Some(Reported::ContentFilter(raw("SAFETY"))),
                true,
                StopReason::ToolUse,
            ),
            (
                Some(Reported::Other(raw("cancelled"))),
                false,
                StopReason::EndTurn,
            ),
            (
                Some(Reported::Other(raw("cancelled"))),
                true,
                StopReason::ToolUse,
            ),
            (None, false, StopReason::EndTurn),
            (None, true, StopReason::ToolUse),
        ];
        for (reported, has_tool_calls, expected) in cases {
            let got = map_stop_reason(reported.as_ref(), has_tool_calls);
            assert_eq!(
                got, expected,
                "reported={reported:?} has_tool_calls={has_tool_calls}"
            );
        }
    }

    /// When the provider label and the parsed content disagree on the
    /// completed/tool-call axis, the parsed content wins.
    #[test]
    fn stop_reason_label_disagreement_trusts_parsed_content() {
        use provider::chat::StopReason as Reported;

        // Label says tool_use, nothing parsed: EndTurn.
        assert_eq!(
            map_stop_reason(Some(&Reported::ToolCall("tool_use".to_string())), false),
            StopReason::EndTurn
        );
        // Label says completed, but calls were parsed: ToolUse.
        assert_eq!(
            map_stop_reason(Some(&Reported::Completed("end_turn".to_string())), true),
            StopReason::ToolUse
        );
    }

    /// A max-tokens-truncated response must not read as a clean
    /// `EndTurn` — end-to-end through the mock server, the wire
    /// `stop_reason: "max_tokens"` surfaces as fq `MaxTokens`.
    #[tokio::test]
    async fn truncated_response_surfaces_max_tokens() {
        use crate::test_support::mock_anthropic::{MockAnthropicServer, MockResponse};

        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-mock-not-real") };
        let server = MockAnthropicServer::start().await;
        server.push_response(
            MockResponse::text("an answer cut off mid-", 10, 64).with_stop_reason("max_tokens"),
        );

        let client = GenAiClient::with_base_url(server.base_url()).expect("client builds");
        let response = client
            .chat(request_with_system_and_user("claude-sonnet-4-5"))
            .await
            .expect("chat via mock");

        assert_eq!(response.stop_reason, StopReason::MaxTokens);
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
        assert_eq!(tool.name.as_ref(), "read_file");
        assert!(tool.description.is_some());
        assert!(tool.schema.is_some());
    }

    #[test]
    fn converts_tool_result_message() {
        let msg = convert_message(
            Message::tool_result(
                crate::events::ToolCallId::new("toolu_01ABC").unwrap(),
                "file contents",
            ),
            "claude-sonnet-4-6",
        )
        .unwrap();
        assert!(matches!(msg.role, provider::chat::ChatRole::Tool));
    }

    // `tool_message_without_id_is_error` lived here. It asserted that a
    // `Tool`-role message with no `tool_call_id` was rejected at
    // conversion — a state `Message::ToolResults` cannot represent, since
    // the id lives on each `ToolResult`. The test is deleted rather than
    // rewritten because there is no longer anything to assert: the
    // compiler enforces it (ADR-0034 D1).

    #[test]
    fn converts_assistant_message_with_tool_calls() {
        let msg = convert_message(
            Message::Assistant {
                parts: crate::events::assistant_parts(
                    Some("I'll read that file.".to_string()),
                    vec![MessageToolCall {
                        tool_call_id: crate::events::ToolCallId::new("toolu_01ABC").unwrap(),
                        tool_name: "read_file".to_string(),
                        parameters: serde_json::json!({"path": "/tmp/x"}),
                    }],
                ),
            },
            "claude-sonnet-4-6",
        )
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

        let client = GenAiClient::new().expect("client builds");
        let request = ChatRequest {
            model: "claude-haiku-4-5".to_string(),
            messages: vec![
                Message::system("You are a test. Reply in exactly one word: OK".to_string()),
                Message::user("Say OK.".to_string()),
            ],
            tools: vec![],
            params: RequestParams {
                effort: None,
                temperature: Some(0.0),
                max_tokens: Some(16),
            },
        };

        let response = client.chat(request).await.expect("chat");
        assert!(
            response.text().as_deref().is_some_and(|c| !c.is_empty()),
            "expected non-empty content, got {:?}",
            response.text()
        );
        assert!(
            response.usage.input_tokens > 0,
            "expected positive input tokens, got {}",
            response.usage.input_tokens
        );
    }

    #[tokio::test]
    async fn with_base_url_overrides_resolved_endpoint() {
        let client = GenAiClient::with_base_url("http://127.0.0.1:9999").expect("client builds");
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
        let client = GenAiClient::from_anthropic_config(&cfg).expect("client builds");
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
            models: Vec::new(),
            pricing: Default::default(),
        };
        let client = GenAiClient::from_anthropic_config(&cfg).expect("client builds");
        let target = client
            .client
            .resolve_service_target("claude-haiku-4-5")
            .await
            .expect("resolve service target");
        assert_eq!(target.endpoint.base_url(), "http://127.0.0.1:54321");
    }

    /// The core multi-provider claim: a model declared under
    /// `[providers.<name>]` resolves to that provider's endpoint, adapter
    /// (from `api_shape`) and key env var — with the full model id
    /// preserved on the wire — while an unlisted model still falls
    /// through to genai's default resolution. Verified against genai's
    /// own `resolve_service_target`, so it exercises the real resolver
    /// chain without a network round-trip.
    #[tokio::test]
    async fn from_providers_routes_declared_model_and_falls_through_otherwise() {
        use crate::config::{AnthropicConfig, ApiShape, ProviderConfig, ProvidersConfig};
        use provider::adapter::AdapterKind;

        let mut extra = std::collections::BTreeMap::new();
        extra.insert(
            "openrouter".to_string(),
            ProviderConfig {
                api_shape: ApiShape::OpenaiCompatible,
                // Deliberately no trailing slash — from_providers must add
                // one so genai's Url::join keeps the `/api/v1` segment.
                base_url: Some("https://openrouter.ai/api/v1".to_string()),
                api_key_env: "FQ_TEST_OPENROUTER_KEY".to_string(),
                models: vec!["openai/gpt-4o-mini".to_string()],
                pricing: Default::default(),
            },
        );
        let cfg = ProvidersConfig {
            anthropic: Some(AnthropicConfig::default()),
            extra,
        };
        let client = GenAiClient::from_providers(&cfg).expect("client builds");

        // Declared model -> provider endpoint + OpenAI adapter, and the
        // namespaced id survives intact (OpenRouter needs the full name).
        let routed = client
            .client
            .resolve_service_target("openai/gpt-4o-mini")
            .await
            .expect("resolve declared model");
        assert_eq!(routed.endpoint.base_url(), "https://openrouter.ai/api/v1/");
        assert_eq!(routed.model.adapter_kind, AdapterKind::OpenAI);
        assert_eq!(&*routed.model.model_name, "openai/gpt-4o-mini");

        // Auth is sourced from the provider's configured env var.
        unsafe { std::env::set_var("FQ_TEST_OPENROUTER_KEY", "sk-or-test-value") };
        assert_eq!(
            routed.auth.single_key_value().expect("key from env"),
            "sk-or-test-value"
        );

        // An unlisted claude model is untouched: default Anthropic path.
        let fallthrough = client
            .client
            .resolve_service_target("claude-haiku-4-5")
            .await
            .expect("resolve unlisted model");
        assert_eq!(fallthrough.model.adapter_kind, AdapterKind::Anthropic);
        assert!(
            fallthrough.endpoint.base_url().contains("anthropic.com"),
            "unlisted model should keep the Anthropic default, got {}",
            fallthrough.endpoint.base_url()
        );
    }

    /// End-to-end against a real OpenAI-compatible provider (OpenRouter),
    /// routed purely by `[providers.<name>]` config. Opt-in: skipped
    /// unless `OPENROUTER_API_KEY` is set (needs a live key + network).
    /// This is the ADR-0003 acceptance proof — a non-Anthropic `model:`
    /// runs a real invocation and reports usage.
    #[tokio::test]
    async fn openrouter_end_to_end_when_key_present() {
        if std::env::var("OPENROUTER_API_KEY").is_err() {
            eprintln!("skipping openrouter_end_to_end: OPENROUTER_API_KEY not set");
            return;
        }

        use crate::config::{ApiShape, ProviderConfig, ProvidersConfig};
        let mut extra = std::collections::BTreeMap::new();
        extra.insert(
            "openrouter".to_string(),
            ProviderConfig {
                api_shape: ApiShape::OpenaiCompatible,
                base_url: Some("https://openrouter.ai/api/v1".to_string()),
                api_key_env: "OPENROUTER_API_KEY".to_string(),
                models: vec!["openai/gpt-4o-mini".to_string()],
                pricing: Default::default(),
            },
        );
        let cfg = ProvidersConfig {
            anthropic: None,
            extra,
        };
        let client = GenAiClient::from_providers(&cfg).expect("client builds");

        let response = client
            .chat(request_with_system_and_user("openai/gpt-4o-mini"))
            .await
            .expect("live OpenRouter chat");

        assert!(
            response.text().as_deref().is_some_and(|c| !c.is_empty()),
            "expected non-empty content from OpenRouter, got {:?}",
            response.text()
        );
        assert!(
            response.usage.output_tokens > 0,
            "expected non-zero output tokens, got {:?}",
            response.usage
        );
        eprintln!(
            "OpenRouter ok: {} in / {} out tokens",
            response.usage.input_tokens, response.usage.output_tokens
        );
    }
}
