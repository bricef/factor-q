//! Cost and failure aggregation over the projected `events` table, and
//! the row types those queries return.
//!
//! Split out of the parent module for file size. The `impl
//! ProjectionStore` block below is part of the same inherent impl, so
//! the methods keep their paths; the row types are re-exported from
//! `store`, so theirs are unchanged too.

use sqlx::Row;

use super::{ProjectionStore, StoreError};

impl ProjectionStore {
    /// Aggregate cost-bearing events into per-agent totals. Cost
    /// now rides on `llm.response` envelopes (envelope-refactor
    /// plan step 3), so the filter is `total_cost IS NOT NULL`
    /// instead of `event_type = 'cost'`. The event-type allowlist
    /// covers per-call cost carriers only — `llm_response`,
    /// `llm_failure` (#447: an empty completion still bills for the
    /// prefill) and the summariser's `invocation_summary` (#216) —
    /// because terminal events (`completed`/`failed`) carry
    /// invocation *totals* and would double-count.
    ///
    /// This query answers *what was spent*, so it is **wide**:
    /// summariser spend is real money and is reported here (#466).
    /// It is not, however, work any one invocation asked for, so the
    /// per-invocation queries exclude it — see
    /// [`Self::cost_by_invocation`]. [`CostSummary::framework_cost`]
    /// names the portion that split leaves unallocated, so a caller
    /// can state `total_cost = allocated + framework_cost` instead of
    /// leaving the reader to find the gap.
    pub async fn cost_summary(
        &self,
        agent: Option<&str>,
        since: Option<&str>,
    ) -> Result<Vec<CostSummary>, StoreError> {
        let mut sql = String::from(
            "SELECT agent_id, \
             COUNT(*) AS event_count, \
             COALESCE(SUM(total_cost), 0.0) AS total_cost, \
             COALESCE(SUM(input_tokens), 0) AS total_input_tokens, \
             COALESCE(SUM(output_tokens), 0) AS total_output_tokens, \
             COALESCE(SUM(cache_read_tokens), 0) AS total_cache_read_tokens, \
             COALESCE(SUM(cache_write_tokens), 0) AS total_cache_write_tokens, \
             COUNT(DISTINCT invocation_id) AS invocation_count, \
             COALESCE(SUM(CASE WHEN event_type = 'invocation_summary' \
             THEN total_cost ELSE 0.0 END), 0.0) AS framework_cost \
             FROM events \
             WHERE event_type IN ('llm_response', 'llm_failure', 'invocation_summary') \
             AND total_cost IS NOT NULL",
        );
        if agent.is_some() {
            sql.push_str(" AND agent_id = ?");
        }
        if since.is_some() {
            sql.push_str(" AND timestamp >= ?");
        }
        sql.push_str(" GROUP BY agent_id ORDER BY total_cost DESC");

        let mut q = sqlx::query(&sql);
        if let Some(a) = agent {
            q = q.bind(a);
        }
        if let Some(s) = since {
            q = q.bind(s);
        }
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|row| CostSummary {
                agent_id: row.get::<String, _>(0),
                event_count: row.get::<i64, _>(1),
                total_cost: row.get::<f64, _>(2),
                total_input_tokens: row.get::<i64, _>(3),
                total_output_tokens: row.get::<i64, _>(4),
                total_cache_read_tokens: row.get::<i64, _>(5),
                total_cache_write_tokens: row.get::<i64, _>(6),
                invocation_count: row.get::<i64, _>(7),
                framework_cost: row.get::<f64, _>(8),
            })
            .collect())
    }

    /// One agent's cost-bearing events grouped per invocation, newest
    /// first (by each invocation's first cost event), capped at
    /// `limit`.
    ///
    /// This query answers *what did this invocation cost*, so it is
    /// **narrow**: `invocation_summary` is excluded (#466). The
    /// summariser is an engine concern — it runs on the invocation's
    /// behalf, never at its request — so charging its spend to the
    /// invocation would misreport the invocation. The money is not
    /// lost: every aggregate reports it ([`Self::cost_summary`],
    /// [`Self::cost_by_model`], [`Self::cost_by_time_bucket`]) and
    /// [`CostSummary::framework_cost`] names it.
    ///
    /// The consequence is deliberate: these rows sum to the agent's
    /// `total_cost - framework_cost`, not to its `total_cost` — and
    /// only when `limit` does not truncate them.
    ///
    /// The columns it groups on (`invocation_id`, and `model` for
    /// [`Self::cost_by_model`]) have been on every event row since the
    /// original schema — no new columns, only new GROUP BYs.
    pub async fn cost_by_invocation(
        &self,
        agent: &str,
        since: Option<&str>,
        limit: i64,
    ) -> Result<Vec<InvocationCostSummary>, StoreError> {
        let mut sql = String::from(
            "SELECT invocation_id, \
             MIN(timestamp) AS first_timestamp, \
             COUNT(*) AS event_count, \
             COALESCE(SUM(total_cost), 0.0) AS total_cost, \
             COALESCE(SUM(input_tokens), 0) AS total_input_tokens, \
             COALESCE(SUM(output_tokens), 0) AS total_output_tokens, \
             COALESCE(SUM(cache_read_tokens), 0) AS total_cache_read_tokens, \
             COALESCE(SUM(cache_write_tokens), 0) AS total_cache_write_tokens \
             FROM events \
             WHERE event_type IN ('llm_response', 'llm_failure') \
             AND total_cost IS NOT NULL \
             AND agent_id = ?",
        );
        if since.is_some() {
            sql.push_str(" AND timestamp >= ?");
        }
        sql.push_str(" GROUP BY invocation_id ORDER BY first_timestamp DESC LIMIT ?");

        let mut q = sqlx::query(&sql).bind(agent);
        if let Some(s) = since {
            q = q.bind(s);
        }
        q = q.bind(limit);
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|row| InvocationCostSummary {
                invocation_id: row.get::<String, _>(0),
                first_timestamp: row.get::<String, _>(1),
                event_count: row.get::<i64, _>(2),
                total_cost: row.get::<f64, _>(3),
                total_input_tokens: row.get::<i64, _>(4),
                total_output_tokens: row.get::<i64, _>(5),
                total_cache_read_tokens: row.get::<i64, _>(6),
                total_cache_write_tokens: row.get::<i64, _>(7),
            })
            .collect())
    }

    /// One invocation's cost aggregate, keyed by id rather than by
    /// agent. Per-invocation, so the same rule as
    /// [`Self::cost_by_invocation`] applies for the same reason —
    /// summariser spend is the engine's, not this invocation's — and
    /// the two therefore share an event-type allowlist. This doc
    /// asserted that sameness before it was true: until #466 only
    /// this query excluded `invocation_summary`, so the two views of
    /// one invocation's cost could disagree. `None` when the
    /// invocation has no cost-bearing events yet.
    pub async fn cost_of_invocation(
        &self,
        invocation_id: &str,
    ) -> Result<Option<InvocationCostSummary>, StoreError> {
        let row = sqlx::query(
            "SELECT invocation_id, \
             MIN(timestamp) AS first_timestamp, \
             COUNT(*) AS event_count, \
             COALESCE(SUM(total_cost), 0.0) AS total_cost, \
             COALESCE(SUM(input_tokens), 0) AS total_input_tokens, \
             COALESCE(SUM(output_tokens), 0) AS total_output_tokens, \
             COALESCE(SUM(cache_read_tokens), 0) AS total_cache_read_tokens, \
             COALESCE(SUM(cache_write_tokens), 0) AS total_cache_write_tokens \
             FROM events \
             WHERE event_type IN ('llm_response', 'llm_failure') AND total_cost IS NOT NULL \
             AND invocation_id = ? \
             GROUP BY invocation_id",
        )
        .bind(invocation_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| InvocationCostSummary {
            invocation_id: row.get::<String, _>(0),
            first_timestamp: row.get::<String, _>(1),
            event_count: row.get::<i64, _>(2),
            total_cost: row.get::<f64, _>(3),
            total_input_tokens: row.get::<i64, _>(4),
            total_output_tokens: row.get::<i64, _>(5),
            total_cache_read_tokens: row.get::<i64, _>(6),
            total_cache_write_tokens: row.get::<i64, _>(7),
        }))
    }

    /// Cost-bearing events summed per time bucket, oldest first.
    ///
    /// *What was spent, when* — an aggregate, so it carries the wide
    /// allowlist of [`Self::cost_summary`], `invocation_summary`
    /// included. Until #466 it was the one aggregate that dropped
    /// summariser spend, so the series and the per-agent table
    /// disagreed over the same window; a bucketed total now sums to
    /// the same money as the agent table it sits above.
    ///
    /// The bucket key is a fixed-width prefix of the RFC3339 UTC timestamp
    /// — `substr` instead of SQLite's date functions, which cannot
    /// parse our nanosecond fractions: 10 chars = `YYYY-MM-DD` (day),
    /// 13 chars = `YYYY-MM-DDTHH` (hour). Buckets with no cost events
    /// simply don't appear; the caller fills gaps for display.
    pub async fn cost_by_time_bucket(
        &self,
        hourly: bool,
        since: Option<&str>,
    ) -> Result<Vec<CostBucketSummary>, StoreError> {
        let prefix_len = if hourly { 13 } else { 10 };
        let mut sql = format!(
            "SELECT substr(timestamp, 1, {prefix_len}) AS bucket, \
             COALESCE(SUM(total_cost), 0.0) AS total_cost \
             FROM events \
             WHERE event_type IN ('llm_response', 'llm_failure', 'invocation_summary') \
             AND total_cost IS NOT NULL",
        );
        if since.is_some() {
            sql.push_str(" AND timestamp >= ?");
        }
        sql.push_str(" GROUP BY bucket ORDER BY bucket ASC");

        let mut q = sqlx::query(&sql);
        if let Some(s) = since {
            q = q.bind(s);
        }
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|row| CostBucketSummary {
                bucket: row.get::<String, _>(0),
                total_cost: row.get::<f64, _>(1),
            })
            .collect())
    }

    /// Cost-bearing events grouped per model, biggest spender first —
    /// across every agent, or one agent when `agent` is set. Another
    /// aggregate, so it shares [`Self::cost_summary`]'s wide
    /// allowlist: the summariser's model appears here with its own
    /// spend, which is the point of a per-model split.
    pub async fn cost_by_model(
        &self,
        agent: Option<&str>,
        since: Option<&str>,
    ) -> Result<Vec<ModelCostSummary>, StoreError> {
        let mut sql = String::from(
            "SELECT COALESCE(model, 'unknown') AS model, \
             COUNT(*) AS event_count, \
             COALESCE(SUM(total_cost), 0.0) AS total_cost, \
             COALESCE(SUM(input_tokens), 0) AS total_input_tokens, \
             COALESCE(SUM(output_tokens), 0) AS total_output_tokens \
             FROM events \
             WHERE event_type IN ('llm_response', 'llm_failure', 'invocation_summary') \
             AND total_cost IS NOT NULL",
        );
        if agent.is_some() {
            sql.push_str(" AND agent_id = ?");
        }
        if since.is_some() {
            sql.push_str(" AND timestamp >= ?");
        }
        sql.push_str(" GROUP BY model ORDER BY total_cost DESC");

        let mut q = sqlx::query(&sql);
        if let Some(a) = agent {
            q = q.bind(a);
        }
        if let Some(s) = since {
            q = q.bind(s);
        }
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|row| ModelCostSummary {
                model: row.get::<String, _>(0),
                event_count: row.get::<i64, _>(1),
                total_cost: row.get::<f64, _>(2),
                total_input_tokens: row.get::<i64, _>(3),
                total_output_tokens: row.get::<i64, _>(4),
            })
            .collect())
    }

    /// Aggregate terminal `failed` events into per-`FailureKind`
    /// counts. Symmetric with [`Self::cost_summary`]: the DB stores
    /// the failure kind in the denormalised `error_kind` column
    /// (the serde snake_case form, e.g. `budget_exceeded`),
    /// so this groups by that column for a stable typed-ish shape the
    /// `fq doctor` command can render without re-reading payloads.
    pub async fn failure_summary(&self) -> Result<Vec<FailureSummary>, StoreError> {
        let rows = sqlx::query(
            "SELECT COALESCE(error_kind, 'unknown') AS kind, COUNT(*) AS n \
             FROM events \
             WHERE event_type = 'failed' \
             GROUP BY kind ORDER BY n DESC, kind",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| FailureSummary {
                error_kind: row.get::<String, _>(0),
                count: row.get::<i64, _>(1),
            })
            .collect())
    }
}

/// One row of a cost summary.
#[derive(Debug, Clone)]
pub struct CostSummary {
    pub agent_id: String,
    pub event_count: i64,
    pub total_cost: f64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    pub total_cache_write_tokens: i64,
    /// Distinct invocations behind the aggregate — "how many runs did
    /// this spend buy".
    pub invocation_count: i64,
    /// The part of `total_cost` the engine spent on this agent's
    /// behalf while owing it to no single invocation — today, the
    /// summariser's `invocation_summary` calls (#216). Reported,
    /// because it is real money; never allocated, because no
    /// invocation asked for it (#466). It is the remainder that makes
    /// the aggregate reconcile:
    ///
    /// ```text
    /// total_cost = <the agent's per-invocation costs> + framework_cost
    /// ```
    ///
    /// Zero for an ordinary agent — summariser events land under the
    /// reserved `summary` agent id — so the identity holds trivially
    /// there and carries the whole row for `summary`.
    pub framework_cost: f64,
}

/// One invocation's share of an agent's spend — a row from
/// [`ProjectionStore::cost_by_invocation`].
#[derive(Debug, Clone)]
pub struct InvocationCostSummary {
    pub invocation_id: String,
    /// RFC3339 timestamp of the invocation's first cost event — its
    /// effective start, as far as the projection knows.
    pub first_timestamp: String,
    pub event_count: i64,
    pub total_cost: f64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    pub total_cache_write_tokens: i64,
}

/// One time bucket's cost sum — a row from
/// [`ProjectionStore::cost_by_time_bucket`].
#[derive(Debug, Clone, PartialEq)]
pub struct CostBucketSummary {
    /// `YYYY-MM-DD` (daily) or `YYYY-MM-DDTHH` (hourly), UTC.
    pub bucket: String,
    pub total_cost: f64,
}

/// One model's share of an agent's spend — a row from
/// [`ProjectionStore::cost_by_model`].
#[derive(Debug, Clone)]
pub struct ModelCostSummary {
    /// Model name as recorded on the event; `unknown` for rows written
    /// before the model column was populated.
    pub model: String,
    pub event_count: i64,
    pub total_cost: f64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
}

/// One row of a failure summary: a terminal `FailureKind` and the
/// number of `failed` events carrying it. Produced by
/// [`ProjectionStore::failure_summary`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureSummary {
    /// Lowercased failure kind as stored in the projection
    /// (`budget_exceeded`, `llm_error`, `max_iterations`, `tool_error`,
    /// `sandbox_violation`, `runtime_error`), or `unknown` for a
    /// `failed` row with no recorded kind.
    pub error_kind: String,
    pub count: i64,
}
