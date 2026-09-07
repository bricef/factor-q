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
use fq_ops::views::{CostReport, CostView, ModelCostView};

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
/// per-agent table, the same spend by model, and the total with the
/// allocation identity under it. A function of the report alone, so
/// the rendering is pinned without an edge.
fn render(report: &CostReport, json: bool) -> anyhow::Result<String> {
    if json {
        return Ok(format!("{}\n", serde_json::to_string_pretty(report)?));
    }

    if report.agents.is_empty() {
        return Ok("No cost events recorded.\n".to_string());
    }

    let mut out = Columns::header("agent");
    for row in &report.agents {
        out.push_str(&Columns::from(row).line(&row.agent_id));
    }
    // The same spend by model, in the same columns: comparing models on
    // one agent is where a reasoning-first model's bill shows as mostly
    // thinking, which the agent rows above cannot say.
    if !report.models.is_empty() {
        out.push('\n');
        out.push_str(&Columns::header("model"));
        for row in &report.models {
            out.push_str(&Columns::from(row).line(&row.model));
        }
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

/// The token columns the by-agent and by-model tables share — one
/// shape fed from either view, so the two tables cannot drift apart in
/// their column names or widths.
struct Columns {
    events: i64,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: Option<i64>,
    cost: f64,
}

impl Columns {
    /// The header line, over a key column named `key`.
    fn header(key: &str) -> String {
        format!(
            "{key:<30} {:<10} {:<14} {:<14} {:<14} {:<14} {:<14} total_cost\n",
            "events", "input_tokens", "output_tokens", "cache_read", "cache_write", "reasoning"
        )
    }

    /// One row, keyed by an agent id or a model name.
    fn line(&self, key: &str) -> String {
        format!(
            "{key:<30} {:<10} {:<14} {:<14} {:<14} {:<14} {:<14} ${:.6}\n",
            self.events,
            self.input,
            self.output,
            self.cache_read,
            self.cache_write,
            reasoning_cell(self.reasoning),
            self.cost
        )
    }
}

impl From<&CostView> for Columns {
    fn from(a: &CostView) -> Self {
        Columns {
            events: a.event_count,
            input: a.total_input_tokens,
            output: a.total_output_tokens,
            cache_read: a.total_cache_read_tokens,
            cache_write: a.total_cache_write_tokens,
            reasoning: a.total_reasoning_tokens,
            cost: a.total_cost,
        }
    }
}

impl From<&ModelCostView> for Columns {
    fn from(m: &ModelCostView) -> Self {
        Columns {
            events: m.event_count,
            input: m.total_input_tokens,
            output: m.total_output_tokens,
            cache_read: m.total_cache_read_tokens,
            cache_write: m.total_cache_write_tokens,
            reasoning: m.total_reasoning_tokens,
            cost: m.total_cost,
        }
    }
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

    /// One model's row, with a little cache traffic so those columns
    /// carry something.
    fn model(name: &str, reasoning: Option<i64>) -> ModelCostView {
        ModelCostView {
            model: name.to_string(),
            event_count: 1,
            total_cost: 0.5,
            total_input_tokens: 100,
            total_output_tokens: 50,
            total_cache_read_tokens: 40,
            total_cache_write_tokens: 20,
            total_reasoning_tokens: reasoning,
        }
    }

    /// Three agents, one per case: a provider that reported no split,
    /// one that reported zero, one that reported a count — and the
    /// same three cases by model.
    fn report() -> CostReport {
        CostReport {
            total_cost: 1.5,
            total_input_tokens: 300,
            total_output_tokens: 150,
            total_cache_read_tokens: 120,
            total_cache_write_tokens: 60,
            total_reasoning_tokens: Some(1_234),
            framework_cost: 0.0,
            agents: vec![
                agent("anthropic-agent", None),
                agent("openai-agent", Some(0)),
                agent("kimi-agent", Some(1_234)),
            ],
            buckets: vec![],
            models: vec![
                model("claude-model", None),
                model("openai-model", Some(0)),
                model("kimi-model", Some(1_234)),
            ],
        }
    }

    /// The line keyed by `key` — an agent id or a model name.
    fn line<'a>(table: &'a str, key: &str) -> &'a str {
        table
            .lines()
            .find(|line| line.starts_with(key))
            .unwrap_or_else(|| panic!("{key} has a line in:\n{table}"))
    }

    /// The reasoning column of one line — the seventh field, on both
    /// tables.
    fn reasoning_column<'a>(table: &'a str, key: &str) -> &'a str {
        line(table, key)
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

    /// The by-model table carries the same columns as the by-agent
    /// table above it — the same names at the same widths — and the
    /// same three cells: `n/a` for a model none of whose calls
    /// reported a split, `0` for a reported zero, the count otherwise.
    /// The cache figures are numbers in both.
    #[test]
    fn the_model_table_shares_the_agent_tables_columns_and_cells() {
        let table = render(&report(), false).unwrap();
        let agent_header = line(&table, "agent ");
        let model_header = line(&table, "model ");
        assert_eq!(
            model_header,
            agent_header.replacen("agent", "model", 1),
            "the two headers differ only in their key column: {table}"
        );
        assert_eq!(reasoning_column(&table, "claude-model"), "n/a");
        assert_eq!(reasoning_column(&table, "openai-model"), "0");
        assert_eq!(reasoning_column(&table, "kimi-model"), "1234");
        let fields: Vec<&str> = line(&table, "kimi-model").split_whitespace().collect();
        assert_eq!(&fields[1..6], ["1", "100", "50", "40", "20"], "{table}");
        assert_eq!(fields[7], "$0.500000", "{table}");
        // The model rows sit under the model header, column for column.
        assert_eq!(
            model_header.find("reasoning"),
            line(&table, "kimi-model").find("1234"),
            "{table}"
        );

        // Without model rows there is no model table.
        let mut bare = report();
        bare.models.clear();
        let table = render(&bare, false).unwrap();
        assert!(
            !table.lines().any(|l| l.starts_with("model ")),
            "got: {table}"
        );
    }

    /// `--json` carries the same distinction as `null` against `0`,
    /// with the key present either way — on the agent rows, the model
    /// rows and the total; the cache totals are numbers throughout.
    #[test]
    fn the_json_carries_null_against_zero() {
        let json: serde_json::Value =
            serde_json::from_str(&render(&report(), true).unwrap()).unwrap();
        for rows in [&json["agents"], &json["models"]] {
            assert!(
                rows[0]
                    .get("total_reasoning_tokens")
                    .is_some_and(|v| v.is_null()),
                "an unreported split is an explicit null: {json}"
            );
            assert_eq!(rows[1]["total_reasoning_tokens"], serde_json::json!(0));
            assert_eq!(rows[2]["total_reasoning_tokens"], serde_json::json!(1_234));
        }
        assert_eq!(
            json["models"][0]["total_cache_read_tokens"],
            serde_json::json!(40)
        );
        assert_eq!(
            json["models"][0]["total_cache_write_tokens"],
            serde_json::json!(20)
        );
        assert_eq!(json["total_reasoning_tokens"], serde_json::json!(1_234));
    }
}
