//! Invocation list and active-work HTML renderers.

use fq_ops::views::{ActiveInvocationView, InvocationSummaryView};

use super::{age, agent_link, display_cap, esc, inv_link, liveness_badge, short, status_span};

/// The one-line invocation summary cell (#216): escaped, with a muted
/// em-dash when the summariser has not produced a line (disabled, or
/// the invocation just started).
fn summary_cell(summary: Option<&str>) -> String {
    match summary {
        Some(line) => format!(r#"<span class="muted">{}</span>"#, esc(line)),
        None => r#"<span class="muted">—</span>"#.to_string(),
    }
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
    b.push_str("<h2>Active now</h2><table><tr><th>invocation</th><th>agent</th><th>summary</th><th>phase</th><th>state</th><th>step</th><th>started</th><th>last advanced</th><th>doing</th></tr>");
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
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            inv_link(&i.invocation_id),
            agent_link(&i.agent_id),
            summary_cell(i.summary.as_deref()),
            esc(&i.phase),
            liveness_badge(i.liveness),
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
            status_span(&i.status),
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
