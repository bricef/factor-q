//! Invocation list and active-work HTML renderers.

use std::collections::HashMap;

use fq_ops::views::{
    ActiveInvocationView, InvocationArchiveState, InvocationEnder, InvocationSummaryView,
};

use super::{age, agent_link, display_cap, esc, inv_link, liveness_badge, short, status_span};

/// The one-line invocation summary cell (#216): escaped, with a muted
/// em-dash when the summariser has not produced a line (disabled, or
/// the invocation just started).
fn summary_cell(summary: Option<&str>, description: Option<&str>) -> String {
    match (summary, description) {
        (Some(line), _) => format!(r#"<span class="muted">{}</span>"#, esc(line)),
        (None, Some(description)) => {
            format!(r#"<em class="muted">{}</em>"#, esc(description))
        }
        (None, None) => r#"<span class="muted">—</span>"#.to_string(),
    }
}

/// The "active right now" table: currently-executing invocations from
/// the worker WAL. Renders to NOTHING when nothing is in flight — the
/// page contract is that the section only exists when there is live
/// work to show.
pub fn active(
    items: &[ActiveInvocationView],
    descriptions: &HashMap<String, String>,
    now_ms: i64,
) -> String {
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
            summary_cell(i.summary.as_deref(), descriptions.get(&i.agent_id).map(String::as_str)),
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
    descriptions: &HashMap<String, String>,
    now_ms: i64,
) -> String {
    let active_html = active(active_rows, descriptions, now_ms);
    let list_html = invocations(items, filters, descriptions, now_ms);
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
    descriptions: &HashMap<String, String>,
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
        "<table><tr><th>invocation</th><th>status</th><th>summary</th><th>started</th><th>agent</th><th>worker</th><th>archive</th><th>ended by</th></tr>",
    );
    for i in items {
        b.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            inv_link(&i.invocation_id),
            status_span(&i.status),
            summary_cell(
                i.summary.as_deref(),
                i.agent_id
                    .as_ref()
                    .and_then(|agent| descriptions.get(agent))
                    .map(String::as_str),
            ),
            esc(&age(i.started_at_ms, now_ms)),
            match i.agent_id.as_deref() {
                Some(agent) => agent_link(agent),
                None => r#"<span class="muted">?</span>"#.to_string(),
            },
            short(&i.worker_id),
            archive_cell(i.archive_state),
            ended_by_cell(i)
        ));
    }
    b.push_str("</table>");
    b
}

/// The `ended by` column: who ended the run, `run` or `drop`, with how the
/// ending reads beside an operator's termination request, when one is on
/// record, as the cell's title. A dash, muted, while the run has not
/// ended, and where the row does not say who ended it.
fn ended_by_cell(item: &InvocationSummaryView) -> String {
    let Some(ender) = item.ended_by else {
        return r#"<span class="muted">—</span>"#.to_string();
    };
    let label = match ender {
        InvocationEnder::Run => "run",
        InvocationEnder::Drop => "drop",
    };
    match item.termination_reading() {
        Some(reading) => format!(r#"<span title="{}">{label}</span>"#, esc(reading.phrase())),
        None => label.to_string(),
    }
}

/// A time as the detail page writes it: `2026-01-02T03:04:05Z`.
fn utc(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|at| at.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| format!("{ms}ms"))
}

/// The detail page's rows for the run's ownership row, as `fq invocation
/// show` reads it: its status and worker; who ended it and when, `not
/// recorded` for a run that ended before its row recorded an ender; and
/// the termination request beside its ending, the first one on record,
/// with the reading, who asked and why. Everything the row's writers or an
/// operator supplied is escaped, and who asked and why are shown as
/// `fq invocation show` shows them, one bounded line with their hidden
/// characters written as escapes ([`fq_ops::text::one_line`]).
pub(super) fn owner_rows(owner: &InvocationSummaryView) -> String {
    let mut rows = format!(
        "<tr><th>status</th><td>{}</td></tr><tr><th>worker</th><td>{}</td></tr>",
        status_span(&owner.status),
        esc(&owner.worker_id)
    );
    let ended = match (owner.ended_by, owner.ended_at_ms) {
        (Some(ender), at) => Some(format!(
            "{}{}",
            match ender {
                InvocationEnder::Run => "run",
                InvocationEnder::Drop => "drop",
            },
            at.map(|at| format!(" at {}", utc(at))).unwrap_or_default()
        )),
        (None, _) if matches!(owner.status.as_str(), "completed" | "failed") => {
            Some(r#"<span class="muted">not recorded</span>"#.to_string())
        }
        (None, _) => None,
    };
    if let Some(ended) = ended {
        rows.push_str(&format!("<tr><th>ended by</th><td>{ended}</td></tr>"));
    }
    if let Some(request) = &owner.termination {
        let mut said = format!("requested {}", utc(request.requested_at_ms));
        if let Some(by) = &request.requested_by {
            said.push_str(&format!(
                " by {}",
                esc(&fq_ops::text::one_line(by, fq_ops::text::REQUESTER_CHARS))
            ));
        }
        if let Some(reason) = &request.reason {
            said.push_str(&format!(
                ": {}",
                esc(&fq_ops::text::one_line(reason, fq_ops::text::REASON_CHARS))
            ));
        }
        let termination = match owner.termination_reading() {
            Some(reading) => format!("{} ({said})", esc(reading.phrase())),
            None => said,
        };
        rows.push_str(&format!(
            "<tr><th>termination</th><td>{termination}</td></tr>"
        ));
    }
    rows
}

/// The archive column: whether the control plane holds the run's archive.
/// A run reads finished from its ending; this says whether the archive
/// followed, marks the one the worker's archive sweeper is holding back,
/// which waits for an operator (the health page lists why), and names a
/// dropped run's that will never come, which waits for nothing.
fn archive_cell(state: InvocationArchiveState) -> &'static str {
    match state {
        InvocationArchiveState::NotDue => r#"<span class="muted">—</span>"#,
        InvocationArchiveState::Pending => "pending",
        InvocationArchiveState::Held => r#"<span class="warn">held</span>"#,
        InvocationArchiveState::Archived => "yes",
        InvocationArchiveState::WillNotCome => r#"<span class="muted">none</span>"#,
    }
}
