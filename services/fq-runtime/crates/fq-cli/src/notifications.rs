//! The `fq notifications` verbs: the operator signals a daemon has
//! raised, listed and read, over the authenticated edge from the
//! daemon's OperatorSignal view.
//!
//! Its own module on the `events.rs` / `workers.rs` precedent: a
//! subcommand's rendering belongs beside itself rather than in the
//! composition root.
//!
//! **The command is `notifications` and the resource is
//! `operator_signal`.** The pane an operator reads these in is called
//! notifications, and so is this command, for the same reason the nav
//! says *health* over `control.status` and *costs* over `cost.summary`:
//! the operator-facing name is the page's, and the contract keeps the
//! precise one. Both severities list here — `--severity alert` narrows
//! to the ones that were allowed to wake somebody.
//!
//! **Listing and reading are one verb pair.** Every row names the
//! identity that reads the whole signal back, so the human table prints
//! it in full and unpadded, the way `fq events query` does, and
//! [`show_notification`] is the walk it invites.

use fq_ops::surface::{OperatorSignalFilter, OperatorSignalKey};
use fq_ops::views::{OperatorSignalDetailView, OperatorSignalView};

use crate::cli::GlobalArgs;
use crate::edge_call::edge_invoke;

/// List the signals a daemon has raised, newest first.
pub(crate) async fn list_notifications(
    global: &GlobalArgs,
    severity: Option<String>,
    source: Option<String>,
    since: Option<String>,
    limit: i64,
    json: bool,
) -> anyhow::Result<()> {
    let cap = fq_ops::surface::OPERATOR_SIGNAL_LIST_MAX_LIMIT;
    let filter = OperatorSignalFilter {
        severity,
        source,
        since,
        limit: Some(u32::try_from(limit).map_err(|_| {
            anyhow::anyhow!(
                "--limit {limit} is not a page size: one page is at most {cap} rows, and \
                 a bigger ask is refused rather than shortened. Narrow with --severity, \
                 --source or --since instead."
            )
        })?),
    };
    let output = edge_invoke(
        global,
        fq_ops::OpId::List(fq_ops::Domain::OperatorSignal),
        serde_json::to_value(filter)?,
    )
    .await?
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let rows: Vec<OperatorSignalView> = serde_json::from_value(output)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("No operator signals matched.");
        return Ok(());
    }
    println!(
        "{:<20} {:<13} {:<26} {:<52} event-id",
        "timestamp", "severity", "kind", "summary"
    );
    for row in rows {
        let ts = row.timestamp.get(..19).unwrap_or(&row.timestamp);
        println!(
            "{:<20} {:<13} {:<26} {:<52} {}",
            ts,
            row.severity,
            row.kind,
            display_cap(&row.summary, 52),
            row.event_id
        );
    }
    Ok(())
}

/// Read one whole signal back — its particulars, what it points at, and
/// the walk through its own source's history.
pub(crate) async fn show_notification(
    global: &GlobalArgs,
    event_id: &str,
    json: bool,
) -> anyhow::Result<()> {
    let output = edge_invoke(
        global,
        fq_ops::OpId::Get(fq_ops::Domain::OperatorSignal),
        serde_json::to_value(OperatorSignalKey {
            event_id: event_id.to_string(),
        })?,
    )
    .await?
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let signal: OperatorSignalDetailView = serde_json::from_value(output)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&signal)?);
        return Ok(());
    }
    print!("{}", render_signal(&signal));
    Ok(())
}

/// The human rendering of one whole signal — its own function so the
/// shape is unit-testable without an edge.
fn render_signal(signal: &OperatorSignalDetailView) -> String {
    let mut out = format!(
        "{} {}\n{}\n\nsource        {}\nkind          {}\nevent-id      {}\n",
        signal.timestamp,
        signal.severity,
        signal.summary,
        signal.source,
        signal.kind,
        signal.event_id,
    );
    if let Some(agent) = &signal.references.agent_id {
        out.push_str(&format!("agent         {agent}\n"));
    }
    if let Some(invocation) = &signal.references.invocation_id {
        out.push_str(&format!("invocation    {invocation}\n"));
    }
    if let Some(url) = &signal.references.url {
        out.push_str(&format!("url           {url}\n"));
    }
    // The particulars, pretty-printed, or a line saying there are
    // none — an absent block and a block that failed to render must
    // not look the same.
    out.push_str("\ndetail\n");
    if signal.detail.is_null() {
        out.push_str("  (none — this kind carries no particulars)\n");
    } else {
        let rendered = serde_json::to_string_pretty(&signal.detail)
            .unwrap_or_else(|e| format!("(unrenderable: {e})"));
        for line in rendered.lines() {
            out.push_str(&format!("  {line}\n"));
        }
    }
    let walk = |label: &str, id: &Option<String>| match id {
        Some(id) => format!("{label} {id}\n"),
        None => String::new(),
    };
    let newer = walk("newer  ", &signal.newer_from_source);
    let older = walk("older  ", &signal.older_from_source);
    if !newer.is_empty() || !older.is_empty() {
        out.push_str(&format!("\nfrom {}\n{newer}{older}", signal.source));
    }
    out
}

/// Char-boundary display cap for a one-line cell — the summary is a
/// producer-written sentence with no bound on the wire.
fn display_cap(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let capped: String = s.chars().take(max_chars - 1).collect();
    format!("{capped}…")
}

#[cfg(test)]
mod tests;
