//! The tool-result cap through the real `run_tool` (#827).
//!
//! Each test runs one invocation — a call to one tool, then an end
//! turn — and reads the tool's answer back from every place that
//! records it: the WAL row, the `tool.result` event, the next
//! `llm.request` event, and the request the model client was actually
//! handed. The cap is applied once, before the WAL write, so all four
//! must carry the same text.

use super::*;
use crate::test_support::sim::RecordingSink;

/// How [`AnswerTool`] answers every call.
enum Answer {
    /// The tool's own answer.
    Output(String),
    /// A tool error, which the host renders into the message the model
    /// reads.
    Failure(String),
}

/// A tool that answers every call the same way.
struct AnswerTool(Answer);

#[async_trait::async_trait]
impl fq_tools::Tool for AnswerTool {
    fn name(&self) -> &str {
        "cap__answer"
    }
    fn description(&self) -> &str {
        "answers with a fixed output"
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    async fn execute(
        &self,
        _ctx: &fq_tools::ToolContext<'_>,
        _params: Value,
    ) -> Result<fq_tools::ToolResult, fq_tools::ToolError> {
        match &self.0 {
            Answer::Output(output) => Ok(fq_tools::ToolResult::ok(output.clone())),
            Answer::Failure(message) => Err(fq_tools::ToolError::ExecutionFailed(message.clone())),
        }
    }
}

/// One tool answer, as each place that records it holds it.
struct Recorded {
    wal: String,
    event: String,
    request_event: String,
    sent_to_model: String,
    is_error: bool,
    error_kind: Option<ToolErrorKind>,
}

impl Recorded {
    /// The text every record agrees on.
    fn text(&self) -> &str {
        assert_eq!(
            self.wal, self.event,
            "the WAL row and the tool.result event must carry the same text"
        );
        assert_eq!(
            self.event, self.request_event,
            "the next llm.request event must carry what tool.result did"
        );
        assert_eq!(
            self.event, self.sent_to_model,
            "the model must be sent what was recorded"
        );
        &self.event
    }
}

/// The answer to the tool call in a request's messages.
fn tool_answer(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::ToolResults { results } => results.first().map(|r| r.output.clone()),
            _ => None,
        })
        .expect("the request carries the tool's answer")
}

/// The kept-byte count a capped text's marker states.
fn stated_kept(text: &str) -> usize {
    let (_, tail) = text
        .rsplit_once("kept the first ")
        .expect("a capped text carries the marker");
    tail.split(' ')
        .next()
        .and_then(|n| n.parse().ok())
        .expect("the marker states the kept count")
}

/// Run one invocation that calls `tool` once under a `max_bytes` cap,
/// and read the answer back from every record of it.
async fn run_capped(tool: Arc<dyn fq_tools::Tool>, max_bytes: usize) -> Recorded {
    let sink = Arc::new(RecordingSink::new());
    let dir = tempdir().expect("tempdir");
    let store = Arc::new(
        WorkerStore::open(&dir.path().join("events.db"))
            .await
            .expect("worker store"),
    );
    let name = tool.name().to_string();
    let mut tools = ToolRegistry::with_builtins();
    tools.register(tool).expect("the tool registers");
    let runner = ReducerRunner::new(
        Arc::new(ReducerContext::builder().tools(Arc::new(tools)).build()),
        Arc::new(
            RunnerConfig::builder()
                .event_sink(Arc::clone(&sink) as Arc<dyn EventSink>)
                .pricing(test_pricing())
                .store(Arc::clone(&store))
                .worker_id(test_worker_id())
                .tool_limits(crate::tools::ToolCallLimits {
                    max_result_bytes: max_bytes,
                    ..crate::tools::ToolCallLimits::default()
                })
                .build(),
        ),
        Harness::new(),
    );
    let agent = Agent::builder()
        .id(unique_agent_id("result-cap"))
        .model("claude-haiku")
        .system_prompt("call the tool")
        .sandbox(Sandbox::new())
        .tools(vec![name.clone()])
        .budget(1.0)
        .build()
        .unwrap();

    let llm = FixtureClient::new();
    llm.push_response(tool_call_response(&name, "call-1", json!({})));
    llm.push_response(end_turn_response("read it"));
    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        runner.run(
            &agent,
            &llm,
            TriggerSource::Manual,
            None,
            json!({"input": "go"}),
        ),
    )
    .await
    .expect("the invocation must not hang");
    let invocation_id = match outcome {
        Ok(InvocationOutcome::Completed { invocation_id, .. }) => invocation_id,
        other => panic!("expected the invocation to complete, got {other:?}"),
    };

    let row = store
        .get_tool_dispatch(&invocation_id.to_string(), "call-1")
        .await
        .expect("WAL read")
        .expect("the call has a WAL row");
    assert_eq!(row.status, DispatchStatus::Completed);
    let events = sink.events();
    let result = events
        .iter()
        .find_map(|e| match &e.payload {
            EventPayload::ToolResult(p) => Some(p.clone()),
            _ => None,
        })
        .expect("a tool.result event");
    let next_request = events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::LlmRequest(p) => Some(p),
            _ => None,
        })
        .nth(1)
        .expect("a second llm.request, after the tool answered");
    let requests = llm.requests();
    assert_eq!(requests.len(), 2, "two model turns");

    Recorded {
        wal: row.result.expect("a completed row carries the result"),
        event: result.output,
        request_event: tool_answer(&next_request.messages),
        sent_to_model: tool_answer(&requests[1].messages),
        is_error: result.is_error,
        error_kind: result.error_kind,
    }
}

/// The shipped cap, one byte over: cut to the cap with the marker, and
/// the same cut text everywhere it is recorded.
#[tokio::test]
async fn a_result_one_byte_over_the_cap_is_capped_the_same_everywhere() {
    let cap = crate::tools::ToolCallLimits::default().max_result_bytes;
    let output = "x".repeat(cap + 1);
    let recorded = run_capped(Arc::new(AnswerTool(Answer::Output(output.clone()))), cap).await;
    let text = recorded.text();

    assert!(
        text.len() <= cap,
        "{} bytes over a {cap}-byte cap",
        text.len()
    );
    assert!(
        text.ends_with(&format!(
            "\n\n(truncated at the 256.0 KiB tool-result cap: kept the first {} of {} bytes; \
             the rest was dropped)",
            stated_kept(text),
            cap + 1
        )),
        "the marker names the cap and the exact counts: {}",
        &text[text.len().saturating_sub(200)..]
    );
    assert!(output.starts_with(&text[..stated_kept(text)]));
    assert_eq!(text, fq_tools::cap_output(output, cap));
    assert!(!recorded.is_error);
}

#[tokio::test]
async fn a_result_of_exactly_the_cap_is_untouched() {
    let cap = crate::tools::ToolCallLimits::default().max_result_bytes;
    let output = "x".repeat(cap);
    let recorded = run_capped(Arc::new(AnswerTool(Answer::Output(output.clone()))), cap).await;
    assert_eq!(recorded.text(), output);
}

/// An error message can be a stack dump: the message the model reads is
/// capped like any other answer, and stays an error of the same kind.
#[tokio::test]
async fn an_error_output_over_the_cap_is_capped_too() {
    let cap = 4096;
    let message = "e".repeat(3 * cap);
    let recorded = run_capped(Arc::new(AnswerTool(Answer::Failure(message.clone()))), cap).await;
    let text = recorded.text();

    assert!(
        text.len() <= cap,
        "{} bytes over a {cap}-byte cap",
        text.len()
    );
    assert!(text.contains("tool-result cap: kept the first"));
    assert!(message.starts_with(&text[..stated_kept(text)]));
    assert!(recorded.is_error);
    assert_eq!(recorded.error_kind, Some(ToolErrorKind::ExecutionFailed));
}

/// A cut that would land inside a multi-byte character moves back to
/// its start. Without that, building the capped text panics in the
/// middle of `run_tool`, after the call has already run.
#[tokio::test]
async fn multi_byte_text_straddling_the_cap_is_cut_on_a_character_boundary() {
    let cap = 4096;
    // Pad a run of three-byte characters by 0, 1 or 2 bytes: one of the
    // three puts the raw cut (what the marker leaves of the cap, the
    // same for all three) inside a character. Pick that one.
    let (output, raw_cut) = (0..3)
        .map(|pad| format!("{}{}", "a".repeat(pad), "€".repeat(cap)))
        .find_map(|output| {
            let ascii = fq_tools::cap_output("a".repeat(output.len()), cap);
            let raw_cut = stated_kept(&ascii);
            (!output.is_char_boundary(raw_cut)).then_some((output, raw_cut))
        })
        .expect("one of three paddings cuts inside a character");

    let recorded = run_capped(Arc::new(AnswerTool(Answer::Output(output.clone()))), cap).await;
    let text = recorded.text();

    assert!(
        text.len() <= cap,
        "{} bytes over a {cap}-byte cap",
        text.len()
    );
    assert!(std::str::from_utf8(recorded.wal.as_bytes()).is_ok());
    let kept = stated_kept(text);
    assert!(output.is_char_boundary(kept));
    assert!(kept < raw_cut && raw_cut - kept < '€'.len_utf8());
    assert!(text.starts_with(&output[..kept]));
}

/// The MCP path, not only a built-in's: the adapter joins every text
/// part a server returns, and the joined answer is what gets capped.
/// A real `McpTool` over a real rmcp client, served by the in-process
/// mock.
#[tokio::test]
async fn an_mcp_result_over_the_cap_is_capped() {
    let cap = 4096;
    let parts = vec!["p".repeat(3000), "q".repeat(3000)];
    let joined = parts.join("\n");
    let tool = crate::mcp::mcp_tool_answering("docs", "fetch", parts).await;
    assert_eq!(tool.name(), "docs__fetch");

    let recorded = run_capped(tool, cap).await;
    let text = recorded.text();

    assert!(
        text.len() <= cap,
        "{} bytes over a {cap}-byte cap",
        text.len()
    );
    assert!(
        text.contains(&format!("of {} bytes", joined.len())),
        "the marker counts the joined parts: {}",
        &text[text.len().saturating_sub(200)..]
    );
    assert_eq!(text, fq_tools::cap_output(joined, cap));
    assert!(!recorded.is_error);
}
