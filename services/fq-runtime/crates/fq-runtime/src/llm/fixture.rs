//! A fixture LLM client returning canned responses.
//!
//! Used for testing the executor and any other component that needs an
//! `LlmClient` without making real API calls. Holds a queue of responses
//! and returns them in order. Also records every request it sees so tests
//! can assert on what was sent.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::{ChatRequest, ChatResponse, LlmClient, LlmError};

/// A test-only LLM client that returns canned responses in order.
#[derive(Default, Clone)]
pub struct FixtureClient {
    inner: Arc<Mutex<FixtureInner>>,
}

#[derive(Default)]
struct FixtureInner {
    responses: Vec<Result<ChatResponse, LlmError>>,
    requests: Vec<ChatRequest>,
    cursor: usize,
}

impl FixtureClient {
    /// Create a new fixture client with no canned responses.
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a successful response onto the queue. Responses are returned
    /// in the order they were pushed.
    pub fn push_response(&self, response: ChatResponse) {
        self.inner.lock().unwrap().responses.push(Ok(response));
    }

    /// Push an error onto the queue.
    pub fn push_error(&self, error: LlmError) {
        self.inner.lock().unwrap().responses.push(Err(error));
    }

    /// Return a clone of every request the client has seen, in order.
    pub fn requests(&self) -> Vec<ChatRequest> {
        self.inner.lock().unwrap().requests.clone()
    }
}

#[async_trait]
impl LlmClient for FixtureClient {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        let mut inner = self.inner.lock().unwrap();
        inner.requests.push(request);
        let cursor = inner.cursor;
        inner.cursor += 1;
        match inner.responses.get(cursor) {
            Some(Ok(response)) => Ok(response.clone()),
            Some(Err(err)) => Err(clone_error(err)),
            None => Err(LlmError::RequestFailed(
                "FixtureClient exhausted: no more canned responses".to_string(),
            )),
        }
    }
}

fn clone_error(err: &LlmError) -> LlmError {
    match err {
        LlmError::Auth(s) => LlmError::Auth(s.clone()),
        LlmError::RateLimited => LlmError::RateLimited,
        LlmError::InvalidResponse(s) => LlmError::InvalidResponse(s.clone()),
        LlmError::RequestFailed(s) => LlmError::RequestFailed(s.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{RequestParams, StopReason, TokenUsage};

    fn sample_request() -> ChatRequest {
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

    fn sample_response(text: &str) -> ChatResponse {
        ChatResponse {
            content: Some(text.to_string()),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        }
    }

    #[tokio::test]
    async fn returns_responses_in_order() {
        let client = FixtureClient::new();
        client.push_response(sample_response("first"));
        client.push_response(sample_response("second"));

        let r1 = client.chat(sample_request()).await.unwrap();
        let r2 = client.chat(sample_request()).await.unwrap();

        assert_eq!(r1.content.as_deref(), Some("first"));
        assert_eq!(r2.content.as_deref(), Some("second"));
    }

    #[tokio::test]
    async fn records_requests() {
        let client = FixtureClient::new();
        client.push_response(sample_response("ok"));

        let mut req = sample_request();
        req.model = "recorded-model".to_string();
        client.chat(req).await.unwrap();

        let recorded = client.requests();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].model, "recorded-model");
    }

    #[tokio::test]
    async fn propagates_errors() {
        let client = FixtureClient::new();
        client.push_error(LlmError::RateLimited);

        let err = client.chat(sample_request()).await.unwrap_err();
        assert!(matches!(err, LlmError::RateLimited));
    }

    #[tokio::test]
    async fn exhausted_client_returns_error() {
        let client = FixtureClient::new();
        let err = client.chat(sample_request()).await.unwrap_err();
        assert!(matches!(err, LlmError::RequestFailed(_)));
    }
}
