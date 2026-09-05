//! The status-driven mapping, shape by shape, with no HTTP involved:
//! the errors are built the way genai builds them. The wire — a real
//! 429, a real stall — is covered against the mock servers in
//! `test_support::mock_anthropic` and `test_support::mock_openai`.

use super::*;
use provider::ModelIden;
use provider::adapter::AdapterKind;
use provider::webc::Error::{ResponseFailedNotJson, ResponseFailedStatus};
use reqwest::StatusCode;
use reqwest::header::{HeaderName, HeaderValue};

const BUDGET: Duration = Duration::from_secs(600);

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.append(
            HeaderName::from_bytes(name.as_bytes()).expect("a valid header name"),
            HeaderValue::from_str(value).expect("a valid header value"),
        );
    }
    map
}

fn model_iden() -> ModelIden {
    ModelIden::new(AdapterKind::Anthropic, "claude-test")
}

/// The shape a failed non-streaming chat call arrives in.
fn model_call_failed(status: u16, headers: HeaderMap) -> provider::Error {
    provider::Error::WebModelCall {
        model_iden: model_iden(),
        webc_error: ResponseFailedStatus {
            status: StatusCode::from_u16(status).expect("a valid status"),
            body: format!("{{\"error\":\"scripted {status}\"}}"),
            headers: Box::new(headers),
        },
    }
}

fn map(err: provider::Error) -> LlmError {
    map_error("claude-test", BUDGET, err)
}

#[test]
fn a_429_is_rate_limited_and_carries_the_wait_the_provider_asked_for() {
    let err = map(model_call_failed(429, headers(&[("retry-after", "2")])));
    match &err {
        LlmError::RateLimited { model, retry_after } => {
            assert_eq!(model, "claude-test");
            assert_eq!(*retry_after, Some(Duration::from_secs(2)));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
    assert!(err.is_transient());
}

#[test]
fn a_429_without_the_header_is_rate_limited_with_no_wait() {
    let err = map(model_call_failed(429, HeaderMap::new()));
    assert!(
        matches!(
            &err,
            LlmError::RateLimited {
                retry_after: None,
                ..
            }
        ),
        "got {err:?}"
    );
    assert!(err.is_transient());
}

/// The header rides on every shape genai reports a failed call in.
#[test]
fn the_header_is_read_on_every_shape_a_failed_call_takes() {
    let adapter_call = provider::Error::WebAdapterCall {
        adapter_kind: AdapterKind::OpenAI,
        webc_error: ResponseFailedStatus {
            status: StatusCode::TOO_MANY_REQUESTS,
            body: String::new(),
            headers: Box::new(headers(&[("retry-after", "3")])),
        },
    };
    let bare_http = provider::Error::HttpError {
        status: StatusCode::TOO_MANY_REQUESTS,
        canonical_reason: "Too Many Requests".to_string(),
        body: String::new(),
        headers: Box::new(headers(&[("retry-after", "4")])),
    };
    for (err, expected) in [(adapter_call, 3), (bare_http, 4)] {
        assert!(
            matches!(
                map(err),
                LlmError::RateLimited { retry_after: Some(wait), .. }
                    if wait == Duration::from_secs(expected)
            ),
            "expected a {expected}s wait"
        );
    }
}

#[test]
fn the_auth_statuses_are_auth() {
    for status in [401, 403] {
        let err = map(model_call_failed(status, HeaderMap::new()));
        assert!(matches!(&err, LlmError::Auth(_)), "{status}: got {err:?}");
        assert!(!err.is_transient(), "{status} is not retried");
    }
}

#[test]
fn every_other_client_error_is_rejected_and_permanent() {
    for status in [400, 402, 404, 413, 422] {
        let err = map(model_call_failed(status, HeaderMap::new()));
        assert!(
            matches!(&err, LlmError::Rejected(message) if message.contains(&status.to_string())),
            "{status}: got {err:?}"
        );
        assert!(!err.is_transient(), "{status} is not retried");
    }
}

#[test]
fn server_errors_are_transient_request_failures() {
    for status in [500, 502, 503, 529] {
        let err = map(model_call_failed(status, HeaderMap::new()));
        assert!(
            matches!(&err, LlmError::RequestFailed(message) if message.contains(&status.to_string())),
            "{status}: got {err:?}"
        );
        assert!(err.is_transient(), "{status} is retried");
    }
}

#[test]
fn missing_credentials_are_auth() {
    let err = map(provider::Error::NoAuthData {
        model_iden: model_iden(),
    });
    assert!(matches!(&err, LlmError::Auth(_)), "got {err:?}");
    assert!(!err.is_transient());
}

/// The catch-all: a failure with no status and no timeout behind it is
/// retried, because sending again is cheap and the alternative is
/// guessing at the library's internals.
#[test]
fn a_failure_without_a_status_is_a_transient_request_failure() {
    let not_json = provider::Error::WebModelCall {
        model_iden: model_iden(),
        webc_error: ResponseFailedNotJson {
            content_type: "text/html".to_string(),
            body: "<html>gateway</html>".to_string(),
        },
    };
    let no_messages = provider::Error::ChatReqHasNoMessages {
        model_iden: model_iden(),
    };
    for err in [not_json, no_messages] {
        let mapped = map(err);
        assert!(
            matches!(&mapped, LlmError::RequestFailed(_)),
            "got {mapped:?}"
        );
        assert!(mapped.is_transient());
    }
}

#[test]
fn retry_after_reads_seconds_dates_and_milliseconds() {
    assert_eq!(
        parse_retry_after(&headers(&[("retry-after", "120")])),
        Some(Duration::from_secs(120))
    );
    assert_eq!(
        parse_retry_after(&headers(&[("retry-after", " 7 ")])),
        Some(Duration::from_secs(7)),
        "surrounding whitespace is not part of the value"
    );

    // An HTTP-date three seconds out reads as roughly three seconds.
    let at = chrono::Utc::now() + chrono::TimeDelta::seconds(3);
    let date = at.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    let wait = parse_retry_after(&headers(&[("retry-after", &date)])).expect("a date parses");
    assert!(
        wait > Duration::from_secs(1) && wait <= Duration::from_secs(3),
        "expected about three seconds, got {wait:?}"
    );

    // `retry-after-ms` wins over `retry-after`, as OpenAI's SDK has it.
    assert_eq!(
        parse_retry_after(&headers(&[("retry-after-ms", "250"), ("retry-after", "5")])),
        Some(Duration::from_millis(250))
    );
}

#[test]
fn a_date_already_past_reads_as_now() {
    let at = chrono::Utc::now() - chrono::TimeDelta::seconds(10);
    let date = at.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    assert_eq!(
        parse_retry_after(&headers(&[("retry-after", &date)])),
        Some(Duration::ZERO)
    );
}

#[test]
fn an_unreadable_retry_after_reads_as_absent() {
    assert_eq!(parse_retry_after(&HeaderMap::new()), None);
    for value in ["soon", "-3", "1.5", ""] {
        assert_eq!(
            parse_retry_after(&headers(&[("retry-after", value)])),
            None,
            "{value:?} is not a delay the runtime can act on"
        );
    }
}
