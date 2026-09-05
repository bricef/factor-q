//! A provider failure a mock can be scripted to serve in place of a
//! response — the fault-injection half of [`super::mock_anthropic`] and
//! [`super::mock_openai`], shared because the failures a provider can
//! inflict do not depend on its wire shape: a status, with or without a
//! `Retry-After`, or a connection that is accepted and then never
//! answered (#546, #278).

use std::time::Duration;

use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde_json::Value;

/// One scripted failure.
#[derive(Debug, Clone)]
pub enum MockFault {
    /// Answer at once with this status, the provider's error body for
    /// it, and these headers.
    Status {
        status: u16,
        headers: Vec<(String, String)>,
    },
    /// Accept the request and hold it for this long before answering —
    /// the hung-provider wedge class. Scripted longer than the client's
    /// budget, so the client gives up first; what the mock answers
    /// after that nobody reads.
    Stall(Duration),
}

impl MockFault {
    /// A response with this status and no headers beyond the body's.
    pub fn status(status: u16) -> Self {
        Self::Status {
            status,
            headers: Vec::new(),
        }
    }

    /// Add a response header. Meaningless on a stall, which is why a
    /// stall ignores it.
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        if let Self::Status { headers, .. } = &mut self {
            headers.push((name.to_string(), value.to_string()));
        }
        self
    }

    /// Send `Retry-After` with the response — seconds or an HTTP-date,
    /// exactly as given.
    pub fn with_retry_after(self, value: &str) -> Self {
        self.with_header("retry-after", value)
    }

    /// A request held open for `held_for` before anything is sent.
    pub fn stall(held_for: Duration) -> Self {
        Self::Stall(held_for)
    }

    /// Serve the fault. `error_body` renders the provider's error shape
    /// for a status; a stall answers with a 503 once the hold ends.
    pub async fn serve(self, error_body: impl FnOnce(u16) -> Value) -> Response {
        match self {
            Self::Status { status, headers } => {
                let code =
                    StatusCode::from_u16(status).expect("a scripted status is a valid HTTP status");
                let mut response = (code, Json(error_body(status))).into_response();
                for (name, value) in headers {
                    response.headers_mut().insert(
                        HeaderName::from_bytes(name.as_bytes()).expect("a valid header name"),
                        HeaderValue::from_str(&value).expect("a valid header value"),
                    );
                }
                response
            }
            Self::Stall(held_for) => {
                tokio::time::sleep(held_for).await;
                (StatusCode::SERVICE_UNAVAILABLE, Json(error_body(503))).into_response()
            }
        }
    }
}
