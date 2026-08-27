//! How a transcript renders as HTML.
//!
//! Split out of `render.rs` to pay for the reasoning affordance (#437
//! phase 4) rather than raise that file's budget — the size ratchets go
//! one way. It is a cohesive block on its own terms: everything here
//! answers "what does one turn look like on the page", and nothing else
//! in `render` needs it.

use fq_ops::transcript::{AssistantToolCall, TranscriptEntry};

use super::{age, esc};

/// The terminal phase, when the transcript is closed by an Outcome.
pub fn transcript_outcome(entries: &[TranscriptEntry]) -> Option<&str> {
    entries.iter().rev().find_map(|e| match e {
        TranscriptEntry::Outcome { phase, .. } => Some(phase.as_str()),
        _ => None,
    })
}

/// The transcript's liveness footer. Carries `id="status"` so the SSE
/// stream can patch it in place (datastar's default outer-morph
/// matches by id) when the run reaches its outcome.
pub fn transcript_status_html(outcome: Option<&str>) -> String {
    match outcome {
        None => {
            r#"<p id="status" class="muted">⟳ live — new turns appear as the run progresses</p>"#
                .to_string()
        }
        Some("completed") => {
            r#"<p id="status" class="ok">■ run completed — no more turns expected</p>"#.to_string()
        }
        Some(phase) => format!(
            r#"<p id="status" class="bad">■ run {} — no more turns expected</p>"#,
            esc(phase)
        ),
    }
}

/// One transcript entry as a standalone HTML fragment — used by the
/// static page and shipped verbatim over the SSE stream as a
/// datastar element patch.
pub fn transcript_entry_html(entry: &TranscriptEntry, now_ms: i64) -> String {
    let mut b = String::new();
    {
        match entry {
            TranscriptEntry::Prompt {
                timestamp_ms,
                system,
                user,
            } => {
                b.push_str(&format!(
                    r#"<div class="turn"><h3>prompt <span class="muted">{}</span></h3>"#,
                    esc(&age(*timestamp_ms, now_ms))
                ));
                if let Some(s) = system {
                    b.push_str(&format!(
                        "<details><summary>system prompt ({} bytes)</summary><pre>{}</pre></details>",
                        s.len(),
                        esc(s)
                    ));
                }
                if let Some(u) = user {
                    b.push_str(&format!("<pre>{}</pre>", esc(u)));
                }
                b.push_str("</div>");
            }
            TranscriptEntry::Assistant {
                timestamp_ms,
                model,
                content,
                reasoning,
                tool_calls,
                cost_usd,
                is_error,
            } => {
                let err = matches!(is_error, Some(true));
                let cost = cost_usd.map(|c| format!(" · ${c:.4}")).unwrap_or_default();
                b.push_str(&format!(
                    r#"<div class="turn{}"><h3>assistant · {}{} <span class="muted">{}</span>{}</h3>"#,
                    if err { " err" } else { "" },
                    esc(model),
                    esc(&cost),
                    esc(&age(*timestamp_ms, now_ms)),
                    if err { r#" <span class="bad">error</span>"# } else { "" },
                ));
                if let Some(r) = reasoning {
                    b.push_str(&reasoning_html(r));
                }
                if let Some(c) = content {
                    b.push_str(&format!("<pre>{}</pre>", esc(c)));
                }
                for tc in tool_calls {
                    b.push_str(&tool_call_html(tc));
                }
                b.push_str("</div>");
            }
            TranscriptEntry::ToolResult {
                timestamp_ms,
                tool_call_id,
                tool_name,
                parameters,
                output,
                is_error,
            } => {
                let err = matches!(is_error, Some(true));
                b.push_str(&format!(
                    r#"<div class="turn{}"><h3>tool result · {} <span class="muted">{} · {}</span>{}</h3>"#,
                    if err { " err" } else { "" },
                    esc(tool_name),
                    esc(tool_call_id),
                    esc(&age(*timestamp_ms, now_ms)),
                    if err { r#" <span class="bad">error</span>"# } else { "" },
                ));
                let params = serde_json::to_string_pretty(parameters)
                    .unwrap_or_else(|_| parameters.to_string());
                b.push_str(&format!(
                    "<details><summary>parameters</summary><pre>{}</pre></details>",
                    esc(&params)
                ));
                match output {
                    Some(o) => b.push_str(&format!("<pre>{}</pre>", esc(o))),
                    None => b.push_str(r#"<p class="muted">(no output recorded)</p>"#),
                }
                b.push_str("</div>");
            }
            TranscriptEntry::Outcome {
                timestamp_ms,
                phase,
            } => {
                let ok = phase == "completed";
                b.push_str(&format!(
                    r#"<div class="turn{}"><h3><span class="{}">run {}</span> <span class="muted">{}</span></h3></div>"#,
                    if ok { "" } else { " err" },
                    if ok { "ok" } else { "bad" },
                    esc(phase),
                    esc(&age(*timestamp_ms, now_ms)),
                ));
            }
        }
    }
    b
}

/// An assistant turn's reasoning, collapsed by default.
///
/// **Collapsed, not omitted.** Reasoning is the least useful part of a
/// transcript read for *what happened*, and can be long — so it folds
/// away. But a turn that produced reasoning must never render as one
/// that did not, which is the difference between presentation collapsing
/// a distinction and a data model losing it (ADR-0034 D4).
///
/// The opaque case is the one that makes this matter. A block we cannot
/// read still carries what the model was working from, so it gets its own
/// line and its raw form behind a second disclosure — "we hold this, we
/// cannot render it, here it is" — rather than being silently dropped for
/// having no text.
fn reasoning_html(reasoning: &fq_ops::transcript::TurnReasoning) -> String {
    let mut b = String::new();
    let label = match (&reasoning.text, &reasoning.opaque) {
        (Some(_), Some(_)) => "reasoning · signed",
        (Some(_), None) => "reasoning",
        (None, Some(_)) => "reasoning · opaque",
        // `reduce_reasoning` never builds this, but rendering must not
        // depend on that invariant holding elsewhere.
        (None, None) => "reasoning",
    };
    b.push_str(&format!(
        r#"<details class="reasoning"><summary>{}</summary>"#,
        esc(label)
    ));
    match &reasoning.text {
        Some(text) => b.push_str(&format!("<pre>{}</pre>", esc(text))),
        None => b.push_str(
            r#"<p class="muted">No readable text — this turn's reasoning is carried as an opaque provider token.</p>"#,
        ),
    }
    if let Some(raw) = &reasoning.opaque {
        b.push_str(&format!(
            r#"<details class="raw"><summary>opaque — click to see raw</summary><pre>{}</pre></details>"#,
            esc(&serde_json::to_string_pretty(raw).unwrap_or_else(|_| raw.to_string()))
        ));
    }
    b.push_str("</details>");
    b
}

fn tool_call_html(tc: &AssistantToolCall) -> String {
    let params =
        serde_json::to_string_pretty(&tc.parameters).unwrap_or_else(|_| tc.parameters.to_string());
    format!(
        r#"<p>→ tool call <b>{}</b> <span class="muted">{}</span></p><pre>{}</pre>"#,
        esc(&tc.tool_name),
        esc(&tc.tool_call_id),
        esc(&params)
    )
}
