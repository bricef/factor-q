//! The notifications pane and its detail page — pure HTML over the
//! wire DTOs, the same `format!`-and-escape discipline as every other
//! renderer here.
//!
//! **Why this is not under `render/`.** Every other page's renderer is
//! a `render.rs` submodule, and this one would be, except that
//! `render.rs` sits exactly on its file-size budget: adding the `mod`
//! declaration alone trips `just lint-sizes`, and the house rule is
//! that a budget is never raised and a frozen file is never
//! restructured as a side effect of a feature. So the pane's rendering
//! sits beside the handler that calls it. The one line this change does
//! spend in `render.rs` is the nav, which is an edit rather than a
//! growth.
//!
//! **Severity is carried in form, not only in colour.** An alert row
//! wears a left stripe and the word `alert` behind a `▲`; a
//! notification wears a chip. Colour repeats that, it does not
//! substitute for it — the palette is three hues on a dark ground and
//! an operator glancing at a phone in the dark is the reader this pane
//! exists for.

use fq_ops::events::SignalSeverity;
use fq_ops::views::{OperatorSignalDetailView, OperatorSignalView};

use crate::render::{age, esc};

/// The pane's filter state, as it rides the query string — which the
/// live region polls verbatim, so a chosen filter survives every tick
/// for free.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SignalFilters {
    /// `notification` | `alert`, already validated against the
    /// surface's vocabulary by the handler.
    pub(crate) severity: Option<String>,
    /// One component — the kind's first segment.
    pub(crate) source: Option<String>,
}

impl SignalFilters {
    /// The canonical query string for this state; the default view
    /// keeps the bare URL.
    fn query(&self) -> String {
        let mut params: Vec<String> = Vec::new();
        if let Some(severity) = &self.severity {
            params.push(format!("severity={}", esc(severity)));
        }
        if let Some(source) = &self.source {
            params.push(format!("source={}", esc(source)));
        }
        if params.is_empty() {
            String::new()
        } else {
            format!("?{}", params.join("&"))
        }
    }

    fn with_severity(&self, severity: Option<&str>) -> Self {
        SignalFilters {
            severity: severity.map(str::to_string),
            source: self.source.clone(),
        }
    }

    fn with_source(&self, source: Option<&str>) -> Self {
        SignalFilters {
            severity: self.severity.clone(),
            source: source.map(str::to_string),
        }
    }

    fn any(&self) -> bool {
        self.severity.is_some() || self.source.is_some()
    }
}

/// One selector entry: bold when it is the current state, a link
/// otherwise — the `window_links` idiom the costs page uses.
fn selector(label: &str, target: &SignalFilters, current: &SignalFilters) -> String {
    if target == current {
        format!("<b>{}</b>", esc(label))
    } else {
        format!(
            r#"<a href="/notifications{}">{}</a>"#,
            target.query(),
            esc(label)
        )
    }
}

/// The severity badge: the word, a glyph, and a colour — in that order
/// of importance. `<b>` on the alert so weight carries it too where the
/// hue does not.
fn severity_badge(severity: SignalSeverity) -> String {
    match severity {
        SignalSeverity::Alert => r#"<span class="bad"><b>▲ alert</b></span>"#.to_string(),
        SignalSeverity::Notification => r#"<span class="chip">notification</span>"#.to_string(),
    }
}

/// The row class that draws the alert stripe. Notifications get none —
/// a stripe on everything is a stripe on nothing.
fn severity_row_class(severity: SignalSeverity) -> &'static str {
    match severity {
        SignalSeverity::Alert => r#" class="alert""#,
        SignalSeverity::Notification => "",
    }
}

/// The pane: newest first, one row per signal, severity in form as well
/// as colour.
///
/// `sources` is the set the filter row offers. It is computed from the
/// rows on show rather than from a registry read, so it names the
/// components that have actually said something — and when a source
/// filter is active there is exactly one, which is why the row also
/// carries the way back out.
pub(crate) fn notifications(
    rows: &[OperatorSignalView],
    filters: &SignalFilters,
    now_ms: i64,
) -> String {
    let mut b = String::new();
    b.push_str(&format!(
        r#"<p class="muted">severity: {} · {} · {}</p>"#,
        selector("all", &filters.with_severity(None), filters),
        selector(
            "notifications",
            &filters.with_severity(Some("notification")),
            filters
        ),
        selector("alerts", &filters.with_severity(Some("alert")), filters),
    ));

    let mut sources: Vec<&str> = rows.iter().map(|r| r.source.as_str()).collect();
    sources.sort_unstable();
    sources.dedup();
    if let Some(active) = &filters.source {
        // One source on show, so the only link worth having is the way
        // back to all of them.
        b.push_str(&format!(
            r#"<p class="muted">source: {} · {}</p>"#,
            selector("all", &filters.with_source(None), filters),
            format_args!("<b>{}</b>", esc(active)),
        ));
    } else if sources.len() > 1 {
        let links: Vec<String> = std::iter::once(selector("all", filters, filters))
            .chain(
                sources
                    .iter()
                    .map(|s| selector(s, &filters.with_source(Some(s)), filters)),
            )
            .collect();
        b.push_str(&format!(
            r#"<p class="muted">source: {}</p>"#,
            links.join(" · ")
        ));
    }

    if rows.is_empty() {
        b.push_str(if filters.any() {
            r#"<p class="muted">no operator signals match the filters.</p>"#
        } else {
            r#"<p class="muted">no operator signals. Nothing has asked for a person's attention.</p>"#
        });
        return b;
    }

    b.push_str(
        r#"<table class="signals"><tr><th>severity</th><th>when</th><th>source</th><th>kind</th><th>summary</th></tr>"#,
    );
    for row in rows {
        b.push_str(&format!(
            r#"<tr{}><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td><a href="/notifications/{}">{}</a></td></tr>"#,
            severity_row_class(row.severity),
            severity_badge(row.severity),
            esc(&age(instant_ms(&row.timestamp).unwrap_or(now_ms), now_ms)),
            esc(&row.source),
            esc(&row.kind),
            esc(&row.event_id),
            esc(&row.summary),
        ));
    }
    b.push_str("</table>");
    // Alerts are the reason the table is kept past the log's window, so
    // the pane says so rather than letting a reader wonder why a
    // three-month-old row is still here.
    if rows.iter().any(|r| r.severity.is_alert()) {
        b.push_str(
            r#"<p class="muted">alerts are never swept — they outlive the event log they came from. Notifications age out with it.</p>"#,
        );
    }
    b
}

/// An RFC3339 timestamp as epoch ms, for [`age`]. `None` for a
/// spelling this cannot parse, which the caller renders as "now"
/// rather than failing the page over one row.
fn instant_ms(timestamp: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|t| t.timestamp_millis())
}

/// A `<tr>` of one label and one value, both escaped by the caller's
/// choice: `value_html` is trusted markup so a cell can carry a link.
fn field(label: &str, value_html: &str) -> String {
    format!("<tr><th>{}</th><td>{}</td></tr>", esc(label), value_html)
}

/// The detail page: the whole signal, what it points at, the envelope
/// it rode in on, and the walk through its own source's history.
pub(crate) fn notification_detail(signal: &OperatorSignalDetailView, now_ms: i64) -> String {
    let mut b = format!(
        r#"<p class="muted"><a href="/notifications">← all notifications</a> · <a href="/notifications?source={}">all from {}</a></p>"#,
        esc(&signal.source),
        esc(&signal.source),
    );
    b.push_str(&format!(
        "<p>{} <b>{}</b></p>",
        severity_badge(signal.severity),
        esc(&signal.summary),
    ));

    b.push_str("<table>");
    b.push_str(&field("source", &esc(&signal.source)));
    b.push_str(&field("kind", &esc(&signal.kind)));
    b.push_str(&field(
        "raised",
        &format!(
            "{} <span class=\"muted\">· {}</span>",
            esc(signal.timestamp.get(..19).unwrap_or(&signal.timestamp)),
            esc(&age(
                instant_ms(&signal.timestamp).unwrap_or(now_ms),
                now_ms
            )),
        ),
    ));
    // What the signal is *about*, when it is about something
    // addressable — links out of the pane and into the thing that
    // needs looking at, which is the whole point of a detail page.
    if let Some(invocation) = &signal.references.invocation {
        b.push_str(&field(
            "invocation",
            &format!(
                r#"<a href="/invocations/{}">{}</a>"#,
                esc(invocation),
                esc(invocation)
            ),
        ));
    }
    if let Some(agent) = &signal.references.agent {
        b.push_str(&field(
            "agent",
            &format!(r#"<a href="/agents/{}">{}</a>"#, esc(agent), esc(agent)),
        ));
    }
    if let Some(url) = &signal.references.url {
        b.push_str(&field(
            "look at",
            &format!(
                r#"<a href="{}" rel="noreferrer noopener">{}</a>"#,
                esc(url),
                esc(url)
            ),
        ));
    }
    b.push_str("</table>");

    b.push_str("<h2>Detail</h2>");
    if signal.detail.is_null() {
        b.push_str(r#"<p class="muted">This kind carries no particulars — the summary above is the whole of it.</p>"#);
    } else {
        let rendered = serde_json::to_string_pretty(&signal.detail)
            .unwrap_or_else(|e| format!("(unrenderable: {e})"));
        b.push_str(&format!("<pre>{}</pre>", esc(&rendered)));
    }

    // The envelope the signal rode in on. An operator signal is
    // daemon-scoped — its envelope names the runtime, not whatever the
    // signal concerns — so the two blocks are deliberately separate:
    // the references above say what it is about, and this says where it
    // came from.
    b.push_str("<h2>Event</h2><table>");
    b.push_str(&field("event id", &esc(&signal.event_id)));
    b.push_str(&field("schema", "factor-q/operator_signal@1"));
    b.push_str(&field("agent", &esc(&signal.agent_id)));
    b.push_str(&field("invocation", &esc(&signal.invocation_id)));
    b.push_str(&field(
        "log position",
        &match signal.seq {
            Some(seq) => format!("{seq}"),
            None => r#"<span class="muted">not recorded</span>"#.to_string(),
        },
    ));
    b.push_str("</table>");

    let walk = |label: &str, id: &Option<String>| match id {
        Some(id) => format!(r#"<a href="/notifications/{}">{}</a>"#, esc(id), esc(label)),
        None => format!(r#"<span class="muted">{}</span>"#, esc(label)),
    };
    if signal.newer_from_source.is_some() || signal.older_from_source.is_some() {
        b.push_str(&format!(
            r#"<p class="muted">from {}: {} · {}</p>"#,
            esc(&signal.source),
            walk("← newer", &signal.newer_from_source),
            walk("older →", &signal.older_from_source),
        ));
    }
    b
}

#[cfg(test)]
mod tests;
