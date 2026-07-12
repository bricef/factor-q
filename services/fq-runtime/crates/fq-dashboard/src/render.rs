//! Pure HTML rendering over the wire DTOs — `format!` and string
//! concatenation only, by design (#105 layer 3): zero client-side JS,
//! zero template engine, `<meta refresh>` for liveness. Every dynamic
//! string goes through [`esc`]. Pure functions so the pages are
//! unit-testable without HTTP or a runtime.

use fq_runtime::health::{ConsumerHealth, StreamHealth};
use fq_runtime::read_service::HealthReport;
use fq_runtime::views::{CostReport, EventView, InvocationDetailView, InvocationSummaryView};

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
    } else {
        format!("{}h ago", secs / 3600)
    }
}

/// The shared page shell: title, auto-refresh, nav, tiny inline CSS.
pub fn page(title: &str, refresh_secs: u64, body: &str) -> String {
    let title = esc(title);
    format!(
        r#"<!doctype html>
<html><head>
<meta charset="utf-8">
<meta http-equiv="refresh" content="{refresh_secs}">
<title>{title} — fq</title>
<style>
body {{ font-family: monospace; margin: 1.5rem; color: #222; }}
h1 {{ font-size: 1.2rem; }} h2 {{ font-size: 1rem; margin-top: 1.5rem; }}
table {{ border-collapse: collapse; margin: 0.5rem 0; }}
th, td {{ border: 1px solid #bbb; padding: 0.25rem 0.6rem; text-align: left; }}
th {{ background: #eee; }}
nav a {{ margin-right: 1rem; }}
.ok {{ color: #060; }} .warn {{ color: #a60; }} .bad {{ color: #a00; }}
.muted {{ color: #888; }}
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

/// The invocations list body.
pub fn invocations(items: &[InvocationSummaryView], include_archived: bool) -> String {
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
        "<table><tr><th>invocation</th><th>status</th><th>agent</th><th>worker</th><th>archived</th></tr>",
    );
    for i in items {
        b.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            inv_link(&i.invocation_id),
            esc(&i.status),
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

    #[test]
    fn invocation_rows_escape_and_link() {
        let items = vec![fq_runtime::views::InvocationSummaryView {
            invocation_id: "0123456789abcdef".into(),
            agent_id: Some("<agent>".into()),
            worker_id: "w".into(),
            status: "in_flight".into(),
            assigned_at_ms: 0,
            archived: false,
        }];
        let html = invocations(&items, false);
        assert!(html.contains(r#"<a href="/invocations/0123456789abcdef">01234567</a>"#));
        assert!(html.contains("&lt;agent&gt;"));
        assert!(!html.contains("<agent>"));
    }
}
