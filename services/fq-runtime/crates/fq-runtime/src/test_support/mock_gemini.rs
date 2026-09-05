//! Mock Gemini `generateContent` server.
//!
//! The third provider shape, beside [`super::mock_anthropic`] and
//! [`super::mock_openai`]. It exists for one reason: Gemini's
//! continuity token — the `thoughtSignature` a thinking model attaches
//! to a function-call part — travels through genai in a shape neither of
//! the other two use (a bare `ThoughtSignature` part with no reasoning
//! text beside it), and until this mock existed nothing hermetic could
//! show whether factor-q carries it back (#600).
//!
//! Sequenced-response (FIFO), matching the sibling mocks. Every request
//! body is captured so a test can assert on the bytes that went out —
//! which is where a signature either sits on the function call it came
//! with, or does not.
//!
//! # Example
//!
//! ```no_run
//! # use fq_runtime::test_support::mock_gemini::{MockGeminiServer, GeminiTurn};
//! # async fn example() {
//! let mock = MockGeminiServer::start().await;
//! mock.push(GeminiTurn::silent().with_function_call("read_file", serde_json::json!({"path": "/x"}), Some("sig")));
//! let _client = mock.client("gemini-3-pro");
//! // ... drive a request ...
//! let sent = mock.requests();
//! mock.shutdown().await;
//! # }
//! ```

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Json as ExtractJson, Path, State};
use axum::response::Json;
use axum::routing::post;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// One canned model turn, as the `parts` Gemini returns.
///
/// Parts are kept as raw wire JSON on purpose: what matters in these
/// tests is exactly which part a `thoughtSignature` rides on, and a
/// typed builder would hide that.
#[derive(Debug, Clone)]
pub struct GeminiTurn {
    parts: Vec<Value>,
    finish_reason: String,
    /// `(prompt, candidates, thoughts)` token counts. Gemini reports
    /// thinking tokens *beside* `candidatesTokenCount`, not inside it;
    /// genai normalises that into the OpenAI-style total plus a
    /// `reasoning_tokens` detail.
    usage: (u64, u64, Option<u64>),
}

impl Default for GeminiTurn {
    fn default() -> Self {
        Self {
            parts: Vec::new(),
            finish_reason: "STOP".to_string(),
            usage: (100, 20, None),
        }
    }
}

impl GeminiTurn {
    /// A turn that says something.
    pub fn text(text: &str) -> Self {
        Self::default().with_text(text)
    }

    /// A turn with no parts yet — build it up with the `with_*` methods.
    pub fn silent() -> Self {
        Self::default()
    }

    /// Append a `text` part.
    pub fn with_text(mut self, text: &str) -> Self {
        self.parts.push(json!({"text": text}));
        self
    }

    /// Append a `functionCall` part, carrying a `thoughtSignature` on
    /// that same part when `signature` is given — which is where Gemini
    /// puts it, and what the next request must hand back.
    ///
    /// No `id`: the real API omits one, and genai synthesises
    /// `call#<name>#<n>` so the tool result can be routed back by name.
    pub fn with_function_call(mut self, name: &str, args: Value, signature: Option<&str>) -> Self {
        let mut part = json!({"functionCall": {"name": name, "args": args}});
        if let Some(signature) = signature {
            part["thoughtSignature"] = json!(signature);
        }
        self.parts.push(part);
        self
    }

    /// Append a thought summary — a `text` part flagged `thought: true`,
    /// which the API returns only when `includeThoughts` was requested.
    pub fn with_thought(mut self, text: &str) -> Self {
        self.parts.push(json!({"thought": true, "text": text}));
        self
    }

    /// Append a bare `thoughtSignature` part, the shape a text-only turn
    /// can carry its signature in.
    pub fn with_signature(mut self, signature: &str) -> Self {
        self.parts.push(json!({"thoughtSignature": signature}));
        self
    }

    /// Report token usage; `thoughts` goes out as `thoughtsTokenCount`.
    pub fn with_usage(mut self, prompt: u64, candidates: u64, thoughts: Option<u64>) -> Self {
        self.usage = (prompt, candidates, thoughts);
        self
    }

    fn to_body(&self, model: &str) -> Value {
        let (prompt, candidates, thoughts) = self.usage;
        let mut usage = json!({
            "promptTokenCount": prompt,
            "candidatesTokenCount": candidates,
            "totalTokenCount": prompt + candidates + thoughts.unwrap_or(0),
        });
        if let Some(thoughts) = thoughts {
            usage["thoughtsTokenCount"] = json!(thoughts);
        }
        json!({
            "candidates": [{
                "content": { "parts": self.parts, "role": "model" },
                "finishReason": self.finish_reason,
                "index": 0
            }],
            "usageMetadata": usage,
            "modelVersion": model
        })
    }
}

#[derive(Default)]
struct MockState {
    responses: Mutex<Vec<GeminiTurn>>,
    requests: Mutex<Vec<Value>>,
}

/// An in-process Gemini endpoint on an ephemeral port, serving
/// `POST /models/{model}:generateContent` — the path genai builds under
/// whatever base URL it is given.
pub struct MockGeminiServer {
    addr: SocketAddr,
    state: Arc<MockState>,
    shutdown: Option<oneshot::Sender<()>>,
    handle: tokio::task::JoinHandle<()>,
}

impl MockGeminiServer {
    pub async fn start() -> Self {
        let state = Arc::new(MockState::default());
        let app = Router::new()
            .route("/models/{model_action}", post(generate))
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

    /// Queue the next turn. FIFO; a request past the end gets an empty
    /// text turn rather than an error, so an over-run script fails on its
    /// own assertion instead of a confusing transport failure.
    pub fn push(&self, turn: GeminiTurn) {
        self.state.responses.lock().unwrap().push(turn);
    }

    /// Every request body received, in order — the bytes we actually
    /// sent, which is the whole point of this mock.
    pub fn requests(&self) -> Vec<Value> {
        self.state.requests.lock().unwrap().clone()
    }

    pub fn base_url(&self) -> String {
        format!("http://{}/", self.addr)
    }

    /// A client routed here, with `model_id` declared under a Gemini-shaped
    /// provider — the same wiring an operator uses for `api_shape =
    /// "gemini"` with an endpoint override.
    pub fn client(&self, model_id: &str) -> crate::llm::GenAiClient {
        let mut extra = std::collections::BTreeMap::new();
        extra.insert(
            "mock-gemini".to_string(),
            crate::config::ProviderConfig {
                api_shape: crate::config::ApiShape::Gemini,
                base_url: Some(self.base_url()),
                api_key_env: "FQ_MOCK_GEMINI_KEY".to_string(),
                models: vec![model_id.to_string()],
                pricing: std::collections::BTreeMap::new(),
            },
        );
        // genai resolves auth from the env var named above; the mock does
        // not check it, but the resolver must find *something* or the
        // request fails before it is sent.
        unsafe { std::env::set_var("FQ_MOCK_GEMINI_KEY", "mock-key") };
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

async fn generate(
    State(state): State<Arc<MockState>>,
    Path(model_action): Path<String>,
    ExtractJson(body): ExtractJson<Value>,
) -> Json<Value> {
    // The path segment is `<model>:generateContent`.
    let model = model_action
        .split_once(':')
        .map(|(model, _)| model)
        .unwrap_or(model_action.as_str())
        .to_string();
    state.requests.lock().unwrap().push(body);
    let turn = {
        let mut queued = state.responses.lock().unwrap();
        if queued.is_empty() {
            GeminiTurn::text("")
        } else {
            queued.remove(0)
        }
    };
    Json(turn.to_body(&model))
}
