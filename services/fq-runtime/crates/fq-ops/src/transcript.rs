//! The transcript: how a conversation renders.
//!
//! The entry shape, and the pure operations over it — truncation,
//! pretty rendering, and the dedup keys that stop a snapshot and a
//! live tail printing the same turn twice. All of it is a function of
//! its input and nothing else.
//!
//! Building entries from what the daemon recorded — WAL rows, live
//! event payloads — stays in `fq-runtime` beside those types.

use std::collections::HashSet;

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

#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize, schemars::JsonSchema)]
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

/// Put a timeline in reading order: by timestamp, and within one
/// millisecond by kind — prompt, then the assistant turn, then the
/// tool results it asked for.
///
/// It sorts entries and consults nothing else, so it belongs with the
/// entries rather than with whatever assembled them; both the WAL-backed
/// build and any other source want the same order.
pub fn sort_timeline(entries: &mut [TranscriptEntry]) {
    entries.sort_by(|a, b| {
        a.timestamp_ms()
            .cmp(&b.timestamp_ms())
            .then(a.order_class().cmp(&b.order_class()))
    });
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
/// - The prompt keys on a constant, like the outcome: an invocation
///   has exactly one opening prompt, so "already printed one" is the
///   whole test. It needs a key now that the prompt arrives as a Turn
///   — `--follow` pins its stream cursor *before* reading the
///   snapshot, so an invocation that starts inside that window has its
///   prompt turn in both, and an unkeyed prompt would print twice.
pub fn dedup_key(entry: &TranscriptEntry) -> Option<String> {
    match entry {
        TranscriptEntry::Prompt { .. } => Some("prompt".to_string()),
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
