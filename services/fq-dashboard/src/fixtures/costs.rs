//! The cost pages' fixtures: the fleet report, its last-24h companion
//! and one agent's drill-down.
//!
//! Their own module for the reason `health.rs` is: `fixtures.rs`
//! crossed the file-size cap when the cost views gained their
//! reasoning column, and the ratchet's answer to a file that grows is
//! to split it, not to raise its budget. The three share one roster of
//! agents, so they travel together; the numbers are unchanged.

use fq_ops::views::{
    AgentCostDetailView, CostReport, CostView, InvocationCostView, ModelCostView, sum_reported,
};

use super::NOW_MS;

pub(super) fn cost_report() -> CostReport {
    let agents = vec![
        CostView {
            agent_id: "m0-issue-fix".to_string(),
            event_count: 1_112,
            total_cost: 95.869_869,
            total_input_tokens: 120_411_850,
            total_output_tokens: 663_307,
            total_cache_read_tokens: 98_000_000,
            total_cache_write_tokens: 1_200_000,
            // The gpt calls report their split; the opus calls do not.
            total_reasoning_tokens: Some(98_400),
            invocation_count: 38,
            framework_cost: 0.0,
        },
        CostView {
            agent_id: "m0-loop".to_string(),
            event_count: 162,
            total_cost: 6.153_685,
            total_input_tokens: 7_409_042,
            total_output_tokens: 58_912,
            total_cache_read_tokens: 5_900_000,
            total_cache_write_tokens: 74_000,
            // A reported zero, beside the `n/a` of the agents whose
            // provider reports no split at all — the screenshot shows
            // the two are not the same cell.
            total_reasoning_tokens: Some(0),
            invocation_count: 6,
            framework_cost: 0.0,
        },
        CostView {
            agent_id: "doc-drift".to_string(),
            event_count: 80,
            total_cost: 1.341_442,
            total_input_tokens: 491_700,
            total_output_tokens: 20_545,
            total_cache_read_tokens: 380_000,
            total_cache_write_tokens: 5_000,
            total_reasoning_tokens: None,
            invocation_count: 15,
            framework_cost: 0.0,
        },
        // The reserved `summary` agent: engine spend on invocation
        // summaries, charged to no invocation (#466), so its whole row is
        // framework cost and its invocation count is zero. It is what
        // gives the total a remainder to name.
        CostView {
            agent_id: "summary".to_string(),
            event_count: 61,
            total_cost: 0.913_4,
            total_input_tokens: 812_000,
            total_output_tokens: 24_600,
            total_cache_read_tokens: 640_000,
            total_cache_write_tokens: 0,
            total_reasoning_tokens: None,
            invocation_count: 0,
            framework_cost: 0.913_4,
        },
        // One-shot e2e instances (uuid-suffixed ids): the costs page
        // folds these into per-family rows so they cannot bury the
        // named agents above — the fold is part of the screenshot.
        CostView {
            agent_id: "overspender-019f339c15767d70b8ffd6d7ca6b0a70".to_string(),
            event_count: 1,
            total_cost: 1.0,
            total_input_tokens: 1_000_000,
            total_output_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_write_tokens: 0,
            total_reasoning_tokens: None,
            invocation_count: 1,
            framework_cost: 0.0,
        },
        CostView {
            agent_id: "overspender-019f339b43c47822bdff48bec821d815".to_string(),
            event_count: 1,
            total_cost: 1.0,
            total_input_tokens: 1_000_000,
            total_output_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_write_tokens: 0,
            total_reasoning_tokens: None,
            invocation_count: 1,
            framework_cost: 0.0,
        },
        CostView {
            agent_id: "self-inspect-019f339c171373c189d801651bdee8e5".to_string(),
            event_count: 2,
            total_cost: 0.000_65,
            total_input_tokens: 250,
            total_output_tokens: 80,
            total_cache_read_tokens: 0,
            total_cache_write_tokens: 0,
            total_reasoning_tokens: None,
            invocation_count: 1,
            framework_cost: 0.0,
        },
    ];
    CostReport {
        total_cost: agents.iter().map(|a| a.total_cost).sum(),
        total_input_tokens: agents.iter().map(|a| a.total_input_tokens).sum(),
        total_output_tokens: agents.iter().map(|a| a.total_output_tokens).sum(),
        total_cache_read_tokens: agents.iter().map(|a| a.total_cache_read_tokens).sum(),
        total_cache_write_tokens: agents.iter().map(|a| a.total_cache_write_tokens).sum(),
        total_reasoning_tokens: agents
            .iter()
            .fold(None, |acc, a| sum_reported(acc, a.total_reasoning_tokens)),
        framework_cost: agents.iter().map(|a| a.framework_cost).sum(),
        agents,
        // A week of daily spend ending at the frozen "now" — the
        // page-top bar chart in the screenshot.
        buckets: vec![
            ("2026-07-05", 11.20),
            ("2026-07-06", 19.85),
            ("2026-07-07", 8.13),
            ("2026-07-08", 24.90),
            ("2026-07-09", 3.41),
            ("2026-07-10", 21.06),
            ("2026-07-11", 14.81),
        ]
        .into_iter()
        .map(|(bucket, total_cost)| fq_ops::views::CostBucketView {
            bucket: bucket.to_string(),
            total_cost,
        })
        .collect(),
        // The same spend split by model — the page's "By model" table.
        // The cache figures sum to the agents' totals above, and the
        // reasoning column shows all three cells: the opus calls report
        // no split (`n/a`), the gpt calls report the agents' whole
        // figure, and the glm calls reported zero.
        models: vec![
            ModelCostView {
                model: "claude-opus-4-8".to_string(),
                event_count: 999,
                total_cost: 88.126_871,
                total_input_tokens: 105_800_000,
                total_output_tokens: 590_000,
                total_cache_read_tokens: 98_600_000,
                total_cache_write_tokens: 1_200_000,
                total_reasoning_tokens: None,
            },
            ModelCostView {
                model: "openai/gpt-5.6-terra".to_string(),
                event_count: 300,
                total_cost: 14.357_865,
                total_input_tokens: 21_699_210,
                total_output_tokens: 152_087,
                total_cache_read_tokens: 5_900_000,
                total_cache_write_tokens: 74_000,
                total_reasoning_tokens: Some(98_400),
            },
            ModelCostView {
                model: "z-ai/glm-5.2".to_string(),
                event_count: 58,
                total_cost: 2.880_260,
                total_input_tokens: 4_813_382,
                total_output_tokens: 31_677,
                total_cache_read_tokens: 420_000,
                total_cache_write_tokens: 5_000,
                total_reasoning_tokens: Some(0),
            },
        ],
    }
}

/// The day-bounded companion report behind the costs page's "last 24h"
/// column: only the agents that spent in the last day, fixed values.
pub(super) fn day_cost_report() -> CostReport {
    let agents = vec![
        CostView {
            agent_id: "m0-issue-fix".to_string(),
            event_count: 145,
            total_cost: 13.156_3,
            total_input_tokens: 16_800_000,
            total_output_tokens: 38_900,
            total_cache_read_tokens: 15_700_000,
            total_cache_write_tokens: 0,
            total_reasoning_tokens: None,
            invocation_count: 3,
            framework_cost: 0.0,
        },
        CostView {
            agent_id: "doc-drift".to_string(),
            event_count: 6,
            total_cost: 0.063_4,
            total_input_tokens: 21_400,
            total_output_tokens: 1_800,
            total_cache_read_tokens: 8_200,
            total_cache_write_tokens: 900,
            total_reasoning_tokens: None,
            invocation_count: 1,
            framework_cost: 0.0,
        },
    ];
    CostReport {
        total_cost: agents.iter().map(|a| a.total_cost).sum(),
        total_input_tokens: agents.iter().map(|a| a.total_input_tokens).sum(),
        total_output_tokens: agents.iter().map(|a| a.total_output_tokens).sum(),
        total_cache_read_tokens: agents.iter().map(|a| a.total_cache_read_tokens).sum(),
        total_cache_write_tokens: agents.iter().map(|a| a.total_cache_write_tokens).sum(),
        total_reasoning_tokens: agents
            .iter()
            .fold(None, |acc, a| sum_reported(acc, a.total_reasoning_tokens)),
        framework_cost: agents.iter().map(|a| a.framework_cost).sum(),
        agents,
        // Unused by the last-24h merge — only per-agent costs are read.
        buckets: vec![],
        models: vec![],
    }
}

/// The single-agent drill-down fixture: the multi-model spender with a
/// few invocations, capped below its invocation count so the
/// "showing N of M" footer is part of the screenshot.
pub(super) fn agent_cost_detail() -> AgentCostDetailView {
    let inv = |id: &str,
               ago_ms: i64,
               calls: i64,
               cost: f64,
               input: i64,
               cache: i64,
               reasoning: Option<i64>| InvocationCostView {
        invocation_id: id.to_string(),
        started_at_ms: NOW_MS - ago_ms,
        event_count: calls,
        total_cost: cost,
        total_input_tokens: input,
        total_output_tokens: input / 500,
        total_cache_read_tokens: cache,
        total_cache_write_tokens: 0,
        total_reasoning_tokens: reasoning,
    };
    AgentCostDetailView {
        agent_id: "m0-issue-fix".to_string(),
        totals: CostView {
            agent_id: "m0-issue-fix".to_string(),
            event_count: 1_112,
            total_cost: 95.869_869,
            total_input_tokens: 120_411_850,
            total_output_tokens: 663_307,
            total_cache_read_tokens: 98_000_000,
            total_cache_write_tokens: 1_200_000,
            // The gpt calls report their split; the opus calls do not.
            total_reasoning_tokens: Some(98_400),
            invocation_count: 38,
            framework_cost: 0.0,
        },
        // The two routes, splitting the totals above: the opus calls
        // report no split, the gpt calls carry the whole figure.
        models: vec![
            ModelCostView {
                model: "claude-opus-4-8".to_string(),
                event_count: 812,
                total_cost: 81.512_004,
                total_input_tokens: 98_712_640,
                total_output_tokens: 511_220,
                total_cache_read_tokens: 92_100_000,
                total_cache_write_tokens: 1_126_000,
                total_reasoning_tokens: None,
            },
            ModelCostView {
                model: "openai/gpt-5.6-terra".to_string(),
                event_count: 300,
                total_cost: 14.357_865,
                total_input_tokens: 21_699_210,
                total_output_tokens: 152_087,
                total_cache_read_tokens: 5_900_000,
                total_cache_write_tokens: 74_000,
                total_reasoning_tokens: Some(98_400),
            },
        ],
        invocations: vec![
            // Two runs on the route that reports the split, one on the
            // route that does not: the by-invocation table shows a
            // count beside an `n/a`, and the two counts are the totals.
            inv(
                "019f534f-4b3c-7f42-a619-b5e43a64fd38",
                600_000,
                52,
                2.213_7,
                6_723_812,
                6_554_327,
                Some(41_200),
            ),
            inv(
                "019f5b3f-31fb-7ae0-b130-3d65ccf40375",
                7_200_000,
                32,
                0.253_8,
                454_471,
                433_790,
                None,
            ),
            inv(
                "019f3844-11aa-7bb0-8cc1-dd22ee33ff44",
                86_400_000,
                54,
                1.576_4,
                4_582_808,
                4_474_643,
                Some(57_200),
            ),
        ],
    }
}
