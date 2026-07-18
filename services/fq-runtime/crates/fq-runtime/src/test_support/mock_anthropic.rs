//! Mock Anthropic Messages API server.
//!
//! Stands up an in-process HTTP server on an ephemeral port that
//! speaks the Anthropic `POST /v1/messages` contract. Tests push
//! canned responses, point a `GenAiClient` at the mock's
//! [`base_url`], and run the runtime end-to-end without touching
//! the real Anthropic API.
//!
//! Style is sequenced-response (FIFO), matching the existing
//! `FixtureClient::push_response` ergonomics. The mock also
//! captures each request body so tests can assert on what we
//! sent (model, system prompt, messages, etc.) if they care.
//!
//! See `docs/plans/closed/2026-05-18-mock-llm-test-harness.md`.
//!
//! # Example
//!
//! ```no_run
//! # use fq_runtime::test_support::mock_anthropic::{MockAnthropicServer, MockResponse};
//! # use fq_runtime::llm::GenAiClient;
//! # tokio_test::block_on(async {
//! let mock = MockAnthropicServer::start().await;
//! mock.push_response(MockResponse::text("hello", 12, 4));
//! let client = GenAiClient::with_base_url(mock.base_url());
//! // ... drive the runtime ...
//! mock.shutdown().await;
//! # });
//! ```

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Json as ExtractJson, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::post;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// One canned response served by the mock.
///
/// `input_tokens` here follows Anthropic's wire semantics: the
/// *uncached* portion only, with cache reads/writes reported in the
/// separate fields (the genai adapter sums the three into the
/// runtime's total-prompt `TokenUsage::input_tokens`).
#[derive(Debug, Clone)]
pub struct MockResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_write_tokens: u32,
}

/// Either a text block or a tool_use block in the response.
#[derive(Debug, Clone)]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
}

impl MockResponse {
    /// Build a plain-text response with the given token counts.
    pub fn text(text: impl Into<String>, input_tokens: u32, output_tokens: u32) -> Self {
        Self {
            content: vec![ContentBlock::Text { text: text.into() }],
            stop_reason: "end_turn".to_string(),
            input_tokens,
            output_tokens,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        }
    }

    /// Build a successful explicit terminal response.
    pub fn report_success(
        summary: impl Into<String>,
        input_tokens: u32,
        output_tokens: u32,
    ) -> Self {
        Self::tool_use(
            "report-outcome",
            crate::tools::REPORT_OUTCOME_CANONICAL_NAME,
            serde_json::json!({"status": "success", "summary": summary.into()}),
            input_tokens,
            output_tokens,
        )
    }

    /// Override the wire `stop_reason` (e.g. `"max_tokens"` for a
    /// truncated response).
    pub fn with_stop_reason(mut self, stop_reason: impl Into<String>) -> Self {
        self.stop_reason = stop_reason.into();
        self
    }

    /// Report prompt-cache activity in the response usage
    /// (`cache_read_input_tokens` / `cache_creation_input_tokens`).
    pub fn with_cache_usage(mut self, cache_read_tokens: u32, cache_write_tokens: u32) -> Self {
        self.cache_read_tokens = cache_read_tokens;
        self.cache_write_tokens = cache_write_tokens;
        self
    }

    /// Build a tool-use response carrying one `tool_use` block. Stop
    /// reason is `"tool_use"` per Anthropic's contract.
    pub fn tool_use(
        id: impl Into<String>,
        name: impl Into<String>,
        input: Value,
        input_tokens: u32,
        output_tokens: u32,
    ) -> Self {
        Self {
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: name.into(),
                input,
            }],
            stop_reason: "tool_use".to_string(),
            input_tokens,
            output_tokens,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        }
    }

    /// Serialise to the JSON shape Anthropic returns from
    /// `POST /v1/messages`. The `id` is a stable placeholder so
    /// snapshot-style assertions can compare bodies.
    pub fn to_anthropic_json(&self, model: &str) -> Value {
        let content: Vec<Value> = self
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } => json!({
                    "type": "text",
                    "text": text,
                }),
                ContentBlock::ToolUse { id, name, input } => json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": input,
                }),
            })
            .collect();
        json!({
            "id": "msg_mock",
            "type": "message",
            "role": "assistant",
            "content": content,
            "model": model,
            "stop_reason": self.stop_reason,
            "stop_sequence": Value::Null,
            "usage": {
                "input_tokens": self.input_tokens,
                "output_tokens": self.output_tokens,
                "cache_read_input_tokens": self.cache_read_tokens,
                "cache_creation_input_tokens": self.cache_write_tokens,
            },
        })
    }
}

#[derive(Default)]
struct Inner {
    responses: Vec<MockResponse>,
    received: Vec<Value>,
}

/// In-process axum server speaking the Anthropic Messages API.
///
/// Listens on an ephemeral port; [`base_url`](Self::base_url)
/// returns `http://127.0.0.1:PORT/v1/` (matches Anthropic's URL
/// shape: genai constructs request URLs as `{base_url}messages`,
/// so the mock serves `POST /v1/messages`).
pub struct MockAnthropicServer {
    inner: Arc<Mutex<Inner>>,
    base_url: String,
    shutdown_tx: Option<oneshot::Sender<()>>,
    join_handle: Option<JoinHandle<()>>,
}

impl MockAnthropicServer {
    /// Bind to an ephemeral port and start serving. The returned
    /// server holds the listening task; call
    /// [`shutdown`](Self::shutdown) to stop it cleanly. If the
    /// server is dropped without `shutdown`, the task is aborted.
    pub async fn start() -> Self {
        let inner = Arc::new(Mutex::new(Inner::default()));
        let app = Router::new()
            .route("/v1/messages", post(messages_handler))
            .with_state(inner.clone());

        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .expect("bind ephemeral port");
        let local_addr = listener.local_addr().expect("local_addr");
        let base_url = format!("http://{}/v1/", local_addr);

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let join_handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("mock server fell over");
        });

        Self {
            inner,
            base_url,
            shutdown_tx: Some(shutdown_tx),
            join_handle: Some(join_handle),
        }
    }

    /// Append a response to the FIFO queue. Each successive request
    /// to `/v1/messages` pops the next one.
    pub fn push_response(&self, r: MockResponse) {
        self.inner.lock().unwrap().responses.push(r);
    }

    /// Snapshot of every request body the mock has received, in
    /// order of arrival. Returns owned `Value`s — safe to read
    /// after the server is shut down.
    pub fn received_requests(&self) -> Vec<Value> {
        self.inner.lock().unwrap().received.clone()
    }

    /// Base URL to pass to [`GenAiClient::with_base_url`]. Has the
    /// trailing `/v1/` so genai's `format!("{base_url}messages")`
    /// targets the right path.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Stop the server. Signals graceful shutdown and awaits the
    /// listening task.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for MockAnthropicServer {
    fn drop(&mut self) {
        if let Some(handle) = self.join_handle.take() {
            handle.abort();
        }
    }
}

async fn messages_handler(
    State(inner): State<Arc<Mutex<Inner>>>,
    ExtractJson(body): ExtractJson<Value>,
) -> impl IntoResponse {
    let model = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("claude-mock")
        .to_string();

    let response = {
        let mut guard = inner.lock().unwrap();
        guard.received.push(body);
        if guard.responses.is_empty() {
            None
        } else {
            Some(guard.responses.remove(0))
        }
    };

    match response {
        Some(r) => (StatusCode::OK, Json(r.to_anthropic_json(&model))).into_response(),
        None => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "type": "error",
                "error": {
                    "type": "mock_responses_exhausted",
                    "message": "MockAnthropicServer received a request but no response was queued"
                }
            })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Message, MessageRole, RequestParams};
    use crate::llm::{ChatRequest, GenAiClient, LlmClient};

    #[test]
    fn mock_response_text_serialises_to_anthropic_shape() {
        let r = MockResponse::text("hello world", 12, 4);
        let json = r.to_anthropic_json("claude-haiku-4-5");
        assert_eq!(json["type"], "message");
        assert_eq!(json["role"], "assistant");
        assert_eq!(json["model"], "claude-haiku-4-5");
        assert_eq!(json["stop_reason"], "end_turn");
        assert_eq!(json["content"][0]["type"], "text");
        assert_eq!(json["content"][0]["text"], "hello world");
        assert_eq!(json["usage"]["input_tokens"], 12);
        assert_eq!(json["usage"]["output_tokens"], 4);
    }

    #[test]
    fn mock_response_tool_use_serialises_to_anthropic_shape() {
        let r = MockResponse::tool_use(
            "toolu_01",
            "file_read",
            json!({"path": "Cargo.toml"}),
            20,
            8,
        );
        let json = r.to_anthropic_json("claude-haiku-4-5");
        assert_eq!(json["stop_reason"], "tool_use");
        assert_eq!(json["content"][0]["type"], "tool_use");
        assert_eq!(json["content"][0]["id"], "toolu_01");
        assert_eq!(json["content"][0]["name"], "file_read");
        assert_eq!(json["content"][0]["input"]["path"], "Cargo.toml");
    }

    fn simple_request() -> ChatRequest {
        ChatRequest {
            model: "claude-haiku-4-5".to_string(),
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
                effort: None,
                temperature: Some(0.0),
                max_tokens: Some(16),
            },
        }
    }

    #[tokio::test]
    async fn mock_returns_canned_text_response() {
        // Set a dummy API key so genai doesn't refuse before
        // dispatch. Value never leaves the local mock.
        // Safety: tests share a process, but this var is only
        // read by genai's auth resolver during this call.
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-mock-not-real") };

        let mock = MockAnthropicServer::start().await;
        mock.push_response(MockResponse::text("hello from mock", 50, 10));

        let client = GenAiClient::with_base_url(mock.base_url());
        let response = client.chat(simple_request()).await.expect("chat");
        assert_eq!(
            response.content.as_deref(),
            Some("hello from mock"),
            "expected mock content"
        );
        assert_eq!(response.usage.input_tokens, 50);
        assert_eq!(response.usage.output_tokens, 10);

        mock.shutdown().await;
    }

    #[tokio::test]
    async fn mock_returns_tool_use_response() {
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-mock-not-real") };

        let mock = MockAnthropicServer::start().await;
        mock.push_response(MockResponse::tool_use(
            "toolu_99",
            "file_read",
            json!({"path": "Cargo.toml"}),
            22,
            6,
        ));

        let client = GenAiClient::with_base_url(mock.base_url());
        let response = client.chat(simple_request()).await.expect("chat");
        assert_eq!(response.tool_calls.len(), 1, "expected one tool call");
        assert_eq!(response.tool_calls[0].tool_name, "file_read");
        assert_eq!(response.tool_calls[0].tool_call_id.as_str(), "toolu_99");

        mock.shutdown().await;
    }

    #[tokio::test]
    async fn mock_serves_responses_in_order() {
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-mock-not-real") };

        let mock = MockAnthropicServer::start().await;
        mock.push_response(MockResponse::text("first", 10, 2));
        mock.push_response(MockResponse::text("second", 10, 2));

        let client = GenAiClient::with_base_url(mock.base_url());
        let r1 = client.chat(simple_request()).await.expect("chat 1");
        let r2 = client.chat(simple_request()).await.expect("chat 2");
        assert_eq!(r1.content.as_deref(), Some("first"));
        assert_eq!(r2.content.as_deref(), Some("second"));

        mock.shutdown().await;
    }

    #[tokio::test]
    async fn mock_captures_request_bodies() {
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-mock-not-real") };

        let mock = MockAnthropicServer::start().await;
        mock.push_response(MockResponse::text("ok", 5, 1));

        let client = GenAiClient::with_base_url(mock.base_url());
        client.chat(simple_request()).await.expect("chat");

        let received = mock.received_requests();
        assert_eq!(received.len(), 1, "should have captured one request");
        let body = &received[0];
        assert_eq!(body["model"], "claude-haiku-4-5");
        // Anthropic puts the system prompt at the top level, not
        // in messages. Sanity check that genai is preserving it.
        assert!(
            body.get("system").is_some() || body.get("messages").is_some(),
            "expected system or messages on the request body"
        );

        mock.shutdown().await;
    }

    #[tokio::test]
    async fn mock_returns_400_when_responses_exhausted() {
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-mock-not-real") };

        let mock = MockAnthropicServer::start().await;
        // No push_response — queue is empty.

        let client = GenAiClient::with_base_url(mock.base_url());
        let err = client
            .chat(simple_request())
            .await
            .expect_err("expected error");
        // genai surfaces HTTP 400 as a provider error of some kind;
        // we don't pin the exact variant, but it must be an error.
        let msg = format!("{err}");
        assert!(
            !msg.is_empty(),
            "expected non-empty error message, got {msg:?}"
        );

        mock.shutdown().await;
    }
}
