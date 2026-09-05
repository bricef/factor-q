//! What a failed provider call means to the runtime.
//!
//! genai reports a failed call in several shapes — a `WebModelCall` or
//! `WebAdapterCall` around a `webc::Error`, a bare `HttpError`, a
//! resolver failure, a request-validation error — and the runtime wants
//! one question answered: can the same request succeed if sent again,
//! and if the provider said how long to wait, how long? The mapping is
//! keyed on the HTTP status where there is one, because that is what the
//! provider actually said, and on the transport error where there is not
//! (#546, #278).

use std::time::Duration;

use reqwest::header::HeaderMap;

use super::provider;
use crate::llm::LlmError;

/// Map a failed `exec_chat` onto [`LlmError`]. `model` is the model the
/// request was for; `budget` is the request deadline, named on a
/// [`LlmError::Timeout`] so the failure event says what it was.
///
/// By status, when the provider answered:
///
/// | status | error | retried |
/// |---|---|---|
/// | 429 | `RateLimited`, carrying `Retry-After` when sent | yes, honouring the header |
/// | 401, 403 | `Auth` | no |
/// | other 4xx | `Rejected` | no |
/// | 5xx | `RequestFailed` | yes |
///
/// Without one: a missing or refused credential is `Auth`; the HTTP
/// client's connect or request timeout is `Timeout`; everything else —
/// a connection that failed or dropped, a body that was not JSON, a
/// library error — is `RequestFailed`, which the retry layer tries
/// again. That catch-all is transient on purpose: the request was
/// well-formed enough to send, so a fresh attempt is cheap and usually
/// enough, and the alternative is guessing at the library's internals.
pub(super) fn map_error(model: &str, budget: Duration, err: provider::Error) -> LlmError {
    let message = err.to_string();
    if let Some(status) = err.status() {
        return match status.as_u16() {
            429 => LlmError::RateLimited {
                model: model.to_string(),
                retry_after: retry_after(&err),
            },
            401 | 403 => LlmError::Auth(message),
            400..=499 => LlmError::Rejected(message),
            _ => LlmError::RequestFailed(message),
        };
    }
    match &err {
        provider::Error::RequiresApiKey { .. }
        | provider::Error::NoAuthResolver { .. }
        | provider::Error::NoAuthData { .. }
        | provider::Error::Resolver { .. } => LlmError::Auth(message),
        provider::Error::WebModelCall { webc_error, .. }
        | provider::Error::WebAdapterCall { webc_error, .. }
            if timed_out(webc_error) =>
        {
            LlmError::Timeout { budget }
        }
        _ => LlmError::RequestFailed(message),
    }
}

/// Whether the HTTP client gave up on the call — its connect or total
/// timeout, both set from [`crate::llm::LlmTimeouts`] when the client
/// is built.
fn timed_out(webc_error: &provider::webc::Error) -> bool {
    matches!(webc_error, provider::webc::Error::Reqwest(err) if err.is_timeout())
}

/// The wait the provider asked for on a failed response, when it sent
/// one the runtime can read. The headers ride on the error genai returns
/// for every shape a failed chat call takes, and the variants' fields
/// are public, so nothing upstream needs to change to reach them.
fn retry_after(err: &provider::Error) -> Option<Duration> {
    use provider::webc::Error::ResponseFailedStatus;
    let headers = match err {
        provider::Error::HttpError { headers, .. } => headers,
        provider::Error::WebModelCall {
            webc_error: ResponseFailedStatus { headers, .. },
            ..
        }
        | provider::Error::WebAdapterCall {
            webc_error: ResponseFailedStatus { headers, .. },
            ..
        } => headers,
        _ => return None,
    };
    parse_retry_after(headers)
}

/// `Retry-After` as RFC 9110 §10.2.3 defines it — a number of seconds or
/// an HTTP-date — plus `retry-after-ms`, which OpenAI's servers send and
/// its own SDK reads first. A date already past reads as "now". An
/// unreadable value reads as absent, and the retry layer then backs off
/// on its own schedule, exactly as if the provider had said nothing.
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let text = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
    };
    if let Some(millis) = text("retry-after-ms").and_then(|value| value.parse::<u64>().ok()) {
        return Some(Duration::from_millis(millis));
    }
    let value = text("retry-after")?;
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    let wait = at.with_timezone(&chrono::Utc) - chrono::Utc::now();
    Some(wait.to_std().unwrap_or(Duration::ZERO))
}

#[cfg(test)]
mod tests;
