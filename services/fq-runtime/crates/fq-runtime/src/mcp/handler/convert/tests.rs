//! The boundary conversions reproduce what the runtime built from rmcp's
//! types before factor-q owned them: the same wire values back to the
//! server, and the same strings forward to the runner.

use super::*;
use serde_json::json;

use crate::mcp::elicitation_decline;

fn sampling_params(value: serde_json::Value) -> CreateMessageRequestParams {
    serde_json::from_value(value).expect("valid sampling params")
}

/// `as_sent` is the params serialized exactly as before — every field
/// the runtime does not otherwise read (`_meta`, `includeContext`,
/// `modelPreferences`, `stopSequences`, `metadata`) included, because an
/// input evaluator judges the whole request.
#[test]
fn sampling_as_sent_is_the_serialized_params() {
    let params = sampling_params(json!({
        "_meta": { "trace": "abc" },
        "messages": [
            { "role": "user", "content": { "type": "text", "text": "hello" } },
            { "role": "assistant", "content": { "type": "text", "text": "hi" } }
        ],
        "modelPreferences": { "hints": [{ "name": "claude" }], "speedPriority": 0.5 },
        "systemPrompt": "You are a helpful test server.",
        "includeContext": "none",
        "temperature": 0.7,
        "maxTokens": 64,
        "stopSequences": ["STOP"],
        "metadata": { "k": [1, 2] }
    }));
    let expected = serde_json::to_string(&params).expect("params serialize");

    let request = sampling_request(params);

    assert_eq!(request.as_sent, expected);
    assert_eq!(
        request.system_prompt.as_deref(),
        Some("You are a helpful test server.")
    );
    assert_eq!(request.temperature, Some(0.7_f32));
    assert_eq!(request.max_tokens, 64);
    assert_eq!(
        request.turns,
        vec![
            SamplingTurn {
                role: SamplingRole::User,
                text: "hello".to_string(),
            },
            SamplingTurn {
                role: SamplingRole::Assistant,
                text: "hi".to_string(),
            },
        ]
    );
}

/// The optional knobs stay absent when the server sends none.
#[test]
fn sampling_request_without_optional_fields() {
    let params = sampling_params(json!({ "messages": [], "maxTokens": 8 }));
    let expected = serde_json::to_string(&params).expect("params serialize");

    let request = sampling_request(params);

    assert_eq!(request.as_sent, expected);
    assert!(request.turns.is_empty());
    assert_eq!(request.system_prompt, None);
    assert_eq!(request.temperature, None);
    assert_eq!(request.max_tokens, 8);
}

/// A turn's text is the flattening the runner applied before: text
/// blocks verbatim, several blocks newline-joined, and any non-text
/// block (image, audio, tool use, tool result) a fixed placeholder.
#[test]
fn sampling_turn_text_flattens_content_as_before() {
    const OMITTED: &str = "[non-text sampling content omitted]";
    let params = sampling_params(json!({
        "messages": [
            { "role": "user", "content": { "type": "text", "text": "single" } },
            { "role": "user", "content": { "type": "image", "data": "aGk=", "mimeType": "image/png" } },
            { "role": "user", "content": [
                { "type": "text", "text": "first" },
                { "type": "audio", "data": "aGk=", "mimeType": "audio/wav" },
                { "type": "text", "text": "last" }
            ] },
            { "role": "assistant", "content": [
                { "type": "tool_use", "id": "t1", "name": "lookup", "input": {} }
            ] },
            { "role": "user", "content": [
                { "type": "tool_result", "toolUseId": "t1", "content": [] }
            ] },
            { "role": "assistant", "content": [] }
        ],
        "maxTokens": 8
    }));

    let texts: Vec<(SamplingRole, String)> = sampling_request(params)
        .turns
        .into_iter()
        .map(|turn| (turn.role, turn.text))
        .collect();

    assert_eq!(
        texts,
        vec![
            (SamplingRole::User, "single".to_string()),
            (SamplingRole::User, OMITTED.to_string()),
            (SamplingRole::User, format!("first\n{OMITTED}\nlast")),
            (SamplingRole::Assistant, OMITTED.to_string()),
            (SamplingRole::User, OMITTED.to_string()),
            (SamplingRole::Assistant, String::new()),
        ]
    );
}

/// For every stop reason, the reply converts to exactly the
/// `CreateMessageResult` the runner used to build:
/// `CreateMessageResult::new(SamplingMessage::assistant_text(text), model)
/// .with_stop_reason(<the protocol constant>)`.
#[test]
fn sampling_reply_converts_to_the_old_create_message_result() {
    let cases = [
        (
            SamplingStopReason::EndTurn,
            CreateMessageResult::STOP_REASON_END_TURN,
            "endTurn",
        ),
        (
            SamplingStopReason::MaxTokens,
            CreateMessageResult::STOP_REASON_END_MAX_TOKEN,
            "maxTokens",
        ),
        (
            SamplingStopReason::StopSequence,
            CreateMessageResult::STOP_REASON_END_SEQUENCE,
            "stopSequence",
        ),
        (
            SamplingStopReason::ToolUse,
            CreateMessageResult::STOP_REASON_TOOL_USE,
            "toolUse",
        ),
    ];
    for (stop_reason, constant, wire_name) in cases {
        let reply = SamplingReply {
            text: "pong".to_string(),
            model: "claude-haiku".to_string(),
            stop_reason,
        };

        let converted = CreateMessageResult::from(reply);

        let old = CreateMessageResult::new(
            SamplingMessage::assistant_text("pong"),
            "claude-haiku".to_string(),
        )
        .with_stop_reason(constant);
        assert_eq!(converted, old, "{stop_reason:?}");
        assert_eq!(
            serde_json::to_value(&converted).expect("result serializes"),
            json!({
                "model": "claude-haiku",
                "stopReason": wire_name,
                "role": "assistant",
                "content": { "type": "text", "text": "pong" }
            }),
            "{stop_reason:?}"
        );
    }
}

/// An empty completion is still one (empty) assistant text block, as
/// `response.text().unwrap_or_default()` produced before.
#[test]
fn an_empty_sampling_reply_is_an_empty_text_block() {
    let converted = CreateMessageResult::from(SamplingReply {
        text: String::new(),
        model: "m".to_string(),
        stop_reason: SamplingStopReason::EndTurn,
    });
    assert_eq!(
        converted,
        CreateMessageResult::new(SamplingMessage::assistant_text(""), "m".to_string())
            .with_stop_reason(CreateMessageResult::STOP_REASON_END_TURN)
    );
}

/// The decline the runner and the handler answer with is
/// `{action: decline, content: None, meta: None}` on the wire.
#[test]
fn elicitation_decline_maps_to_a_bare_decline() {
    let wire = CreateElicitationResult::from(elicitation_decline());
    assert_eq!(wire.action, ElicitationAction::Decline);
    assert_eq!(wire.content, None);
    assert_eq!(wire.meta, None);
    assert_eq!(
        serde_json::to_value(&wire).expect("result serializes"),
        json!({ "action": "decline" })
    );
}

/// An accepted reply carries its content and, like every reply, no
/// `_meta`; each outcome maps to its protocol action.
#[test]
fn elicitation_reply_maps_action_and_content() {
    let accepted = CreateElicitationResult::from(ElicitationReply {
        action: ElicitationOutcome::Accept,
        content: Some(json!({ "city": "Lisbon" })),
    });
    assert_eq!(accepted.action, ElicitationAction::Accept);
    assert_eq!(accepted.content, Some(json!({ "city": "Lisbon" })));
    assert_eq!(accepted.meta, None);
    assert_eq!(
        serde_json::to_value(&accepted).expect("result serializes"),
        json!({ "action": "accept", "content": { "city": "Lisbon" } })
    );

    let cancelled = CreateElicitationResult::from(ElicitationReply {
        action: ElicitationOutcome::Cancel,
        content: None,
    });
    assert_eq!(cancelled.action, ElicitationAction::Cancel);
    assert_eq!(cancelled.content, None);
    assert_eq!(cancelled.meta, None);
}

/// Form mode keeps its message and schema; `as_sent` is the params
/// serialized as they arrived (including a mode-less legacy request,
/// which rmcp reads as form mode).
#[test]
fn elicitation_form_request_carries_message_schema_and_as_sent() {
    for value in [
        json!({
            "mode": "form",
            "_meta": { "trace": "abc" },
            "message": "Which city?",
            "requestedSchema": {
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"]
            }
        }),
        json!({
            "message": "Which city?",
            "requestedSchema": {
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"]
            }
        }),
    ] {
        let params: CreateElicitationRequestParams =
            serde_json::from_value(value).expect("valid elicitation params");
        let expected_as_sent = serde_json::to_string(&params).expect("params serialize");
        let CreateElicitationRequestParams::FormElicitationParams {
            requested_schema: expected_schema,
            ..
        } = params.clone()
        else {
            panic!("expected form-mode params");
        };

        let request = elicitation_request(params);

        assert_eq!(request.as_sent(), expected_as_sent);
        let ElicitationRequest::Form {
            message,
            requested_schema,
            as_sent,
        } = request
        else {
            panic!("form-mode params convert to the form variant");
        };
        assert_eq!(message, "Which city?");
        assert_eq!(requested_schema, expected_schema);
        assert_eq!(as_sent, expected_as_sent);
    }
}

/// URL mode is represented, not dropped: it still has to reach the
/// runner's gates and input evaluators before it is declined.
#[test]
fn elicitation_url_request_is_represented() {
    let params: CreateElicitationRequestParams = serde_json::from_value(json!({
        "mode": "url",
        "message": "Sign in here",
        "url": "https://example.com/login",
        "elicitationId": "e-1"
    }))
    .expect("valid elicitation params");
    let expected_as_sent = serde_json::to_string(&params).expect("params serialize");

    let request = elicitation_request(params);

    assert_eq!(
        request,
        ElicitationRequest::Url {
            message: "Sign in here".to_string(),
            url: "https://example.com/login".to_string(),
            as_sent: expected_as_sent,
        }
    );
}

/// A root converts with `Root::new(uri).with_name(name)` semantics: a
/// name when there is one, none when there is not.
#[test]
fn workspace_root_converts_to_root() {
    let named = Root::from(WorkspaceRoot::new("file:///data").with_name("/data"));
    assert_eq!(named, Root::new("file:///data").with_name("/data"));
    assert_eq!(
        serde_json::to_value(&named).expect("root serializes"),
        json!({ "uri": "file:///data", "name": "/data" })
    );

    let unnamed = Root::from(WorkspaceRoot::new("file:///data"));
    assert_eq!(unnamed, Root::new("file:///data"));
    assert_eq!(
        serde_json::to_value(&unnamed).expect("root serializes"),
        json!({ "uri": "file:///data" })
    );
}

/// Every level maps to its namesake in both directions, and its name is
/// the protocol's spelling of it.
#[test]
fn log_levels_map_one_to_one() {
    let pairs = [
        (McpLogLevel::Debug, LoggingLevel::Debug),
        (McpLogLevel::Info, LoggingLevel::Info),
        (McpLogLevel::Notice, LoggingLevel::Notice),
        (McpLogLevel::Warning, LoggingLevel::Warning),
        (McpLogLevel::Error, LoggingLevel::Error),
        (McpLogLevel::Critical, LoggingLevel::Critical),
        (McpLogLevel::Alert, LoggingLevel::Alert),
        (McpLogLevel::Emergency, LoggingLevel::Emergency),
    ];
    for (owned, wire) in pairs {
        assert_eq!(LoggingLevel::from(owned), wire, "{owned:?}");
        assert_eq!(McpLogLevel::from(wire), owned, "{wire:?}");
        assert_eq!(
            serde_json::to_value(wire).expect("level serializes"),
            json!(owned.as_str()),
            "{owned:?}"
        );
    }
}
