//! LLM client abstraction.
//!
//! factor-q owns its own request/response types so the internal contract
//! and the event schema do not depend on any specific LLM client library.
//! Concrete implementations adapt these types to whatever underlying
//! library they use — see `llm::genai` for the `genai` adapter and
//! `llm::fixture` for the canned-response client used in tests.
//!
//! The call_id is owned by the executor, not the client. Each call the
//! executor makes gets a fresh UUID v7 assigned before the client is
//! invoked; the same id is used to correlate `llm.request`,
//! `llm.response`, and `cost` events for that call.

pub mod fixture;
pub mod genai;

pub use genai::GenAiClient;

use async_trait::async_trait;

use crate::events::{Message, MessageToolCall, RequestParams, StopReason, TokenUsage, ToolSchema};

/// A request to an LLM, without the call_id (assigned by the executor).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub params: RequestParams,
}

/// A response from an LLM, without the call_id.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChatResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<MessageToolCall>,
    pub stop_reason: StopReason,
    pub usage: TokenUsage,
}

/// Abstraction over any LLM client. Implementations are responsible for
/// converting between factor-q's types and their underlying library's.
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError>;
}

/// Errors from an LLM client call.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("authentication failed: {0}")]
    Auth(String),

    #[error("rate limited")]
    RateLimited,

    #[error("invalid response from provider: {0}")]
    InvalidResponse(String),

    #[error("request failed: {0}")]
    RequestFailed(String),

    /// The runtime refused to dispatch because the model has no pricing
    /// (ADR-0004 guarantee — refusing beats untracked spend). Permanent;
    /// never retried. Unreachable when the startup pricing guarantee is
    /// enforced, so this is defence in depth.
    #[error("no pricing for model '{0}'; refusing to dispatch (would be untracked spend)")]
    UnpricedModel(String),
}

impl LlmError {
    /// Whether this error is transient and worth retrying. Rate limits and
    /// request/transport failures (including network "web call failed"
    /// errors) are transient; auth failures and invalid responses are
    /// permanent — retrying only wastes time and budget.
    pub fn is_transient(&self) -> bool {
        matches!(self, LlmError::RateLimited | LlmError::RequestFailed(_))
    }
}

/// Bounded retry-with-backoff policy for transient LLM errors. These are
/// tuning knobs, so they are configuration (design principle 8), not
/// constants — surfaced in the daemon config with these defaults.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RetryConfig {
    /// Total attempts including the first; `1` disables retry.
    pub max_attempts: u32,
    /// Delay before the first retry; doubles each subsequent attempt.
    pub base_delay_ms: u64,
    /// Cap on any single backoff delay.
    pub max_delay_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_delay_ms: 500,
            max_delay_ms: 30_000,
        }
    }
}

/// An [`LlmClient`] decorator that retries transient errors with
/// exponential backoff and full jitter. Retrying a model call is safe: it
/// is a stateless request with no side effect (unlike a tool call). A
/// retry re-attempts the same turn, so it does not consume a reducer
/// iteration — the reducer only advances on a successful response.
pub struct RetryingLlmClient<C> {
    inner: C,
    config: RetryConfig,
}

impl<C> RetryingLlmClient<C> {
    pub fn new(inner: C, config: RetryConfig) -> Self {
        Self { inner, config }
    }
}

#[async_trait]
impl<C: LlmClient> LlmClient for RetryingLlmClient<C> {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        let mut attempt: u32 = 1;
        loop {
            match self.inner.chat(request.clone()).await {
                Ok(response) => return Ok(response),
                Err(err) if err.is_transient() && attempt < self.config.max_attempts => {
                    let delay = backoff_delay(attempt, &self.config);
                    tracing::warn!(
                        attempt,
                        max_attempts = self.config.max_attempts,
                        delay_ms = delay.as_millis() as u64,
                        error = %err,
                        "transient LLM error; retrying after backoff"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }
}

/// Exponential backoff with full jitter: a random delay in
/// `[0, min(max_delay, base * 2^(attempt-1))]`. Full jitter keeps a fleet
/// of agents from retrying in lockstep against a recovering API.
fn backoff_delay(attempt: u32, config: &RetryConfig) -> std::time::Duration {
    let shift = (attempt - 1).min(20);
    let exp = config.base_delay_ms.saturating_mul(1u64 << shift);
    let ceiling = exp.min(config.max_delay_ms);
    let millis = if ceiling == 0 {
        0
    } else {
        jitter_source() % (ceiling + 1)
    };
    std::time::Duration::from_millis(millis)
}

/// A cheap process-local pseudo-random source for jitter, avoiding a `rand`
/// dependency. Not for cryptographic use.
fn jitter_source() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use crate::events::{RequestParams, StopReason, TokenUsage};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Fails transiently or permanently for the first `fail_first` calls,
    /// then succeeds; records how many times it was called.
    struct FlakyClient {
        fail_first: u32,
        transient: bool,
        calls: AtomicU32,
    }

    #[async_trait]
    impl LlmClient for FlakyClient {
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, LlmError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_first {
                Err(if self.transient {
                    LlmError::RequestFailed("web call failed".to_string())
                } else {
                    LlmError::Auth("bad key".to_string())
                })
            } else {
                Ok(canned())
            }
        }
    }

    fn flaky(fail_first: u32, transient: bool) -> FlakyClient {
        FlakyClient {
            fail_first,
            transient,
            calls: AtomicU32::new(0),
        }
    }

    fn canned() -> ChatResponse {
        ChatResponse {
            content: Some("done".to_string()),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        }
    }

    fn request() -> ChatRequest {
        ChatRequest {
            model: "test-model".to_string(),
            messages: vec![],
            tools: vec![],
            params: RequestParams {
                temperature: None,
                max_tokens: None,
            },
        }
    }

    /// Zero delays so tests do not actually sleep.
    fn fast() -> RetryConfig {
        RetryConfig {
            max_attempts: 4,
            base_delay_ms: 0,
            max_delay_ms: 0,
        }
    }

    #[tokio::test]
    async fn retries_transient_then_succeeds() {
        let client = RetryingLlmClient::new(flaky(2, true), fast());
        assert!(client.chat(request()).await.is_ok());
        assert_eq!(
            client.inner.calls.load(Ordering::SeqCst),
            3,
            "2 transient failures + 1 success"
        );
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        let client = RetryingLlmClient::new(flaky(99, true), fast());
        assert!(client.chat(request()).await.is_err());
        assert_eq!(
            client.inner.calls.load(Ordering::SeqCst),
            4,
            "bounded at max_attempts"
        );
    }

    #[tokio::test]
    async fn does_not_retry_permanent_errors() {
        let client = RetryingLlmClient::new(flaky(99, false), fast());
        assert!(client.chat(request()).await.is_err());
        assert_eq!(
            client.inner.calls.load(Ordering::SeqCst),
            1,
            "permanent error, no retry"
        );
    }
}
