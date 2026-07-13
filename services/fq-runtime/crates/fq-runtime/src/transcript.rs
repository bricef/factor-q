//! Payload-bearing transcript for one invocation.
//!
//! `fq invocation transcript <id>` reconstructs the full conversation
//! transcript — LLM turns and tool calls with their payloads — from the
//! worker WAL (`llm_dispatch` + `tool_dispatch`), which is the only place
//! the payloads are persisted (the projection stores headers only).

use std::collections::HashSet;

use crate::ChatResponse;
use crate::events::{LlmResponsePayload, Message, MessageRole, ToolResultPayload};
use crate::worker::{LlmDispatchRow, ToolDispatchRow};
use serde::Serialize;
use serde_json::Value;

/// Default byte cap applied to each rendered payload chunk in pretty
/// mode. `--full` / `--no-truncate` lifts it. JSON output is never
/// truncated (it is for machines).
pub const DEFAULT_TRUNCATE_BYTES: usize = 2000;

/// One entry in the ordered transcript timeline.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptEntry {
    /// The system prompt + initial user message, reconstructed from the
    /// first `llm_dispatch.request_payload`. Rendered once at the top so
    /// the later per-turn deltas don't re-embed the whole history.
    Prompt {
        timestamp_ms: i64,
        system: Option<String>,
        user: Option<String>,
    },
    /// An assistant LLM turn: the model's text and any tool calls it
    /// requested, taken from `llm_dispatch.response`.
    Assistant {
        timestamp_ms: i64,
        model: String,
        content: Option<String>,
        tool_calls: Vec<AssistantToolCall>,
        cost_usd: Option<f64>,
        is_error: Option<bool>,
    },
    /// A tool result: name, parameters, and the tool's output, taken
    /// from a `tool_dispatch` row.
    ToolResult {
        timestamp_ms: i64,
        /// Correlation id linking this result to the assistant tool call
        /// that requested it. Present in both the WAL row and the live
        /// `tool.result` event, so it is the reliable dedup key at the
        /// snapshot→live seam.
        tool_call_id: String,
        tool_name: String,
        parameters: Value,
        output: Option<String>,
        is_error: Option<bool>,
    },
    /// The invocation's terminal outcome. Not a WAL dispatch row —
    /// synthesised by `views::transcript` from the invocation's
    /// state/archive record, so a transcript states explicitly whether
    /// more turns are expected. Absent while the run is in flight;
    /// always the final entry once present.
    Outcome {
        timestamp_ms: i64,
        /// The terminal phase, `completed` / `failed`.
        phase: String,
    },
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct AssistantToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
    pub parameters: Value,
}

impl TranscriptEntry {
    fn timestamp_ms(&self) -> i64 {
        match self {
            TranscriptEntry::Prompt { timestamp_ms, .. }
            | TranscriptEntry::Assistant { timestamp_ms, .. }
            | TranscriptEntry::ToolResult { timestamp_ms, .. }
            | TranscriptEntry::Outcome { timestamp_ms, .. } => *timestamp_ms,
        }
    }

    /// Ordering tiebreak within one `intent_at`: prompt first, then the
    /// assistant turn, then any tool result requested by it. Keeps a
    /// tool result rendering after the assistant turn that asked for it
    /// even when the two share a millisecond.
    fn order_class(&self) -> u8 {
        match self {
            TranscriptEntry::Prompt { .. } => 0,
            TranscriptEntry::Assistant { .. } => 1,
            TranscriptEntry::ToolResult { .. } => 2,
            TranscriptEntry::Outcome { .. } => 3,
        }
    }
}

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
        && let Some(entry) = prompt_from_request(first.intent_at, &first.request_payload)
    {
        entries.push(entry);
    }

    for row in llm_rows {
        let (content, tool_calls, is_error) = match &row.response {
            Some(raw) => parse_llm_response(raw),
            None => (None, Vec::new(), row.is_error),
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

    entries.sort_by(|a, b| {
        a.timestamp_ms()
            .cmp(&b.timestamp_ms())
            .then(a.order_class().cmp(&b.order_class()))
    });
    entries
}

/// Reconstruct the system prompt + first user message from a serialised
/// `LlmRequestPayload`. Returns `None` if the payload carries neither.
fn prompt_from_request(intent_at: i64, raw: &str) -> Option<TranscriptEntry> {
    let payload: LlmRequestLike = serde_json::from_str(raw).ok()?;
    let mut system = None;
    let mut user = None;
    for msg in &payload.messages {
        match msg.role {
            MessageRole::System if system.is_none() => system = msg.content.clone(),
            MessageRole::User if user.is_none() => user = msg.content.clone(),
            _ => {}
        }
    }
    if system.is_none() && user.is_none() {
        return None;
    }
    Some(TranscriptEntry::Prompt {
        timestamp_ms: intent_at,
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
fn parse_llm_response(raw: &str) -> (Option<String>, Vec<AssistantToolCall>, Option<bool>) {
    match serde_json::from_str::<ChatResponse>(raw) {
        Ok(resp) => {
            let calls = resp
                .tool_calls
                .into_iter()
                .map(|tc| AssistantToolCall {
                    tool_call_id: tc.tool_call_id.as_str().to_string(),
                    tool_name: tc.tool_name,
                    parameters: tc.parameters,
                })
                .collect();
            (resp.content, calls, None)
        }
        Err(_) => (Some(raw.to_string()), Vec::new(), None),
    }
}

/// Parse `s` as JSON, falling back to a JSON string containing the raw
/// text if it is not valid JSON. Tool parameters are stored as JSON
/// text but we never want a parse failure to lose the payload.
fn parse_json_lenient(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.to_string()))
}

// ------------------------------------------------------------------
// Rendering
// ------------------------------------------------------------------

/// Truncate `s` to at most `max` bytes (on a char boundary), appending a
/// notice when trimmed. `None` = no truncation.
fn truncate(s: &str, max: Option<usize>) -> String {
    match max {
        None => s.to_string(),
        Some(max) if s.len() <= max => s.to_string(),
        Some(max) => {
            let mut end = max;
            while end > 0 && !s.is_char_boundary(end) {
                end -= 1;
            }
            let omitted = s.len() - end;
            format!(
                "{}\n    … [truncated {omitted} bytes; --full to see everything]",
                &s[..end]
            )
        }
    }
}

fn render_params(params: &Value, max: Option<usize>) -> String {
    let text = serde_json::to_string_pretty(params).unwrap_or_else(|_| params.to_string());
    truncate(&text, max)
}

/// Cap every payload-bearing string field of `entries` at `max` bytes
/// (char-boundary safe, truncation marked — same policy as
/// `render_pretty`). Applied server-side by the read service so a
/// multi-megabyte transcript doesn't cross the wire just to render a
/// summary page; the dashboard's `?full=1` skips it, mirroring `--full`.
/// Tool-call *parameters* are left whole — they are typically small and
/// structurally JSON, where mid-string truncation would corrupt them.
pub fn truncate_entries(entries: &mut [TranscriptEntry], max: usize) {
    let cap = Some(max);
    for entry in entries {
        match entry {
            TranscriptEntry::Prompt { system, user, .. } => {
                if let Some(s) = system {
                    *s = truncate(s, cap);
                }
                if let Some(u) = user {
                    *u = truncate(u, cap);
                }
            }
            TranscriptEntry::Assistant { content, .. } => {
                if let Some(c) = content {
                    *c = truncate(c, cap);
                }
            }
            TranscriptEntry::ToolResult { output, .. } => {
                if let Some(o) = output {
                    *o = truncate(o, cap);
                }
            }
            // No payload strings to cap.
            TranscriptEntry::Outcome { .. } => {}
        }
    }
}

/// Render the transcript as human-readable pretty text. `truncate_bytes`
/// = `None` means no truncation (`--full`).
pub fn render_pretty(entries: &[TranscriptEntry], truncate_bytes: Option<usize>) -> String {
    let mut out = String::new();
    for entry in entries {
        match entry {
            TranscriptEntry::Prompt { system, user, .. } => {
                out.push_str("── prompt ─────────────────────────────────\n");
                if let Some(s) = system {
                    out.push_str("system:\n");
                    out.push_str(&indent(&truncate(s, truncate_bytes)));
                    out.push('\n');
                }
                if let Some(u) = user {
                    out.push_str("user:\n");
                    out.push_str(&indent(&truncate(u, truncate_bytes)));
                    out.push('\n');
                }
            }
            TranscriptEntry::Assistant {
                model,
                content,
                tool_calls,
                cost_usd,
                is_error,
                ..
            } => {
                let cost = cost_usd
                    .map(|c| format!(" cost=${c:.6}"))
                    .unwrap_or_default();
                let err = if *is_error == Some(true) {
                    " [error]"
                } else {
                    ""
                };
                out.push_str(&format!("── assistant (model={model}{cost}){err} ──\n"));
                match content {
                    Some(c) if !c.is_empty() => {
                        out.push_str(&indent(&truncate(c, truncate_bytes)));
                        out.push('\n');
                    }
                    _ => {}
                }
                for tc in tool_calls {
                    out.push_str(&format!(
                        "  → tool call: {} (id={})\n",
                        tc.tool_name, tc.tool_call_id
                    ));
                    out.push_str(&indent(&render_params(&tc.parameters, truncate_bytes)));
                    out.push('\n');
                }
            }
            TranscriptEntry::ToolResult {
                tool_name,
                parameters,
                output,
                is_error,
                ..
            } => {
                let err = if *is_error == Some(true) {
                    " [error]"
                } else {
                    ""
                };
                out.push_str(&format!("── tool result: {tool_name}{err} ──\n"));
                out.push_str("  parameters:\n");
                out.push_str(&indent(&render_params(parameters, truncate_bytes)));
                out.push('\n');
                if let Some(o) = output {
                    out.push_str("  output:\n");
                    out.push_str(&indent(&truncate(o, truncate_bytes)));
                    out.push('\n');
                }
            }
            TranscriptEntry::Outcome { phase, .. } => {
                out.push_str(&format!("── run {phase} ────────────────────────────\n"));
            }
        }
    }
    out
}

fn indent(s: &str) -> String {
    s.lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ------------------------------------------------------------------
// Live --follow support
// ------------------------------------------------------------------

/// Dedup key for the snapshot→live seam: identifies an already-rendered
/// entry so a live event carrying the same call is not printed twice.
///
/// - Tool results key on their `tool_call_id` (carried by both the WAL
///   row and the live event).
/// - Tool-requesting assistant turns key on their first tool call id.
/// - A text-only assistant turn has no id shared between the stored
///   `ChatResponse` and the live event, so it returns `None` (best
///   effort). This is low-risk: such a turn is either the final answer
///   (no live event follows a completed invocation) or a rare mid-run
///   text turn, and the seam window is a single WAL read.
pub fn dedup_key(entry: &TranscriptEntry) -> Option<String> {
    match entry {
        TranscriptEntry::Prompt { .. } => None,
        TranscriptEntry::Assistant { tool_calls, .. } => tool_calls
            .first()
            .map(|tc| format!("call:{}", tc.tool_call_id)),
        TranscriptEntry::ToolResult { tool_call_id, .. } => Some(format!("tool:{tool_call_id}")),
        // At most one outcome exists per invocation; a fixed key keeps
        // the snapshot→live seam from ever printing it twice.
        TranscriptEntry::Outcome { .. } => Some("outcome".to_string()),
    }
}

/// Build a set of dedup keys already covered by the snapshot, so the
/// live renderer can skip re-printing them.
pub fn snapshot_keys(entries: &[TranscriptEntry]) -> HashSet<String> {
    entries.iter().filter_map(dedup_key).collect()
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
        .tool_calls
        .iter()
        .map(|tc| AssistantToolCall {
            tool_call_id: tc.tool_call_id.as_str().to_string(),
            tool_name: tc.tool_name.clone(),
            parameters: tc.parameters.clone(),
        })
        .collect();
    TranscriptEntry::Assistant {
        timestamp_ms,
        model,
        content: payload.content.clone(),
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
        }
    }

    const FIRST_REQUEST: &str = r#"{"messages":[
        {"role":"system","content":"You are a helpful agent."},
        {"role":"user","content":"List the files."}
    ]}"#;

    // These mirror the *real* wire shape the WAL persists: a serialised
    // `ChatResponse` (content + tool_calls + stop_reason + usage) — NO
    // `call_id`. A fixture that adds `call_id` would spuriously parse as
    // an `LlmResponsePayload` and hide the response-type mismatch.
    fn response_with_tool_call() -> String {
        serde_json::json!({
            "content": "Let me list the files.",
            "tool_calls": [{
                "tool_call_id": "tc-100",
                "tool_name": "shell",
                "parameters": {"cmd": "ls"}
            }],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })
        .to_string()
    }

    fn response_final() -> String {
        serde_json::json!({
            "content": "Done — there are two files.",
            "tool_calls": [],
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
        let text = render_pretty(&entries, Some(DEFAULT_TRUNCATE_BYTES));

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

        let truncated = render_pretty(&entries, Some(DEFAULT_TRUNCATE_BYTES));
        assert!(truncated.contains("truncated"), "should note truncation");
        assert!(truncated.len() < result.len(), "truncated output shorter");

        let full = render_pretty(&entries, None);
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
        assert!(render_pretty(&entries, None).is_empty());
    }

    #[test]
    fn snapshot_keys_capture_tool_call_and_result_ids() {
        // Both the requesting assistant turn and the tool result must be
        // deduped at the --follow seam, under distinct prefixes.
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
    }
    #[tokio::test]
    async fn store_round_trip_transcript_ordering_and_payloads() {
        // Full store-level exercise per the issue's acceptance
        // criterion: build a temp events.db, write intent+completed
        // rows into both dispatch tables (mirroring worker/store.rs
        // tests), read them back through the same list helpers the
        // CLI uses, and assert the collected transcript's ordering
        // and payload content.
        use crate::WorkerStore;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let path = dir.path().join("events.db");
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

        let text = render_pretty(&entries, Some(DEFAULT_TRUNCATE_BYTES));
        assert!(text.contains("You are a helpful agent."));
        assert!(text.contains("Let me list the files."));
        assert!(text.contains("a.txt"));
        assert!(text.contains("Done — there are two files."));
    }
}
