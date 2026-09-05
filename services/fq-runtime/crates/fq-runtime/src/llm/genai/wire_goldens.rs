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
use crate::events::{Effort, Message, RequestParams, ToolResult, ToolSchema};
use crate::llm::{ChatRequest, ChatResponse, LlmClient};
use crate::test_support::mock_anthropic::{ContentBlock, MockAnthropicServer, MockResponse};
use crate::test_support::mock_gemini::{GeminiTurn, MockGeminiServer};
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

/// Replay the assistant turn and answer every tool call it made, in
/// order, as one tool-results turn — what the reducer emits for a
/// parallel round (#511).
fn continue_with_tool_results(
    messages: &mut Vec<Message>,
    response: &ChatResponse,
    outputs: &[&str],
) {
    let calls = response.tool_calls();
    assert_eq!(calls.len(), outputs.len(), "one output per scripted call");
    messages.push(Message::Assistant {
        parts: response.parts.clone(),
    });
    messages.push(Message::ToolResults {
        results: calls
            .iter()
            .zip(outputs)
            .map(|(call, output)| ToolResult {
                tool_call_id: call.tool_call_id.clone(),
                output: output.to_string(),
                is_error: false,
            })
            .collect(),
    });
}

/// The `type` of every content block in a wire message.
fn block_types(message: &Value) -> Vec<&str> {
    message["content"]
        .as_array()
        .expect("content is a block list")
        .iter()
        .map(|block| block["type"].as_str().expect("every block has a type"))
        .collect()
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

/// A signed block and two `tool_use` blocks in one turn — the parallel
/// round #511 is about — answered by one tool-results turn carrying both.
/// Request 2 must replay the thinking block first, then both calls, then
/// **one** user message holding both `tool_result` blocks in call order.
/// The shape is asserted in words before the byte-level pin, so a moved
/// golden says what moved.
#[tokio::test]
async fn anthropic_parallel_tool_calls_signed() {
    const MODEL: &str = "claude-sonnet-4-6";

    let mock = anthropic_mock().await;
    mock.push_response(anthropic_turn(
        vec![
            thinking("Both files matter; read them together.", "sig-parallel"),
            tool_use("toolu_01", "/runbook"),
            tool_use("toolu_02", "/services"),
        ],
        "tool_use",
        100,
        35,
    ));
    mock.push_response(MockResponse::text("Restart the deploy service.", 190, 8));
    let client = GenAiClient::with_base_url(mock.base_url()).expect("client builds");

    let mut messages = vec![
        Message::system("You are a careful assistant."),
        Message::user("Investigate the failing deploy."),
    ];
    let mut turns = Vec::new();

    let first = send(&client, MODEL, &messages, read_file_tool(), None).await;
    turns.push(decoded(&first));
    continue_with_tool_results(
        &mut messages,
        &first,
        &["runbook says: restart", "deploy-svc"],
    );

    let second = send(&client, MODEL, &messages, read_file_tool(), None).await;
    turns.push(decoded(&second));

    let requests = mock.received_requests();
    mock.shutdown().await;

    let replayed = requests[1]["messages"]
        .as_array()
        .expect("request 2 carries the conversation");
    assert_eq!(replayed.len(), 3, "user, assistant, and one answering turn");
    assert_eq!(replayed[1]["role"], "assistant");
    assert_eq!(
        block_types(&replayed[1]),
        ["thinking", "tool_use", "tool_use"],
        "the signed block leads, then both calls"
    );
    assert_eq!(replayed[2]["role"], "user");
    assert_eq!(
        block_types(&replayed[2]),
        ["tool_result", "tool_result"],
        "one user message carries both results"
    );
    let answered: Vec<&str> = replayed[2]["content"]
        .as_array()
        .expect("blocks")
        .iter()
        .map(|block| block["tool_use_id"].as_str().expect("tool_use_id"))
        .collect();
    assert_eq!(answered, ["toolu_01", "toolu_02"], "in call order");

    pin("anthropic_parallel_tool_calls_signed", turns, requests);
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

/// Two calls in one turn, answered by one tool-results turn. This wire
/// wants one `tool` message per result, and the adapter unfolds the
/// single turn into exactly that, in call order — the same reducer
/// message the Anthropic golden above batches into one user turn.
#[tokio::test]
async fn openai_parallel_tool_calls() {
    const MODEL: &str = "kimi-k2";

    let mock = MockOpenAiServer::start().await;
    mock.push(
        MockChoice::silent()
            .with_tool_call("call_1", "read_file", json!({"path": "/runbook"}))
            .with_tool_call("call_2", "read_file", json!({"path": "/services"}))
            .with_usage(1047, 60, None),
    );
    mock.push(MockChoice::text("Restart the deploy service.").with_usage(1260, 26, None));
    let client = mock.client(MODEL);

    let mut messages = vec![
        Message::system("You are a careful assistant."),
        Message::user("Investigate the failing deploy."),
    ];
    let mut turns = Vec::new();

    let first = send(&client, MODEL, &messages, read_file_tool(), None).await;
    turns.push(decoded(&first));
    continue_with_tool_results(
        &mut messages,
        &first,
        &["runbook says: restart", "deploy-svc"],
    );

    let second = send(&client, MODEL, &messages, read_file_tool(), None).await;
    turns.push(decoded(&second));

    let requests = mock.requests();
    mock.shutdown().await;

    let replayed = requests[1]["messages"]
        .as_array()
        .expect("request 2 carries the conversation");
    let roles: Vec<&str> = replayed
        .iter()
        .map(|message| message["role"].as_str().expect("role"))
        .collect();
    assert_eq!(
        roles,
        ["system", "user", "assistant", "tool", "tool"],
        "one `tool` message per result on this wire"
    );
    let answered: Vec<&str> = replayed[3..]
        .iter()
        .map(|message| message["tool_call_id"].as_str().expect("tool_call_id"))
        .collect();
    assert_eq!(answered, ["call_1", "call_2"], "in call order");

    pin("openai_parallel_tool_calls", turns, requests);
}

// endregion: openai-compatible

// region: gemini

/// The model turns of a Gemini request body, each as its list of parts.
fn model_turns(request: &Value) -> Vec<&Vec<Value>> {
    request["contents"]
        .as_array()
        .expect("a Gemini request carries `contents`")
        .iter()
        .filter(|turn| turn["role"] == "model")
        .map(|turn| turn["parts"].as_array().expect("a turn has parts"))
        .collect()
}

/// The `thoughtSignature` values anywhere in a request body, in order.
fn signatures_in(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    match value {
        Value::Object(map) => {
            for (key, inner) in map {
                if key == "thoughtSignature" {
                    if let Some(signature) = inner.as_str() {
                        out.push(signature.to_string());
                    }
                } else {
                    out.extend(signatures_in(inner));
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                out.extend(signatures_in(item));
            }
        }
        _ => {}
    }
    out
}

/// The kinds of the reasoning parts in a decoded turn, as
/// `(reasoning kind, token type)` pairs.
fn reasoning_shapes(turn: &Value) -> Vec<(String, String)> {
    turn["parts"]
        .as_array()
        .expect("parts")
        .iter()
        .filter(|part| part["kind"] == "reasoning")
        .map(|part| {
            (
                part["content"]["kind"].as_str().unwrap_or("").to_string(),
                part["content"]["token"]["type"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
            )
        })
        .collect()
}

/// **The scenario #600 is about.** A thinking Gemini model answers with a
/// function call carrying a `thoughtSignature` on that part. The token is
/// Gemini's continuity token — no readable text beside it, so it is
/// *opaque* (ADR-0034 D2) — and the next request must hand it back on the
/// function call it came with, or a thinking model loses its continuity
/// (and Gemini 3 rejects the call outright).
#[tokio::test]
async fn gemini_signed_function_call_loop() {
    const MODEL: &str = "gemini-3-pro";

    let mock = MockGeminiServer::start().await;
    mock.push(
        GeminiTurn::silent()
            .with_function_call("read_file", json!({"path": "/runbook"}), Some("sig-turn-1"))
            .with_usage(100, 20, Some(12)),
    );
    mock.push(GeminiTurn::text("Restart the deploy service.").with_usage(150, 8, Some(3)));
    let client = mock.client(MODEL);

    let mut messages = vec![
        Message::system("You are a careful assistant."),
        Message::user("Investigate the failing deploy."),
    ];
    let mut turns = Vec::new();

    let first = send(&client, MODEL, &messages, read_file_tool(), None).await;
    turns.push(decoded(&first));
    assert_eq!(
        reasoning_shapes(&turns[0]),
        [("opaque".to_string(), "thought_signature".to_string())],
        "a bare signature is recorded as opaque reasoning, not dropped"
    );
    assert_eq!(
        first.usage.reasoning_tokens, 12,
        "thoughtsTokenCount is the split"
    );
    continue_with_tool_result(&mut messages, &first, "runbook says: restart");

    let second = send(&client, MODEL, &messages, read_file_tool(), None).await;
    turns.push(decoded(&second));

    let requests = mock.requests();
    mock.shutdown().await;

    let model_turn = model_turns(&requests[1])
        .into_iter()
        .next()
        .expect("request 2 replays the model turn");
    let call = model_turn
        .iter()
        .find(|part| part.get("functionCall").is_some())
        .expect("the replayed turn carries the function call");
    assert_eq!(
        call["thoughtSignature"].as_str(),
        Some("sig-turn-1"),
        "the signature goes back on the function call it came with; model turn was {model_turn:?}"
    );

    pin("gemini_signed_function_call_loop", turns, requests);
}

/// With `includeThoughts` the API also returns a thought summary — a
/// `text` part flagged `thought` — beside the signed call. The summary is
/// readable (plain), the signature is not (opaque); both are recorded,
/// and only the signature goes back, which is all Gemini wants.
#[tokio::test]
async fn gemini_signature_with_thought_summary() {
    const MODEL: &str = "gemini-3-pro";

    let mock = MockGeminiServer::start().await;
    mock.push(
        GeminiTurn::silent()
            .with_thought("Consider the runbook before guessing.")
            .with_function_call("read_file", json!({"path": "/runbook"}), Some("sig-a"))
            .with_usage(100, 25, Some(40)),
    );
    mock.push(GeminiTurn::text("Restart it.").with_usage(160, 6, Some(2)));
    let client = mock.client(MODEL);

    let mut messages = vec![Message::user("Investigate the failing deploy.")];
    let mut turns = Vec::new();

    let first = send(&client, MODEL, &messages, read_file_tool(), None).await;
    turns.push(decoded(&first));
    assert_eq!(
        reasoning_shapes(&turns[0]),
        [
            ("opaque".to_string(), "thought_signature".to_string()),
            ("plain".to_string(), String::new()),
        ],
        "the signature and the summary are both recorded, signature first"
    );
    continue_with_tool_result(&mut messages, &first, "runbook says: restart");

    let second = send(&client, MODEL, &messages, read_file_tool(), None).await;
    turns.push(decoded(&second));

    let requests = mock.requests();
    mock.shutdown().await;

    assert_eq!(
        signatures_in(&requests[1]),
        ["sig-a"],
        "the signature is replayed exactly once; the summary is not sent back"
    );

    pin("gemini_signature_with_thought_summary", turns, requests);
}

/// A turn with visible text *and* a signed call. genai's Gemini adapter
/// collects every signature ahead of the text and the calls when it
/// decodes, and on the way back attaches a pending signature to the
/// next part it meets — so here the token lands as a bare part before
/// the text, and the call gets Gemini 3's `skip_thought_signature_validator`
/// stand-in. That is upstream's approximation, pinned so that a change
/// in it is visible; what this crate guarantees is only that the
/// signature is carried and sent back.
#[tokio::test]
async fn gemini_text_and_signed_call() {
    const MODEL: &str = "gemini-3-pro";

    let mock = MockGeminiServer::start().await;
    mock.push(
        GeminiTurn::silent()
            .with_text("Reading it now.")
            .with_function_call("read_file", json!({"path": "/runbook"}), Some("sig-b"))
            .with_usage(100, 30, Some(8)),
    );
    mock.push(GeminiTurn::text("Restart it.").with_usage(170, 6, None));
    let client = mock.client(MODEL);

    let mut messages = vec![Message::user("Investigate the failing deploy.")];
    let mut turns = Vec::new();

    let first = send(&client, MODEL, &messages, read_file_tool(), None).await;
    turns.push(decoded(&first));
    continue_with_tool_result(&mut messages, &first, "runbook says: restart");

    let second = send(&client, MODEL, &messages, read_file_tool(), None).await;
    turns.push(decoded(&second));

    let requests = mock.requests();
    mock.shutdown().await;

    assert!(
        signatures_in(&requests[1]).contains(&"sig-b".to_string()),
        "the signature is sent back; request 2 was {}",
        requests[1]
    );

    pin("gemini_text_and_signed_call", turns, requests);
}

/// Turn 1 on one Gemini model, turn 2 on another with the same history.
/// The token is tied to the model that produced it (ADR-0034 D5), so it
/// must not reach the second; what does reach it is genai's own
/// no-signature stand-in for a Gemini 3 function call.
#[tokio::test]
async fn gemini_cross_model_strip() {
    const FIRST_MODEL: &str = "gemini-3-pro";
    const SECOND_MODEL: &str = "gemini-3-flash";

    let mock = MockGeminiServer::start().await;
    mock.push(
        GeminiTurn::silent()
            .with_function_call("read_file", json!({"path": "/runbook"}), Some("sig-pro"))
            .with_usage(100, 20, Some(5)),
    );
    mock.push(GeminiTurn::text("Restart it.").with_usage(140, 5, None));
    // Both models must route to the mock.
    let client = {
        let mut extra = std::collections::BTreeMap::new();
        extra.insert(
            "mock-gemini".to_string(),
            crate::config::ProviderConfig {
                api_shape: crate::config::ApiShape::Gemini,
                base_url: Some(mock.base_url()),
                api_key_env: "FQ_MOCK_GEMINI_KEY".to_string(),
                models: vec![FIRST_MODEL.to_string(), SECOND_MODEL.to_string()],
                pricing: std::collections::BTreeMap::new(),
            },
        );
        unsafe { std::env::set_var("FQ_MOCK_GEMINI_KEY", "mock-key") };
        GenAiClient::from_providers(
            &crate::config::ProvidersConfig {
                anthropic: None,
                extra,
            },
            crate::llm::LlmTimeouts::default(),
        )
        .expect("client builds")
    };

    let mut messages = vec![Message::user("Investigate the failing deploy.")];
    let mut turns = Vec::new();

    let first = send(&client, FIRST_MODEL, &messages, read_file_tool(), None).await;
    turns.push(decoded(&first));
    continue_with_tool_result(&mut messages, &first, "runbook says: restart");

    let second = send(&client, SECOND_MODEL, &messages, read_file_tool(), None).await;
    turns.push(decoded(&second));

    let requests = mock.requests();
    mock.shutdown().await;

    assert!(
        !signatures_in(&requests[1]).contains(&"sig-pro".to_string()),
        "a signature never crosses a model edge; request 2 was {}",
        requests[1]
    );

    pin("gemini_cross_model_strip", turns, requests);
}

// endregion: gemini
