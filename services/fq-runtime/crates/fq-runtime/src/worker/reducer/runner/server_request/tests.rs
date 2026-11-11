//! The pure end of ADR-0018 servicing: verdict mapping, elicitation
//! schema validation, and the model request a sampling request becomes
//! (its `maxTokens` clamp included, #831), checked without a broker.
//!
//! Moved with their code out of `runner/tests.rs` (#78). The
//! broker-backed `handle_sampling` tests stayed there on purpose — they
//! drive a live runner through the `sampling_world` / `sampling_runner`
//! fixtures shared with the rest of that file, and dragging those over
//! would have made this move about test plumbing rather than the seam.
//! The request-cap tests that need a live runner sit beside them for the
//! same reason.

use super::*;
use crate::worker::reducer::harness::DEFAULT_MAX_TOKENS;
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

/// The model request a sampling request runs as: the server's system
/// prompt first, each turn as its role's message (an assistant turn a
/// single text part), the temperature widened to `f64`, `maxTokens` as
/// the cap when it is under the agent's own, and no tools — the same
/// request the runner built from the rmcp params.
#[test]
fn sampling_request_runs_as_the_same_model_request() {
    let request = crate::mcp::sampling_request_from_wire(json!({
        "messages": [
            { "role": "user", "content": { "type": "text", "text": "hello" } },
            { "role": "assistant", "content": [
                { "type": "text", "text": "partial" },
                { "type": "image", "data": "aGk=", "mimeType": "image/png" }
            ] }
        ],
        "systemPrompt": "Be brief.",
        "temperature": 0.5,
        "maxTokens": 32
    }))
    .expect("sampling params");

    let built = sampling_to_model_request("claude-haiku", &request, DEFAULT_MAX_TOKENS);

    let expected = ModelRequest {
        model: "claude-haiku".to_string(),
        messages: vec![
            Message::System {
                text: "Be brief.".to_string(),
            },
            Message::User {
                text: "hello".to_string(),
            },
            Message::Assistant {
                parts: vec![AssistantPart::Text {
                    text: "partial\n[non-text sampling content omitted]".to_string(),
                }],
            },
        ],
        tools: Vec::new(),
        params: RequestParams {
            effort: None,
            temperature: Some(0.5),
            max_tokens: Some(32),
        },
    };
    assert_eq!(
        serde_json::to_value(&built).expect("request serializes"),
        serde_json::to_value(&expected).expect("request serializes")
    );
}

/// No system prompt and no temperature: no system message, and the
/// temperature left to the provider.
#[test]
fn a_bare_sampling_request_adds_no_system_message() {
    let request = crate::mcp::sampling_request_from_wire(json!({
        "messages": [{ "role": "user", "content": { "type": "text", "text": "hi" } }],
        "maxTokens": 8
    }))
    .expect("sampling params");

    let built = sampling_to_model_request("m", &request, DEFAULT_MAX_TOKENS);

    assert_eq!(built.messages.len(), 1);
    assert!(matches!(&built.messages[0], Message::User { text } if text == "hi"));
    assert_eq!(built.params.temperature, None);
    assert_eq!(built.params.max_tokens, Some(8));
    assert!(built.tools.is_empty());
}

/// #831: a server's `maxTokens` above the agent's own output cap is
/// clamped to it — a server never gets a longer completion than the
/// agent's turns do — and one at or below it is left alone.
#[test]
fn the_servers_max_tokens_is_clamped_to_the_agents_own() {
    let asking = |max_tokens: u32| {
        crate::mcp::sampling_request_from_wire(json!({
            "messages": [{ "role": "user", "content": { "type": "text", "text": "hi" } }],
            "maxTokens": max_tokens
        }))
        .expect("sampling params")
    };

    let clamped = sampling_to_model_request("m", &asking(100_000), DEFAULT_MAX_TOKENS);
    assert_eq!(clamped.params.max_tokens, Some(DEFAULT_MAX_TOKENS));

    let at_cap = sampling_to_model_request("m", &asking(512), 512);
    assert_eq!(at_cap.params.max_tokens, Some(512));
    let under = sampling_to_model_request("m", &asking(100_000), 512);
    assert_eq!(under.params.max_tokens, Some(512), "an agent's own cap");
}

/// A model response becomes the reply the handler returns: its text
/// (empty when the model said nothing), the agent's model, and the stop
/// reason under its protocol name.
#[test]
fn model_response_becomes_the_sampling_reply() {
    let cases = [
        (StopReason::EndTurn, SamplingStopReason::EndTurn),
        (StopReason::MaxTokens, SamplingStopReason::MaxTokens),
        (StopReason::StopSequence, SamplingStopReason::StopSequence),
        (StopReason::ToolUse, SamplingStopReason::ToolUse),
    ];
    for (stop_reason, expected) in cases {
        let response = ModelResponse {
            parts: vec![AssistantPart::Text {
                text: "pong".to_string(),
            }],
            stop_reason,
            usage: crate::events::TokenUsage::default(),
        };
        assert_eq!(
            model_response_to_sampling_reply("claude-haiku", response),
            SamplingReply {
                text: "pong".to_string(),
                model: "claude-haiku".to_string(),
                stop_reason: expected,
            }
        );
    }

    let silent = ModelResponse {
        parts: Vec::new(),
        stop_reason: StopReason::EndTurn,
        usage: crate::events::TokenUsage::default(),
    };
    assert_eq!(model_response_to_sampling_reply("m", silent).text, "");
}
