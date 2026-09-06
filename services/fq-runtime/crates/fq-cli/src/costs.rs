//! `fq costs`: per-agent spend totals, read over the authenticated
//! edge.
//!
//! The client half of `cost.summary` (plan Phase 4, verb 13): one call,
//! then rendering. The report — what it computes, and the allocation
//! rule it carries — is `fq_daemon::cost_report`, in the other binary.
//!
//! It used to open the projection itself, which meant spend was
//! readable with the daemon stopped. It is not any more, and unlike
//! `fq doctor` (whose subject is the daemon) nothing about the answer
//! changes: cost figures are kept indefinitely — the retention sweep
//! exempts cost-bearing rows — so this still answers over the whole
//! history rather than a window, from the same rows, through the
//! daemon that owns them.

use crate::cli::GlobalArgs;
use crate::edge_call::edge_invoke;
use fq_ops::surface::CostSummaryParams;
use fq_ops::views::CostReport;

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
    let report: CostReport = serde_json::from_value(output)?;
    print!("{}", render(&report, json)?);
    Ok(())
}

/// The report as `fq costs` prints it: the JSON verbatim, or the
/// per-agent table with its total and the allocation identity under
/// it. A function of the report alone, so the rendering is pinned
/// without an edge.
fn render(report: &CostReport, json: bool) -> anyhow::Result<String> {
    if json {
        return Ok(format!("{}\n", serde_json::to_string_pretty(report)?));
    }

    if report.agents.is_empty() {
        return Ok("No cost events recorded.\n".to_string());
    }

    let mut out = format!(
        "{:<30} {:<10} {:<14} {:<14} {:<14} {:<14} {:<14} total_cost\n",
        "agent",
        "events",
        "input_tokens",
        "output_tokens",
        "cache_read",
        "cache_write",
        "reasoning"
    );
    for row in &report.agents {
        out.push_str(&format!(
            "{:<30} {:<10} {:<14} {:<14} {:<14} {:<14} {:<14} ${:.6}\n",
            row.agent_id,
            row.event_count,
            row.total_input_tokens,
            row.total_output_tokens,
            row.total_cache_read_tokens,
            row.total_cache_write_tokens,
            reasoning_cell(row.total_reasoning_tokens),
            row.total_cost
        ));
    }
    out.push('\n');
    out.push_str(&format!(
        "Total across all agents: ${:.6}\n",
        report.total_cost
    ));
    // The total and the per-invocation figures an operator sees
    // elsewhere do not match, and that is correct: summariser spend is
    // the engine's, charged to no invocation (#466). Print the identity
    // rather than leave the difference to be discovered and filed as a
    // bug — a remainder that is named reconciles, one that is merely
    // absent is a support question.
    out.push_str(&format!(
        "  invocations ${:.6} + framework ${:.6}\n",
        report.total_cost - report.framework_cost,
        report.framework_cost
    ));
    out.push_str("  framework = engine spend (invocation summaries), charged to no invocation\n");
    Ok(out)
}

/// The reasoning column. `n/a` is a provider that reported no
/// thought-versus-spoken split, which is every Anthropic call; it is
/// not a `0`, which is a provider that reported one and it was zero.
/// `--json` carries the same distinction as `null` against `0`.
fn reasoning_cell(tokens: Option<i64>) -> String {
    tokens.map_or_else(|| "n/a".to_string(), |n| n.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fq_ops::views::CostView;

    fn agent(id: &str, reasoning: Option<i64>) -> CostView {
        CostView {
            agent_id: id.to_string(),
            event_count: 1,
            total_cost: 0.5,
            total_input_tokens: 100,
            total_output_tokens: 50,
            total_cache_read_tokens: 0,
            total_cache_write_tokens: 0,
            total_reasoning_tokens: reasoning,
            invocation_count: 1,
            framework_cost: 0.0,
        }
    }

    /// Three agents, one per case: a provider that reported no split,
    /// one that reported zero, one that reported a count.
    fn report() -> CostReport {
        CostReport {
            total_cost: 1.5,
            total_input_tokens: 300,
            total_output_tokens: 150,
            total_cache_read_tokens: 0,
            total_cache_write_tokens: 0,
            total_reasoning_tokens: Some(1_234),
            framework_cost: 0.0,
            agents: vec![
                agent("anthropic-agent", None),
                agent("openai-agent", Some(0)),
                agent("kimi-agent", Some(1_234)),
            ],
            buckets: vec![],
            models: vec![],
        }
    }

    /// The reasoning column of one agent's line — the seventh field.
    fn reasoning_column<'a>(table: &'a str, agent: &str) -> &'a str {
        table
            .lines()
            .find(|line| line.starts_with(agent))
            .unwrap_or_else(|| panic!("{agent} has a line in:\n{table}"))
            .split_whitespace()
            .nth(6)
            .expect("seven fields before the cost")
    }

    /// `n/a` is not `0`: a provider that reported no split — every
    /// Anthropic call — renders as the former, one that reported zero
    /// as the latter.
    #[test]
    fn the_table_renders_an_unreported_split_as_na_and_a_reported_zero_as_zero() {
        let table = render(&report(), false).unwrap();
        let header = table.lines().next().unwrap();
        assert!(
            header.contains("reasoning"),
            "the header names the column: {table}"
        );
        assert_eq!(reasoning_column(&table, "anthropic-agent"), "n/a");
        assert_eq!(reasoning_column(&table, "openai-agent"), "0");
        assert_eq!(reasoning_column(&table, "kimi-agent"), "1234");
        // The column sits under its header.
        let kimi = table.lines().find(|l| l.starts_with("kimi-agent")).unwrap();
        assert_eq!(header.find("reasoning"), kimi.find("1234"), "{table}");
    }

    /// `--json` carries the same distinction as `null` against `0`,
    /// with the key present either way.
    #[test]
    fn the_json_carries_null_against_zero() {
        let json: serde_json::Value =
            serde_json::from_str(&render(&report(), true).unwrap()).unwrap();
        let agents = &json["agents"];
        assert!(
            agents[0]
                .get("total_reasoning_tokens")
                .is_some_and(|v| v.is_null()),
            "an unreported split is an explicit null: {json}"
        );
        assert_eq!(agents[1]["total_reasoning_tokens"], serde_json::json!(0));
        assert_eq!(
            agents[2]["total_reasoning_tokens"],
            serde_json::json!(1_234)
        );
        assert_eq!(json["total_reasoning_tokens"], serde_json::json!(1_234));
    }
}
