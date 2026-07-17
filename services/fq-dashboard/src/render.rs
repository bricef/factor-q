//! Pure HTML rendering over the wire DTOs — `format!` and string
//! concatenation only, by design (#105 layer 3): zero client-side JS,
//! zero template engine, `<meta refresh>` for liveness. Every dynamic
//! string goes through [`esc`]. Pure functions so the pages are
//! unit-testable without HTTP or a runtime.

use fq_runtime::health::{ConsumerHealth, StreamHealth};
use fq_runtime::read_service::{AgentDetailView, AgentsView, HealthReport};
use fq_runtime::transcript::{AssistantToolCall, TranscriptEntry};
use fq_runtime::views::{
    ActiveInvocationView, AgentCostDetailView, CostBucketView, CostReport, CostView, EventView,
    InvocationDetailView, InvocationSummaryView, ModelCostView,
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

/// The live page shell: the same chrome as [`page`], but instead of a
/// whole-page `<meta refresh>` the body sits in a `#main` region that
/// datastar re-fetches every `refresh_secs` seconds and morphs in
/// place (the `Datastar-Request` negotiation in main.rs). A tick
/// without a reload preserves everything the server HTML does not
/// dictate — open `<details>` folds (see [`fold`]), scroll position,
/// text selection. No-JS browsers keep working via the `<noscript>`
/// full-page refresh that [`page_opts`] emits for `None`.
pub fn live_page(title: &str, refresh_secs: u64, body: &str) -> String {
    page_opts(
        title,
        None,
        r#"<script type="module" src="/assets/datastar.js"></script>"#,
        &format!(
            // The poll target is the page's own URL (path AND query —
            // `/costs?window=7d` must poll `/costs?window=7d`), read
            // from `location` so no handler has to thread its URI into
            // the shell. Datastar expressions evaluate as JS.
            r#"<div id="main" data-on-interval__duration.{refresh_secs}s="@get(window.location.pathname + window.location.search)">{body}</div>"#,
        ),
    )
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
/* Numeric table cells: right-aligned, digits lined up. */
td.n {{ text-align: right; font-variant-numeric: tabular-nums; }}
/* Share-of-spend bar (costs page). Single-series magnitude only — the
   percentage text beside it carries the value; the bar is the glance. */
.bar {{ display: inline-block; vertical-align: baseline; width: 72px; height: 7px; background: #21252b; margin-right: 0.5rem; }}
.bar i {{ display: block; height: 100%; background: #7aa2e8; opacity: 0.55; }}
tr.sub td, tr.sub th {{ border-top: 2px solid #3a3f47; }}
/* Spend-over-time bars (costs page). Single series, one hue; the
   value lives in the hover title and on the tallest bar's label —
   quiet buckets render as gaps, not lies. */
.chart {{ display: flex; align-items: flex-end; gap: 3px; margin: 0.6rem 0 0.2rem; }}
.chart .cslot {{ display: flex; flex-direction: column; align-items: center; justify-content: flex-end; flex: 1; max-width: 36px; }}
.chart .cslot i {{ display: block; width: 100%; background: #7aa2e8; opacity: 0.55; }}
.chart .cslot b {{ font-size: 0.75rem; margin-bottom: 2px; }}
.chart .cslot span {{ font-size: 0.7rem; color: #7d838c; margin-top: 3px; }}
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
<nav><a href="/">health</a><a href="/invocations">invocations</a><a href="/events">events</a><a href="/costs">costs</a><a href="/agents">agents</a></nav>
<h1>{title}</h1>
{body}
</body></html>
"#
    )
}

/// The "runtime unreachable" page — the dashboard's own crash-domain
/// contract: it renders this rather than breaking (plan, layer 3).
/// The build-skew banner (#168): shown at the top of every page while
/// the daemon's last-observed build differs from this binary's. Loud
/// but warn-and-continue — the page under it still renders whatever
/// decoded. The wire is a length-framed binary codec, so cross-build
/// pairings can fail to decode; this banner is what turns that from
/// "runtime unreachable" (the #154 misdiagnosis) into an explanation
/// and a remedy.
pub fn skew_banner(own_sha: &str, daemon_sha: &str) -> String {
    format!(
        r#"<p class="warn"><b>⚠ build skew:</b> dashboard @{} · daemon @{} — some data may fail to load; redeploy to matching builds</p>"#,
        esc(own_sha),
        esc(daemon_sha),
    )
}

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

/// An agent id linking to its definition page.
fn agent_link(id: &str) -> String {
    format!(r#"<a href="/agents/{}">{}</a>"#, esc(id), esc(id))
}

/// A collapsible section that survives live-region morphs: the stable
/// id lets the morph pair the old and new nodes, and
/// `data-preserve-attr` keeps the reader's open/closed choice when a
/// poll re-renders the fold — without it every tick slams the fold
/// shut. `summary_html` / `body_html` are trusted markup; callers
/// escape their dynamic strings as usual.
fn fold(id: &str, summary_html: &str, body_html: &str) -> String {
    format!(
        r#"<details id="{}" data-preserve-attr="open"><summary>{summary_html}</summary>{body_html}</details>"#,
        esc(id),
    )
}

/// ": <link>, <link>" suffix for a count that carries ids; empty when
/// there are none.
fn linked_ids(ids: &[String]) -> String {
    if ids.is_empty() {
        return String::new();
    }
    format!(
        ": {}",
        ids.iter()
            .map(|id| inv_link(id))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The one-line invocation summary cell (#216): escaped, with a muted
/// em-dash when the summariser has not produced a line (disabled, or
/// the invocation just started).
fn summary_cell(summary: Option<&str>) -> String {
    match summary {
        Some(line) => format!(r#"<span class="muted">{}</span>"#, esc(line)),
        None => r#"<span class="muted">—</span>"#.to_string(),
    }
}

/// The health page body.
pub fn health(report: &HealthReport) -> String {
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
                        num_redelivered,
                        ..
                    } => {
                        let state = if *lag == 0 {
                            r#"<span class="ok">✓ caught up</span>"#.to_string()
                        } else if *lag < 10 {
                            r#"<span class="warn">◐ slightly behind</span>"#.to_string()
                        } else {
                            r#"<span class="bad">✗ lagging</span>"#.to_string()
                        };
                        let redelivery_suffix = if *num_redelivered > 0 {
                            format!(r#" / <span class="warn">redelivered {num_redelivered}</span>"#)
                        } else {
                            String::new()
                        };
                        (
                            esc(name),
                            state,
                            lag.to_string(),
                            format!("ack {ack_pending} / num {num_pending}{redelivery_suffix}"),
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
        r#"<tr><th>executions</th><td class="{}">{} in-flight ({} working{}, {} stuck{})</td></tr>"#,
        exec_class,
        report.executions.in_flight,
        report.executions.working,
        linked_ids(&report.executions.working_ids),
        report.executions.stuck,
        linked_ids(&report.executions.stuck_ids)
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
    summary: Option<&str>,
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
    // The one-line operator summary (#216): the "what is this run
    // about" header, snapshotted at page load (the SSE stream patches
    // turns, not this line — a reload refreshes it).
    if let Some(summary) = summary {
        b.push_str(&format!(
            r#"<p><span class="muted">summary —</span> {}</p>"#,
            esc(summary)
        ));
    }

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
    b.push_str("<h2>Active now</h2><table><tr><th>invocation</th><th>agent</th><th>summary</th><th>phase</th><th>step</th><th>started</th><th>last advanced</th><th>doing</th></tr>");
    for i in items {
        let mut doing: Vec<String> = i
            .open_tools
            .iter()
            .map(|t| match t.command.as_deref() {
                // The command is the operator's answer to "doing
                // what?" — show it muted beside the tool name,
                // display-capped (the wire already caps harder).
                Some(command) => format!(
                    r#"tool {} <span class="muted">— {}</span>"#,
                    esc(&t.tool_name),
                    esc(&display_cap(command, 72)),
                ),
                None => format!("tool {}", esc(&t.tool_name)),
            })
            .collect();
        doing.extend(i.open_llms.iter().map(|m| format!("llm {}", esc(m))));
        let doing = if doing.is_empty() {
            r#"<span class="muted">—</span>"#.to_string()
        } else {
            doing.join(", ")
        };
        b.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            inv_link(&i.invocation_id),
            agent_link(&i.agent_id),
            summary_cell(i.summary.as_deref()),
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

/// Which invocation rows the list shows. Archived rows are opt-in;
/// the terminal statuses are opt-out, so the default view keeps
/// history while letting an operator hide the routine outcomes and
/// focus on live or anomalous rows. Filter state rides the query
/// string, which the live region polls verbatim — toggles survive
/// ticks for free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvocationFilters {
    pub include_archived: bool,
    pub show_completed: bool,
    pub show_failed: bool,
}

impl Default for InvocationFilters {
    fn default() -> Self {
        InvocationFilters {
            include_archived: false,
            show_completed: true,
            show_failed: true,
        }
    }
}

impl InvocationFilters {
    /// The canonical query string for this state — only non-default
    /// values appear, so the default view keeps the bare URL.
    fn query(&self) -> String {
        let mut params: Vec<&str> = Vec::new();
        if self.include_archived {
            params.push("archived=1");
        }
        if !self.show_completed {
            params.push("completed=0");
        }
        if !self.show_failed {
            params.push("failed=0");
        }
        if params.is_empty() {
            String::new()
        } else {
            format!("?{}", params.join("&"))
        }
    }

    /// One toggle link: the current state with `flip` applied.
    fn toggle(&self, flip: fn(&mut Self), on_label: &str, off_label: &str, is_on: bool) -> String {
        let mut flipped = *self;
        flip(&mut flipped);
        format!(
            r#"<a href="/invocations{}">{}</a>"#,
            flipped.query(),
            if is_on { off_label } else { on_label },
        )
    }

    /// True when a terminal-status row is hidden by this state.
    fn hides(&self, status: &str) -> bool {
        (!self.show_completed && status == "completed") || (!self.show_failed && status == "failed")
    }
}

/// The full invocations page body: the active table above the list,
/// omitted entirely when nothing is in flight (in which case the page
/// is byte-identical to the plain list). The list only earns its own
/// heading when the active section exists above it.
pub fn invocations_page(
    active_rows: &[ActiveInvocationView],
    items: &[InvocationSummaryView],
    filters: InvocationFilters,
    now_ms: i64,
) -> String {
    let active_html = active(active_rows, now_ms);
    let list_html = invocations(items, filters, now_ms);
    if active_html.is_empty() {
        list_html
    } else {
        format!("{active_html}<h2>All invocations</h2>{list_html}")
    }
}

/// The invocations list body. Terminal-status filtering happens here,
/// over the already-fetched rows — the read service's status filter
/// selects one status, it cannot exclude, and the list is capped at
/// 100 rows anyway.
pub fn invocations(
    items: &[InvocationSummaryView],
    filters: InvocationFilters,
    now_ms: i64,
) -> String {
    let mut b = String::new();
    b.push_str(&format!(
        "<p>{} · {} · {}</p>",
        filters.toggle(
            |f| f.include_archived = !f.include_archived,
            "show archived",
            "hide archived",
            filters.include_archived,
        ),
        filters.toggle(
            |f| f.show_completed = !f.show_completed,
            "show completed",
            "hide completed",
            filters.show_completed,
        ),
        filters.toggle(
            |f| f.show_failed = !f.show_failed,
            "show failed",
            "hide failed",
            filters.show_failed,
        ),
    ));
    let items: Vec<&InvocationSummaryView> =
        items.iter().filter(|i| !filters.hides(&i.status)).collect();
    if items.is_empty() {
        if filters.show_completed && filters.show_failed {
            b.push_str(r#"<p class="muted">no invocations.</p>"#);
        } else {
            b.push_str(r#"<p class="muted">no invocations match the filters.</p>"#);
        }
        return b;
    }
    b.push_str(
        "<table><tr><th>invocation</th><th>status</th><th>summary</th><th>started</th><th>agent</th><th>worker</th><th>archived</th></tr>",
    );
    for i in items {
        b.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            inv_link(&i.invocation_id),
            esc(&i.status),
            summary_cell(i.summary.as_deref()),
            esc(&age(i.started_at_ms, now_ms)),
            match i.agent_id.as_deref() {
                Some(agent) => agent_link(agent),
                None => r#"<span class="muted">?</span>"#.to_string(),
            },
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
        b.push_str(&format!(
            "<tr><th>agent</th><td>{}</td></tr>",
            agent_link(agent)
        ));
    }
    if let Some(summary) = &d.summary {
        b.push_str(&format!(
            "<tr><th>summary</th><td>{}</td></tr>",
            esc(summary)
        ));
    }
    // Cost so far: live regions morph this every tick while the run
    // spends, so the row doubles as a burn meter on in-flight work.
    if let Some(cost) = &d.cost {
        b.push_str(&format!(
            r#"<tr><th>cost so far</th><td>${:.4} <span class="muted">· {} llm calls · {} in / {} out{}</span></td></tr>"#,
            cost.total_cost,
            fmt_grouped(cost.event_count),
            fmt_tokens(cost.total_input_tokens),
            fmt_tokens(cost.total_output_tokens),
            if cost.total_cache_read_tokens > 0 {
                format!(" · {} cache read", fmt_tokens(cost.total_cache_read_tokens))
            } else {
                String::new()
            },
        ));
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

/// The family prefix of a one-shot agent id — an id whose last
/// `-`-separated segment is a 32-hex uuid, the shape e2e suites and
/// probes stamp on ephemeral instances (`overspender-019f33…` →
/// `overspender`). Named agents return `None` and keep their own row;
/// one-shots collapse into a per-family line so 161 test instances
/// cannot bury the six agents an operator actually watches.
fn one_shot_family(agent_id: &str) -> Option<&str> {
    let (prefix, suffix) = agent_id.rsplit_once('-')?;
    let is_lower_hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    (!prefix.is_empty() && suffix.len() == 32 && suffix.bytes().all(is_lower_hex)).then_some(prefix)
}

/// Comma-grouped integer ("1,597") — the exact form, used in `title=`
/// hovers and small cells.
fn fmt_grouped(n: i64) -> String {
    let digits = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// Char-boundary display cap with an ellipsis — for one-line gists
/// inside table cells (the wire may carry more than a cell should).
fn display_cap(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let capped: String = s.chars().take(max_chars).collect();
        format!("{capped}…")
    }
}

/// Compact token count as bare text ("171.39M", "58.9K", "1,597") —
/// the cell-less sibling of [`token_cell`] for inline copy.
fn fmt_tokens(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1e6)
    } else if n >= 10_000 {
        format!("{:.1}K", n as f64 / 1e3)
    } else {
        fmt_grouped(n)
    }
}

/// A right-aligned token-count cell: compact in the cell ("171.39M",
/// "58.9K"), exact in the hover title. Small counts render exact with
/// no title.
fn token_cell(n: i64) -> String {
    if n >= 1_000_000 {
        format!(
            r#"<td class="n" title="{}">{:.2}M</td>"#,
            fmt_grouped(n),
            n as f64 / 1e6
        )
    } else if n >= 10_000 {
        format!(
            r#"<td class="n" title="{}">{:.1}K</td>"#,
            fmt_grouped(n),
            n as f64 / 1e3
        )
    } else {
        format!(r#"<td class="n">{}</td>"#, fmt_grouped(n))
    }
}

/// A share-of-spend cell: a small bar plus the percentage as text (the
/// text carries the value; the bar is the glance).
fn share_cell(cost: f64, total: f64) -> String {
    if total <= 0.0 {
        return "<td></td>".to_string();
    }
    let pct = cost / total * 100.0;
    let width = pct.round().clamp(0.0, 100.0);
    let label = if pct < 0.05 {
        r#"<span class="muted">&lt;0.1%</span>"#.to_string()
    } else {
        format!("{pct:.1}%")
    };
    format!(r#"<td><span class="bar"><i style="width:{width:.0}%"></i></span>{label}</td>"#)
}

/// A one-shot family's folded totals.
#[derive(Default)]
struct FamilyAgg {
    runs: i64,
    calls: i64,
    cost: f64,
}

/// The costs page's time window. `All` is the default; the bounded two
/// map to a `since` the caller computes and passes to the read service
/// (render stays pure — no wall clock in here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    All,
    Days7,
    Day,
}

impl Window {
    /// Parse the `?window=` query value; anything unrecognised is `All`.
    pub fn from_query(value: Option<&str>) -> Self {
        match value {
            Some("7d") => Window::Days7,
            Some("24h") => Window::Day,
            _ => Window::All,
        }
    }

    /// How far back the window reaches, in ms; `None` = unbounded.
    pub fn since_ms(self) -> Option<i64> {
        match self {
            Window::All => None,
            Window::Days7 => Some(7 * 86_400_000),
            Window::Day => Some(86_400_000),
        }
    }
}

/// The "window: all · 7d · 24h" selector row over `base` (the page's
/// own path) — the current window is bold text, the others link.
fn window_links(current: Window, base: &str) -> String {
    let parts: Vec<String> = [
        (Window::All, "all", String::new()),
        (Window::Days7, "7d", "?window=7d".to_string()),
        (Window::Day, "24h", "?window=24h".to_string()),
    ]
    .into_iter()
    .map(|(w, label, query)| {
        if w == current {
            format!("<b>{label}</b>")
        } else {
            format!(r#"<a href="{base}{query}">{label}</a>"#)
        }
    })
    .collect();
    format!(r#"<p class="muted">window: {}</p>"#, parts.join(" · "))
}

/// A right-aligned "last 24h" spend cell; a muted dash when the agent
/// spent nothing in the day window.
fn day_cell(cost: Option<f64>) -> String {
    match cost {
        Some(c) if c > 0.0 => format!(r#"<td class="n">${c:.2}</td>"#),
        _ => r#"<td class="n muted">—</td>"#.to_string(),
    }
}

/// The per-model spend table — shared between the top-level costs page
/// (all agents) and the per-agent drill-down, so the two by-model
/// views cannot drift apart. `total` is the share denominator.
fn by_model_table(models: &[ModelCostView], total: f64) -> String {
    let mut b = String::from(
        "<table><tr><th>model</th><th class=\"n\">llm calls</th><th class=\"n\">input</th><th class=\"n\">output</th><th class=\"n\">total cost</th><th>share</th></tr>",
    );
    for m in models {
        b.push_str(&format!(
            r#"<tr><td>{}</td><td class="n">{}</td>{}{}<td class="n">${:.4}</td>{}</tr>"#,
            esc(&m.model),
            fmt_grouped(m.event_count),
            token_cell(m.total_input_tokens),
            token_cell(m.total_output_tokens),
            m.total_cost,
            share_cell(m.total_cost, total),
        ));
    }
    b.push_str("</table>");
    b
}

/// The spend-over-time bar chart: one bar per slot, hourly slots for
/// the day window and daily otherwise, oldest on the left, ending at
/// "now". Slots are generated from the clock so quiet buckets render
/// as real gaps; the projection's sparse buckets are joined in by
/// their fixed-width UTC key. Bar heights are pixel-computed against
/// the window's max; the tallest bar carries its value, every bar
/// carries the exact figure in its hover title.
fn cost_chart(buckets: &[CostBucketView], window: Window, now_ms: i64) -> String {
    const BAR_MAX_PX: f64 = 72.0;
    let Some(now) = chrono::DateTime::from_timestamp_millis(now_ms) else {
        return String::new();
    };
    // (key, label) per slot, oldest first. The "all" window shows its
    // most recent 30 days — the tables below still cover everything.
    let hourly = window == Window::Day;
    let slots: Vec<(String, String)> = if hourly {
        (0..24)
            .rev()
            .map(|i| {
                let t = now - chrono::Duration::hours(i);
                (
                    t.format("%Y-%m-%dT%H").to_string(),
                    t.format("%H").to_string(),
                )
            })
            .collect()
    } else {
        let days: i64 = match window {
            Window::Days7 => 8,
            _ => 30,
        };
        (0..days)
            .rev()
            .map(|i| {
                let t = now - chrono::Duration::days(i);
                (t.format("%Y-%m-%d").to_string(), t.format("%d").to_string())
            })
            .collect()
    };
    let by_key: std::collections::HashMap<&str, f64> = buckets
        .iter()
        .map(|b| (b.bucket.as_str(), b.total_cost))
        .collect();
    let costs: Vec<f64> = slots
        .iter()
        .map(|(key, _)| by_key.get(key.as_str()).copied().unwrap_or(0.0))
        .collect();
    let max = costs.iter().cloned().fold(0.0_f64, f64::max);
    if max <= 0.0 {
        return String::new();
    }
    let mut b = String::from(r#"<div id="cost-chart" class="chart">"#);
    for ((key, label), cost) in slots.iter().zip(&costs) {
        let px = (cost / max * BAR_MAX_PX).round() as i64;
        let value = if (*cost - max).abs() < f64::EPSILON {
            format!("<b>${cost:.2}</b>")
        } else {
            String::new()
        };
        b.push_str(&format!(
            r#"<div class="cslot" title="{} · ${:.4}">{}<i style="height:{}px"></i><span>{}</span></div>"#,
            esc(key),
            cost,
            value,
            px,
            esc(label),
        ));
    }
    b.push_str("</div>");
    b
}

/// The costs page body: named agents as rows (the operator's
/// spend-watch), one-shot test instances folded into per-family lines
/// under a `<details>`, and totals split named vs one-shot so synthetic
/// e2e spend (the $1-per-event budget-guard runs) cannot silently
/// inflate "what did we actually spend". `day` is the same report
/// bounded to the last 24h — the per-agent spend-velocity column;
/// `window` bounds `report` itself and drives the selector row.
pub fn costs(report: &CostReport, day: &CostReport, window: Window, now_ms: i64) -> String {
    let mut b = window_links(window, "/costs");
    if report.agents.is_empty() {
        b.push_str(r#"<p class="muted">no cost events recorded.</p>"#);
        return b;
    }
    b.push_str(&cost_chart(&report.buckets, window, now_ms));
    let day_costs: std::collections::HashMap<&str, f64> = day
        .agents
        .iter()
        .map(|a| (a.agent_id.as_str(), a.total_cost))
        .collect();
    let mut named: Vec<&CostView> = Vec::new();
    let mut families: std::collections::BTreeMap<&str, FamilyAgg> = Default::default();
    for a in &report.agents {
        match one_shot_family(&a.agent_id) {
            Some(family) => {
                let f = families.entry(family).or_default();
                f.runs += 1;
                f.calls += a.event_count;
                f.cost += a.total_cost;
            }
            None => named.push(a),
        }
    }

    if !named.is_empty() {
        b.push_str("<h2>By agent</h2>");
        b.push_str(
            "<table><tr><th>agent</th><th class=\"n\">invocations</th><th class=\"n\">llm calls</th><th class=\"n\">input</th><th class=\"n\">output</th><th class=\"n\">cache read</th><th class=\"n\">cache write</th><th class=\"n\">last 24h</th><th class=\"n\">total cost</th><th>share</th></tr>",
        );
        let mut sub = FamilyAgg::default();
        let (mut sub_in, mut sub_out, mut sub_cr, mut sub_cw) = (0i64, 0i64, 0i64, 0i64);
        let mut sub_day = 0.0_f64;
        let mut sub_invs = 0i64;
        for a in &named {
            sub.calls += a.event_count;
            sub.cost += a.total_cost;
            sub_in += a.total_input_tokens;
            sub_out += a.total_output_tokens;
            sub_cr += a.total_cache_read_tokens;
            sub_cw += a.total_cache_write_tokens;
            sub_invs += a.invocation_count;
            let day = day_costs.get(a.agent_id.as_str()).copied();
            sub_day += day.unwrap_or(0.0);
            b.push_str(&format!(
                r#"<tr><td><a href="/costs/{}">{}</a></td><td class="n">{}</td><td class="n">{}</td>{}{}{}{}{}<td class="n">${:.4}</td>{}</tr>"#,
                esc(&a.agent_id),
                esc(&a.agent_id),
                fmt_grouped(a.invocation_count),
                fmt_grouped(a.event_count),
                token_cell(a.total_input_tokens),
                token_cell(a.total_output_tokens),
                token_cell(a.total_cache_read_tokens),
                token_cell(a.total_cache_write_tokens),
                day_cell(day),
                a.total_cost,
                share_cell(a.total_cost, report.total_cost),
            ));
        }
        b.push_str(&format!(
            r#"<tr class="sub"><th>named agents</th><td class="n">{}</td><td class="n">{}</td>{}{}{}{}{}<td class="n"><b>${:.4}</b></td>{}</tr>"#,
            fmt_grouped(sub_invs),
            fmt_grouped(sub.calls),
            token_cell(sub_in),
            token_cell(sub_out),
            token_cell(sub_cr),
            token_cell(sub_cw),
            day_cell(Some(sub_day)),
            sub.cost,
            share_cell(sub.cost, report.total_cost),
        ));
        b.push_str("</table>");
    }

    if !report.models.is_empty() {
        b.push_str("<h2>By model</h2>");
        b.push_str(&by_model_table(&report.models, report.total_cost));
    }

    let one_shot_cost: f64 = families.values().map(|f| f.cost).sum();
    if !families.is_empty() {
        let one_shot_ids: i64 = families.values().map(|f| f.runs).sum();
        let mut rows: Vec<(&str, &FamilyAgg)> = families.iter().map(|(k, v)| (*k, v)).collect();
        rows.sort_by(|a, b| {
            b.1.cost
                .partial_cmp(&a.1.cost)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(b.0))
        });
        let mut fold_body = String::from(
            "<table><tr><th>family</th><th class=\"n\">runs</th><th class=\"n\">llm calls</th><th class=\"n\">total cost</th></tr>",
        );
        for (family, f) in rows {
            fold_body.push_str(&format!(
                r#"<tr><td>{}-*</td><td class="n">{}</td><td class="n">{}</td><td class="n">${:.4}</td></tr>"#,
                esc(family),
                f.runs,
                fmt_grouped(f.calls),
                f.cost,
            ));
        }
        fold_body.push_str("</table>");
        b.push_str(&fold(
            "one-shot-agents",
            &format!("one-shot agents — {one_shot_ids} ids · ${one_shot_cost:.4}"),
            &fold_body,
        ));
    }

    let named_cost = report.total_cost - one_shot_cost;
    let split = if families.is_empty() {
        String::new()
    } else {
        format!(" — named ${named_cost:.4} · one-shot ${one_shot_cost:.4}")
    };
    b.push_str(&format!(
        r#"<p><b>total ${:.4}</b><span class="muted">{} · {} in / {} out</span></p>"#,
        report.total_cost,
        split,
        if report.total_input_tokens >= 1_000_000 {
            format!("{:.1}M", report.total_input_tokens as f64 / 1e6)
        } else {
            fmt_grouped(report.total_input_tokens)
        },
        if report.total_output_tokens >= 1_000_000 {
            format!("{:.1}M", report.total_output_tokens as f64 / 1e6)
        } else {
            fmt_grouped(report.total_output_tokens)
        },
    ));
    b
}

/// The single-agent cost drill-down body (`/costs/<agent>`): the
/// agent's totals, its per-model split, and per-invocation rows that
/// link each run's spend back to its invocation page (and from there
/// the transcript).
pub fn agent_costs(d: &AgentCostDetailView, window: Window, now_ms: i64) -> String {
    let mut b = format!(
        r#"<p class="muted"><a href="/costs">← all agents</a> · <a href="/agents/{}">definition</a></p>"#,
        esc(&d.agent_id)
    );
    b.push_str(&window_links(
        window,
        &format!("/costs/{}", esc(&d.agent_id)),
    ));
    b.push_str(&format!(
        r#"<p><b>total ${:.4}</b><span class="muted"> · {} invocations · {} llm calls · {} in / {} out</span></p>"#,
        d.totals.total_cost,
        fmt_grouped(d.totals.invocation_count),
        fmt_grouped(d.totals.event_count),
        if d.totals.total_input_tokens >= 1_000_000 {
            format!("{:.1}M", d.totals.total_input_tokens as f64 / 1e6)
        } else {
            fmt_grouped(d.totals.total_input_tokens)
        },
        if d.totals.total_output_tokens >= 1_000_000 {
            format!("{:.1}M", d.totals.total_output_tokens as f64 / 1e6)
        } else {
            fmt_grouped(d.totals.total_output_tokens)
        },
    ));

    b.push_str("<h2>By model</h2>");
    b.push_str(&by_model_table(&d.models, d.totals.total_cost));

    b.push_str("<h2>By invocation</h2>");
    b.push_str(
        "<table><tr><th>invocation</th><th>started</th><th class=\"n\">llm calls</th><th class=\"n\">input</th><th class=\"n\">output</th><th class=\"n\">cache read</th><th class=\"n\">cost</th></tr>",
    );
    for i in &d.invocations {
        b.push_str(&format!(
            r#"<tr><td>{}</td><td>{}</td><td class="n">{}</td>{}{}{}<td class="n">${:.4}</td></tr>"#,
            inv_link(&i.invocation_id),
            esc(&age(i.started_at_ms, now_ms)),
            fmt_grouped(i.event_count),
            token_cell(i.total_input_tokens),
            token_cell(i.total_output_tokens),
            token_cell(i.total_cache_read_tokens),
            i.total_cost,
        ));
    }
    b.push_str("</table>");
    b.push_str(&format!(
        r#"<p class="muted">showing {} of {} invocations</p>"#,
        d.invocations.len(),
        d.totals.invocation_count,
    ));
    b
}

/// The agents page body: every definition in the daemon's live
/// registry (so `fq reload` is reflected on refresh), plus any
/// per-file load errors — a broken definition should be as loud here
/// as it is in the daemon log.
pub fn agents(view: &AgentsView) -> String {
    let mut b = String::new();
    if !view.errors.is_empty() {
        b.push_str(&format!(
            r#"<p class="warn"><b>⚠ {} definition(s) failed to load</b></p>"#,
            view.errors.len()
        ));
        let mut errors_body = String::new();
        for e in &view.errors {
            errors_body.push_str(&format!("<pre>{}</pre>", esc(e)));
        }
        b.push_str(&fold("load-errors", "load errors", &errors_body));
    }
    if view.agents.is_empty() {
        b.push_str(r#"<p class="muted">no agents loaded.</p>"#);
        return b;
    }
    b.push_str(
        "<table><tr><th>agent</th><th>model</th><th>trigger</th><th class=\"n\">tools</th><th class=\"n\">budget</th><th class=\"n\">prompt</th></tr>",
    );
    for a in &view.agents {
        b.push_str(&format!(
            r#"<tr><td>{}</td><td>{}</td><td>{}</td><td class="n">{}</td><td class="n">{}</td><td class="n">{} B</td></tr>"#,
            agent_link(&a.agent_id),
            esc(&a.model),
            match a.trigger.as_deref() {
                Some(t) => esc(t),
                None => r#"<span class="muted">—</span>"#.to_string(),
            },
            a.tool_count,
            match a.budget {
                Some(budget) => format!("${budget:.2}"),
                None => r#"<span class="muted">—</span>"#.to_string(),
            },
            fmt_grouped(a.prompt_bytes),
        ));
    }
    b.push_str("</table>");
    b
}

/// The single-agent definition page (`/agents/<id>`): the definition's
/// fields, links to the agent's other surfaces, and the system prompt
/// in a collapsed `<details>` (the transcript page's pattern) so the
/// page stays scannable however long the prompt is.
pub fn agent_detail(d: &AgentDetailView) -> String {
    let mut b = format!(
        r#"<p class="muted"><a href="/agents">← all agents</a> · <a href="/costs/{}">costs</a> · <a href="/events?agent={}">events</a></p>"#,
        esc(&d.agent_id),
        esc(&d.agent_id),
    );
    b.push_str("<table>");
    b.push_str(&format!(
        "<tr><th>model</th><td>{}</td></tr>",
        esc(&d.model)
    ));
    if let Some(effort) = &d.effort {
        b.push_str(&format!("<tr><th>effort</th><td>{}</td></tr>", esc(effort)));
    }
    if let Some(budget) = d.budget {
        b.push_str(&format!("<tr><th>budget</th><td>${budget:.2}</td></tr>"));
    }
    if let Some(max) = d.max_iterations {
        b.push_str(&format!("<tr><th>max iterations</th><td>{max}</td></tr>"));
    }
    if let Some(trigger) = &d.trigger {
        b.push_str(&format!(
            "<tr><th>trigger</th><td>fq.trigger.{}</td></tr>",
            esc(trigger)
        ));
    }
    b.push_str(&format!(
        "<tr><th>tools</th><td>{}</td></tr>",
        if d.tools.is_empty() {
            r#"<span class="muted">none</span>"#.to_string()
        } else {
            esc(&d.tools.join(", "))
        }
    ));
    if !d.mcp_servers.is_empty() {
        b.push_str(&format!(
            "<tr><th>mcp servers</th><td>{}</td></tr>",
            esc(&d.mcp_servers.join(", "))
        ));
    }
    b.push_str(&format!(
        r#"<tr><th>source</th><td class="muted">{}</td></tr>"#,
        esc(&d.path)
    ));
    b.push_str("</table>");
    b.push_str(&fold(
        "system-prompt",
        &format!("system prompt ({} bytes)", d.system_prompt.len()),
        &format!("<pre>{}</pre>", esc(&d.system_prompt)),
    ));
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The health page links working and stuck invocation ids (#130) —
    /// asserted against the same fixture the screenshot gallery renders.
    #[test]
    fn health_links_working_and_stuck_ids() {
        let report = crate::fixtures::health_report();
        let html = health(&report);
        assert!(html.contains("2 in-flight (1 working"), "got: {html}");
        assert!(
            html.contains(r#"<a href="/invocations/019f5b3f-31fb-7ae0-b130-3d65ccf40375">"#),
            "working id not linked: {html}"
        );
        assert!(
            html.contains(r#"<a href="/invocations/019f534f-4b3c-7f42-a619-b5e43a64fd38">"#),
            "stuck id not linked: {html}"
        );
    }

    /// Retry pressure (#49) is visible on the streams table when a
    /// consumer has outstanding redeliveries.
    #[test]
    fn health_shows_redelivery_pressure() {
        let html = health(&crate::fixtures::health_report());
        assert!(html.contains("redelivered 4"), "got: {html}");
    }

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

    /// The live shell: datastar loaded, the #main region polling its
    /// own URL on the configured cadence, and the no-JS fallback in
    /// place of the old hard refresh.
    #[test]
    fn live_page_polls_its_own_url_and_keeps_a_noscript_fallback() {
        let html = live_page("costs", 7, "<p>x</p>");
        assert!(
            html.contains(r#"<script type="module" src="/assets/datastar.js"></script>"#),
            "got: {html}"
        );
        assert!(
            html.contains(
                r#"<div id="main" data-on-interval__duration.7s="@get(window.location.pathname + window.location.search)"><p>x</p></div>"#
            ),
            "got: {html}"
        );
        assert!(
            html.contains(r#"<noscript><meta http-equiv="refresh" content="5"></noscript>"#),
            "no-JS fallback missing: {html}"
        );
        assert!(
            !html.contains(r#"<meta http-equiv="refresh" content="7">"#),
            "hard refresh must be gone: {html}"
        );
    }

    /// Folds carry a stable id and preserve their open state across
    /// live-region morphs — the whole point of the change.
    #[test]
    fn folds_carry_stable_ids_and_preserve_open() {
        let html = fold("one-shot-agents", "one-shot agents", "<p>rows</p>");
        assert_eq!(
            html,
            r#"<details id="one-shot-agents" data-preserve-attr="open"><summary>one-shot agents</summary><p>rows</p></details>"#
        );
        // The three fold sites emit through the helper.
        let costs_html = costs_all(&cost_report(vec![
            cost_view("a", 1, 1.0),
            cost_view("overspender-019f339c15767d70b8ffd6d7ca6b0a70", 1, 1.0),
        ]));
        assert!(
            costs_html.contains(r#"<details id="one-shot-agents" data-preserve-attr="open">"#),
            "got: {costs_html}"
        );
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
        let html = transcript(&entries, 1_000, false, "inv-1", None);
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
        let html = transcript(&entries, 10_000, true, "inv-1", None);
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
        let html = transcript(&entries, 60_000, true, "inv-1", None);
        assert!(html.contains("system prompt (3 bytes)"), "got: {html}");
        assert!(html.contains("do the thing"));
        assert!(html.contains("assistant · claude-opus-4-8"));
        assert!(html.contains("tool call <b>exec</b>"));
        // Full view links back to the truncated one.
        assert!(html.contains(r#"href="/invocations/inv-1/transcript""#));
    }

    /// #216: the one-line summary renders (escaped) in both tables,
    /// with a muted em-dash when absent.
    #[test]
    fn summary_column_renders_escaped_with_fallback() {
        let mut items = vec![fq_runtime::views::InvocationSummaryView {
            invocation_id: "inv-s".into(),
            agent_id: Some("m0-issue-fix".into()),
            worker_id: "w".into(),
            status: "in_flight".into(),
            assigned_at_ms: 0,
            started_at_ms: 0,
            archived: false,
            summary: Some("Fixing #7: <script>alert(1)</script>".into()),
        }];
        let html = invocations(&items, InvocationFilters::default(), 1_000);
        assert!(html.contains("<th>summary</th>"), "got: {html}");
        assert!(
            html.contains("Fixing #7: &lt;script&gt;alert(1)&lt;/script&gt;"),
            "summary escaped: {html}"
        );
        items[0].summary = None;
        let html = invocations(&items, InvocationFilters::default(), 1_000);
        assert!(html.contains("—"), "fallback dash: {html}");

        let active_rows = [fq_runtime::views::ActiveInvocationView {
            invocation_id: "inv-a".into(),
            agent_id: "m0-issue-fix".into(),
            phase: "reducing".into(),
            step_index: 1,
            started_at_ms: 0,
            updated_at_ms: 0,
            open_tools: vec![],
            open_llms: vec![],
            summary: Some("Editing widget.rs".into()),
        }];
        let html = active(&active_rows, 1_000);
        assert!(html.contains("Editing widget.rs"), "got: {html}");
    }

    /// The one-line summary (#216) renders on the invocation detail
    /// page as a table row, and on the transcript page as a header
    /// line — both escaped, both absent when there is no summary.
    #[test]
    fn summary_renders_on_detail_and_transcript_pages() {
        let detail = fq_runtime::views::InvocationDetailView {
            invocation_id: "inv-1".into(),
            agent_id: Some("m0-issue-fix".into()),
            owner: None,
            archive: None,
            live: None,
            recent_events: vec![],
            summary: Some("Fixing #83: <b>ci</b> running".into()),
            cost: Some(fq_runtime::views::InvocationCostView {
                invocation_id: "inv-1".into(),
                started_at_ms: 0,
                event_count: 52,
                total_cost: 2.2137,
                total_input_tokens: 6_723_812,
                total_output_tokens: 10_095,
                total_cache_read_tokens: 6_554_327,
                total_cache_write_tokens: 0,
            }),
        };
        let html = invocation_detail(&detail, 1_000);
        assert_eq!(
            html.matches("<th>summary</th>").count(),
            1,
            "exactly one summary row: {html}"
        );
        assert!(
            html.contains("Fixing #83: &lt;b&gt;ci&lt;/b&gt; running"),
            "summary must be escaped: {html}"
        );
        assert!(!html.contains("<b>ci</b>"), "raw markup leaked: {html}");

        // Cost so far renders with compact counts; absent when no
        // priced call has landed yet.
        assert!(
            html.contains(r#"<th>cost so far</th><td>$2.2137 <span class="muted">· 52 llm calls · 6.72M in / 10.1K out · 6.55M cache read</span></td>"#),
            "got: {html}"
        );
        let mut no_cost = detail.clone();
        no_cost.cost = None;
        assert!(!invocation_detail(&no_cost, 1_000).contains("cost so far"));

        let mut no_summary = detail.clone();
        no_summary.summary = None;
        assert!(!invocation_detail(&no_summary, 1_000).contains("<th>summary</th>"));

        let html = transcript(&[], 1_000, false, "inv-1", Some("Fixing #83: ci running"));
        assert!(
            html.contains(r#"<span class="muted">summary —</span> Fixing #83: ci running"#),
            "got: {html}"
        );
        let html = transcript(&[], 1_000, false, "inv-1", None);
        assert!(!html.contains("summary —"), "got: {html}");
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
            summary: None,
        }];
        assert_eq!(active(&[], 1_000), "");
        // With no active rows the page is byte-identical to the plain list.
        assert_eq!(
            invocations_page(&[], &items, InvocationFilters::default(), 1_000),
            invocations(&items, InvocationFilters::default(), 1_000)
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
            open_tools: vec![fq_runtime::views::OpenToolView {
                tool_name: "exec".into(),
                command: Some("gh issue view 86 --repo bricef/factor-q".into()),
            }],
            open_llms: vec![],
            summary: None,
        }];
        let html = invocations_page(&active_rows, &[], InvocationFilters::default(), 600_000);
        assert!(html.contains("Active now"), "got: {html}");
        assert!(html.contains(r#"<a href="/invocations/0123456789abcdef">01234567</a>"#));
        assert!(
            html.contains(
                r#"tool exec <span class="muted">— gh issue view 86 --repo bricef/factor-q</span>"#
            ),
            "got: {html}"
        );
        assert!(html.contains("<td>10m ago</td>"), "started age: {html}");
        assert!(html.contains("<td>1m ago</td>"), "advanced age: {html}");
        // The list below gains its heading only when active is present.
        assert!(html.contains("All invocations"), "got: {html}");
    }

    fn cost_view(agent: &str, calls: i64, cost: f64) -> CostView {
        CostView {
            agent_id: agent.to_string(),
            event_count: calls,
            total_cost: cost,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_write_tokens: 0,
            invocation_count: 1,
        }
    }

    fn cost_report(agents: Vec<CostView>) -> CostReport {
        CostReport {
            total_cost: agents.iter().map(|a| a.total_cost).sum(),
            total_input_tokens: agents.iter().map(|a| a.total_input_tokens).sum(),
            total_output_tokens: agents.iter().map(|a| a.total_output_tokens).sum(),
            total_cache_read_tokens: agents.iter().map(|a| a.total_cache_read_tokens).sum(),
            total_cache_write_tokens: agents.iter().map(|a| a.total_cache_write_tokens).sum(),
            agents,
            buckets: vec![],
            models: vec![],
        }
    }

    /// The top-level costs page renders the report's per-model split
    /// with shares against the grand total; no models, no section.
    #[test]
    fn costs_render_the_by_model_split() {
        let mut report = cost_report(vec![
            cost_view("m0-issue-fix", 100, 75.0),
            cost_view("m0-loop", 10, 25.0),
        ]);
        report.models = vec![
            ModelCostView {
                model: "claude-opus-4-8".to_string(),
                event_count: 80,
                total_cost: 90.0,
                total_input_tokens: 100_000_000,
                total_output_tokens: 500_000,
            },
            ModelCostView {
                model: "z-ai/glm-5.2".to_string(),
                event_count: 30,
                total_cost: 10.0,
                total_input_tokens: 8_000_000,
                total_output_tokens: 60_000,
            },
        ];
        let html = costs(&report, &CostReport::default(), Window::All, TEST_NOW_MS);
        assert!(html.contains("<h2>By agent</h2>"), "got: {html}");
        assert!(html.contains("<h2>By model</h2>"), "got: {html}");
        assert!(html.contains("claude-opus-4-8"), "got: {html}");
        assert!(html.contains("z-ai/glm-5.2"), "got: {html}");
        assert!(html.contains("90.0%"), "got: {html}");
        assert!(html.contains("10.0%"), "got: {html}");

        // Without model rows the section is absent entirely.
        let bare = costs_all(&cost_report(vec![cost_view("a", 1, 1.0)]));
        assert!(!bare.contains("By model"), "got: {bare}");
    }

    /// An id is a one-shot instance only when its last segment is
    /// exactly 32 lowercase hex chars — named agents, short suffixes,
    /// and uppercase all stay named.
    #[test]
    fn one_shot_family_matches_uuid_suffixed_ids_only() {
        assert_eq!(
            one_shot_family("overspender-019f339c15767d70b8ffd6d7ca6b0a70"),
            Some("overspender")
        );
        assert_eq!(
            one_shot_family("step4-tool-wal-019f339c178c74409c1552ce7ddf6ff8"),
            Some("step4-tool-wal")
        );
        assert_eq!(one_shot_family("m0-issue-fix"), None);
        assert_eq!(one_shot_family("deadbeef"), None);
        // 31 hex chars — not a uuid suffix.
        assert_eq!(
            one_shot_family("agent-019f339c15767d70b8ffd6d7ca6b0a7"),
            None
        );
        // Uppercase hex is not the uuid7 wire form.
        assert_eq!(
            one_shot_family("agent-019F339C15767D70B8FFD6D7CA6B0A70"),
            None
        );
        // A bare 32-hex id with no family prefix stays named.
        assert_eq!(one_shot_family("019f339c15767d70b8ffd6d7ca6b0a70"), None);
    }

    /// One-shot instances collapse into per-family rows under the fold;
    /// named agents keep their own rows, and the totals line splits
    /// named vs one-shot spend.
    /// `costs()` with an unbounded window and an empty day report — the
    /// shape most render assertions want.
    fn costs_all(report: &CostReport) -> String {
        costs(report, &CostReport::default(), Window::All, TEST_NOW_MS)
    }

    /// 2026-07-16T12:00:00Z — a fixed clock for chart-slot tests.
    const TEST_NOW_MS: i64 = 1_784_203_200_000;

    #[test]
    fn costs_collapse_one_shot_agents_into_families() {
        let html = costs_all(&cost_report(vec![
            cost_view("m0-issue-fix", 2474, 121.397646),
            cost_view("overspender-019f339c15767d70b8ffd6d7ca6b0a70", 1, 1.0),
            cost_view("overspender-019f339b43c47822bdff48bec821d815", 1, 1.0),
            cost_view("e2e-agent-019f339c10bd7200a1a72a3f07606067", 1, 0.0),
        ]));
        // Named row present; raw one-shot ids never rendered.
        assert!(html.contains("m0-issue-fix"), "got: {html}");
        assert!(!html.contains("019f339c15767d70"), "got: {html}");
        // Family rows fold the instances.
        assert!(html.contains("<td>overspender-*</td>"), "got: {html}");
        assert!(html.contains("<td>e2e-agent-*</td>"), "got: {html}");
        assert!(
            html.contains("one-shot agents — 3 ids · $2.0000"),
            "got: {html}"
        );
        // The totals line splits honest spend from synthetic e2e spend.
        assert!(html.contains("total $123.3976"), "got: {html}");
        assert!(html.contains("named $121.3976"), "got: {html}");
        assert!(html.contains("one-shot $2.0000"), "got: {html}");
    }

    /// Cache token sums are on the wire (`CostView`) and must reach the
    /// page; token cells compact with the exact value in the hover.
    #[test]
    fn costs_render_cache_columns_and_share() {
        let mut a = cost_view("m0-issue-fix", 2474, 75.0);
        a.total_input_tokens = 171_392_966;
        a.total_cache_read_tokens = 26_118_676;
        let b = cost_view("m0-loop", 162, 25.0);
        let html = costs_all(&cost_report(vec![a, b]));
        assert!(
            html.contains("<th class=\"n\">cache read</th>"),
            "got: {html}"
        );
        assert!(
            html.contains(r#"<td class="n" title="26,118,676">26.12M</td>"#),
            "got: {html}"
        );
        assert!(
            html.contains(r#"<td class="n" title="171,392,966">171.39M</td>"#),
            "got: {html}"
        );
        // Share column: text carries the value, bar carries the glance.
        assert!(html.contains("75.0%"), "got: {html}");
        assert!(html.contains("25.0%"), "got: {html}");
        assert!(html.contains(r#"style="width:75%""#), "got: {html}");
        // No one-shot agents → no fold, no split in the total line.
        assert!(!html.contains("one-shot"), "got: {html}");
        assert!(html.contains("total $100.0000"), "got: {html}");
    }

    /// Agent ids are attacker-adjacent strings and stay escaped.
    #[test]
    fn costs_escape_agent_ids() {
        let html = costs_all(&cost_report(vec![cost_view("<agent>", 1, 0.5)]));
        assert!(html.contains("&lt;agent&gt;"), "got: {html}");
        assert!(!html.contains("<agent>"), "got: {html}");
    }

    /// The window selector: the current window is bold text, the other
    /// two are links back to the page.
    #[test]
    fn costs_window_selector_marks_current_and_links_others() {
        let html = costs(
            &cost_report(vec![cost_view("a", 1, 1.0)]),
            &CostReport::default(),
            Window::Days7,
            TEST_NOW_MS,
        );
        assert!(html.contains("<b>7d</b>"), "got: {html}");
        assert!(html.contains(r#"<a href="/costs">all</a>"#), "got: {html}");
        assert!(
            html.contains(r#"<a href="/costs?window=24h">24h</a>"#),
            "got: {html}"
        );
        // An empty windowed report still renders the selector — the way
        // back out of a quiet window.
        let empty = costs(
            &CostReport::default(),
            &CostReport::default(),
            Window::Day,
            TEST_NOW_MS,
        );
        assert!(empty.contains("<b>24h</b>"), "got: {empty}");
        assert!(empty.contains("no cost events"), "got: {empty}");
    }

    /// The last-24h column reads from the day-bounded report; agents
    /// with no day spend show a muted dash.
    #[test]
    fn costs_day_column_reads_from_the_day_report() {
        let report = cost_report(vec![
            cost_view("m0-issue-fix", 10, 121.0),
            cost_view("m0-loop", 5, 6.0),
        ]);
        let day = cost_report(vec![cost_view("m0-issue-fix", 2, 13.16)]);
        let html = costs(&report, &day, Window::All, TEST_NOW_MS);
        assert!(
            html.contains("<th class=\"n\">last 24h</th>"),
            "got: {html}"
        );
        assert!(html.contains("$13.16"), "got: {html}");
        assert!(
            html.contains(r#"<td class="n muted">—</td>"#),
            "got: {html}"
        );
    }

    /// Named agents link to their drill-down and carry the invocation
    /// count; the folded family rows do not link (a family is not an
    /// agent id).
    #[test]
    fn costs_link_named_agents_to_their_drilldown() {
        let mut a = cost_view("m0-issue-fix", 2474, 121.0);
        a.invocation_count = 43;
        let html = costs_all(&cost_report(vec![
            a,
            cost_view("overspender-019f339c15767d70b8ffd6d7ca6b0a70", 1, 1.0),
        ]));
        assert!(
            html.contains(r#"<a href="/costs/m0-issue-fix">m0-issue-fix</a>"#),
            "got: {html}"
        );
        assert!(
            html.contains("<th class=\"n\">invocations</th>"),
            "got: {html}"
        );
        assert!(html.contains(r#"<td class="n">43</td>"#), "got: {html}");
        assert!(!html.contains(r#"href="/costs/overspender"#), "got: {html}");
    }

    /// The drill-down page: totals strip, per-model split with share,
    /// and per-invocation rows linking to the invocation detail page.
    #[test]
    fn agent_costs_render_models_and_linked_invocations() {
        use fq_runtime::views::{InvocationCostView, ModelCostView};
        let mut totals = cost_view("m0-issue-fix", 1187, 101.38);
        totals.invocation_count = 43;
        let d = AgentCostDetailView {
            agent_id: "m0-issue-fix".to_string(),
            totals,
            models: vec![ModelCostView {
                model: "claude-opus-4-8".to_string(),
                event_count: 1187,
                total_cost: 101.38,
                total_input_tokens: 126_872_419,
                total_output_tokens: 702_313,
            }],
            invocations: vec![InvocationCostView {
                invocation_id: "019f6176-78c3-7cb3-9f0a-73c98b760b70".to_string(),
                started_at_ms: 0,
                event_count: 52,
                total_cost: 2.2137,
                total_input_tokens: 6_723_812,
                total_output_tokens: 10_095,
                total_cache_read_tokens: 6_554_327,
                total_cache_write_tokens: 0,
            }],
        };
        let html = agent_costs(&d, Window::All, 1_860_000);
        assert!(
            html.contains(r#"<a href="/costs">← all agents</a>"#),
            "got: {html}"
        );
        // Window links target this agent's own path.
        assert!(
            html.contains(r#"<a href="/costs/m0-issue-fix?window=7d">7d</a>"#),
            "got: {html}"
        );
        assert!(html.contains("By model"), "got: {html}");
        assert!(html.contains("claude-opus-4-8"), "got: {html}");
        assert!(html.contains("By invocation"), "got: {html}");
        assert!(
            html.contains(
                r#"<a href="/invocations/019f6176-78c3-7cb3-9f0a-73c98b760b70">019f6176</a>"#
            ),
            "got: {html}"
        );
        assert!(html.contains("<td>31m ago</td>"), "got: {html}");
        assert!(html.contains("$2.2137"), "got: {html}");
        assert!(html.contains("showing 1 of 43 invocations"), "got: {html}");
    }

    /// The agents list links each definition and surfaces registry
    /// load errors loudly.
    #[test]
    fn agents_list_links_definitions_and_surfaces_load_errors() {
        use fq_runtime::read_service::AgentSummaryView;
        let view = AgentsView {
            agents: vec![
                AgentSummaryView {
                    agent_id: "m0-issue-fix".to_string(),
                    model: "claude-opus-4-8".to_string(),
                    budget: Some(12.0),
                    trigger: Some("m0-issue-fix".to_string()),
                    tool_count: 3,
                    prompt_bytes: 4_212,
                },
                AgentSummaryView {
                    agent_id: "doc-drift".to_string(),
                    model: "claude-sonnet-4-5".to_string(),
                    budget: None,
                    trigger: None,
                    tool_count: 1,
                    prompt_bytes: 900,
                },
            ],
            errors: vec!["failed to parse /agents/broken.md: missing model".to_string()],
        };
        let html = agents(&view);
        assert!(
            html.contains(r#"<a href="/agents/m0-issue-fix">m0-issue-fix</a>"#),
            "got: {html}"
        );
        assert!(html.contains("$12.00"), "got: {html}");
        assert!(html.contains("4,212 B"), "got: {html}");
        // Missing budget/trigger render as muted dashes, not blanks.
        assert!(
            html.contains(r#"<span class="muted">—</span>"#),
            "got: {html}"
        );
        assert!(
            html.contains("1 definition(s) failed to load"),
            "got: {html}"
        );
        assert!(html.contains("broken.md"), "got: {html}");
        // Empty registry has its own message.
        assert!(agents(&AgentsView::default()).contains("no agents loaded"));
    }

    /// The agent definition page: fields, cross-links, and the system
    /// prompt inside a collapsed <details> — escaped, since a prompt is
    /// arbitrary text.
    #[test]
    fn agent_detail_collapses_and_escapes_the_prompt() {
        let d = AgentDetailView {
            agent_id: "m0-issue-fix".to_string(),
            model: "claude-opus-4-8".to_string(),
            system_prompt: "Fix issues end-to-end. Never claim <b>unpersisted</b> work."
                .to_string(),
            tools: vec!["exec".to_string(), "file_read".to_string()],
            mcp_servers: vec!["github".to_string()],
            budget: Some(12.0),
            max_iterations: Some(200),
            effort: Some("high".to_string()),
            trigger: Some("m0-issue-fix".to_string()),
            path: "/home/fq/agents/m0-issue-fix.md".to_string(),
        };
        let html = agent_detail(&d);
        assert!(
            html.contains(r#"<details id="system-prompt" data-preserve-attr="open"><summary>system prompt (59 bytes)</summary>"#),
            "got: {html}"
        );
        assert!(
            !html.contains("<b>unpersisted</b>"),
            "prompt leaked markup: {html}"
        );
        assert!(
            html.contains("&lt;b&gt;unpersisted&lt;/b&gt;"),
            "got: {html}"
        );
        assert!(
            html.contains(r#"<a href="/costs/m0-issue-fix">costs</a>"#),
            "got: {html}"
        );
        assert!(
            html.contains(r#"<a href="/events?agent=m0-issue-fix">events</a>"#),
            "got: {html}"
        );
        assert!(html.contains("fq.trigger.m0-issue-fix"), "got: {html}");
        assert!(html.contains("exec, file_read"), "got: {html}");
        assert!(html.contains("m0-issue-fix.md"), "got: {html}");
    }

    /// Agent names across the invocation surfaces link to the agent
    /// page; an unknown agent renders a muted placeholder, not a link.
    #[test]
    fn invocation_surfaces_link_agent_names() {
        let items = vec![
            fq_runtime::views::InvocationSummaryView {
                invocation_id: "inv-1".into(),
                agent_id: Some("m0-loop".into()),
                worker_id: "w".into(),
                status: "completed".into(),
                assigned_at_ms: 0,
                started_at_ms: 0,
                archived: false,
                summary: None,
            },
            fq_runtime::views::InvocationSummaryView {
                invocation_id: "inv-2".into(),
                agent_id: None,
                worker_id: "w".into(),
                status: "failed".into(),
                assigned_at_ms: 0,
                started_at_ms: 0,
                archived: false,
                summary: None,
            },
        ];
        let html = invocations(&items, InvocationFilters::default(), 1_000);
        assert!(
            html.contains(r#"<a href="/agents/m0-loop">m0-loop</a>"#),
            "got: {html}"
        );
        assert!(
            html.contains(r#"<span class="muted">?</span>"#),
            "got: {html}"
        );

        let active_rows = [fq_runtime::views::ActiveInvocationView {
            invocation_id: "inv-3".into(),
            agent_id: "m0-issue-fix".into(),
            phase: "reducing".into(),
            step_index: 1,
            started_at_ms: 0,
            updated_at_ms: 0,
            open_tools: vec![],
            open_llms: vec![],
            summary: None,
        }];
        let html = active(&active_rows, 1_000);
        assert!(
            html.contains(r#"<a href="/agents/m0-issue-fix">m0-issue-fix</a>"#),
            "got: {html}"
        );
    }

    /// Terminal-status filters: hide/show completed and failed rows,
    /// with toggle links that flip one flag while preserving the rest
    /// (the live region polls the same query string, so state
    /// survives ticks).
    #[test]
    fn invocation_filters_hide_terminal_rows_and_compose_links() {
        let mk = |id: &str, status: &str| fq_runtime::views::InvocationSummaryView {
            invocation_id: id.into(),
            agent_id: Some("a".into()),
            worker_id: "w".into(),
            status: status.into(),
            assigned_at_ms: 0,
            started_at_ms: 0,
            archived: false,
            summary: None,
        };
        let items = vec![
            mk("inv-live", "in_flight"),
            mk("inv-done", "completed"),
            mk("inv-boom", "failed"),
        ];

        // Default: everything visible, both toggles say "hide".
        let html = invocations(&items, InvocationFilters::default(), 1_000);
        for id in ["inv-live", "inv-done", "inv-boom"] {
            assert!(html.contains(id), "default shows {id}: {html}");
        }
        assert!(
            html.contains(r#"<a href="/invocations?completed=0">hide completed</a>"#),
            "got: {html}"
        );
        assert!(
            html.contains(r#"<a href="/invocations?failed=0">hide failed</a>"#),
            "got: {html}"
        );

        // Hiding completed drops only those rows; its link flips to
        // "show" and the OTHER toggles carry the completed=0 state.
        let filters = InvocationFilters {
            show_completed: false,
            ..Default::default()
        };
        let html = invocations(&items, filters, 1_000);
        assert!(!html.contains("inv-done"), "got: {html}");
        assert!(
            html.contains("inv-live") && html.contains("inv-boom"),
            "got: {html}"
        );
        assert!(
            html.contains(r#"<a href="/invocations">show completed</a>"#),
            "got: {html}"
        );
        assert!(
            html.contains(r#"<a href="/invocations?completed=0&failed=0">hide failed</a>"#),
            "got: {html}"
        );
        assert!(
            html.contains(r#"<a href="/invocations?archived=1&completed=0">show archived</a>"#),
            "got: {html}"
        );

        // Everything hidden → the honest empty message.
        let filters = InvocationFilters {
            show_completed: false,
            show_failed: false,
            ..Default::default()
        };
        let html = invocations(&items[1..], filters, 1_000);
        assert!(
            html.contains("no invocations match the filters"),
            "got: {html}"
        );
    }

    /// The spend chart: continuous slots from the clock, sparse
    /// buckets joined by key, quiet slots as zero-height gaps, the
    /// max bar labeled, hourly granularity on the day window.
    #[test]
    fn cost_chart_fills_slots_and_labels_the_max() {
        let bucket = |key: &str, cost: f64| CostBucketView {
            bucket: key.to_string(),
            total_cost: cost,
        };
        // TEST_NOW_MS is 2026-07-16T12:00Z: 30 daily slots end there.
        let buckets = vec![
            bucket("2026-07-14", 4.0),
            bucket("2026-07-16", 8.0),
            // Outside the 30-day pane — must be ignored, not drawn.
            bucket("2020-01-01", 99.0),
        ];
        let html = cost_chart(&buckets, Window::All, TEST_NOW_MS);
        assert_eq!(html.matches("cslot").count(), 30, "got: {html}");
        // Max bar: full height and labeled; half-cost bar: half height.
        assert!(
            html.contains(r#"title="2026-07-16 · $8.0000"><b>$8.00</b><i style="height:72px""#),
            "got: {html}"
        );
        assert!(
            html.contains(r#"title="2026-07-14 · $4.0000"><i style="height:36px""#),
            "got: {html}"
        );
        // A quiet day renders a gap, not a bar.
        assert!(
            html.contains(r#"title="2026-07-15 · $0.0000"><i style="height:0px""#),
            "got: {html}"
        );
        assert!(!html.contains("2020-01-01"), "stale bucket drawn: {html}");

        // Day window: 24 hourly slots, keys carry the hour.
        let html = cost_chart(&[bucket("2026-07-16T09", 1.5)], Window::Day, TEST_NOW_MS);
        assert_eq!(html.matches("cslot").count(), 24, "got: {html}");
        assert!(
            html.contains(r#"title="2026-07-16T09 · $1.5000""#),
            "got: {html}"
        );

        // 7d window: 8 daily slots.
        let html = cost_chart(&[bucket("2026-07-12", 2.0)], Window::Days7, TEST_NOW_MS);
        assert_eq!(html.matches("cslot").count(), 8, "got: {html}");

        // No spend in the pane → no chart at all.
        assert_eq!(cost_chart(&[], Window::All, TEST_NOW_MS), "");
        assert_eq!(
            cost_chart(&[bucket("2020-01-01", 9.0)], Window::All, TEST_NOW_MS),
            ""
        );
    }

    #[test]
    fn window_parses_query_and_bounds() {
        assert_eq!(Window::from_query(None), Window::All);
        assert_eq!(Window::from_query(Some("7d")), Window::Days7);
        assert_eq!(Window::from_query(Some("24h")), Window::Day);
        assert_eq!(Window::from_query(Some("bogus")), Window::All);
        assert_eq!(Window::All.since_ms(), None);
        assert_eq!(Window::Day.since_ms(), Some(86_400_000));
        assert_eq!(Window::Days7.since_ms(), Some(604_800_000));
    }

    #[test]
    fn token_cells_compact_with_exact_hover() {
        assert_eq!(fmt_grouped(1_597), "1,597");
        assert_eq!(fmt_grouped(171_392_966), "171,392,966");
        assert_eq!(fmt_grouped(420), "420");
        assert_eq!(token_cell(420), r#"<td class="n">420</td>"#);
        assert_eq!(
            token_cell(58_912),
            r#"<td class="n" title="58,912">58.9K</td>"#
        );
        assert_eq!(
            token_cell(7_409_042),
            r#"<td class="n" title="7,409,042">7.41M</td>"#
        );
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
            summary: None,
        }];
        let html = invocations(&items, InvocationFilters::default(), 1_200_000);
        assert!(html.contains(r#"<a href="/invocations/0123456789abcdef">01234567</a>"#));
        assert!(html.contains("&lt;agent&gt;"));
        assert!(!html.contains("<agent>"));
        assert!(html.contains("<th>started</th>"));
        assert!(html.contains("<td>10m ago</td>"), "got: {html}");
    }
}
