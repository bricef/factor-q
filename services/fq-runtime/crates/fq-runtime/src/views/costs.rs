//! The cost reads: what was spent, by whom, on what model, when — and
//! how much of it belongs to no invocation at all.
//!
//! Split out of [`super`] as its own sibling, mirroring the store side
//! they read through (`control_plane::projection::store::costs`). The
//! `impl Views` block below is part of the same inherent impl, so the
//! methods keep their paths; the DTOs stay with the rest of the view
//! shapes in [`super`].
//!
//! **The allocation rule (#466).** Summariser spend is the engine's own
//! — real money, but owed to no single invocation. So the aggregates
//! report it and the per-invocation views do not, which means
//! per-invocation figures deliberately fall short of the total. The
//! shortfall is named rather than left to be discovered:
//! [`CostView::framework_cost`] per agent,
//! [`CostReport::framework_cost`] across the fleet, and for every agent
//!
//! ```text
//! total_cost = <its per-invocation costs> + framework_cost
//! ```

use super::{
    AgentCostDetailView, CostBucketView, CostReport, CostView, InvocationCostView, ModelCostView,
    Views, ViewsError,
};

impl Views {
    /// Per-agent cost/token aggregate plus grand totals, including the
    /// portion of the total that no invocation caused.
    pub async fn costs(
        &self,
        agent: Option<&str>,
        since: Option<&str>,
        hourly_buckets: bool,
    ) -> Result<CostReport, ViewsError> {
        let rows = self.projection.cost_summary(agent, since).await?;
        let mut report = CostReport::default();
        for r in rows {
            report.total_cost += r.total_cost;
            report.total_input_tokens += r.total_input_tokens;
            report.total_output_tokens += r.total_output_tokens;
            report.total_cache_read_tokens += r.total_cache_read_tokens;
            report.total_cache_write_tokens += r.total_cache_write_tokens;
            report.framework_cost += r.framework_cost;
            report.agents.push(CostView::from(r));
        }
        report.models = self
            .projection
            .cost_by_model(agent, since)
            .await?
            .into_iter()
            .map(ModelCostView::from)
            .collect();
        // The time series ignores the agent filter deliberately: the
        // chart answers "what is the fleet burning", the agent filter
        // narrows the tables. Revisit if a per-agent chart is wanted.
        report.buckets = self
            .projection
            .cost_by_time_bucket(hourly_buckets, since)
            .await?
            .into_iter()
            .map(CostBucketView::from)
            .collect();
        Ok(report)
    }

    /// One agent's cost drill-down — totals plus per-model and
    /// per-invocation breakdowns (invocations newest first, capped at
    /// `invocation_limit`). `None` when the agent has no cost events
    /// in the window.
    ///
    /// `invocations` sums to `totals.total_cost -
    /// totals.framework_cost` when the limit does not truncate it; for
    /// the reserved `summary` agent that means totals with no
    /// invocations under them, which is the allocation rule showing
    /// through rather than missing data.
    pub async fn agent_costs(
        &self,
        agent: &str,
        since: Option<&str>,
        invocation_limit: i64,
    ) -> Result<Option<AgentCostDetailView>, ViewsError> {
        let mut rows = self.projection.cost_summary(Some(agent), since).await?;
        let Some(totals) = rows.pop() else {
            return Ok(None);
        };
        let models = self
            .projection
            .cost_by_model(Some(agent), since)
            .await?
            .into_iter()
            .map(ModelCostView::from)
            .collect();
        let invocations = self
            .projection
            .cost_by_invocation(agent, since, invocation_limit)
            .await?
            .into_iter()
            .map(InvocationCostView::from)
            .collect();
        Ok(Some(AgentCostDetailView {
            agent_id: agent.to_string(),
            totals: CostView::from(totals),
            models,
            invocations,
        }))
    }
}
