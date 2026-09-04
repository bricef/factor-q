//! The wire goldens for the reasoning round-trip.
//!
//! Every scenario drives a real multi-turn conversation through
//! [`GenAiClient`] against an in-process provider mock and records two
//! things per turn: the parts factor-q decoded from the provider's
//! response, and the request body factor-q then sent back. Both are
//! pinned as canonical JSON under `tests/snapshots/reasoning_wire/`.
//!
//! Why the wire and not genai's types: the dependency under this adapter
//! is allowed to change shape — the fork pinned for #437 phase 6 models a
//! thinking block as one `Custom` part, upstream `0.7.0-beta.21` models
//! it as a signature part plus a reasoning part — and none of that may
//! reach the provider or the reducer. What the provider receives and what
//! the reducer records *is* the contract. If a file here moves during a
//! dependency change, behaviour moved with it: review the diff before
//! regenerating, and say why in the commit.
//!
//! The scenarios are the shapes the 2026-09-04 live run produced (the
//! Opus 5 block is the real one, signature included) plus the ones the
//! adapter documents: redacted thinking, several blocks in one turn, a
//! signed turn that ends without a tool call, the cross-model strip, and
//! both spellings of the OpenAI-compatible reasoning field.
//!
//! Regenerate with `UPDATE_SNAPSHOT=1 cargo test -p fq-runtime wire_goldens`.

use serde_json::{Value, json};

use super::GenAiClient;
use crate::events::{Effort, Message, RequestParams, ToolSchema};
use crate::llm::{ChatRequest, ChatResponse, LlmClient};
use crate::test_support::mock_anthropic::{ContentBlock, MockAnthropicServer, MockResponse};
use crate::test_support::mock_openai::{MockChoice, MockOpenAiServer};

/// The thinking block `claude-opus-5` returned on turn 1 of the
/// 2026-09-04 live run, verbatim: empty text, real signature. The
/// shape #537 was about, and the one a hand-written fixture would not
/// have thought to include.
const OPUS5_SIGNATURE: &str = "CAIShwQKjgEIERgCKkDfDyWUdRcu3hhoNHxiG0EaIauD1yH/i9EPAgEC3UnqjYz/Bke3zp709yo4w04iNlAtF553RrQqhz9lGWd8xSC3Mg1jbGF1ZGUtb3B1cy01OAFCCHRoaW5raW5nWiQ4MjdiMDk5Yy1iMGVlLTQ1ZmEtOTE3OS1jY2Y5ZDMzODc4N2aoAbP06NQGEgxVMNRnp6gkwtgprnoaDEMfTLMDP2XfCKbprSIwmPI6XqlPHUU48y+NC5PRIEU6FlSGqfoaD1oxqfqkqfpzIWkZYeHAtvR4ewx/cxWZKqUC6EuTUIiiOiT5FyL3Y4wJhf7tgAW8H84tmFhJjkYiPwQN/32Goejv6/b+yygqJdFaiS6LcYuGQKD5cWM4sCAK94iZQpOVpW3ixALbvVoRDXm4tP84upMAzJMwIzz66NWfkfIN5FkIH+DYM9xpdX96tEjWzlE8iALjUIKB3MkOpbclpXVxqtCFEEzU4OLoiHciuFoRad1wyg0ren4Fw30vWoqsAxJpmAyIn9yM52NsRCJwYl/4gRBsnV2mjHqEjmNnXmfobhDJfk3Q3wzzKpvlYDPbqTSrv3cgLbsWbzFXFSSFxA5qCMhHho3FNYUejgkrDDekzagcmxsystLG33IDEhVn7QDGplYTBXdyhrdCaBi9nyhAdoBZpj7KlktqQZ1PLbNeY84YAQ==";

// region: harness

fn read_file_tool() -> Vec<ToolSchema> {
    vec![ToolSchema {
        name: "read_file".to_string(),
        description: "Read a file from the workspace.".to_string(),
        parameters_schema: json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        }),
    }]
}

fn params(effort: Option<Effort>) -> RequestParams {
    RequestParams {
        effort,
        temperature: None,
        max_tokens: Some(1024),
    }
}

/// What the reducer would record for this turn.
fn decoded(response: &ChatResponse) -> Value {
    json!({
        "parts": response.parts,
        "stop_reason": response.stop_reason,
        "usage": response.usage,
    })
}

async fn send(
    client: &GenAiClient,
    model: &str,
    messages: &[Message],
    tools: Vec<ToolSchema>,
    effort: Option<Effort>,
) -> ChatResponse {
    client
        .chat(ChatRequest {
            model: model.to_string(),
            messages: messages.to_vec(),
            tools,
            params: params(effort),
        })
        .await
        .expect("the mock answers every scripted turn")
}

/// Replay the assistant turn and answer its first tool call — the
/// reducer's own continuation, in miniature.
fn continue_with_tool_result(messages: &mut Vec<Message>, response: &ChatResponse, output: &str) {
    let call_id = response
        .tool_calls()
        .first()
        .expect("the scripted turn requested a tool")
        .tool_call_id
        .clone();
    messages.push(Message::Assistant {
        parts: response.parts.clone(),
    });
    messages.push(Message::tool_result(call_id, output));
}

async fn anthropic_mock() -> MockAnthropicServer {
    // genai resolves the key from the environment before it sends; the
    // mock never checks it.
    unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-mock-not-real") };
    MockAnthropicServer::start().await
}

fn anthropic_turn(
    content: Vec<ContentBlock>,
    stop_reason: &str,
    input_tokens: u32,
    output_tokens: u32,
) -> MockResponse {
    MockResponse {
        content,
        stop_reason: stop_reason.to_string(),
        input_tokens,
        output_tokens,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    }
}

fn thinking(text: &str, signature: &str) -> ContentBlock {
    ContentBlock::Thinking {
        thinking: text.to_string(),
        signature: signature.to_string(),
    }
}

fn tool_use(id: &str, path: &str) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.to_string(),
        name: "read_file".to_string(),
        input: json!({"path": path}),
    }
}

fn text(text: &str) -> ContentBlock {
    ContentBlock::Text {
        text: text.to_string(),
    }
}

/// Compare against the pinned file, or write it under `UPDATE_SNAPSHOT`.
fn pin(name: &str, turns: Vec<Value>, requests: Vec<Value>) {
    let actual = fq_test_support::canonical_json(&json!({
        "turns": turns,
        "requests": requests,
    }));
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots/reasoning_wire")
        .join(format!("{name}.json"));
    if std::env::var_os("UPDATE_SNAPSHOT").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing snapshot {path:?} — run `UPDATE_SNAPSHOT=1 cargo test -p fq-runtime \
             wire_goldens` and commit the result"
        )
    });
    assert_eq!(
        actual, expected,
        "the reasoning wire for `{name}` changed: either what factor-q decoded from the \
         provider or what it sent back. A dependency change must leave this file untouched; \
         review the diff before regenerating, and say why in the commit."
    );
}

// endregion: harness

// region: anthropic

/// Three turns, two of them signed: the ordinary tool loop a thinking
/// model runs. Every replayed assistant turn must carry its own block,
/// signature intact, ahead of the tool call.
#[tokio::test]
async fn anthropic_signed_tool_loop() {
    const MODEL: &str = "claude-sonnet-4-6";

    let mock = anthropic_mock().await;
    mock.push_response(anthropic_turn(
        vec![
            thinking("Need the runbook before guessing.", "sig-turn-1"),
            tool_use("toolu_01", "/runbook"),
        ],
        "tool_use",
        100,
        20,
    ));
    mock.push_response(
        anthropic_turn(
            vec![
                thinking("It says restart; confirm the service name.", "sig-turn-2"),
                tool_use("toolu_02", "/services"),
            ],
            "tool_use",
            40,
            25,
        )
        .with_cache_usage(100, 0),
    );
    mock.push_response(MockResponse::text("Restart the deploy service.", 180, 8));
    let client = GenAiClient::with_base_url(mock.base_url()).expect("client builds");

    let mut messages = vec![
        Message::system("You are a careful assistant."),
        Message::user("Investigate the failing deploy."),
    ];
    let mut turns = Vec::new();

    let first = send(&client, MODEL, &messages, read_file_tool(), None).await;
    turns.push(decoded(&first));
    continue_with_tool_result(&mut messages, &first, "runbook says: restart");

    let second = send(&client, MODEL, &messages, read_file_tool(), None).await;
    turns.push(decoded(&second));
    continue_with_tool_result(&mut messages, &second, "deploy-svc");

    let third = send(&client, MODEL, &messages, read_file_tool(), None).await;
    turns.push(decoded(&third));

    let requests = mock.received_requests();
    mock.shutdown().await;
    pin("anthropic_signed_tool_loop", turns, requests);
}

/// The real Opus 5 block from the live run: no readable text, a
/// signature, and `effort: high`. Carried whole or not at all.
#[tokio::test]
async fn anthropic_empty_text_signed_opus5() {
    const MODEL: &str = "claude-opus-5";

    let mock = anthropic_mock().await;
    mock.push_response(
        anthropic_turn(
            vec![
                thinking("", OPUS5_SIGNATURE),
                ContentBlock::ToolUse {
                    id: "toolu_01T9hENsmBLpzUTZhLfyPr2u".to_string(),
                    name: "read_file".to_string(),
                    input: json!({"path": "/work/notes.txt"}),
                },
            ],
            "tool_use",
            2,
            179,
        )
        .with_cache_usage(0, 1648),
    );
    mock.push_response(
        MockResponse::text(
            "Reasoning round-trip probe: the badger is orange.\n33",
            4,
            30,
        )
        .with_cache_usage(1648, 246),
    );
    let client = GenAiClient::with_base_url(mock.base_url()).expect("client builds");

    let mut messages = vec![
        Message::system("You are a careful assistant."),
        Message::user("Read /work/notes.txt and report its first line."),
    ];
    let mut turns = Vec::new();

    let first = send(
        &client,
        MODEL,
        &messages,
        read_file_tool(),
        Some(Effort::High),
    )
    .await;
    turns.push(decoded(&first));
    continue_with_tool_result(
        &mut messages,
        &first,
        "Reasoning round-trip probe: the badger is orange.\nSecond line.",
    );

    let second = send(
        &client,
        MODEL,
        &messages,
        read_file_tool(),
        Some(Effort::High),
    )
    .await;
    turns.push(decoded(&second));

    let requests = mock.received_requests();
    mock.shutdown().await;
    pin("anthropic_empty_text_signed_opus5", turns, requests);
}

/// Thinking the API withheld: an opaque payload beside visible text and a
/// tool call. It is not readable and it still goes back verbatim.
#[tokio::test]
async fn anthropic_redacted_thinking() {
    const MODEL: &str = "claude-sonnet-4-6";

    let mock = anthropic_mock().await;
    mock.push_response(anthropic_turn(
        vec![
            ContentBlock::RedactedThinking {
                data: "EqQBCgIYAhIMredacted-payload-bytes".to_string(),
            },
            text("Checking the runbook."),
            tool_use("toolu_01", "/runbook"),
        ],
        "tool_use",
        100,
        30,
    ));
    mock.push_response(MockResponse::text("Restart it.", 150, 6));
    let client = GenAiClient::with_base_url(mock.base_url()).expect("client builds");

    let mut messages = vec![Message::user("Investigate the failing deploy.")];
    let mut turns = Vec::new();

    let first = send(&client, MODEL, &messages, read_file_tool(), None).await;
    turns.push(decoded(&first));
    continue_with_tool_result(&mut messages, &first, "runbook says: restart");

    let second = send(&client, MODEL, &messages, read_file_tool(), None).await;
    turns.push(decoded(&second));

    let requests = mock.received_requests();
    mock.shutdown().await;
    pin("anthropic_redacted_thinking", turns, requests);
}

/// Two signed blocks in one turn, then text, then the call. Order is the
/// provider's (ADR-0034 I6) and every block keeps its own signature.
#[tokio::test]
async fn anthropic_multi_block_turn() {
    const MODEL: &str = "claude-sonnet-4-6";

    let mock = anthropic_mock().await;
    mock.push_response(anthropic_turn(
        vec![
            thinking("First: what failed?", "sig-a"),
            thinking("Second: where is that recorded?", "sig-b"),
            text("Two things to check."),
            tool_use("toolu_01", "/runbook"),
        ],
        "tool_use",
        100,
        40,
    ));
    mock.push_response(MockResponse::text("Both point at the restart.", 160, 7));
    let client = GenAiClient::with_base_url(mock.base_url()).expect("client builds");

    let mut messages = vec![Message::user("Investigate the failing deploy.")];
    let mut turns = Vec::new();

    let first = send(&client, MODEL, &messages, read_file_tool(), None).await;
    turns.push(decoded(&first));
    continue_with_tool_result(&mut messages, &first, "runbook says: restart");

    let second = send(&client, MODEL, &messages, read_file_tool(), None).await;
    turns.push(decoded(&second));

    let requests = mock.received_requests();
    mock.shutdown().await;
    pin("anthropic_multi_block_turn", turns, requests);
}

/// A signed turn that ends without a tool call, followed by more user
/// input. The assistant history is replayed with its block, the same as
/// on a tool turn — the adapter does not know or care which it was.
#[tokio::test]
async fn anthropic_signed_end_turn_then_user() {
    const MODEL: &str = "claude-sonnet-4-6";

    let mock = anthropic_mock().await;
    mock.push_response(anthropic_turn(
        vec![
            thinking("They have not said which deploy.", "sig-1"),
            text("Which deploy do you mean?"),
        ],
        "end_turn",
        60,
        15,
    ));
    mock.push_response(MockResponse::text("Then restart it.", 90, 5));
    let client = GenAiClient::with_base_url(mock.base_url()).expect("client builds");

    let mut messages = vec![Message::user("Investigate the failing deploy.")];
    let mut turns = Vec::new();

    let first = send(&client, MODEL, &messages, vec![], None).await;
    turns.push(decoded(&first));
    messages.push(Message::Assistant {
        parts: first.parts.clone(),
    });
    messages.push(Message::user("The api deploy, twenty minutes ago."));

    let second = send(&client, MODEL, &messages, vec![], None).await;
    turns.push(decoded(&second));

    let requests = mock.received_requests();
    mock.shutdown().await;
    pin("anthropic_signed_end_turn_then_user", turns, requests);
}

/// Turn 1 on one model, turn 2 on another with the same history. The
/// block is tied to the model that produced it and must not reach the
/// second (ADR-0034 D5); the rest of the turn goes through.
#[tokio::test]
async fn anthropic_cross_model_strip() {
    const FIRST_MODEL: &str = "claude-opus-5";
    const SECOND_MODEL: &str = "claude-sonnet-4-6";

    let mock = anthropic_mock().await;
    mock.push_response(anthropic_turn(
        vec![
            thinking("Opus-only working.", "sig-opus"),
            tool_use("toolu_01", "/runbook"),
        ],
        "tool_use",
        100,
        20,
    ));
    mock.push_response(MockResponse::text("Restart it.", 140, 5));
    let client = GenAiClient::with_base_url(mock.base_url()).expect("client builds");

    let mut messages = vec![Message::user("Investigate the failing deploy.")];
    let mut turns = Vec::new();

    let first = send(&client, FIRST_MODEL, &messages, read_file_tool(), None).await;
    turns.push(decoded(&first));
    continue_with_tool_result(&mut messages, &first, "runbook says: restart");

    let second = send(&client, SECOND_MODEL, &messages, read_file_tool(), None).await;
    turns.push(decoded(&second));

    let requests = mock.received_requests();
    mock.shutdown().await;
    pin("anthropic_cross_model_strip", turns, requests);
}

// endregion: anthropic

// region: openai-compatible

/// Kimi's and DeepSeek's own wire: reasoning in `reasoning_content`, the
/// split in `completion_tokens_details`, and the field echoed back.
#[tokio::test]
async fn openai_reasoning_content_tool_loop() {
    const MODEL: &str = "kimi-k2";

    let mock = MockOpenAiServer::start().await;
    mock.push(
        MockChoice::silent()
            .with_reasoning("The runbook is the cheapest place to start.")
            .with_tool_call("call_1", "read_file", json!({"path": "/runbook"}))
            .with_usage(1047, 74, Some(6)),
    );
    mock.push(MockChoice::text("Restart the deploy service.").with_usage(1197, 26, Some(3)));
    let client = mock.client(MODEL);

    let mut messages = vec![
        Message::system("You are a careful assistant."),
        Message::user("Investigate the failing deploy."),
    ];
    let mut turns = Vec::new();

    let first = send(&client, MODEL, &messages, read_file_tool(), None).await;
    turns.push(decoded(&first));
    continue_with_tool_result(&mut messages, &first, "runbook says: restart");

    let second = send(&client, MODEL, &messages, read_file_tool(), None).await;
    turns.push(decoded(&second));

    let requests = mock.requests();
    mock.shutdown().await;
    pin("openai_reasoning_content_tool_loop", turns, requests);
}

/// The same models served through OpenRouter, which spells the field
/// `reasoning` — what kimi-k3 actually returned on the live run.
#[tokio::test]
async fn openai_openrouter_reasoning_key() {
    const MODEL: &str = "moonshotai/kimi-k3";

    let mock = MockOpenAiServer::start().await;
    mock.push(
        MockChoice::silent()
            .with_openrouter_reasoning("Read file.")
            .with_tool_call(
                "builtin__file_read:0",
                "read_file",
                json!({"path": "/work/notes.txt"}),
            )
            .with_usage(1047, 74, Some(6)),
    );
    mock.push(
        MockChoice::text("Reasoning round-trip probe: the badger is orange.\n33").with_usage(
            1562,
            26,
            Some(3),
        ),
    );
    let client = mock.client(MODEL);

    let mut messages = vec![Message::user(
        "Read /work/notes.txt and report its first line.",
    )];
    let mut turns = Vec::new();

    let first = send(
        &client,
        MODEL,
        &messages,
        read_file_tool(),
        Some(Effort::Medium),
    )
    .await;
    turns.push(decoded(&first));
    continue_with_tool_result(
        &mut messages,
        &first,
        "Reasoning round-trip probe: the badger is orange.\nSecond line.",
    );

    let second = send(
        &client,
        MODEL,
        &messages,
        read_file_tool(),
        Some(Effort::Medium),
    )
    .await;
    turns.push(decoded(&second));

    let requests = mock.requests();
    mock.shutdown().await;
    pin("openai_openrouter_reasoning_key", turns, requests);
}

// endregion: openai-compatible
