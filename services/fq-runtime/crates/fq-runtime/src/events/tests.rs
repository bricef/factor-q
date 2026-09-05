//! The event-vocabulary tests that need the runtime: one guards the
//! projection from [`crate::llm::LlmError`], the other holds the wire
//! spellings in lockstep with the `report_outcome` tool schema. Every
//! other test of these shapes travels with them, in `fq-ops`.

use super::*;

/// #125 wire compat: a pre-task_status `completed` payload (no
/// field on the wire) deserializes to `Success` — undeclared runs
/// and historical events read exactly as before.
#[test]
fn completed_payload_without_task_status_defaults_to_success() {
    let old_wire = serde_json::json!({
        "result_summary": "done",
        "total_llm_calls": 3,
        "total_tool_calls": 2,
        "total_cost": 0.01,
        "total_duration_ms": 1000
    });
    let p: CompletedPayload = serde_json::from_value(old_wire).unwrap();
    assert_eq!(p.task_status, TaskStatus::Success);
    // And the declared spellings match the fq-tools schema enum.
    for s in fq_tools::builtin::TASK_STATUS_VALUES {
        assert!(
            TaskStatus::parse(s).is_some(),
            "schema value {s} must parse"
        );
    }
}

/// The kind is a projection of `LlmError`, so the mapping is the one
/// place the two can drift. `EmptyResponse` is deliberately not
/// reachable from any error — the runner sets it, because the error it
/// synthesises for an empty completion is a `RequestFailed` and would
/// otherwise be indistinguishable from a transport failure.
#[test]
fn error_kind_mirrors_the_llm_error() {
    use crate::llm::LlmError;
    let cases = [
        (LlmError::Auth("x".into()), LlmErrorKind::Auth),
        (
            LlmError::RateLimited {
                model: "x".into(),
                retry_after: None,
            },
            LlmErrorKind::RateLimited,
        ),
        (
            LlmError::InvalidResponse("x".into()),
            LlmErrorKind::InvalidResponse,
        ),
        (LlmError::Rejected("x".into()), LlmErrorKind::Rejected),
        (
            LlmError::RequestFailed("x".into()),
            LlmErrorKind::RequestFailed,
        ),
        (
            LlmError::Timeout {
                budget: std::time::Duration::from_secs(1),
            },
            LlmErrorKind::Timeout,
        ),
        (
            LlmError::UnpricedModel("x".into()),
            LlmErrorKind::UnpricedModel,
        ),
    ];
    for (err, expected) in &cases {
        assert_eq!(LlmErrorKind::from(err), *expected);
    }
    assert_eq!(
        serde_json::to_value(LlmErrorKind::EmptyResponse).unwrap(),
        serde_json::json!("empty_response")
    );
    // The two kinds #546 added, spelled as the schema doc has them.
    assert_eq!(
        serde_json::to_value(LlmErrorKind::Rejected).unwrap(),
        serde_json::json!("rejected")
    );
    assert_eq!(
        serde_json::to_value(LlmErrorKind::Timeout).unwrap(),
        serde_json::json!("timeout")
    );
}
