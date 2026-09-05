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

use std::time::Duration;

use async_trait::async_trait;

use crate::events::{AssistantPart, Message, RequestParams, StopReason, TokenUsage, ToolSchema};

/// A request to an LLM, without the call_id (assigned by the executor).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub params: RequestParams,
}

/// A response from an LLM, without the call_id.
///
/// A response *is* an assistant turn, so it carries that turn kind's
/// parts (ADR-0034 D3) — which is why a tool result cannot appear here.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChatResponse {
    pub parts: Vec<AssistantPart>,
    pub stop_reason: StopReason,
    pub usage: TokenUsage,
}

impl ChatResponse {
    /// The synthetic response for an invocation that ended by declaring
    /// an outcome: the declared summary as the turn's only text, and no
    /// tool calls, because `report_outcome` is a terminal declaration
    /// rather than a dispatch (ADR-0014).
    pub fn completed(summary: Option<String>) -> Self {
        Self {
            parts: summary
                .map(|text| vec![AssistantPart::Text { text }])
                .unwrap_or_default(),
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        }
    }

    /// The turn's visible text, joined when the provider split it across
    /// parts. `None` when the turn carried none — a tool-only turn, or one
    /// that was pure reasoning.
    pub fn text(&self) -> Option<String> {
        let joined: Vec<&str> = self
            .parts
            .iter()
            .filter_map(|part| match part {
                AssistantPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        (!joined.is_empty()).then(|| joined.join("\n"))
    }

    /// The tool calls this turn requested, in order.
    pub fn tool_calls(&self) -> Vec<&crate::events::MessageToolCall> {
        self.parts
            .iter()
            .filter_map(|part| match part {
                AssistantPart::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect()
    }
}

/// Abstraction over any LLM client. Implementations are responsible for
/// converting between factor-q's types and their underlying library's.
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError>;
}

/// Errors from an LLM client call.
///
/// The variants are the joints the retry policy and the operator care
/// about, not the provider's taxonomy: each one says whether the same
/// request can succeed if sent again ([`Self::is_transient`]), and the
/// event schema mirrors them one-to-one as `LlmErrorKind`.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// No usable credentials, or the provider refused them (401, 403).
    #[error("authentication failed: {0}")]
    Auth(String),

    /// HTTP 429: the provider is throttling `model`. `retry_after` is
    /// the wait its `Retry-After` header asked for, when it sent one;
    /// the retry layer honours it up to `RetryConfig::max_retry_after_ms`
    /// and otherwise backs off exponentially (#278).
    #[error("rate limited on model '{model}'{}", retry_after_suffix(.retry_after))]
    RateLimited {
        model: String,
        retry_after: Option<Duration>,
    },

    /// A 200 whose body the adapter could not read as an assistant turn.
    #[error("invalid response from provider: {0}")]
    InvalidResponse(String),

    /// The provider rejected the request itself: a 4xx other than the
    /// auth statuses and 429 — a malformed request, an unknown model, a
    /// context that is too long, a payload that is too large, an unpaid
    /// account (402). The same bytes cannot succeed on a second try, so
    /// this is permanent; the message carries the status and the
    /// provider's body, which is the operator's only handle on it.
    #[error("request rejected by provider: {0}")]
    Rejected(String),

    /// The call did not complete, and nothing says the request is at
    /// fault: a 5xx, a connection that failed or dropped, a body that
    /// was not JSON. Transient — the next attempt is a fresh connection
    /// against a provider that may have recovered.
    #[error("request failed: {0}")]
    RequestFailed(String),

    /// No answer within `[worker] llm_timeout_secs` (#546): the provider
    /// accepted the connection and did not finish, or never accepted it.
    /// The call is abandoned and its connection dropped, which is what
    /// keeps a hung provider from parking the invocation. Transient. What
    /// the provider did with the abandoned request is unobservable — it
    /// may have billed it — so the failure event records no usage.
    #[error("no response from provider within {}s", .budget.as_secs())]
    Timeout { budget: Duration },

    /// The runtime refused to dispatch because the model has no pricing
    /// (ADR-0004 guarantee — refusing beats untracked spend). Permanent;
    /// never retried. Unreachable when the startup pricing guarantee is
    /// enforced, so this is defence in depth.
    #[error("no pricing for model '{0}'; refusing to dispatch (would be untracked spend)")]
    UnpricedModel(String),
}

impl LlmError {
    /// Whether this error is transient and worth retrying: the same
    /// request, sent again, has a real chance of succeeding. Rate
    /// limits, timeouts and request/transport failures (5xx, connection
    /// errors) are; auth failures, rejected requests and invalid
    /// responses are not — the provider has already answered the
    /// question, and retrying only wastes time and budget.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            LlmError::RateLimited { .. } | LlmError::RequestFailed(_) | LlmError::Timeout { .. }
        )
    }
}

/// The `Retry-After` clause of a rate-limit message, so an operator
/// reading a failure event sees what the provider asked for.
fn retry_after_suffix(retry_after: &Option<Duration>) -> String {
    match retry_after {
        Some(wait) => format!("; provider asked for a {}s wait", wait.as_secs()),
        None => String::new(),
    }
}

/// The deadlines on one provider call — `[worker] llm_timeout_secs` and
/// `llm_connect_timeout_secs`, applied to the HTTP client and again
/// around the call (#546, review finding B1).
///
/// Both exist so that no provider call can hold an invocation
/// indefinitely: a stalled provider ends in [`LlmError::Timeout`] at
/// `request`, and an endpoint that never answers the SYN fails at
/// `connect` rather than after the whole budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LlmTimeouts {
    /// Establishing the connection: TCP and TLS.
    pub connect: Duration,
    /// The whole call, end to end: connect, send, wait for the answer,
    /// read the whole body.
    pub request: Duration,
}

impl Default for LlmTimeouts {
    /// 5 s to connect and 10 minutes for the call — what the official
    /// Anthropic and OpenAI SDKs default to. The request budget is long
    /// because a large extended-thinking answer takes minutes to
    /// produce, and cutting it off costs twice: the provider has billed
    /// the tokens, and the retry pays for them again. A hang, by
    /// contrast, costs only time, and the point of the budget is that
    /// the time is bounded. Operators running fast models can trim it.
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(5),
            request: Duration::from_secs(600),
        }
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
    /// The longest `Retry-After` a rate-limited call is waited out in
    /// place. A provider that asks for more is not waited for: the call
    /// fails as rate-limited straight away, carrying the delay, so the
    /// invocation can be deferred rather than a worker parked doing
    /// nothing (#278, decided 2026-09-04). Default 120 s.
    pub max_retry_after_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_delay_ms: 500,
            max_delay_ms: 30_000,
            max_retry_after_ms: 120_000,
        }
    }
}

/// An [`LlmClient`] decorator that retries transient errors with
/// exponential backoff and full jitter. Retrying a model call is safe: it
/// is a stateless request with no side effect (unlike a tool call). A
/// retry re-attempts the same turn, so it does not consume a reducer
/// iteration — the reducer only advances on a successful response.
///
/// A rate limit that names its own delay is treated differently from
/// the other transient errors: the provider's `Retry-After` is the
/// floor on the wait, with the jittered backoff added on top so a fleet
/// told the same number does not come back in lockstep. Past
/// `RetryConfig::max_retry_after_ms` the call is given up at once.
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
                    let Some(delay) = retry_delay(&err, attempt, &self.config) else {
                        tracing::warn!(
                            attempt,
                            max_retry_after_ms = self.config.max_retry_after_ms,
                            error = %err,
                            "provider asked for a longer wait than the retry policy holds a \
                             worker for; giving the call up"
                        );
                        return Err(err);
                    };
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

/// How long to wait before the next attempt, or `None` when the provider
/// asked for longer than `max_retry_after_ms` — the case the retry layer
/// does not absorb, because holding a worker for minutes on a provider's
/// say-so is a deferral decision, not a retry (#278).
fn retry_delay(err: &LlmError, attempt: u32, config: &RetryConfig) -> Option<Duration> {
    let backoff = backoff_delay(attempt, config);
    match err {
        LlmError::RateLimited {
            retry_after: Some(asked),
            ..
        } => {
            if *asked > Duration::from_millis(config.max_retry_after_ms) {
                return None;
            }
            // The provider's number is the floor; the jitter on top is
            // ours, so a fleet told "2" does not all return at 2.000.
            Some(*asked + backoff)
        }
        _ => Some(backoff),
    }
}

/// Exponential backoff with full jitter: a random delay in
/// `[0, min(max_delay, base * 2^(attempt-1))]`. Full jitter keeps a fleet
/// of agents from retrying in lockstep against a recovering API.
fn backoff_delay(attempt: u32, config: &RetryConfig) -> Duration {
    let shift = (attempt - 1).min(20);
    let exp = config.base_delay_ms.saturating_mul(1u64 << shift);
    let ceiling = exp.min(config.max_delay_ms);
    let millis = if ceiling == 0 {
        0
    } else {
        jitter_source() % (ceiling + 1)
    };
    Duration::from_millis(millis)
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
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Instant;

    /// Fails with the scripted errors, in order, then succeeds; records
    /// how many times it was called.
    struct ScriptedClient {
        errors: Mutex<VecDeque<LlmError>>,
        calls: AtomicU32,
    }

    #[async_trait]
    impl LlmClient for ScriptedClient {
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.errors.lock().unwrap().pop_front() {
                Some(err) => Err(err),
                None => Ok(canned()),
            }
        }
    }

    fn scripted(errors: Vec<LlmError>) -> ScriptedClient {
        ScriptedClient {
            errors: Mutex::new(errors.into()),
            calls: AtomicU32::new(0),
        }
    }

    fn calls(client: &RetryingLlmClient<ScriptedClient>) -> u32 {
        client.inner.calls.load(Ordering::SeqCst)
    }

    fn transient() -> LlmError {
        LlmError::RequestFailed("web call failed".to_string())
    }

    fn rate_limited(retry_after: Option<Duration>) -> LlmError {
        LlmError::RateLimited {
            model: "test-model".to_string(),
            retry_after,
        }
    }

    fn canned() -> ChatResponse {
        ChatResponse {
            parts: vec![AssistantPart::Text {
                text: "done".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
            },
        }
    }

    fn request() -> ChatRequest {
        ChatRequest {
            model: "test-model".to_string(),
            messages: vec![],
            tools: vec![],
            params: RequestParams {
                effort: None,
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
            max_retry_after_ms: 120_000,
        }
    }

    #[tokio::test]
    async fn retries_transient_then_succeeds() {
        let client = RetryingLlmClient::new(scripted(vec![transient(), transient()]), fast());
        assert!(client.chat(request()).await.is_ok());
        assert_eq!(calls(&client), 3, "2 transient failures + 1 success");
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        let client =
            RetryingLlmClient::new(scripted((0..99).map(|_| transient()).collect()), fast());
        assert!(client.chat(request()).await.is_err());
        assert_eq!(calls(&client), 4, "bounded at max_attempts");
    }

    #[tokio::test]
    async fn does_not_retry_permanent_errors() {
        let client = RetryingLlmClient::new(
            scripted(vec![LlmError::Auth("bad key".to_string())]),
            fast(),
        );
        assert!(client.chat(request()).await.is_err());
        assert_eq!(calls(&client), 1, "permanent error, no retry");
    }

    /// The expected classification, written out variant by variant. The
    /// match is exhaustive on purpose: a new variant does not compile
    /// until it says here whether it is retried.
    fn retried_by_policy(err: &LlmError) -> bool {
        match err {
            LlmError::Auth(_) => false,
            LlmError::RateLimited { .. } => true,
            LlmError::InvalidResponse(_) => false,
            LlmError::Rejected(_) => false,
            LlmError::RequestFailed(_) => true,
            LlmError::Timeout { .. } => true,
            LlmError::UnpricedModel(_) => false,
        }
    }

    #[test]
    fn the_classifier_partitions_every_variant() {
        let errors = [
            LlmError::Auth("x".into()),
            rate_limited(None),
            rate_limited(Some(Duration::from_secs(2))),
            LlmError::InvalidResponse("x".into()),
            LlmError::Rejected("400".into()),
            LlmError::RequestFailed("503".into()),
            LlmError::Timeout {
                budget: Duration::from_secs(1),
            },
            LlmError::UnpricedModel("x".into()),
        ];
        for err in &errors {
            assert_eq!(err.is_transient(), retried_by_policy(err), "{err}");
        }
    }

    /// #278: a 429 that names its wait is retried after at least that
    /// long — the provider's number is the floor, not a suggestion.
    #[tokio::test]
    async fn a_rate_limit_waits_the_retry_after_the_provider_asked_for() {
        let asked = Duration::from_millis(300);
        let client = RetryingLlmClient::new(scripted(vec![rate_limited(Some(asked))]), fast());
        let started = Instant::now();
        assert!(client.chat(request()).await.is_ok());
        assert!(
            started.elapsed() >= asked,
            "retried before the provider's wait was up: {:?}",
            started.elapsed()
        );
        assert_eq!(calls(&client), 2);
    }

    /// Without the header a 429 is an ordinary transient error: retried
    /// on the exponential schedule, with no floor under the wait.
    #[tokio::test]
    async fn a_rate_limit_without_the_header_backs_off_like_any_transient_error() {
        let config = RetryConfig {
            base_delay_ms: 20,
            max_delay_ms: 20,
            ..fast()
        };
        let client = RetryingLlmClient::new(
            scripted(vec![rate_limited(None), rate_limited(None)]),
            config,
        );
        let started = Instant::now();
        assert!(client.chat(request()).await.is_ok());
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "backoff ran past two 20ms ceilings: {:?}",
            started.elapsed()
        );
        assert_eq!(calls(&client), 3);
    }

    /// A wait past `max_retry_after_ms` is not absorbed: the call fails
    /// at once, still carrying the wait, with the attempts unused.
    #[tokio::test]
    async fn a_retry_after_past_the_cap_is_given_up_at_once() {
        let config = RetryConfig {
            max_retry_after_ms: 1_000,
            ..fast()
        };
        let client = RetryingLlmClient::new(
            scripted(vec![rate_limited(Some(Duration::from_secs(300)))]),
            config,
        );
        let started = Instant::now();
        let err = client.chat(request()).await.expect_err("given up");
        assert!(
            matches!(
                err,
                LlmError::RateLimited { retry_after: Some(wait), .. }
                    if wait == Duration::from_secs(300)
            ),
            "the error still names the wait: {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "waited anyway: {:?}",
            started.elapsed()
        );
        assert_eq!(calls(&client), 1);
    }

    /// The cap is inclusive: a wait exactly at it is still honoured.
    #[tokio::test]
    async fn a_retry_after_at_the_cap_is_still_waited() {
        let config = RetryConfig {
            max_retry_after_ms: 100,
            ..fast()
        };
        let client = RetryingLlmClient::new(
            scripted(vec![rate_limited(Some(Duration::from_millis(100)))]),
            config,
        );
        assert!(client.chat(request()).await.is_ok());
        assert_eq!(calls(&client), 2);
    }

    /// #546: a timeout is transient, and the attempt cap is what stops
    /// it being retried forever.
    #[tokio::test]
    async fn timeouts_are_retried_up_to_the_attempt_cap() {
        let timeout = || LlmError::Timeout {
            budget: Duration::from_secs(1),
        };
        let client = RetryingLlmClient::new(scripted((0..99).map(|_| timeout()).collect()), fast());
        let err = client
            .chat(request())
            .await
            .expect_err("every attempt timed out");
        assert!(matches!(err, LlmError::Timeout { .. }), "{err:?}");
        assert_eq!(calls(&client), 4, "bounded at max_attempts");
    }

    #[tokio::test]
    async fn a_rejected_request_is_not_retried() {
        let client =
            RetryingLlmClient::new(scripted(vec![LlmError::Rejected("400".into())]), fast());
        assert!(client.chat(request()).await.is_err());
        assert_eq!(calls(&client), 1, "the provider already answered");
    }

    #[test]
    fn the_messages_say_what_the_provider_asked_for() {
        assert_eq!(
            rate_limited(Some(Duration::from_secs(2))).to_string(),
            "rate limited on model 'test-model'; provider asked for a 2s wait"
        );
        assert_eq!(
            rate_limited(None).to_string(),
            "rate limited on model 'test-model'"
        );
        assert_eq!(
            LlmError::Timeout {
                budget: Duration::from_secs(600)
            }
            .to_string(),
            "no response from provider within 600s"
        );
    }
}
