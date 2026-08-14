//! The pure end of ADR-0018 servicing: verdict mapping and elicitation
//! schema validation, checked without a broker.
//!
//! Moved with their code out of `runner/tests.rs` (#78). The
//! broker-backed `handle_sampling` tests stayed there on purpose — they
//! drive a live runner through the `sampling_world` / `sampling_runner`
//! fixtures shared with the rest of that file, and dragging those over
//! would have made this move about test plumbing rather than the seam.

use super::*;
use serde_json::json;

#[test]
fn evaluator_verdict_maps_outcomes() {
    assert!(matches!(
        evaluator_verdict(Some(json!({ "approved": true }))),
        EvaluatorOutcome::Approved
    ));
    match evaluator_verdict(Some(json!({ "approved": false, "reason": "nope" }))) {
        EvaluatorOutcome::Denied(reason) => assert_eq!(reason, "nope"),
        EvaluatorOutcome::Approved => panic!("expected denied"),
    }
    // A missing / unparseable verdict fails closed (denies).
    assert!(matches!(
        evaluator_verdict(None),
        EvaluatorOutcome::Denied(_)
    ));
}

#[test]
fn elicitation_schema_validation_enforces_per_field_rules() {
    let schema: ElicitationSchema = serde_json::from_value(json!({
        "type": "object",
        "properties": {
            "name": { "type": "string", "minLength": 2 },
            "age": { "type": "integer", "minimum": 0, "maximum": 150 },
            "email": { "type": "string", "format": "email" },
            "color": { "type": "string", "enum": ["red", "green"] }
        },
        "required": ["name"]
    }))
    .expect("valid elicitation schema");

    let ok = |v: serde_json::Value| validate_against_elicitation_schema(&v, &schema).is_ok();
    let err = |v: serde_json::Value| validate_against_elicitation_schema(&v, &schema).is_err();

    assert!(ok(
        json!({ "name": "Ada", "age": 30, "email": "ada@example.com", "color": "red" })
    ));
    assert!(err(json!({ "age": 30 })), "missing required name");
    assert!(err(json!({ "name": 5 })), "wrong type");
    assert!(err(json!({ "name": "A" })), "below minLength");
    assert!(err(json!({ "name": "Ada", "age": 999 })), "above maximum");
    assert!(err(json!({ "name": "Ada", "age": 1.5 })), "non-integer");
    assert!(err(json!({ "name": "Ada", "email": "nope" })), "bad email");
    assert!(err(json!({ "name": "Ada", "color": "blue" })), "bad enum");
    assert!(
        err(json!({ "name": "Ada", "extra": 1 })),
        "unexpected field"
    );
}
