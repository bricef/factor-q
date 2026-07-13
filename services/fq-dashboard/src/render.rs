//! Pure HTML rendering over the wire DTOs — `format!` and string
//! concatenation only, by design (#105 layer 3): zero client-side JS,
//! zero template engine, `<meta refresh>` for liveness. Every dynamic
//! string goes through [`esc`]. Pure functions so the pages are
//! unit-testable without HTTP or a runtime.

use fq_runtime::health::{ConsumerHealth, StreamHealth};
use fq_runtime::read_service::HealthReport;
use fq_runtime::transcript::{AssistantToolCall, TranscriptEntry};
use fq_runtime::views::{
    ActiveInvocationView, CostReport, EventView, InvocationDetailView, InvocationSummaryView,
};

/// Minimal HTML escape for text and attribute positions.
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Human age from epoch-ms relative to `now_ms` ("12s ago", "3m ago").
pub fn age(ms: i64, now_ms: i64) -> String {
    let secs = (now_ms.saturating_sub(ms)) / 1000;
    if secs < 0 {
        return "in the future".to_string();
    }
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// The shared page shell: title, auto-refresh, nav, tiny inline CSS.
pub fn page(title: &str, refresh_secs: u64, body: &str) -> String {
    page_opts(title, Some(refresh_secs), "", body)
}

/// Page shell with explicit head control. `refresh_secs: None` drops
/// the meta-refresh but keeps a `<noscript>` fallback refresh — used by
/// the SSE-streamed transcript page, where a page reload every 5s would
/// defeat the stream (no-JS browsers keep polling instead).
pub fn page_opts(title: &str, refresh_secs: Option<u64>, extra_head: &str, body: &str) -> String {
    let title = esc(title);
    let refresh = match refresh_secs {
        Some(secs) => format!(r#"<meta http-equiv="refresh" content="{secs}">"#),
        None => r#"<noscript><meta http-equiv="refresh" content="5"></noscript>"#.to_string(),
    };
    format!(
        r#"<!doctype html>
<html><head>
<meta charset="utf-8">
{refresh}{extra_head}
<title>{title} — fq</title>
<style>
/* Dark by default (owner preference). Semantics keep their hue —
   ok green / warn amber / bad red — tuned for contrast on the dark
   ground; a light theme can arrive later as a prefers-color-scheme
   override. */
:root {{ color-scheme: dark; }}
body {{ font-family: monospace; margin: 1.5rem; color: #d4d7dc; background: #14161a; }}
h1 {{ font-size: 1.2rem; }} h2 {{ font-size: 1rem; margin-top: 1.5rem; }}
a {{ color: #7aa2e8; }}
table {{ border-collapse: collapse; margin: 0.5rem 0; }}
th, td {{ border: 1px solid #3a3f47; padding: 0.25rem 0.6rem; text-align: left; }}
th {{ background: #21252b; }}
nav a {{ margin-right: 1rem; }}
.ok {{ color: #5fbf77; }} .warn {{ color: #d9a04c; }} .bad {{ color: #e06c6c; }}
.muted {{ color: #7d838c; }}
pre {{ background: #1c2026; border: 1px solid #333941; padding: 0.5rem; white-space: pre-wrap; overflow-wrap: anywhere; margin: 0.3rem 0; max-width: 72rem; }}
details {{ margin: 0.3rem 0; }} summary {{ cursor: pointer; color: #9aa1ab; }}
.turn {{ border-left: 3px solid #3a3f47; padding-left: 0.8rem; margin: 1.2rem 0; }}
.turn h3 {{ font-size: 1rem; margin: 0.2rem 0; }}
.turn.err {{ border-left-color: #e06c6c; }}
/* The transcript timeline scrolls inside its own panel and OPENS AT
   THE BOTTOM: column-reverse anchors scroll to the newest entry with
   zero JS, and keeps it pinned as the SSE stream adds turns. Contract:
   the DOM holds entries NEWEST-FIRST (the server renders reversed and
   the stream prepends); column-reverse flips them back so the visual
   order stays oldest-at-top. */
#turns {{ display: flex; flex-direction: column-reverse; overflow-y: auto; max-height: calc(100vh - 16rem); border-top: 1px solid #21252b; border-bottom: 1px solid #21252b; }}
</style>
</head><body>
<nav><a href="/">health</a><a href="/invocations">invocations</a><a href="/events">events</a><a href="/costs">costs</a></nav>
<h1>{title}</h1>
{body}
</body></html>
"#
    )
}

/// The "runtime unreachable" page — the dashboard's own crash-domain
/// contract: it renders this rather than breaking (plan, layer 3).
pub fn unreachable(read_addr: &str, error: &str, last_seen_ms: Option<i64>, now_ms: i64) -> String {
    let seen = match last_seen_ms {
        Some(ms) => format!("last seen {}", age(ms, now_ms)),
        None => "never seen since this dashboard started".to_string(),
    };
    format!(
        r#"<p class="bad">runtime unreachable at {} — {}</p><p class="muted">{}</p>"#,
        esc(read_addr),
        esc(error),
        esc(&seen),
    )
}

fn short(id: &str) -> String {
    esc(&id.chars().take(8).collect::<String>())
}

fn inv_link(id: &str) -> String {
    format!(r#"<a href="/invocations/{}">{}</a>"#, esc(id), short(id))
}

/// The health page body.
pub fn health(report: &HealthReport, now_ms: i64) -> String {
    let mut b = String::new();

    b.push_str(&format!(
        r#"<p>daemon <span class="ok">reachable</span> — version {}</p>"#,
        esc(&report.version)
    ));

    b.push_str("<h2>Streams</h2><table><tr><th>stream</th><th>messages</th><th>consumer</th><th>state</th><th>lag</th><th>pending</th></tr>");
    for s in &report.streams {
        match s {
            StreamHealth::Unavailable { stream, error } => {
                b.push_str(&format!(
                    r#"<tr><td>{}</td><td colspan="5" class="bad">✗ {}</td></tr>"#,
                    esc(stream),
                    esc(error)
                ));
            }
            StreamHealth::Available {
                stream,
                messages,
                consumer,
                ..
            } => {
                let (cname, cstate, lag, pending) = match consumer {
                    ConsumerHealth::Active {
                        name,
                        lag,
                        ack_pending,
                        num_pending,
                        ..
                    } => {
                        let state = if *lag == 0 {
                            r#"<span class="ok">✓ caught up</span>"#.to_string()
                        } else if *lag < 10 {
                            r#"<span class="warn">◐ slightly behind</span>"#.to_string()
                        } else {
                            r#"<span class="bad">✗ lagging</span>"#.to_string()
                        };
                        (
                            esc(name),
                            state,
                            lag.to_string(),
                            format!("ack {ack_pending} / num {num_pending}"),
                        )
                    }
                    ConsumerHealth::Missing { name } => (
                        esc(name),
                        r#"<span class="muted">not present</span>"#.to_string(),
                        "-".to_string(),
                        "-".to_string(),
                    ),
                    ConsumerHealth::Error { name, error } => (
                        esc(name),
                        format!(r#"<span class="bad">✗ {}</span>"#, esc(error)),
                        "-".to_string(),
                        "-".to_string(),
                    ),
                };
                b.push_str(&format!(
                    "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                    esc(stream),
                    messages,
                    cname,
                    cstate,
                    lag,
                    pending
                ));
            }
        }
    }
    b.push_str("</table>");

    b.push_str("<h2>Runtime</h2><table>");
    b.push_str(&format!(
        "<tr><th>projection events</th><td>{}</td></tr>",
        report.event_count
    ));
    let exec_class = if report.executions.stuck > 0 {
        "bad"
    } else {
        "ok"
    };
    b.push_str(&format!(
        r#"<tr><th>executions</th><td class="{}">{} in-flight ({} stuck{})</td></tr>"#,
        exec_class,
        report.executions.in_flight,
        report.executions.stuck,
        if report.executions.stuck > 0 {
            format!(
                ": {}",
                report
                    .executions
                    .stuck_ids
                    .iter()
                    .map(|id| inv_link(id))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            String::new()
        }
    ));
    let rec_class = if report.recovery.ambiguous > 0 || report.recovery.stale_workers > 0 {
        "warn"
    } else {
        "ok"
    };
    b.push_str(&format!(
        r#"<tr><th>recovery</th><td class="{}">{} ambiguous, {} stale workers</td></tr>"#,
        rec_class, report.recovery.ambiguous, report.recovery.stale_workers
    ));
    b.push_str("</table>");

    if !report.failures.is_empty() {
        b.push_str("<h2>Permanent failures</h2><table><tr><th>kind</th><th>count</th></tr>");
        for f in &report.failures {
            b.push_str(&format!(
                "<tr><td>{}</td><td>{}</td></tr>",
                esc(&f.error_kind),
                f.count
            ));
        }
        b.push_str("</table>");
    }

    b.push_str(&format!(
        r#"<p class="muted">rendered {}</p>"#,
        esc(&age(now_ms, now_ms))
    ));
    b
}

/// The transcript page body: the step-by-step conversation — prompt,
/// assistant turns, tool results — with payloads. Every dynamic string
/// is escaped: tool output is the first *attacker-influenced* content
/// the dashboard renders (a tool result can contain arbitrary HTML).
/// Tool output is shown verbatim and NOT redacted (#72) — the page
/// says so.
pub fn transcript(
    entries: &[TranscriptEntry],
    now_ms: i64,
    full: bool,
    invocation_id: &str,
) -> String {
    let mut b = String::new();
    let toggle = if full {
        format!(
            r#"full payloads — <a href="/invocations/{}/transcript">truncated view</a>"#,
            esc(invocation_id)
        )
    } else {
        format!(
            r#"payloads truncated per chunk — <a href="/invocations/{}/transcript?full=1">full view</a>"#,
            esc(invocation_id)
        )
    };
    b.push_str(&format!(
        r#"<p class="muted">verbatim tool output, not redacted — may contain secrets (#72). {toggle} · <a href="/invocations/{}">detail</a></p>"#,
        esc(invocation_id)
    ));

    // NEWEST-FIRST in the DOM: #turns is a column-reverse scroll panel
    // (see the shell CSS), so reversed emission renders oldest-at-top
    // visually while the panel opens — and stays — at the newest turn.
    b.push_str(r#"<div id="turns">"#);
    for entry in entries.iter().rev() {
        b.push_str(&transcript_entry_html(entry, now_ms));
    }
    b.push_str("</div>");
    if entries.is_empty() {
        b.push_str(r#"<p class="muted">no transcript entries.</p>"#);
    }
    b.push_str(&transcript_status_html(transcript_outcome(entries)));
    b
}

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

/// The "active right now" table: currently-executing invocations from
/// the worker WAL. Renders to NOTHING when nothing is in flight — the
/// page contract is that the section only exists when there is live
/// work to show.
pub fn active(items: &[ActiveInvocationView], now_ms: i64) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut b = String::new();
    b.push_str("<h2>Active now</h2><table><tr><th>invocation</th><th>agent</th><th>phase</th><th>step</th><th>started</th><th>last advanced</th><th>doing</th></tr>");
    for i in items {
        let mut doing: Vec<String> = i
            .open_tools
            .iter()
            .map(|t| format!("tool {}", esc(t)))
            .collect();
        doing.extend(i.open_llms.iter().map(|m| format!("llm {}", esc(m))));
        let doing = if doing.is_empty() {
            r#"<span class="muted">—</span>"#.to_string()
        } else {
            doing.join(", ")
        };
        b.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            inv_link(&i.invocation_id),
            esc(&i.agent_id),
            esc(&i.phase),
            i.step_index,
            esc(&age(i.started_at_ms, now_ms)),
            esc(&age(i.updated_at_ms, now_ms)),
            doing
        ));
    }
    b.push_str("</table>");
    b
}

/// The full invocations page body: the active table above the list,
/// omitted entirely when nothing is in flight (in which case the page
/// is byte-identical to the plain list). The list only earns its own
/// heading when the active section exists above it.
pub fn invocations_page(
    active_rows: &[ActiveInvocationView],
    items: &[InvocationSummaryView],
    include_archived: bool,
    now_ms: i64,
) -> String {
    let active_html = active(active_rows, now_ms);
    let list_html = invocations(items, include_archived, now_ms);
    if active_html.is_empty() {
        list_html
    } else {
        format!("{active_html}<h2>All invocations</h2>{list_html}")
    }
}

/// The invocations list body.
pub fn invocations(items: &[InvocationSummaryView], include_archived: bool, now_ms: i64) -> String {
    let mut b = String::new();
    b.push_str(&format!(
        r#"<p><a href="/invocations{}">{}</a></p>"#,
        if include_archived { "" } else { "?archived=1" },
        if include_archived {
            "hide archived"
        } else {
            "show archived"
        }
    ));
    if items.is_empty() {
        b.push_str(r#"<p class="muted">no invocations.</p>"#);
        return b;
    }
    b.push_str(
        "<table><tr><th>invocation</th><th>status</th><th>started</th><th>agent</th><th>worker</th><th>archived</th></tr>",
    );
    for i in items {
        b.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            inv_link(&i.invocation_id),
            esc(&i.status),
            esc(&age(i.started_at_ms, now_ms)),
            esc(i.agent_id.as_deref().unwrap_or("?")),
            short(&i.worker_id),
            if i.archived { "yes" } else { "no" }
        ));
    }
    b.push_str("</table>");
    b
}

/// The single-invocation detail body — including the live "what is it
/// doing right now" block when the invocation is in flight.
pub fn invocation_detail(d: &InvocationDetailView, now_ms: i64) -> String {
    let mut b = String::new();
    b.push_str("<table>");
    b.push_str(&format!(
        "<tr><th>invocation</th><td>{}</td></tr>",
        esc(&d.invocation_id)
    ));
    if let Some(agent) = &d.agent_id {
        b.push_str(&format!("<tr><th>agent</th><td>{}</td></tr>", esc(agent)));
    }
    if let Some(o) = &d.owner {
        b.push_str(&format!(
            "<tr><th>status</th><td>{}</td></tr><tr><th>worker</th><td>{}</td></tr>",
            esc(&o.status),
            esc(&o.worker_id)
        ));
    }
    if let Some(a) = &d.archive {
        b.push_str(&format!(
            "<tr><th>archived</th><td>phase={} {}</td></tr>",
            esc(&a.final_phase),
            esc(&age(a.archived_at_ms, now_ms))
        ));
    }
    b.push_str("</table>");
    b.push_str(&format!(
        r#"<p><a href="/invocations/{}/transcript">transcript →</a></p>"#,
        esc(&d.invocation_id)
    ));

    if let Some(live) = &d.live {
        b.push_str("<h2>Live execution</h2><table>");
        b.push_str(&format!(
            "<tr><th>phase</th><td>{}</td></tr><tr><th>step</th><td>{}</td></tr><tr><th>last advanced</th><td>{}</td></tr>",
            esc(&live.phase),
            live.step_index,
            esc(&age(live.updated_at_ms, now_ms))
        ));
        b.push_str("</table>");
        let open_tools: Vec<_> = live
            .tools
            .iter()
            .filter(|t| t.status != "completed")
            .collect();
        let open_llms: Vec<_> = live
            .llms
            .iter()
            .filter(|l| l.status != "completed")
            .collect();
        if !open_tools.is_empty() || !open_llms.is_empty() {
            b.push_str("<table><tr><th>dispatch</th><th>what</th><th>state</th></tr>");
            for t in open_tools {
                b.push_str(&format!(
                    "<tr><td>tool</td><td>{}</td><td>{}</td></tr>",
                    esc(&t.tool_name),
                    esc(&t.status)
                ));
            }
            for l in open_llms {
                b.push_str(&format!(
                    "<tr><td>llm</td><td>{}</td><td>{}</td></tr>",
                    esc(&l.model),
                    esc(&l.status)
                ));
            }
            b.push_str("</table>");
        }
    }

    if !d.recent_events.is_empty() {
        b.push_str("<h2>Recent events</h2><table><tr><th>timestamp</th><th>event</th></tr>");
        for e in &d.recent_events {
            b.push_str(&format!(
                "<tr><td>{}</td><td>{}</td></tr>",
                esc(e.timestamp.get(..19).unwrap_or(&e.timestamp)),
                esc(&e.event_type)
            ));
        }
        b.push_str("</table>");
    }
    b
}

/// The events page body.
pub fn events(rows: &[EventView]) -> String {
    if rows.is_empty() {
        return r#"<p class="muted">no events matched.</p>"#.to_string();
    }
    let mut b = String::new();
    b.push_str(
        "<table><tr><th>timestamp</th><th>agent</th><th>event</th><th>cost</th><th>invocation</th></tr>",
    );
    for r in rows {
        let cost = r
            .total_cost
            .map(|c| format!("${c:.6}"))
            .unwrap_or_else(|| "-".to_string());
        b.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            esc(r.timestamp.get(..19).unwrap_or(&r.timestamp)),
            esc(&r.agent_id),
            esc(&r.event_type),
            esc(&cost),
            inv_link(&r.invocation_id)
        ));
    }
    b.push_str("</table>");
    b
}

/// The costs page body.
pub fn costs(report: &CostReport) -> String {
    if report.agents.is_empty() {
        return r#"<p class="muted">no cost events recorded.</p>"#.to_string();
    }
    let mut b = String::new();
    b.push_str(
        "<table><tr><th>agent</th><th>events</th><th>input tokens</th><th>output tokens</th><th>total cost</th></tr>",
    );
    for a in &report.agents {
        b.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>${:.6}</td></tr>",
            esc(&a.agent_id),
            a.event_count,
            a.total_input_tokens,
            a.total_output_tokens,
            a.total_cost
        ));
    }
    b.push_str(&format!(
        r#"<tr><th>total</th><td></td><td>{}</td><td>{}</td><td><b>${:.6}</b></td></tr>"#,
        report.total_input_tokens, report.total_output_tokens, report.total_cost
    ));
    b.push_str("</table>");
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esc_neutralises_html() {
        assert_eq!(
            esc(r#"<script>alert("x&y")</script>"#),
            "&lt;script&gt;alert(&quot;x&amp;y&quot;)&lt;/script&gt;"
        );
    }

    #[test]
    fn age_renders_units() {
        assert_eq!(age(0, 12_000), "12s ago");
        assert_eq!(age(0, 120_000), "2m ago");
        assert_eq!(age(0, 7_200_000), "2h ago");
        assert_eq!(age(0, 172_800_000), "2d ago");
    }

    #[test]
    fn page_carries_refresh_and_escaped_title() {
        let html = page("a<b", 7, "<p>x</p>");
        assert!(html.contains(r#"content="7""#));
        assert!(html.contains("a&lt;b"));
        assert!(html.contains("<p>x</p>"));
    }

    #[test]
    fn unreachable_shows_last_seen_or_never() {
        let never = unreachable("127.0.0.1:9471", "refused", None, 1_000);
        assert!(never.contains("never seen"));
        let seen = unreachable("127.0.0.1:9471", "refused", Some(0), 30_000);
        assert!(seen.contains("last seen 30s ago"));
    }

    /// Tool output is attacker-influenced content: markup in a payload
    /// must render as text, never as HTML.
    #[test]
    fn transcript_escapes_hostile_payloads() {
        let entries = vec![fq_runtime::transcript::TranscriptEntry::ToolResult {
            timestamp_ms: 0,
            tool_call_id: "tc-1".into(),
            tool_name: "exec".into(),
            parameters: serde_json::json!({"cmd": "<img src=x onerror=alert(1)>"}),
            output: Some("<script>alert('pwned')</script>".into()),
            is_error: Some(true),
        }];
        let html = transcript(&entries, 1_000, false, "inv-1");
        assert!(!html.contains("<script>"), "raw script leaked: {html}");
        assert!(html.contains("&lt;script&gt;"), "got: {html}");
        assert!(!html.contains("<img"), "raw img leaked: {html}");
        // Error results are visually flagged.
        assert!(html.contains(r#"class="turn err""#), "got: {html}");
        // Truncated view links to the full one.
        assert!(
            html.contains("/invocations/inv-1/transcript?full=1"),
            "got: {html}"
        );
    }

    /// The scroll-panel contract: DOM order is newest-first (the
    /// column-reverse panel flips it back visually), so the page opens
    /// at — and stays pinned to — the latest turn.
    #[test]
    fn transcript_dom_holds_entries_newest_first() {
        use fq_runtime::transcript::TranscriptEntry;
        let entries = vec![
            TranscriptEntry::Prompt {
                timestamp_ms: 0,
                system: None,
                user: Some("FIRST".into()),
            },
            TranscriptEntry::Outcome {
                timestamp_ms: 9,
                phase: "completed".to_string(),
            },
        ];
        let html = transcript(&entries, 10_000, true, "inv-1");
        let first = html.find("FIRST").expect("prompt rendered");
        let outcome = html.find("run completed").expect("outcome rendered");
        assert!(
            outcome < first,
            "newest entry must come first in the DOM (column-reverse flips it back)"
        );
    }

    #[test]
    fn transcript_renders_all_entry_kinds() {
        use fq_runtime::transcript::{AssistantToolCall, TranscriptEntry};
        let entries = vec![
            TranscriptEntry::Prompt {
                timestamp_ms: 0,
                system: Some("sys".into()),
                user: Some("do the thing".into()),
            },
            TranscriptEntry::Assistant {
                timestamp_ms: 1_000,
                model: "claude-opus-4-8".into(),
                content: Some("on it".into()),
                tool_calls: vec![AssistantToolCall {
                    tool_call_id: "tc-1".into(),
                    tool_name: "exec".into(),
                    parameters: serde_json::json!({"command": "ls"}),
                }],
                cost_usd: Some(0.01),
                is_error: Some(false),
            },
        ];
        let html = transcript(&entries, 60_000, true, "inv-1");
        assert!(html.contains("system prompt (3 bytes)"), "got: {html}");
        assert!(html.contains("do the thing"));
        assert!(html.contains("assistant · claude-opus-4-8"));
        assert!(html.contains("tool call <b>exec</b>"));
        // Full view links back to the truncated one.
        assert!(html.contains(r#"href="/invocations/inv-1/transcript""#));
    }

    #[test]
    fn active_table_omitted_when_nothing_in_flight() {
        let items = [fq_runtime::views::InvocationSummaryView {
            invocation_id: "abc".into(),
            agent_id: None,
            worker_id: "w".into(),
            status: "completed".into(),
            assigned_at_ms: 0,
            started_at_ms: 0,
            archived: false,
        }];
        assert_eq!(active(&[], 1_000), "");
        // With no active rows the page is byte-identical to the plain list.
        assert_eq!(
            invocations_page(&[], &items, false, 1_000),
            invocations(&items, false, 1_000)
        );
    }

    #[test]
    fn active_table_shows_live_work_above_the_list() {
        let active_rows = [fq_runtime::views::ActiveInvocationView {
            invocation_id: "0123456789abcdef".into(),
            agent_id: "m0-issue-fix".into(),
            phase: "dispatching_tools".into(),
            step_index: 165,
            started_at_ms: 0,
            updated_at_ms: 540_000,
            open_tools: vec!["exec".into()],
            open_llms: vec![],
        }];
        let html = invocations_page(&active_rows, &[], false, 600_000);
        assert!(html.contains("Active now"), "got: {html}");
        assert!(html.contains(r#"<a href="/invocations/0123456789abcdef">01234567</a>"#));
        assert!(html.contains("tool exec"), "got: {html}");
        assert!(html.contains("<td>10m ago</td>"), "started age: {html}");
        assert!(html.contains("<td>1m ago</td>"), "advanced age: {html}");
        // The list below gains its heading only when active is present.
        assert!(html.contains("All invocations"), "got: {html}");
    }

    #[test]
    fn invocation_rows_escape_link_and_show_start() {
        let items = vec![fq_runtime::views::InvocationSummaryView {
            invocation_id: "0123456789abcdef".into(),
            agent_id: Some("<agent>".into()),
            worker_id: "w".into(),
            status: "in_flight".into(),
            assigned_at_ms: 600_000,
            started_at_ms: 600_000,
            archived: false,
        }];
        let html = invocations(&items, false, 1_200_000);
        assert!(html.contains(r#"<a href="/invocations/0123456789abcdef">01234567</a>"#));
        assert!(html.contains("&lt;agent&gt;"));
        assert!(!html.contains("<agent>"));
        assert!(html.contains("<th>started</th>"));
        assert!(html.contains("<td>10m ago</td>"), "got: {html}");
    }
}
