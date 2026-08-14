//! `fq costs`: per-agent spend totals, read over the authenticated
//! edge.
//!
//! The client half of `cost.summary` (plan Phase 4, verb 13): one call,
//! then rendering. The report — what it computes, and the allocation
//! rule it carries — is [`crate::cost_report`], daemon-side.
//!
//! It used to open the projection itself, which meant spend was
//! readable with the daemon stopped. It is not any more, and unlike
//! `fq doctor` (whose subject is the daemon) nothing about the answer
//! changes: cost figures are kept indefinitely — the retention sweep
//! exempts cost-bearing rows — so this still answers over the whole
//! history rather than a window, from the same rows, through the
//! daemon that owns them.

use crate::cli::GlobalArgs;
use crate::cost_report::CostSummaryParams;
use crate::edge_call::edge_invoke;

/// Show per-agent cost totals.
pub(crate) async fn show_costs(
    global: &GlobalArgs,
    agent: Option<&str>,
    since: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    let output = edge_invoke(
        global,
        fq_ops::OpId::Report(fq_ops::ReportId::Cost(fq_ops::CostReport::Summary)),
        serde_json::to_value(CostSummaryParams {
            agent: agent.map(str::to_string),
            since: since.map(str::to_string),
            hourly_buckets: false,
        })?,
    )
    .await?
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let report: fq_runtime::views::CostReport = serde_json::from_value(output)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    if report.agents.is_empty() {
        println!("No cost events recorded.");
        return Ok(());
    }

    println!(
        "{:<30} {:<10} {:<14} {:<14} {:<14} {:<14} total_cost",
        "agent", "events", "input_tokens", "output_tokens", "cache_read", "cache_write"
    );
    for row in &report.agents {
        println!(
            "{:<30} {:<10} {:<14} {:<14} {:<14} {:<14} ${:.6}",
            row.agent_id,
            row.event_count,
            row.total_input_tokens,
            row.total_output_tokens,
            row.total_cache_read_tokens,
            row.total_cache_write_tokens,
            row.total_cost
        );
    }
    println!();
    println!("Total across all agents: ${:.6}", report.total_cost);
    // The total and the per-invocation figures an operator sees
    // elsewhere do not match, and that is correct: summariser spend is
    // the engine's, charged to no invocation (#466). Print the identity
    // rather than leave the difference to be discovered and filed as a
    // bug — a remainder that is named reconciles, one that is merely
    // absent is a support question.
    println!(
        "  invocations ${:.6} + framework ${:.6}",
        report.total_cost - report.framework_cost,
        report.framework_cost
    );
    println!("  framework = engine spend (invocation summaries), charged to no invocation");
    Ok(())
}
