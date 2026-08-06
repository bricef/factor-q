//! `fq costs`: per-agent spend totals from the projection.
//!
//! Split out of `lib.rs` (#189). Cost figures are kept indefinitely — the
//! retention sweep exempts cost-bearing rows — so this report answers over the
//! whole history, not a window.

use crate::cli::GlobalArgs;
use crate::open_views;

/// Show per-agent cost totals from the SQLite projection.
pub(crate) async fn show_costs(
    global: &GlobalArgs,
    agent: Option<&str>,
    since: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    let views = open_views(global).await?;
    let report = views.costs(agent, since, false).await?;

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
    Ok(())
}
