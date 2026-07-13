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
    CostReport, CostView, EventView, ExecutionsView, FailureView, InvocationDetailView,
    InvocationSummaryView, LiveExecutionView, LlmDispatchView, RecoveryView, ToolDispatchView,
};

use crate::render;

/// The fixtures' frozen "now" (2026-07-11T21:00:00Z).
const NOW_MS: i64 = 1_783_803_600_000;
const REFRESH_SECS: u64 = 5;

fn health_report() -> HealthReport {
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
            working: 0,
            working_ids: vec![],
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
        },
        InvocationSummaryView {
            invocation_id: "019f3844-11aa-7bb0-8cc1-dd22ee33ff44".to_string(),
            agent_id: Some("m0-loop".to_string()),
            worker_id: "019f3840-7c23-7f70-9c01-eb8e377290ee".to_string(),
            status: "completed".to_string(),
            assigned_at_ms: NOW_MS - 7_200_000,
            started_at_ms: NOW_MS - 7_200_000,
            archived: false,
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
        },
        InvocationSummaryView {
            invocation_id: "019f33a7-9988-7776-6554-332211009988".to_string(),
            agent_id: None,
            worker_id: "019f33a6-68b6-7670-83b9-07c2fd570bd4".to_string(),
            status: "failed".to_string(),
            assigned_at_ms: NOW_MS - 172_800_000,
            started_at_ms: NOW_MS - 172_800_000,
            archived: false,
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
        },
        CostView {
            agent_id: "m0-loop".to_string(),
            event_count: 162,
            total_cost: 6.153_685,
            total_input_tokens: 7_409_042,
            total_output_tokens: 58_912,
            total_cache_read_tokens: 5_900_000,
            total_cache_write_tokens: 74_000,
        },
        CostView {
            agent_id: "doc-drift".to_string(),
            event_count: 80,
            total_cost: 1.341_442,
            total_input_tokens: 491_700,
            total_output_tokens: 20_545,
            total_cache_read_tokens: 380_000,
            total_cache_write_tokens: 5_000,
        },
    ];
    CostReport {
        total_cost: agents.iter().map(|a| a.total_cost).sum(),
        total_input_tokens: agents.iter().map(|a| a.total_input_tokens).sum(),
        total_output_tokens: agents.iter().map(|a| a.total_output_tokens).sum(),
        total_cache_read_tokens: agents.iter().map(|a| a.total_cache_read_tokens).sum(),
        total_cache_write_tokens: agents.iter().map(|a| a.total_cache_write_tokens).sum(),
        agents,
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
                ),
            ),
        ),
        (
            "events",
            render::page("events", REFRESH_SECS, &render::events(&event_rows())),
        ),
        (
            "costs",
            render::page("costs", REFRESH_SECS, &render::costs(&cost_report())),
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
