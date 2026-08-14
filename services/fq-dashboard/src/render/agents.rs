//! The agent-definition pages — `/agents` and `/agents/<id>`.
//!
//! Split out of [`super`] as its own sibling because these two are the
//! only pages that render what an agent *is* rather than what it did.
//! They read the daemon's live registry through the read service's
//! definition DTOs; every other page here renders runtime activity out
//! of `fq_runtime::views`. The shared cells and shells stay in
//! [`super`], with the rest of their callers.

use fq_runtime::read_service::{AgentDetailView, AgentsView};

use super::{agent_link, esc, fmt_grouped, fold};

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
