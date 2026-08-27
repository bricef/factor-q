//! `fq status`: what this client is configured to reach, whether a
//! daemon answered, and what that daemon says about itself.
//!
//! The client half of `control.status` (plan Phase 4, verb 14): one
//! edge call, then rendering. The report — the build, the stream
//! probe, the registry census, the projection position and the
//! recovery counts — is `fq_daemon::status_report`, in the other
//! binary.
//!
//! **This is the verb an operator runs when things are broken, so it
//! must not become another broken thing.** Every other migrated read
//! traded "works with the daemon stopped" for "answers from the daemon
//! that owns the data", and for most of them that trade is clean. Here
//! it would not be: a `fq status` that answered `Connection refused`
//! would fail exactly where it is reached for. So the trade is made
//! deliberately and only halfway.
//!
//! * **A daemon that cannot be reached is a finding, not an error.**
//!   It is reported as one, in the section where the daemon's answers
//!   would have been, naming what is consequently absent rather than
//!   leaving the reader to notice.
//! * **What was never the daemon's stays local.** The edge address
//!   this client dials is its own configuration, so it is answered
//!   whatever happened on the wire. The store paths are not: they
//!   belong to the process that owns those files, which need not be
//!   on this machine, so they travel on the report and are absent —
//!   with the reason — when none arrives.
//! * **The exit code still separates the two.** A degraded answer
//!   exits non-zero after printing, so `fq status && deploy` fails
//!   closed while an operator reading the output still gets everything
//!   that was knowable.
//!
//! What is genuinely lost is that the store *contents* — the
//! projection row count and the recovery counts — used to be readable
//! with nothing running. What they described then was a fold nobody
//! was advancing, presented without the caveat; the honest version of
//! that answer is the one this verb now gives, which is that there is
//! no runtime to describe.

use crate::cli::GlobalArgs;
use crate::edge_call::edge_invoke;
use fq_ops::surface::{StatusParams, StatusRegistry, StatusReport, StatusStores};

/// The `fq status --json` document: the local blocks, then either the
/// daemon's report or the reason there is not one.
///
/// Grouped rather than flat, and the grouping is the contract: it
/// answers the question a degraded status raises, which is *which of
/// these did I learn from a running daemon, and which would have been
/// true anyway*. A consumer testing `daemon == null` is testing
/// "nothing is running", which is the single most useful thing this
/// command reports.
#[derive(serde::Serialize)]
struct StatusDocument {
    config: StatusConfig,
    /// The daemon's own report, when one answered.
    daemon: Option<StatusReport>,
    /// Why none answered, when none did. Exactly one of this and
    /// `daemon` is ever set.
    daemon_error: Option<String>,
}

/// The configuration this client resolved — **its own**, which is not
/// necessarily the daemon's. It says which broker and which daemon
/// this `fq` would talk to, and where it would look for state; a
/// client pointed at a different config file than the daemon it dials
/// is a real and diagnosable situation, and printing the client's view
/// is what makes it visible.
#[derive(serde::Serialize)]
struct StatusConfig {
    /// The edge address this client dials — the only piece of
    /// configuration it owns. The broker URL, the agents directory and
    /// the cache directory are the *daemon's*, and a client that
    /// printed its own copy would be describing its own machine.
    edge: String,
}

pub(crate) async fn show_status(global: &GlobalArgs, json: bool) -> anyhow::Result<()> {
    let edge = crate::edge_call::daemon_addr(global)?;

    let (daemon, daemon_error) = match daemon_status(global).await {
        Ok(report) => (Some(report), None),
        // The whole chain, one line: the outer sentence says which
        // step failed (dial, pair, invoke) and the inner one says why.
        Err(err) => (None, Some(format!("{err:#}"))),
    };

    let document = StatusDocument {
        config: StatusConfig { edge: edge.clone() },
        daemon,
        daemon_error,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&document)?);
    } else {
        print!("{}", render_status_human(&document));
    }

    if let Some(reason) = &document.daemon_error {
        // Non-zero AFTER the answer, not instead of it. A health gate
        // (`fq status && …`) has to fail closed when there is no
        // runtime, and an operator has to keep the half of the report
        // that did not need one — the same division `fq doctor
        // --fail-on-issues` makes between stdout and the exit code.
        anyhow::bail!("{reason} — no daemon answered, so the status above is local facts only");
    }
    Ok(())
}

/// Ask the daemon for `control.status`.
async fn daemon_status(global: &GlobalArgs) -> anyhow::Result<StatusReport> {
    let output = edge_invoke(
        global,
        fq_ops::OpId::Report(fq_ops::ReportId::Control(fq_ops::ControlReport::Status)),
        serde_json::to_value(StatusParams {})?,
    )
    .await?
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(serde_json::from_value(output)?)
}

/// Pure: render the whole human overview, so every branch of it — and
/// especially the one an operator only ever sees when something is
/// wrong — is testable without a daemon, a broker or a store.
fn render_status_human(doc: &StatusDocument) -> String {
    let mut out = String::from("factor-q status\n\n");
    out.push_str("Config\n");
    out.push_str(&format!("  edge:             {}\n", doc.config.edge));

    out.push_str("\nDaemon\n");
    let Some(report) = &doc.daemon else {
        let reason = doc.daemon_error.as_deref().unwrap_or("unknown");
        out.push_str(&format!(
            "  connection:       ✗ no daemon answered at {}\n",
            doc.config.edge
        ));
        out.push_str(&format!("  reason:           {reason}\n"));
        out.push_str(
            "  -> that is the finding: there is no runtime here to report on.\n\
             \x20 -> stream health, the agent registry, projection rows and recovery state\n\
             \x20    are the daemon's to answer, so they are absent below rather than stale.\n\
             \x20 -> the store paths too: they are the daemon's, and a client that printed\n\
             \x20    its own guess would be describing this machine, not the runtime.\n",
        );
        return out;
    };

    out.push_str(&format!(
        "  connection:       ✓ answered at {}\n",
        doc.config.edge
    ));
    out.push_str(&format!("  version:          {}\n", report.version));
    out.push_str(&render_registry_human(&report.registry));
    out.push_str(&format!("  projection rows:  {}\n", report.projection_rows));

    for stream in &report.streams {
        out.push_str(&render_stream_health_human(stream));
    }
    out.push_str(&render_stores_human(&report.stores));

    // Recovery state: points the operator at the commands they'd need
    // if anything is off; renders "All clear." otherwise.
    out.push_str("\nRecovery state\n");
    out.push_str(&render_recovery_guidance(
        report.recovery.ambiguous,
        report.recovery.stale_workers,
    ));
    out
}

/// Pure: the registry census as one line, plus a line per rejection.
/// A rejected definition is named rather than counted, because the
/// message carries the file and the parse error — everything the
/// operator needs to go fix it.
fn render_registry_human(registry: &StatusRegistry) -> String {
    let mut out = if registry.load_errors.is_empty() {
        format!("  agents:           {} loaded\n", registry.agents)
    } else {
        format!(
            "  agents:           {} loaded, {} rejected\n",
            registry.agents,
            registry.load_errors.len()
        )
    };
    for error in &registry.load_errors {
        out.push_str(&format!("    -> {error}\n"));
    }
    out
}

/// Pure: the store block, as the daemon that answered reported it —
/// the paths it is actually running on and whether the files are
/// there. Rendered only on that branch, because there is nothing
/// truthful to print about another host's stores.
fn render_stores_human(stores: &StatusStores) -> String {
    let mut out = String::from("\nStores\n");
    out.push_str(&format!("  worker db:        {}\n", stores.worker_path));
    out.push_str(&format!(
        "  control-plane db: {}\n",
        stores.control_plane_path
    ));
    out.push_str(&format!("  projection db:    {}\n", stores.projection_path));
    if let Some(legacy) = &stores.legacy_events_db {
        out.push_str(&format!(
            "  legacy events.db: {} (pending split — start `fqd` to migrate)\n",
            legacy
        ));
    }
    if stores.initialised {
        out.push_str("  state:            initialised — all three store files exist\n");
    } else {
        out.push_str("  state:            not initialised (start `fqd` to create)\n");
    }
    out
}

/// Pure: render one probed stream exactly as `fq status` always has.
/// The probe is the daemon's now (it owns the broker connection) but
/// the data is the same typed [`fq_ops::health::StreamHealth`] and
/// the rendering is unchanged.
fn render_stream_health_human(health: &fq_ops::health::StreamHealth) -> String {
    use fq_ops::health::{ConsumerHealth, StreamHealth};

    let mut out = format!("\nStream: {}\n", health.stream());
    match health {
        StreamHealth::Unavailable { error, .. } => {
            out.push_str(&format!("  state:            ✗ {error}\n"));
        }
        StreamHealth::Available {
            messages,
            bytes,
            first_seq,
            last_seq,
            consumer,
            ..
        } => {
            out.push_str(&format!("  messages:         {messages}\n"));
            out.push_str(&format!(
                "  bytes:            {}\n",
                fq_tools::builtin::exec::human_bytes(*bytes)
            ));
            out.push_str(&format!("  first seq:        {first_seq}\n"));
            out.push_str(&format!("  last seq:         {last_seq}\n"));
            match consumer {
                ConsumerHealth::Active {
                    name,
                    delivered,
                    lag,
                    ack_pending,
                    num_pending,
                    num_redelivered,
                } => {
                    let status = if *lag == 0 {
                        "✓ caught up"
                    } else if *lag < 10 {
                        "◐ slightly behind"
                    } else {
                        "✗ lagging"
                    };
                    out.push_str(&format!(
                        "  consumer {name}: {status} (delivered {delivered}, lag {lag})\n"
                    ));
                    if *ack_pending > 0 {
                        out.push_str(&format!("    ack pending:    {ack_pending}\n"));
                    }
                    if *num_pending > 0 {
                        out.push_str(&format!("    num pending:    {num_pending}\n"));
                    }
                    if *num_redelivered > 0 {
                        out.push_str(&format!(
                            "    redelivered:    {num_redelivered} (retrying; bound {})\n",
                            fq_ops::surface::TRIGGER_MAX_DELIVER
                        ));
                    }
                }
                ConsumerHealth::Error { name, error } => {
                    out.push_str(&format!("  consumer {name}: ✗ info failed: {error}\n"));
                }
                ConsumerHealth::Missing { name } => {
                    out.push_str(&format!(
                        "  consumer {name}: not present (no daemon has initialised it)\n"
                    ));
                }
            }
        }
    }
    out
}

/// Pure: render the recovery-guidance block of `fq status`
/// from two counts. The text includes the next-step commands
/// so the operator can copy-paste rather than remember syntax.
fn render_recovery_guidance(ambiguous_count: i64, stale_worker_count: i64) -> String {
    if ambiguous_count == 0 && stale_worker_count == 0 {
        return "  All clear.\n".to_string();
    }
    let mut out = String::new();
    if ambiguous_count > 0 {
        out.push_str(&format!(
            "  Ambiguous invocations: {ambiguous_count}\n\
             \x20\x20  -> `fq invocation list --status=ambiguous` to inspect\n\
             \x20\x20  -> `fq invocation drop <id>` to triage individually\n"
        ));
    }
    if stale_worker_count > 0 {
        out.push_str(&format!(
            // No remediation to offer, deliberately: a stale row is
            // evidence, and reclaiming it is the daemon's scheduled
            // retention sweep, not an operator chore. The line says so
            // rather than leaving the count looking like a to-do.
            "  Stale workers: {stale_worker_count}\n\
             \x20\x20  -> `fq workers list --stale-only` to inspect\n\
             \x20\x20  -> rows are reclaimed by the daemon's retention sweep; no action needed\n"
        ));
    }
    out
}

#[cfg(test)]
mod tests;
