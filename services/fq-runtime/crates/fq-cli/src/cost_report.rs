//! The Cost domain's reports, daemon-side (plan Phase 4, verb 13):
//! `cost.summary` and `cost.by_agent`.
//!
//! The first reports the surface has ever declared, so a word on what
//! makes them reports rather than reads. A report is a **named, typed
//! computation scoped to a domain** — not a Get on a pretend-resource,
//! not a view, not a query language. `Cost` carries no catalogue
//! resource at all: there is no cost atom to Get and no cost fold to
//! List, only these two named promises. That is exactly the shape the
//! domain model predicted for a report-only domain, and it is what
//! makes an aggregate a **privilege boundary**: authority is Read on
//! `Cost`, so spend is grantable to a caller who may not read the raw
//! event log it is computed from. The handlers below read the
//! projection with system authority, as every report's handler does —
//! input lineage is contract prose, not machinery.
//!
//! **The allocation rule travels with the declaration (#466).** An
//! invocation summary costs money and that money is the engine's, not
//! the invocation's — so the aggregates report it and the
//! per-invocation views do not, which leaves per-invocation figures
//! deliberately short of the total. That gap is contract, not
//! folklore, so it is stated in the declared descriptions where any
//! reader of `fq ops list` meets it, in the words the
//! `invocation_summary` design note already uses
//! (`docs/design/committed/event-schema.md`).

use std::sync::Arc;

use fq_edge::wire::WireError;
use fq_runtime::views::Views;

/// The typed parameters of `cost.summary`.
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct CostSummaryParams {
    /// Narrow the per-agent rows to one agent. Absent reports the
    /// whole fleet.
    #[serde(default)]
    pub(crate) agent: Option<String>,
    /// Lower bound on time, in the `views::since` grammar. Absent
    /// reports the whole recorded history — cost-bearing rows are
    /// exempt from the retention sweep, so that is a real answer here
    /// rather than whatever happened to survive it.
    #[serde(default)]
    pub(crate) since: Option<String>,
    /// Bucket the time series hourly instead of daily.
    #[serde(default)]
    pub(crate) hourly_buckets: bool,
}

/// The typed parameters of `cost.by_agent`.
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct CostByAgentParams {
    /// The agent to break down. Required — this report is one agent's
    /// drill-down where `cost.summary` is the fleet's.
    pub(crate) agent: String,
    #[serde(default)]
    pub(crate) since: Option<String>,
    /// Cap on the per-invocation rows, newest first.
    #[serde(default = "default_invocation_limit")]
    pub(crate) invocation_limit: i64,
}

fn default_invocation_limit() -> i64 {
    50
}

/// Parse a `since` parameter into the bound the stores compare
/// against. An unparseable spelling is a verdict on the request, not a
/// reason to answer over the whole history — the same treatment, and
/// the same grammar, the Event atom's filter gives it.
fn since_bound(spelling: Option<&str>, op: &str) -> Result<Option<String>, WireError> {
    spelling
        .map(|s| {
            fq_runtime::views::since::lower_bound(s).map_err(|e| WireError::InvalidInput {
                op: op.to_string(),
                message: format!("since {e}"),
            })
        })
        .transpose()
}

/// Register `cost.summary` and `cost.by_agent` on the daemon's edge.
pub(crate) fn register_cost_reports(
    registry: &mut fq_edge::EdgeRegistry,
    views: Arc<Views>,
) -> anyhow::Result<()> {
    let summary_views = views.clone();
    let by_agent_views = views;

    let decl = fq_ops::Report::new::<CostSummaryParams, fq_runtime::views::CostReport>(
        fq_ops::CostReport::Summary,
        "Fleet spend: per-agent rows, the per-model split, the time series, and the totals.",
        fq_ops::Stability::Experimental,
    )
    .description(
        "Answers over the whole recorded history unless `since` narrows it: cost-bearing \
         rows are exempt from the retention sweep and kept indefinitely, so this report \
         never silently windows. The time series ignores `agent` deliberately — the chart \
         answers what the fleet is burning, the tables answer who burned it. Totals \
         include summary costs; `framework_cost` says how much of the total they are.",
    );
    registry
        .report::<CostSummaryParams, fq_runtime::views::CostReport, _, _>(
            decl,
            move |params: CostSummaryParams| {
                let views = summary_views.clone();
                async move {
                    let since = since_bound(params.since.as_deref(), "cost.summary")?;
                    views
                        .costs(
                            params.agent.as_deref(),
                            since.as_deref(),
                            params.hourly_buckets,
                        )
                        .await
                        .map_err(|e| WireError::Internal {
                            message: e.to_string(),
                        })
                }
            },
        )
        .map_err(|e| anyhow::anyhow!("operator registry: {e}"))?;

    let decl = fq_ops::Report::new::<CostByAgentParams, fq_runtime::views::AgentCostDetailView>(
        fq_ops::CostReport::ByAgent,
        "One agent's spend, broken down by model and by invocation.",
        fq_ops::Stability::Experimental,
    )
    .description(
        "The drill-down behind one row of `cost.summary`. `invocations` is newest first \
         and capped by `invocation_limit`; `totals.invocation_count` carries the uncapped \
         count, so a truncated list is visible as one. An agent with no cost events in \
         the window is not found rather than an empty breakdown — there is no row of the \
         summary to drill into. Totals include summary costs; the per-invocation rows do \
         not.",
    );
    registry
        .report::<CostByAgentParams, fq_runtime::views::AgentCostDetailView, _, _>(
            decl,
            move |params: CostByAgentParams| {
                let views = by_agent_views.clone();
                async move {
                    let since = since_bound(params.since.as_deref(), "cost.by_agent")?;
                    views
                        .agent_costs(&params.agent, since.as_deref(), params.invocation_limit)
                        .await
                        .map_err(|e| WireError::Internal {
                            message: e.to_string(),
                        })?
                        .ok_or_else(|| WireError::NotFound {
                            op: "cost.by_agent".into(),
                            message: format!(
                                "no cost events for agent `{}` in this window",
                                params.agent
                            ),
                        })
                }
            },
        )
        .map_err(|e| anyhow::anyhow!("operator registry: {e}"))?;

    Ok(())
}
