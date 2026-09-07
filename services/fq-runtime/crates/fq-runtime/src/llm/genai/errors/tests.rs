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

/// A 408 is the server's own timeout, not a verdict on the request —
/// so it is retried like a 5xx, not refused like the other 4xx.
#[test]
fn a_408_is_a_transient_request_failure() {
    let err = map(model_call_failed(408, HeaderMap::new()));
    assert!(matches!(&err, LlmError::RequestFailed(_)), "got {err:?}");
    assert!(err.is_transient(), "408 is retried");
}

/// Google sends its wait in the body, not a header: the 429 Gemini
/// answered on 2026-09-07, trimmed to the parts that matter.
const GOOGLE_429: &str = r#"{"error":{"code":429,"message":"You exceeded your current quota, please check your plan and billing details.\n* Quota exceeded for metric: generativelanguage.googleapis.com/generate_content_free_tier_requests, limit: 5, model: gemini-3-flash\nPlease retry in 11.472599491s.","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.Help","links":[{"description":"Learn more about Gemini API quotas","url":"https://ai.google.dev/gemini-api/docs/rate-limits"}]},{"@type":"type.googleapis.com/google.rpc.QuotaFailure","violations":[{"quotaMetric":"generativelanguage.googleapis.com/generate_content_free_tier_requests","quotaId":"GenerateRequestsPerMinutePerProjectPerModel-FreeTier","quotaDimensions":{"location":"global","model":"gemini-3-flash"},"quotaValue":"5"}]},{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"11s"}]}}"#;

/// A failed call whose body is the provider's own, not the scripted stub.
fn model_call_failed_with_body(status: u16, headers: HeaderMap, body: &str) -> provider::Error {
    provider::Error::WebModelCall {
        model_iden: model_iden(),
        webc_error: ResponseFailedStatus {
            status: StatusCode::from_u16(status).expect("a valid status"),
            body: body.to_string(),
            headers: Box::new(headers),
        },
    }
}

fn a_retry_delay_body(delay: &str) -> String {
    format!(
        r#"{{"error":{{"code":429,"status":"RESOURCE_EXHAUSTED","details":[{{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":{delay}}}]}}}}"#
    )
}

#[test]
fn a_google_429_without_the_header_carries_the_wait_from_the_body() {
    let err = map(model_call_failed_with_body(
        429,
        HeaderMap::new(),
        GOOGLE_429,
    ));
    match &err {
        LlmError::RateLimited { model, retry_after } => {
            assert_eq!(model, "claude-test");
            assert_eq!(*retry_after, Some(Duration::from_secs(11)));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
    assert!(err.is_transient());
}

#[test]
fn the_header_is_believed_over_the_body() {
    let err = map(model_call_failed_with_body(
        429,
        headers(&[("retry-after", "3")]),
        GOOGLE_429,
    ));
    assert!(
        matches!(
            &err,
            LlmError::RateLimited { retry_after: Some(wait), .. }
                if *wait == Duration::from_secs(3)
        ),
        "got {err:?}"
    );
}

/// The body rides on the same three shapes the header does.
#[test]
fn the_body_delay_is_read_on_every_shape_a_failed_call_takes() {
    let adapter_call = provider::Error::WebAdapterCall {
        adapter_kind: AdapterKind::Gemini,
        webc_error: ResponseFailedStatus {
            status: StatusCode::TOO_MANY_REQUESTS,
            body: GOOGLE_429.to_string(),
            headers: Box::new(HeaderMap::new()),
        },
    };
    let bare_http = provider::Error::HttpError {
        status: StatusCode::TOO_MANY_REQUESTS,
        canonical_reason: "Too Many Requests".to_string(),
        body: GOOGLE_429.to_string(),
        headers: Box::new(HeaderMap::new()),
    };
    for err in [adapter_call, bare_http] {
        assert!(
            matches!(
                map(err),
                LlmError::RateLimited { retry_after: Some(wait), .. }
                    if wait == Duration::from_secs(11)
            ),
            "expected the body's 11s wait"
        );
    }
}

#[test]
fn a_body_retry_delay_is_a_protobuf_duration() {
    assert_eq!(
        parse_retry_delay(&a_retry_delay_body("\"42s\"")),
        Some(Duration::from_secs(42))
    );
    assert_eq!(
        parse_retry_delay(&a_retry_delay_body("\"11.472599491s\"")),
        Some(Duration::from_secs_f64(11.472599491)),
        "fractional seconds are kept"
    );
    assert_eq!(
        parse_retry_delay(&a_retry_delay_body("\"0.5s\"")),
        Some(Duration::from_millis(500))
    );
    assert_eq!(
        parse_retry_delay(&a_retry_delay_body("\" 7s \"")),
        Some(Duration::from_secs(7)),
        "surrounding whitespace is not part of the value"
    );
}

#[test]
fn an_unreadable_body_delay_reads_as_absent() {
    // Not Google's shape at all: the scripted stub, OpenAI's error object,
    // a body that is not JSON.
    for body in [
        "{\"error\":\"scripted 429\"}",
        "{\"error\":{\"message\":\"Rate limit reached\",\"type\":\"tokens\"}}",
        "rate limited",
        "",
    ] {
        assert_eq!(parse_retry_delay(body), None, "{body:?} carries no wait");
    }
    // Google's shape without a RetryInfo detail.
    let quota_only = r#"{"error":{"code":429,"details":[{"@type":"type.googleapis.com/google.rpc.QuotaFailure","violations":[]}]}}"#;
    assert_eq!(parse_retry_delay(quota_only), None);
    // A RetryInfo whose delay is not a duration the runtime can act on.
    for delay in [
        "\"soon\"",
        "\"-3s\"",
        "\"\"",
        "\"3\"",
        "{\"seconds\":3}",
        "3",
        "null",
    ] {
        assert_eq!(
            parse_retry_delay(&a_retry_delay_body(delay)),
            None,
            "{delay} is not a delay the runtime can act on"
        );
    }
}
