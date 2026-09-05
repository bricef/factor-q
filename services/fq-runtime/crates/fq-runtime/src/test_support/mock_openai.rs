//! Mock OpenAI-compatible Chat Completions server.
//!
//! The sibling of [`super::mock_anthropic`], for the *other* provider
//! shape. It exists because that shape is where reasoning actually
//! round-trips: Kimi- and DeepSeek-class models carry the substance of a
//! turn in `reasoning_content`, and genai's OpenAI adapter hoists our
//! reasoning parts back into that sibling field on the way out (#437).
//!
//! Reading the adapter tells you that should work. Only an HTTP server
//! that captures the bytes we send tells you it *does* — which is the
//! difference between asserting on our types and asserting on the wire,
//! and the wire is what the provider sees.
//!
//! Sequenced-response (FIFO), matching `MockAnthropicServer` and
//! `FixtureClient::push_response`, with a [`MockFault`] taking a slot in
//! the same queue. Every request body is captured so a test can assert
//! on what went out.
//!
//! # Example
//!
//! ```no_run
//! # use fq_runtime::test_support::mock_openai::{MockOpenAiServer, MockChoice};
//! # async fn example() {
//! let mock = MockOpenAiServer::start().await;
//! mock.push(MockChoice::text("hello").with_reasoning("thought about it"));
//! let _client = mock.client("kimi-k2");
//! // ... drive a request ...
//! let sent = mock.requests();
//! mock.shutdown().await;
//! # }
//! ```

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Json as ExtractJson, State};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::post;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use super::fault::MockFault;

/// One canned assistant turn the mock will return.
#[derive(Debug, Clone, Default)]
pub struct MockChoice {
    content: Option<String>,
    reasoning: Option<String>,
    /// Which sibling field carries `reasoning`. `None` means the
    /// `reasoning_content` key that Kimi's and DeepSeek's own APIs use;
    /// OpenRouter serves the same models under a `reasoning` key.
    reasoning_key: Option<&'static str>,
    tool_calls: Vec<(String, String, Value)>,
    /// `(prompt, completion, reasoning)` token counts; `reasoning` goes
    /// out as `completion_tokens_details.reasoning_tokens` when set.
    usage: Option<(u64, u64, Option<u64>)>,
}

impl MockChoice {
    /// A turn that says something.
    pub fn text(content: &str) -> Self {
        Self {
            content: Some(content.to_string()),
            ..Default::default()
        }
    }

    /// A turn with no visible text at all — which is the shape a
    /// reasoning-first model routinely returns alongside a tool call,
    /// and the case where losing reasoning loses the whole turn.
    pub fn silent() -> Self {
        Self::default()
    }

    /// Attach provider reasoning, as `reasoning_content`.
    pub fn with_reasoning(mut self, reasoning: &str) -> Self {
        self.reasoning = Some(reasoning.to_string());
        self
    }

    /// Attach provider reasoning under OpenRouter's `reasoning` key —
    /// the shape kimi-k3 returned on the 2026-09-04 live run.
    pub fn with_openrouter_reasoning(mut self, reasoning: &str) -> Self {
        self.reasoning = Some(reasoning.to_string());
        self.reasoning_key = Some("reasoning");
        self
    }

    /// Report token usage, optionally with the reasoning split the
    /// OpenAI-compatible wire carries in `completion_tokens_details`.
    pub fn with_usage(mut self, prompt: u64, completion: u64, reasoning: Option<u64>) -> Self {
        self.usage = Some((prompt, completion, reasoning));
        self
    }

    /// Attach a tool call, which is what makes the turn a continuation
    /// rather than the end of the conversation.
    pub fn with_tool_call(mut self, id: &str, name: &str, arguments: Value) -> Self {
        self.tool_calls
            .push((id.to_string(), name.to_string(), arguments));
        self
    }

    fn to_body(&self, model: &str) -> Value {
        let mut message = json!({ "role": "assistant" });
        message["content"] = match &self.content {
            Some(text) => json!(text),
            None => Value::Null,
        };
        if let Some(reasoning) = &self.reasoning {
            let key = self.reasoning_key.unwrap_or("reasoning_content");
            message[key] = json!(reasoning);
        }
        if !self.tool_calls.is_empty() {
            message["tool_calls"] = Value::Array(
                self.tool_calls
                    .iter()
                    .map(|(id, name, args)| {
                        json!({
                            "type": "function",
                            "id": id,
                            "function": { "name": name, "arguments": args.to_string() }
                        })
                    })
                    .collect(),
            );
        }
        let finish = if self.tool_calls.is_empty() {
            "stop"
        } else {
            "tool_calls"
        };
        let usage = match self.usage {
            None => json!({ "prompt_tokens": 100, "completion_tokens": 20 }),
            Some((prompt, completion, None)) => {
                json!({ "prompt_tokens": prompt, "completion_tokens": completion })
            }
            Some((prompt, completion, Some(reasoning))) => json!({
                "prompt_tokens": prompt,
                "completion_tokens": completion,
                "completion_tokens_details": { "reasoning_tokens": reasoning }
            }),
        };
        json!({
            "model": model,
            "choices": [{ "index": 0, "message": message, "finish_reason": finish }],
            "usage": usage
        })
    }
}

/// One queue entry: an answer, or a failure served in its place.
enum Scripted {
    Choice(MockChoice),
    Fault(MockFault),
}

#[derive(Default)]
struct MockState {
    responses: Mutex<Vec<Scripted>>,
    requests: Mutex<Vec<Value>>,
}

/// An in-process OpenAI-compatible endpoint on an ephemeral port.
pub struct MockOpenAiServer {
    addr: SocketAddr,
    state: Arc<MockState>,
    shutdown: Option<oneshot::Sender<()>>,
    handle: tokio::task::JoinHandle<()>,
}

impl MockOpenAiServer {
    pub async fn start() -> Self {
        let state = Arc::new(MockState::default());
        let app = Router::new()
            .route("/chat/completions", post(completions))
            .with_state(state.clone());
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let (tx, rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        Self {
            addr,
            state,
            shutdown: Some(tx),
            handle,
        }
    }

    /// Queue the next response. FIFO; a request past the end gets an
    /// empty assistant turn rather than an error, so a test that
    /// over-runs its script fails on its own assertion instead of a
    /// confusing transport failure.
    pub fn push(&self, choice: MockChoice) {
        self.state
            .responses
            .lock()
            .unwrap()
            .push(Scripted::Choice(choice));
    }

    /// Queue a failure in place of the next response — a status with
    /// headers of the test's choosing, or a request held past the
    /// client's deadline.
    pub fn push_fault(&self, fault: MockFault) {
        self.state
            .responses
            .lock()
            .unwrap()
            .push(Scripted::Fault(fault));
    }

    /// Every request body received, in order — the bytes we actually
    /// sent, which is the whole point of this mock.
    pub fn requests(&self) -> Vec<Value> {
        self.state.requests.lock().unwrap().clone()
    }

    pub fn base_url(&self) -> String {
        format!("http://{}/", self.addr)
    }

    /// A client routed here, with `model_id` declared as an
    /// OpenAI-compatible model — the same wiring an operator uses for a
    /// Kimi or DeepSeek endpoint.
    pub fn client(&self, model_id: &str) -> crate::llm::GenAiClient {
        let mut extra = std::collections::BTreeMap::new();
        extra.insert(
            "mock".to_string(),
            crate::config::ProviderConfig {
                api_shape: crate::config::ApiShape::OpenaiCompatible,
                base_url: Some(self.base_url()),
                api_key_env: "FQ_MOCK_OPENAI_KEY".to_string(),
                models: vec![model_id.to_string()],
                pricing: std::collections::BTreeMap::new(),
            },
        );
        // genai resolves auth from the env var named above; the mock does
        // not check it, but the resolver must find *something* or the
        // request fails before it is sent.
        unsafe { std::env::set_var("FQ_MOCK_OPENAI_KEY", "mock-key") };
        crate::llm::GenAiClient::from_providers(
            &crate::config::ProvidersConfig {
                anthropic: None,
                extra,
            },
            crate::llm::LlmTimeouts::default(),
        )
        .expect("the mock's client builds")
    }

    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = self.handle.await;
    }
}

async fn completions(
    State(state): State<Arc<MockState>>,
    ExtractJson(body): ExtractJson<Value>,
) -> Response {
    let model = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("mock-model")
        .to_string();
    state.requests.lock().unwrap().push(body);
    let scripted = {
        let mut queued = state.responses.lock().unwrap();
        if queued.is_empty() {
            Scripted::Choice(MockChoice::text(""))
        } else {
            queued.remove(0)
        }
    };
    match scripted {
        Scripted::Choice(choice) => Json(choice.to_body(&model)).into_response(),
        Scripted::Fault(fault) => fault.serve(openai_error_body).await,
    }
}

/// The error shape an OpenAI-compatible endpoint returns.
fn openai_error_body(status: u16) -> Value {
    let kind = match status {
        429 => "rate_limit_error",
        400..=499 => "invalid_request_error",
        _ => "server_error",
    };
    json!({
        "error": {
            "message": format!("MockOpenAiServer scripted a {status}"),
            "type": kind,
            "param": Value::Null,
            "code": Value::Null,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Message, RequestParams};
    use crate::llm::{ChatRequest, LlmClient, LlmError};
    use std::time::Duration;

    /// The OpenAI-compatible shape gets the same `Retry-After` read —
    /// including `retry-after-ms`, which OpenAI sends and its SDK reads
    /// first — through the adapter genai uses for every such provider.
    #[tokio::test]
    async fn a_429_from_an_openai_compatible_provider_carries_retry_after_ms() {
        let mock = MockOpenAiServer::start().await;
        mock.push_fault(
            MockFault::status(429)
                .with_header("retry-after-ms", "250")
                .with_retry_after("5"),
        );
        let client = mock.client("kimi-k2");

        let err = client
            .chat(ChatRequest {
                model: "kimi-k2".to_string(),
                messages: vec![Message::user("hello".to_string())],
                tools: vec![],
                params: RequestParams {
                    effort: None,
                    temperature: None,
                    max_tokens: None,
                },
            })
            .await
            .expect_err("a 429 is an error");
        match &err {
            LlmError::RateLimited { model, retry_after } => {
                assert_eq!(model, "kimi-k2");
                assert_eq!(*retry_after, Some(Duration::from_millis(250)));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
        assert_eq!(mock.requests().len(), 1, "the request went out once");

        mock.shutdown().await;
    }
}
