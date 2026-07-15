//! Canned, deterministic page fixtures — the input for the dashboard's
//! screenshot pipeline (`fq-dashboard render-fixtures`, driven by
//! `scripts/dashboard-screenshots.sh` and the CI screenshots job).
//!
//! Everything here is fixed data with fixed timestamps so the rendered
//! HTML — and therefore the screenshots — are byte-stable across runs:
//! a visual diff means the rendering changed, never the clock.

use std::path::Path;

use fq_runtime::health::{ConsumerHealth, StreamHealth};
use fq_runtime::read_service::HealthReport;
use fq_runtime::views::{
    AgentCostDetailView, CostReport, CostView, EventView, ExecutionsView, FailureView,
    InvocationCostView, InvocationDetailView, InvocationSummaryView, LiveExecutionView,
    LlmDispatchView, ModelCostView, RecoveryView, ToolDispatchView,
};

use crate::render;

/// The fixtures' frozen "now" (2026-07-11T21:00:00Z).
const NOW_MS: i64 = 1_783_803_600_000;
const REFRESH_SECS: u64 = 5;

pub(crate) fn health_report() -> HealthReport {
    HealthReport {
        version: "0.1.0+abc123def456".to_string(),
        streams: vec![
            StreamHealth::Available {
                stream: "fq-events".to_string(),
                messages: 60_744,
                bytes: 393_248_768,
                first_seq: 1,
                last_seq: 60_744,
                consumer: ConsumerHealth::Active {
                    name: "fq-projector".to_string(),
                    delivered: 60_744,
                    lag: 0,
                    ack_pending: 0,
                    num_pending: 0,
                    num_redelivered: 0,
                },
            },
            StreamHealth::Available {
                stream: "fq-triggers".to_string(),
                messages: 3,
                bytes: 333,
                first_seq: 30,
                last_seq: 32,
                consumer: ConsumerHealth::Active {
                    name: "fq-dispatcher".to_string(),
                    delivered: 29,
                    lag: 3,
                    ack_pending: 1,
                    num_pending: 2,
                    num_redelivered: 4,
                },
            },
        ],
        event_count: 64_016,
        recovery: RecoveryView {
            ambiguous: 3,
            stale_workers: 2,
            stale_worker_ids: vec!["019f3383-d8a5".to_string(), "019f339a-9613".to_string()],
        },
        executions: ExecutionsView {
            in_flight: 2,
            working: 1,
            working_ids: vec!["019f5b3f-31fb-7ae0-b130-3d65ccf40375".to_string()],
            stuck: 1,
            stuck_ids: vec!["019f534f-4b3c-7f42-a619-b5e43a64fd38".to_string()],
        },
        failures: vec![
            FailureView {
                error_kind: "budgetexceeded".to_string(),
                count: 2,
            },
            FailureView {
                error_kind: "toolerror".to_string(),
                count: 1,
            },
            FailureView {
                error_kind: "triggerexhausted".to_string(),
                count: 1,
            },
        ],
    }
}

fn active_rows() -> Vec<fq_runtime::views::ActiveInvocationView> {
    vec![
        fq_runtime::views::ActiveInvocationView {
            invocation_id: "019f534f-4b3c-7f42-a619-b5e43a64fd38".to_string(),
            agent_id: "m0-issue-fix".to_string(),
            phase: "dispatching_tools".to_string(),
            step_index: 165,
            started_at_ms: NOW_MS - 600_000,
            updated_at_ms: NOW_MS - 45_000,
            open_tools: vec!["exec".to_string()],
            open_llms: vec![],
            summary: Some("Fixing #83: SECURITY.md drafted, running just ci".to_string()),
        },
        fq_runtime::views::ActiveInvocationView {
            invocation_id: "019f5b3f-31fb-7ae0-b130-3d65ccf40375".to_string(),
            agent_id: "m0-loop".to_string(),
            phase: "awaiting_model".to_string(),
            step_index: 44,
            started_at_ms: NOW_MS - 300_000,
            updated_at_ms: NOW_MS - 8_000,
            open_tools: vec![],
            open_llms: vec!["claude-opus-4-8".to_string()],
            summary: None,
        },
    ]
}

fn invocation_rows() -> Vec<InvocationSummaryView> {
    vec![
        InvocationSummaryView {
            invocation_id: "019f534f-4b3c-7f42-a619-b5e43a64fd38".to_string(),
            agent_id: Some("m0-issue-fix".to_string()),
            worker_id: "019f5339-4d8e-7840-96e0-fd8553d2d171".to_string(),
            status: "in_flight".to_string(),
            assigned_at_ms: NOW_MS - 600_000,
            started_at_ms: NOW_MS - 600_000,
            archived: false,
            summary: Some("Fixing #83: SECURITY.md drafted, running just ci".to_string()),
        },
        InvocationSummaryView {
            invocation_id: "019f3844-11aa-7bb0-8cc1-dd22ee33ff44".to_string(),
            agent_id: Some("m0-loop".to_string()),
            worker_id: "019f3840-7c23-7f70-9c01-eb8e377290ee".to_string(),
            status: "completed".to_string(),
            assigned_at_ms: NOW_MS - 7_200_000,
            started_at_ms: NOW_MS - 7_200_000,
            archived: false,
            summary: Some("Done: docs drift fixed, PR #141 opened".to_string()),
        },
        InvocationSummaryView {
            invocation_id: "019f36b6-55ff-7001-9223-445566778899".to_string(),
            agent_id: Some("doc-drift".to_string()),
            worker_id: String::new(),
            status: "completed".to_string(),
            // Archive-only row: assigned_at_ms carries archived_at, the
            // started column shows the true start (a little earlier).
            assigned_at_ms: NOW_MS - 86_400_000,
            started_at_ms: NOW_MS - 90_000_000,
            archived: true,
            summary: None,
        },
        InvocationSummaryView {
            invocation_id: "019f33a7-9988-7776-6554-332211009988".to_string(),
            agent_id: None,
            worker_id: "019f33a6-68b6-7670-83b9-07c2fd570bd4".to_string(),
            status: "failed".to_string(),
            assigned_at_ms: NOW_MS - 172_800_000,
            started_at_ms: NOW_MS - 172_800_000,
            archived: false,
            summary: Some("Failed: budget exceeded before a PR could be opened".to_string()),
        },
    ]
}

fn event_rows() -> Vec<EventView> {
    let mk = |ts: &str, agent: &str, inv: &str, ty: &str, cost: Option<f64>| EventView {
        event_id: format!("evt-{ty}-{inv}"),
        timestamp: ts.to_string(),
        agent_id: agent.to_string(),
        invocation_id: inv.to_string(),
        event_type: ty.to_string(),
        model: Some("claude-sonnet-4-5".to_string()),
        total_cost: cost,
        error_kind: None,
        duration_ms: Some(1_234),
    };
    vec![
        mk(
            "2026-07-11T20:59:12.000Z",
            "m0-issue-fix",
            "019f534f-4b3c-7f42-a619-b5e43a64fd38",
            "tool_call",
            None,
        ),
        mk(
            "2026-07-11T20:59:11.000Z",
            "m0-issue-fix",
            "019f534f-4b3c-7f42-a619-b5e43a64fd38",
            "llm_response",
            None,
        ),
        mk(
            "2026-07-11T20:58:40.000Z",
            "m0-issue-fix",
            "019f534f-4b3c-7f42-a619-b5e43a64fd38",
            "cost",
            Some(0.087_314),
        ),
        mk(
            "2026-07-11T20:31:02.000Z",
            "doc-drift",
            "019f36b6-55ff-7001-9223-445566778899",
            "completed",
            None,
        ),
        mk(
            "2026-07-11T20:30:00.000Z",
            "doc-drift",
            "019f36b6-55ff-7001-9223-445566778899",
            "triggered",
            None,
        ),
    ]
}

fn invocation_detail() -> InvocationDetailView {
    InvocationDetailView {
        invocation_id: "019f534f-4b3c-7f42-a619-b5e43a64fd38".to_string(),
        agent_id: Some("m0-issue-fix".to_string()),
        summary: Some("Fixing #83: SECURITY.md drafted, running just ci".to_string()),
        owner: Some(invocation_rows().remove(0)),
        // No archive row: this fixture is the standout live case — an
        // in-flight invocation — and a real one is never archived while
        // its WAL row is live (caught by looking at the screenshot).
        archive: None,
        live: Some(LiveExecutionView {
            phase: "dispatching_tools".to_string(),
            step_index: 165,
            started_at_ms: NOW_MS - 600_000,
            updated_at_ms: NOW_MS - 45_000,
            terminal_at_ms: None,
            tools: vec![ToolDispatchView {
                tool_call_id: "tc-1".to_string(),
                tool_name: "exec".to_string(),
                status: "intent".to_string(),
                is_error: None,
                intent_at_ms: NOW_MS - 45_000,
                dispatched_at_ms: None,
                completed_at_ms: None,
            }],
            llms: vec![LlmDispatchView {
                request_id: "req-9".to_string(),
                model: "claude-opus-4-8".to_string(),
                status: "dispatched".to_string(),
                cost_usd: None,
                is_error: None,
                intent_at_ms: NOW_MS - 50_000,
                dispatched_at_ms: Some(NOW_MS - 49_000),
                completed_at_ms: None,
            }],
        }),
        recent_events: event_rows().into_iter().take(3).collect(),
    }
}

fn transcript_entries() -> Vec<fq_runtime::transcript::TranscriptEntry> {
    use fq_runtime::transcript::{AssistantToolCall, TranscriptEntry};
    vec![
        TranscriptEntry::Prompt {
            timestamp_ms: NOW_MS - 600_000,
            system: Some(
                "You are m0-issue-fix. Fix the referenced issue end-to-end: clone the \
                 repo, branch, make the minimal change, validate with `just ci`, open \
                 a PR. Report honestly — never claim work you could not persist."
                    .to_string(),
            ),
            user: Some(r#"{"task":"fix","issue":86,"repo":"bricef/factor-q"}"#.to_string()),
        },
        TranscriptEntry::Assistant {
            timestamp_ms: NOW_MS - 590_000,
            model: "claude-opus-4-8".to_string(),
            content: Some("Reading the issue first, then cloning into the workspace.".to_string()),
            tool_calls: vec![AssistantToolCall {
                tool_call_id: "tc-1".to_string(),
                tool_name: "exec".to_string(),
                parameters: serde_json::json!({
                    "command": "gh issue view 86 --repo bricef/factor-q",
                    "cwd": "${workspace}"
                }),
            }],
            cost_usd: Some(0.0214),
            is_error: Some(false),
        },
        TranscriptEntry::ToolResult {
            timestamp_ms: NOW_MS - 585_000,
            tool_call_id: "tc-1".to_string(),
            tool_name: "exec".to_string(),
            parameters: serde_json::json!({
                "command": "gh issue view 86 --repo bricef/factor-q"
            }),
            // The <script> is deliberate: the screenshot itself proves
            // payloads render as text, never as markup.
            output: Some(
                "title: feat: file_list and file_search built-ins\nstate: OPEN\n\
                 <script>alert('escaped, not executed')</script>\nlabels: enhancement"
                    .to_string(),
            ),
            is_error: Some(false),
        },
        TranscriptEntry::ToolResult {
            timestamp_ms: NOW_MS - 560_000,
            tool_call_id: "tc-2".to_string(),
            tool_name: "file_write".to_string(),
            parameters: serde_json::json!({"path": "\"${workspace}/notes.md\""}),
            output: Some(
                "invalid parameters: the path contains literal quote/backslash \
                 characters, which usually means the tool-call argument was wrapped \
                 in an extra layer of quoting — resend the bare path with no embedded \
                 quotes"
                    .to_string(),
            ),
            is_error: Some(true),
        },
    ]
}

fn cost_report() -> CostReport {
    let agents = vec![
        CostView {
            agent_id: "m0-issue-fix".to_string(),
            event_count: 1_112,
            total_cost: 95.869_869,
            total_input_tokens: 120_411_850,
            total_output_tokens: 663_307,
            total_cache_read_tokens: 98_000_000,
            total_cache_write_tokens: 1_200_000,
            invocation_count: 38,
        },
        CostView {
            agent_id: "m0-loop".to_string(),
            event_count: 162,
            total_cost: 6.153_685,
            total_input_tokens: 7_409_042,
            total_output_tokens: 58_912,
            total_cache_read_tokens: 5_900_000,
            total_cache_write_tokens: 74_000,
            invocation_count: 6,
        },
        CostView {
            agent_id: "doc-drift".to_string(),
            event_count: 80,
            total_cost: 1.341_442,
            total_input_tokens: 491_700,
            total_output_tokens: 20_545,
            total_cache_read_tokens: 380_000,
            total_cache_write_tokens: 5_000,
            invocation_count: 15,
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
            invocation_count: 1,
        },
        CostView {
            agent_id: "overspender-019f339b43c47822bdff48bec821d815".to_string(),
            event_count: 1,
            total_cost: 1.0,
            total_input_tokens: 1_000_000,
            total_output_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_write_tokens: 0,
            invocation_count: 1,
        },
        CostView {
            agent_id: "self-inspect-019f339c171373c189d801651bdee8e5".to_string(),
            event_count: 2,
            total_cost: 0.000_65,
            total_input_tokens: 250,
            total_output_tokens: 80,
            total_cache_read_tokens: 0,
            total_cache_write_tokens: 0,
            invocation_count: 1,
        },
    ];
    CostReport {
        total_cost: agents.iter().map(|a| a.total_cost).sum(),
        total_input_tokens: agents.iter().map(|a| a.total_input_tokens).sum(),
        total_output_tokens: agents.iter().map(|a| a.total_output_tokens).sum(),
        total_cache_read_tokens: agents.iter().map(|a| a.total_cache_read_tokens).sum(),
        total_cache_write_tokens: agents.iter().map(|a| a.total_cache_write_tokens).sum(),
        agents,
        // The same spend split by model — the page's "By model" table.
        models: vec![
            ModelCostView {
                model: "claude-opus-4-8".to_string(),
                event_count: 999,
                total_cost: 88.126_871,
                total_input_tokens: 105_800_000,
                total_output_tokens: 590_000,
            },
            ModelCostView {
                model: "openai/gpt-5.6-terra".to_string(),
                event_count: 300,
                total_cost: 14.357_865,
                total_input_tokens: 21_699_210,
                total_output_tokens: 152_087,
            },
            ModelCostView {
                model: "z-ai/glm-5.2".to_string(),
                event_count: 58,
                total_cost: 2.880_260,
                total_input_tokens: 4_813_382,
                total_output_tokens: 31_677,
            },
        ],
    }
}

/// The day-bounded companion report behind the costs page's "last 24h"
/// column: only the agents that spent in the last day, fixed values.
fn day_cost_report() -> CostReport {
    let agents = vec![
        CostView {
            agent_id: "m0-issue-fix".to_string(),
            event_count: 145,
            total_cost: 13.156_3,
            total_input_tokens: 16_800_000,
            total_output_tokens: 38_900,
            total_cache_read_tokens: 15_700_000,
            total_cache_write_tokens: 0,
            invocation_count: 3,
        },
        CostView {
            agent_id: "doc-drift".to_string(),
            event_count: 6,
            total_cost: 0.063_4,
            total_input_tokens: 21_400,
            total_output_tokens: 1_800,
            total_cache_read_tokens: 8_200,
            total_cache_write_tokens: 900,
            invocation_count: 1,
        },
    ];
    CostReport {
        total_cost: agents.iter().map(|a| a.total_cost).sum(),
        total_input_tokens: agents.iter().map(|a| a.total_input_tokens).sum(),
        total_output_tokens: agents.iter().map(|a| a.total_output_tokens).sum(),
        total_cache_read_tokens: agents.iter().map(|a| a.total_cache_read_tokens).sum(),
        total_cache_write_tokens: agents.iter().map(|a| a.total_cache_write_tokens).sum(),
        agents,
        // Unused by the last-24h merge — only per-agent costs are read.
        models: vec![],
    }
}

/// The single-agent drill-down fixture: the multi-model spender with a
/// few invocations, capped below its invocation count so the
/// "showing N of M" footer is part of the screenshot.
fn agent_cost_detail() -> AgentCostDetailView {
    let inv =
        |id: &str, ago_ms: i64, calls: i64, cost: f64, input: i64, cache: i64| InvocationCostView {
            invocation_id: id.to_string(),
            started_at_ms: NOW_MS - ago_ms,
            event_count: calls,
            total_cost: cost,
            total_input_tokens: input,
            total_output_tokens: input / 500,
            total_cache_read_tokens: cache,
            total_cache_write_tokens: 0,
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
            invocation_count: 38,
        },
        models: vec![
            ModelCostView {
                model: "claude-opus-4-8".to_string(),
                event_count: 812,
                total_cost: 81.512_004,
                total_input_tokens: 98_712_640,
                total_output_tokens: 511_220,
            },
            ModelCostView {
                model: "openai/gpt-5.6-terra".to_string(),
                event_count: 300,
                total_cost: 14.357_865,
                total_input_tokens: 21_699_210,
                total_output_tokens: 152_087,
            },
        ],
        invocations: vec![
            inv(
                "019f534f-4b3c-7f42-a619-b5e43a64fd38",
                600_000,
                52,
                2.213_7,
                6_723_812,
                6_554_327,
            ),
            inv(
                "019f5b3f-31fb-7ae0-b130-3d65ccf40375",
                7_200_000,
                32,
                0.253_8,
                454_471,
                433_790,
            ),
            inv(
                "019f3844-11aa-7bb0-8cc1-dd22ee33ff44",
                86_400_000,
                54,
                1.576_4,
                4_582_808,
                4_474_643,
            ),
        ],
    }
}

/// The agents-list fixture: the dogfood roster plus one broken
/// definition, so the load-error surface is part of the screenshot.
fn agents_view() -> fq_runtime::read_service::AgentsView {
    use fq_runtime::read_service::AgentSummaryView;
    let mk = |id: &str, model: &str, budget: Option<f64>, trigger: Option<&str>, tools, prompt| {
        AgentSummaryView {
            agent_id: id.to_string(),
            model: model.to_string(),
            budget,
            trigger: trigger.map(String::from),
            tool_count: tools,
            prompt_bytes: prompt,
        }
    };
    fq_runtime::read_service::AgentsView {
        agents: vec![
            mk(
                "doc-drift",
                "claude-sonnet-4-5",
                Some(2.0),
                Some("doc-drift"),
                4,
                2_180,
            ),
            mk(
                "m0-issue-fix",
                "claude-opus-4-8",
                Some(12.0),
                Some("m0-issue-fix"),
                6,
                4_212,
            ),
            mk("m0-loop", "claude-opus-4-8", Some(20.0), None, 6, 3_704),
            mk(
                "m0-review-fix",
                "claude-opus-4-8",
                Some(15.0),
                Some("m0-review-fix"),
                6,
                3_950,
            ),
        ],
        errors: vec![
            "failed to parse /home/fq/agents/experimental.md: missing required field `model`"
                .to_string(),
        ],
    }
}

/// The agent-detail fixture: the multi-tool dogfood fixer with its
/// system prompt in the collapsed details block.
fn agent_detail_view() -> fq_runtime::read_service::AgentDetailView {
    fq_runtime::read_service::AgentDetailView {
        agent_id: "m0-issue-fix".to_string(),
        model: "claude-opus-4-8".to_string(),
        system_prompt: "You are m0-issue-fix. Fix the referenced issue end-to-end: clone the \
                        repo, branch, make the minimal change, validate with `just ci`, open \
                        a PR. Report honestly — never claim work you could not persist."
            .to_string(),
        tools: vec![
            "exec".to_string(),
            "file_read".to_string(),
            "file_write".to_string(),
        ],
        mcp_servers: vec!["github".to_string()],
        budget: Some(12.0),
        max_iterations: Some(200),
        effort: Some("high".to_string()),
        trigger: Some("m0-issue-fix".to_string()),
        path: "/home/fq/agents/m0-issue-fix.md".to_string(),
    }
}

/// Render every page to `<out>/<name>.html`; returns the page names.
pub fn write_all(out: &Path) -> std::io::Result<Vec<String>> {
    std::fs::create_dir_all(out)?;
    let pages: Vec<(&str, String)> = vec![
        (
            "health",
            render::page(
                "health",
                REFRESH_SECS,
                &render::health(&health_report(), NOW_MS),
            ),
        ),
        (
            "invocations",
            render::page(
                "invocations",
                REFRESH_SECS,
                &render::invocations_page(&active_rows(), &invocation_rows(), true, NOW_MS),
            ),
        ),
        (
            "invocation-detail",
            render::page(
                "invocation 019f534f",
                REFRESH_SECS,
                &render::invocation_detail(&invocation_detail(), NOW_MS),
            ),
        ),
        (
            // The live case: no Outcome yet — the status footer shows
            // the stream-is-live signal.
            "transcript",
            render::page_opts(
                "transcript 019f534f",
                None,
                "",
                &render::transcript(
                    &transcript_entries(),
                    NOW_MS,
                    false,
                    "019f534f-4b3c-7f42-a619-b5e43a64fd38",
                    Some("Fixing #83: SECURITY.md drafted, running just ci"),
                ),
            ),
        ),
        (
            // The finished case: an Outcome closes the timeline and the
            // status footer says no more turns are expected.
            "transcript-completed",
            render::page_opts(
                "transcript 019f534f",
                None,
                "",
                &render::transcript(
                    &{
                        let mut entries = transcript_entries();
                        entries.push(fq_runtime::transcript::TranscriptEntry::Outcome {
                            timestamp_ms: NOW_MS - 30_000,
                            phase: "completed".to_string(),
                        });
                        entries
                    },
                    NOW_MS,
                    false,
                    "019f534f-4b3c-7f42-a619-b5e43a64fd38",
                    Some("Done: SECURITY.md landed via PR #141, ci green"),
                ),
            ),
        ),
        (
            "events",
            render::page("events", REFRESH_SECS, &render::events(&event_rows())),
        ),
        (
            "costs",
            render::page(
                "costs",
                REFRESH_SECS,
                &render::costs(&cost_report(), &day_cost_report(), render::Window::All),
            ),
        ),
        (
            "costs-agent",
            render::page(
                "costs · m0-issue-fix",
                REFRESH_SECS,
                &render::agent_costs(&agent_cost_detail(), render::Window::All, NOW_MS),
            ),
        ),
        (
            "agents",
            render::page("agents", REFRESH_SECS, &render::agents(&agents_view())),
        ),
        (
            "agent-detail",
            render::page(
                "agent · m0-issue-fix",
                REFRESH_SECS,
                &render::agent_detail(&agent_detail_view()),
            ),
        ),
        (
            "unreachable",
            render::page(
                "health",
                REFRESH_SECS,
                &render::unreachable(
                    "127.0.0.1:9471",
                    "connect: Connection refused (os error 111)",
                    Some(NOW_MS - 90_000),
                    NOW_MS,
                ),
            ),
        ),
    ];
    let mut names = Vec::with_capacity(pages.len());
    for (name, html) in pages {
        std::fs::write(out.join(format!("{name}.html")), html)?;
        names.push(name.to_string());
    }
    Ok(names)
}
