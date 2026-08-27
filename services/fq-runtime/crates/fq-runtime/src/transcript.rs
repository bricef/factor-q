//! Payload-bearing transcript for one invocation.
//!
//! `fq invocation transcript <id>` reconstructs the full conversation
//! transcript — LLM turns and tool calls with their payloads — from the
//! worker WAL (`llm_dispatch` + `tool_dispatch`), which is the only place
//! the payloads are persisted (the projection stores headers only).

// The entry shape and the pure rendering over it are
// `fq_ops::transcript`, re-exported so a caller reaches them by the
// same path as before. What stays is the building: from WAL rows and
// live event payloads, which are this crate's.
pub use fq_ops::transcript::*;

use crate::ChatResponse;
use crate::events::{LlmResponsePayload, Message, ToolResultPayload};
use crate::worker::{LlmDispatchRow, ToolDispatchRow};
use serde::Serialize;
use serde_json::Value;

/// Build the ordered transcript from the worker WAL rows for one
/// invocation. Pure over its inputs so it is unit-testable without a
/// live daemon: the caller supplies the `llm_dispatch` / `tool_dispatch`
/// rows (already ordered by `intent_at` by the store helpers).
///
/// - The first LLM dispatch's `request_payload` seeds a [`TranscriptEntry::Prompt`]
///   holding the system prompt + initial user message. Later requests
///   re-send the whole history, so only the first is mined for the
///   seed — avoiding N-fold repetition (see the issue's decision).
/// - Each LLM dispatch contributes a [`TranscriptEntry::Assistant`] from
///   its `response` (assistant text + requested tool calls).
/// - Each tool dispatch contributes a [`TranscriptEntry::ToolResult`].
///
/// Entries are sorted by `intent_at`, tool-after-LLM on ties.
pub fn collect_transcript(
    llm_rows: &[LlmDispatchRow],
    tool_rows: &[ToolDispatchRow],
) -> Vec<TranscriptEntry> {
    let mut entries: Vec<TranscriptEntry> = Vec::new();

    // Seed the prompt from the first LLM request payload.
    if let Some(first) = llm_rows.first()
        && let Some(prompt) = prompt_from_request(first.intent_at, &first.request_payload)
    {
        entries.push(prompt.into_entry());
    }

    for row in llm_rows {
        let (content, reasoning, tool_calls, is_error) = match &row.response {
            Some(raw) => parse_llm_response(raw),
            None => (None, None, Vec::new(), row.is_error),
        };
        entries.push(TranscriptEntry::Assistant {
            // `intent_at` (when the turn started) is the sort key for
            // every entry — it matches the store's `ORDER BY intent_at`,
            // is causally monotonic under the serial worker, and (unlike
            // `completed_at`) is always present, so an in-flight dispatch
            // is not mis-slotted by a fallback to a different clock.
            timestamp_ms: row.intent_at,
            model: row.model.clone(),
            content,
            reasoning,
            tool_calls,
            cost_usd: row.cost_usd,
            is_error: row.is_error.or(is_error),
        });
    }

    for row in tool_rows {
        entries.push(TranscriptEntry::ToolResult {
            timestamp_ms: row.intent_at,
            tool_call_id: row.tool_call_id.clone(),
            tool_name: row.tool_name.clone(),
            parameters: parse_json_lenient(&row.parameters),
            output: row.result.clone(),
            is_error: row.is_error,
        });
    }

    sort_timeline(&mut entries);
    entries
}

/// An invocation's opening prompt — the system prompt and the first
/// user message — as a value of its own, separate from the timeline
/// entry that renders it.
///
/// The split exists because the prompt is read out of two different
/// records of the same fact: the WAL's first `llm_dispatch` request
/// payload (this module's [`collect_transcript`], which the read
/// service still serves the dashboard from), and the `llm.request`
/// event that the runner publishes immediately before the same call
/// ([`crate::turn::TurnFold`], which the operator surface's
/// `turn.list` folds). Both paths hand back this value; only the
/// rendering path wraps it in a [`TranscriptEntry`].
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct TranscriptPrompt {
    /// The first LLM request's `intent_at` — when the invocation
    /// started talking, and the sort key the WAL-backed transcript
    /// orders every entry by.
    pub timestamp_ms: i64,
    pub system: Option<String>,
    pub user: Option<String>,
}

impl TranscriptPrompt {
    /// The prompt as the timeline entry that renders it. Consuming:
    /// the strings are large (a system prompt runs to thousands of
    /// tokens) and there is no reason to clone them at the seam.
    pub fn into_entry(self) -> TranscriptEntry {
        TranscriptEntry::Prompt {
            timestamp_ms: self.timestamp_ms,
            system: self.system,
            user: self.user,
        }
    }
}

/// Reconstruct the system prompt + first user message from a serialised
/// `LlmRequestPayload`. Returns `None` if the payload carries neither.
pub(crate) fn prompt_from_request(intent_at: i64, raw: &str) -> Option<TranscriptPrompt> {
    let payload: LlmRequestLike = serde_json::from_str(raw).ok()?;
    prompt_from_messages(intent_at, &payload.messages)
}

/// Mine the opening prompt out of a request's message list: the first
/// system message and the first user message. `None` if it carries
/// neither.
///
/// The one definition of "what the prompt is", shared by both paths
/// that need it — the WAL-backed transcript, which parses a stored
/// request payload, and the Turn fold, which has the live
/// `llm.request` event's messages in hand
/// ([`crate::turn::TurnFold::apply`]). Two readings of the same fact
/// must not be allowed to drift.
pub(crate) fn prompt_from_messages(
    timestamp_ms: i64,
    messages: &[Message],
) -> Option<TranscriptPrompt> {
    let mut system = None;
    let mut user = None;
    for msg in messages {
        match msg {
            Message::System { text } if system.is_none() => system = Some(text.clone()),
            Message::User { text } if user.is_none() => user = Some(text.clone()),
            _ => {}
        }
    }
    if system.is_none() && user.is_none() {
        return None;
    }
    Some(TranscriptPrompt {
        timestamp_ms,
        system,
        user,
    })
}

/// Minimal shape we need from a persisted `LlmRequestPayload`: just the
/// messages. Deserialising into the full event type would couple us to
/// every unrelated field; the messages are all the transcript needs.
#[derive(serde::Deserialize)]
struct LlmRequestLike {
    #[serde(default)]
    messages: Vec<Message>,
}

/// Parse a serialised `ChatResponse` — the shape the worker WAL persists
/// for `llm_dispatch.response` (`write_llm_completed` stores
/// `serde_json::to_string(&ChatResponse)`) — into (content, tool_calls,
/// is_error). NOT `LlmResponsePayload`: that event-payload type also
/// requires a `call_id`, so parsing a stored `ChatResponse` into it
/// always fails and falls back to raw JSON. Lenient: a payload that does
/// not match is rendered as raw text rather than dropped, so a schema
/// drift never blanks the turn.
type ParsedResponse = (
    Option<String>,
    Option<fq_ops::transcript::TurnReasoning>,
    Vec<AssistantToolCall>,
    Option<bool>,
);

fn parse_llm_response(raw: &str) -> ParsedResponse {
    match serde_json::from_str::<ChatResponse>(raw) {
        Ok(resp) => {
            let calls = resp
                .tool_calls()
                .into_iter()
                .map(|tc| AssistantToolCall {
                    tool_call_id: tc.tool_call_id.as_str().to_string(),
                    tool_name: tc.tool_name.clone(),
                    parameters: tc.parameters.clone(),
                })
                .collect();
            (
                resp.text(),
                crate::events::reduce_reasoning(&resp.parts),
                calls,
                None,
            )
        }
        Err(_) => (Some(raw.to_string()), None, Vec::new(), None),
    }
}

/// Parse `s` as JSON, falling back to a JSON string containing the raw
/// text if it is not valid JSON. Tool parameters are stored as JSON
/// text but we never want a parse failure to lose the payload.
fn parse_json_lenient(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.to_string()))
}

/// Render a single tool-result entry from a live `ToolResultPayload`.
/// Used by the `--follow` path, which sees NATS events rather than WAL
/// rows and cannot always recover the tool name / parameters (they rode
/// the earlier `tool.call` event); those are best-effort.
pub fn tool_result_entry(
    timestamp_ms: i64,
    tool_name: String,
    parameters: Value,
    payload: &ToolResultPayload,
) -> TranscriptEntry {
    TranscriptEntry::ToolResult {
        timestamp_ms,
        tool_call_id: payload.tool_call_id.to_string(),
        tool_name,
        parameters,
        output: Some(payload.output.clone()),
        is_error: Some(payload.is_error),
    }
}

/// Render a single assistant entry from a live `LlmResponsePayload`.
pub fn assistant_entry(
    timestamp_ms: i64,
    model: String,
    cost_usd: Option<f64>,
    payload: &LlmResponsePayload,
) -> TranscriptEntry {
    let tool_calls = payload
        .tool_calls()
        .map(|tc| AssistantToolCall {
            tool_call_id: tc.tool_call_id.as_str().to_string(),
            tool_name: tc.tool_name.clone(),
            parameters: tc.parameters.clone(),
        })
        .collect();
    TranscriptEntry::Assistant {
        timestamp_ms,
        model,
        content: payload.text(),
        reasoning: crate::events::reduce_reasoning(&payload.parts),
        tool_calls,
        cost_usd,
        is_error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::DispatchStatus;

    fn llm_row(
        intent_at: i64,
        completed_at: i64,
        request: &str,
        response: &str,
        cost: f64,
    ) -> LlmDispatchRow {
        LlmDispatchRow {
            invocation_id: "inv".to_string(),
            request_id: format!("req-{intent_at}"),
            model: "claude-haiku".to_string(),
            status: DispatchStatus::Completed,
            request_payload: request.to_string(),
            response: Some(response.to_string()),
            cost_usd: Some(cost),
            is_error: Some(false),
            intent_at,
            dispatched_at: Some(intent_at + 1),
            completed_at: Some(completed_at),
            seq: None,
        }
    }

    fn tool_row(
        intent_at: i64,
        completed_at: i64,
        name: &str,
        params: &str,
        result: &str,
    ) -> ToolDispatchRow {
        ToolDispatchRow {
            invocation_id: "inv".to_string(),
            tool_call_id: format!("tc-{intent_at}"),
            tool_name: name.to_string(),
            status: DispatchStatus::Completed,
            parameters: params.to_string(),
            result: Some(result.to_string()),
            is_error: Some(false),
            intent_at,
            dispatched_at: Some(intent_at + 1),
            completed_at: Some(completed_at),
            seq: None,
        }
    }

    const FIRST_REQUEST: &str = r#"{"messages":[
        {"kind":"system","text":"You are a helpful agent."},
        {"kind":"user","text":"List the files."}
    ]}"#;

    // These mirror the *real* wire shape the WAL persists: a serialised
    // `ChatResponse` (parts + stop_reason + usage) — NO `call_id`. A
    // fixture that adds `call_id` would spuriously parse as an
    // `LlmResponsePayload` and hide the response-type mismatch.
    //
    // `parts` rather than `content` + `tool_calls` since schema v3
    // (ADR-0034): an assistant turn is an ordered part list.
    fn response_with_tool_call() -> String {
        serde_json::json!({
            "parts": [
                {"kind": "text", "text": "Let me list the files."},
                {"kind": "tool_call", "tool_call_id": "tc-100", "tool_name": "shell",
                 "parameters": {"cmd": "ls"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })
        .to_string()
    }

    fn response_final() -> String {
        serde_json::json!({
            "parts": [{"kind": "text", "text": "Done — there are two files."}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 20, "output_tokens": 8}
        })
        .to_string()
    }

    #[test]
    fn collect_orders_tool_result_after_requesting_assistant_turn() {
        // Assistant turn at t=100 requests a tool; the tool result
        // completes at t=100 too. The result must render after the turn.
        let llm = vec![
            llm_row(100, 100, FIRST_REQUEST, &response_with_tool_call(), 0.01),
            llm_row(300, 300, FIRST_REQUEST, &response_final(), 0.02),
        ];
        let tools = vec![tool_row(
            100,
            100,
            "shell",
            r#"{"cmd":"ls"}"#,
            "a.txt\nb.txt",
        )];

        let entries = collect_transcript(&llm, &tools);

        // prompt, assistant#1, tool_result, assistant#2
        assert_eq!(entries.len(), 4);
        assert!(matches!(entries[0], TranscriptEntry::Prompt { .. }));
        assert!(matches!(entries[1], TranscriptEntry::Assistant { .. }));
        assert!(matches!(entries[2], TranscriptEntry::ToolResult { .. }));
        assert!(matches!(entries[3], TranscriptEntry::Assistant { .. }));
    }

    #[test]
    fn orders_by_intent_at_not_completion() {
        // A slow assistant turn (intent 100, completes 300) precedes a
        // tool dispatched at intent 150. By intent_at the assistant comes
        // first; a completed_at sort would wrongly flip them (assistant
        // 300 after tool 160).
        let llm = vec![llm_row(
            100,
            300,
            FIRST_REQUEST,
            &response_with_tool_call(),
            0.01,
        )];
        let tools = vec![tool_row(150, 160, "shell", r#"{"cmd":"ls"}"#, "ok")];
        let entries = collect_transcript(&llm, &tools);
        assert_eq!(entries.len(), 3);
        assert!(matches!(entries[0], TranscriptEntry::Prompt { .. }));
        assert!(
            matches!(entries[1], TranscriptEntry::Assistant { .. }),
            "assistant must precede the later-intent tool"
        );
        assert!(matches!(entries[2], TranscriptEntry::ToolResult { .. }));
    }

    #[test]
    fn collect_extracts_prompt_and_payloads() {
        let llm = vec![llm_row(
            100,
            100,
            FIRST_REQUEST,
            &response_with_tool_call(),
            0.01,
        )];
        let tools = vec![tool_row(
            100,
            100,
            "shell",
            r#"{"cmd":"ls"}"#,
            "a.txt\nb.txt",
        )];
        let entries = collect_transcript(&llm, &tools);

        match &entries[0] {
            TranscriptEntry::Prompt { system, user, .. } => {
                assert_eq!(system.as_deref(), Some("You are a helpful agent."));
                assert_eq!(user.as_deref(), Some("List the files."));
            }
            other => panic!("expected prompt, got {other:?}"),
        }
        match &entries[1] {
            TranscriptEntry::Assistant {
                content,
                tool_calls,
                cost_usd,
                ..
            } => {
                assert_eq!(content.as_deref(), Some("Let me list the files."));
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].tool_name, "shell");
                assert_eq!(tool_calls[0].parameters["cmd"], "ls");
                assert_eq!(*cost_usd, Some(0.01));
            }
            other => panic!("expected assistant, got {other:?}"),
        }
        match &entries[2] {
            TranscriptEntry::ToolResult {
                tool_name,
                parameters,
                output,
                ..
            } => {
                assert_eq!(tool_name, "shell");
                assert_eq!(parameters["cmd"], "ls");
                assert_eq!(output.as_deref(), Some("a.txt\nb.txt"));
            }
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    fn assistant_with(reasoning: Option<fq_ops::transcript::TurnReasoning>) -> TranscriptEntry {
        TranscriptEntry::Assistant {
            timestamp_ms: 100,
            model: "kimi-k2".to_string(),
            content: Some("The answer is 4.".to_string()),
            reasoning,
            tool_calls: Vec::new(),
            cost_usd: None,
            is_error: None,
        }
    }

    /// Reasoning is recorded either way; the flag governs display only.
    /// Hidden by default because a transcript is read to answer *what
    /// happened*, and reasoning is the least useful part of that while
    /// often being the longest.
    #[test]
    fn reasoning_renders_only_behind_the_flag() {
        let entries = vec![assistant_with(Some(fq_ops::transcript::TurnReasoning {
            text: Some("I ruled out 41 first.".to_string()),
            opaque: None,
        }))];

        let hidden = render_pretty(&entries, RenderOptions::pretty());
        assert!(
            !hidden.contains("I ruled out 41"),
            "reasoning is hidden by default: {hidden}"
        );
        assert!(
            hidden.contains("The answer is 4."),
            "…but the turn itself still renders: {hidden}"
        );

        let shown = render_pretty(
            &entries,
            RenderOptions {
                truncate_bytes: Some(DEFAULT_TRUNCATE_BYTES),
                reasoning: true,
            },
        );
        assert!(
            shown.contains("I ruled out 41"),
            "--reasoning shows it: {shown}"
        );
    }

    /// **I7 in the rendering.** A turn whose reasoning we cannot read
    /// must not render as a turn that had none. Presentation may collapse
    /// the distinction; it may not erase it.
    #[test]
    fn opaque_reasoning_renders_as_present_not_absent() {
        let entries = vec![assistant_with(Some(fq_ops::transcript::TurnReasoning {
            text: None,
            opaque: Some(serde_json::json!("encrypted-blob")),
        }))];

        let shown = render_pretty(
            &entries,
            RenderOptions {
                truncate_bytes: Some(DEFAULT_TRUNCATE_BYTES),
                reasoning: true,
            },
        );
        assert!(
            shown.contains("opaque"),
            "an unreadable turn must say so rather than show nothing: {shown}"
        );
    }

    /// A turn that genuinely had no reasoning prints nothing about it,
    /// even with the flag on — the other half of the same distinction.
    #[test]
    fn absent_reasoning_prints_nothing_even_with_the_flag() {
        let entries = vec![assistant_with(None)];
        let shown = render_pretty(
            &entries,
            RenderOptions {
                truncate_bytes: Some(DEFAULT_TRUNCATE_BYTES),
                reasoning: true,
            },
        );
        assert!(
            !shown.contains("reasoning"),
            "no reasoning means no reasoning line: {shown}"
        );
    }

    #[test]
    fn render_pretty_contains_payloads() {
        let llm = vec![llm_row(
            100,
            100,
            FIRST_REQUEST,
            &response_with_tool_call(),
            0.01,
        )];
        let tools = vec![tool_row(
            100,
            100,
            "shell",
            r#"{"cmd":"ls"}"#,
            "a.txt\nb.txt",
        )];
        let entries = collect_transcript(&llm, &tools);
        let text = render_pretty(&entries, RenderOptions::pretty());

        assert!(
            text.contains("You are a helpful agent."),
            "system prompt: {text}"
        );
        assert!(
            text.contains("Let me list the files."),
            "assistant text: {text}"
        );
        assert!(text.contains("shell"), "tool name: {text}");
        assert!(text.contains("a.txt"), "tool output: {text}");
    }

    #[test]
    fn render_pretty_truncates_large_output_but_full_does_not() {
        let big = "x".repeat(5000);
        let result = format!("{{\"out\":\"{big}\"}}");
        let llm = vec![llm_row(100, 100, FIRST_REQUEST, &response_final(), 0.01)];
        let tools = vec![tool_row(200, 200, "shell", "{}", &result)];
        let entries = collect_transcript(&llm, &tools);

        let truncated = render_pretty(&entries, RenderOptions::pretty());
        assert!(truncated.contains("truncated"), "should note truncation");
        assert!(truncated.len() < result.len(), "truncated output shorter");

        let full = render_pretty(
            &entries,
            RenderOptions {
                truncate_bytes: None,
                reasoning: false,
            },
        );
        assert!(!full.contains("truncated"), "full must not truncate");
        assert!(full.contains(&big), "full must contain the whole output");
    }

    #[test]
    fn json_entries_are_serialisable_ordered_array() {
        let llm = vec![llm_row(
            100,
            100,
            FIRST_REQUEST,
            &response_with_tool_call(),
            0.01,
        )];
        let tools = vec![tool_row(100, 100, "shell", r#"{"cmd":"ls"}"#, "ok")];
        let entries = collect_transcript(&llm, &tools);
        let v = serde_json::to_value(&entries).unwrap();
        let arr = v.as_array().expect("array");
        assert_eq!(arr[0]["kind"], "prompt");
        assert_eq!(arr[1]["kind"], "assistant");
        assert_eq!(arr[2]["kind"], "tool_result");
    }

    #[test]
    fn empty_rows_produce_empty_transcript() {
        let entries = collect_transcript(&[], &[]);
        assert!(entries.is_empty());
        assert!(
            render_pretty(
                &entries,
                RenderOptions {
                    truncate_bytes: None,
                    reasoning: false
                }
            )
            .is_empty()
        );
    }

    #[test]
    fn snapshot_keys_capture_prompt_tool_call_and_result_ids() {
        // The requesting assistant turn, the tool result, and the
        // prompt must all be deduped at the --follow seam, under
        // distinct keys.
        let llm = vec![llm_row(
            100,
            100,
            FIRST_REQUEST,
            &response_with_tool_call(),
            0.01,
        )];
        let tools = vec![tool_row(100, 100, "shell", r#"{"cmd":"ls"}"#, "ok")];
        let entries = collect_transcript(&llm, &tools);
        let keys = snapshot_keys(&entries);
        assert!(
            keys.contains("call:tc-100"),
            "assistant key missing: {keys:?}"
        );
        assert!(
            keys.contains("tool:tc-100"),
            "tool-result key missing: {keys:?}"
        );
        // The prompt arrives as a Turn now, so `--follow` can see it
        // on the stream as well as in the snapshot; a constant key is
        // what keeps it from printing twice.
        assert!(keys.contains("prompt"), "prompt key missing: {keys:?}");
    }
    #[tokio::test]
    async fn store_round_trip_transcript_ordering_and_payloads() {
        // Full store-level exercise per the issue's acceptance
        // criterion: build a temp worker.db, write intent+completed
        // rows into both dispatch tables (mirroring worker/store.rs
        // tests), read them back through the same list helpers the
        // CLI uses, and assert the collected transcript's ordering
        // and payload content.
        use crate::WorkerStore;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let path = dir.path().join("worker.db");
        let store = WorkerStore::open(&path).await.expect("open");
        let inv = "inv-rt";

        // LLM turn 1 (requests a tool), tool result, LLM turn 2 (final).
        store
            .write_llm_intent(inv, "req-1", "claude-haiku", FIRST_REQUEST, 100)
            .await
            .unwrap();
        store.write_llm_dispatched(inv, "req-1", 101).await.unwrap();
        store
            .write_llm_completed(inv, "req-1", &response_with_tool_call(), false, 0.01, 102)
            .await
            .unwrap();

        store
            .write_tool_intent(inv, "tc-1", "shell", r#"{"cmd":"ls"}"#, 110)
            .await
            .unwrap();
        store.write_tool_dispatched(inv, "tc-1", 111).await.unwrap();
        store
            .write_tool_completed(inv, "tc-1", "a.txt\nb.txt", false, 112)
            .await
            .unwrap();

        store
            .write_llm_intent(inv, "req-2", "claude-haiku", FIRST_REQUEST, 200)
            .await
            .unwrap();
        store.write_llm_dispatched(inv, "req-2", 201).await.unwrap();
        store
            .write_llm_completed(inv, "req-2", &response_final(), false, 0.02, 202)
            .await
            .unwrap();

        // Read back read-only, exactly as the CLI handler does.
        let ro = WorkerStore::open_read_only(&path).await.expect("open ro");
        let llm_rows = ro.list_llm_dispatches_for_invocation(inv).await.unwrap();
        let tool_rows = ro.list_tool_dispatches_for_invocation(inv).await.unwrap();

        let entries = collect_transcript(&llm_rows, &tool_rows);
        // prompt, assistant#1, tool_result, assistant#2
        assert_eq!(entries.len(), 4);
        assert!(matches!(entries[0], TranscriptEntry::Prompt { .. }));
        assert!(matches!(entries[1], TranscriptEntry::Assistant { .. }));
        assert!(matches!(entries[2], TranscriptEntry::ToolResult { .. }));
        assert!(matches!(entries[3], TranscriptEntry::Assistant { .. }));

        let text = render_pretty(&entries, RenderOptions::pretty());
        assert!(text.contains("You are a helpful agent."));
        assert!(text.contains("Let me list the files."));
        assert!(text.contains("a.txt"));
        assert!(text.contains("Done — there are two files."));
    }
}
