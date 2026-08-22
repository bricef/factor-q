//! Unit tests for [`super`]. Extracted from the parent module so the
//! file that ships is the file you read (#390); `super::*` keeps the
//! same access it had inline.

use super::*;

/// The health page links working and stuck invocation ids (#130) —
/// asserted against the same fixture the screenshot gallery renders.
#[test]
fn health_links_working_and_stuck_ids() {
    let status = crate::fixtures::status_report();
    let doctor = crate::fixtures::doctor_report();
    let html = health(&status, &doctor);
    assert!(html.contains("2 in-flight (1 working"), "got: {html}");
    assert!(
        html.contains(r#"<a href="/invocations/019f5b3f-31fb-7ae0-b130-3d65ccf40375">"#),
        "working id not linked: {html}"
    );
    assert!(
        html.contains(r#"<a href="/invocations/019f534f-4b3c-7f42-a619-b5e43a64fd38">"#),
        "stuck id not linked: {html}"
    );
}

/// Retry pressure (#49) is visible on the streams table when a
/// consumer has outstanding redeliveries.
#[test]
fn health_shows_redelivery_pressure() {
    let html = health(
        &crate::fixtures::status_report(),
        &crate::fixtures::doctor_report(),
    );
    assert!(html.contains("redelivered 4"), "got: {html}");
}

#[test]
fn esc_neutralises_html() {
    assert_eq!(
        esc(r#"<script>alert("x&y")</script>"#),
        "&lt;script&gt;alert(&quot;x&amp;y&quot;)&lt;/script&gt;"
    );
}

#[test]
fn age_renders_units() {
    assert_eq!(age(0, 12_000), "12s ago");
    assert_eq!(age(0, 120_000), "2m ago");
    assert_eq!(age(0, 7_200_000), "2h ago");
    assert_eq!(age(0, 172_800_000), "2d ago");
}

/// The live shell: datastar loaded, the #main region polling its
/// own URL on the configured cadence, and the no-JS fallback in
/// place of the old hard refresh.
#[test]
fn live_page_polls_its_own_url_and_keeps_a_noscript_fallback() {
    let html = live_page("costs", 7, "<p>x</p>");
    assert!(
        html.contains(r#"<script type="module" src="/assets/datastar.js"></script>"#),
        "got: {html}"
    );
    assert!(
        html.contains(
            r#"<div id="main" data-on-interval__duration.7s="@get(window.location.pathname + window.location.search)"><p>x</p></div>"#
        ),
        "got: {html}"
    );
    assert!(
        html.contains(r#"<noscript><meta http-equiv="refresh" content="5"></noscript>"#),
        "no-JS fallback missing: {html}"
    );
    assert!(
        !html.contains(r#"<meta http-equiv="refresh" content="7">"#),
        "hard refresh must be gone: {html}"
    );
}

/// Folds carry a stable id and preserve their open state across
/// live-region morphs — the whole point of the change.
#[test]
fn folds_carry_stable_ids_and_preserve_open() {
    let html = fold("one-shot-agents", "one-shot agents", "<p>rows</p>");
    assert_eq!(
        html,
        r#"<details id="one-shot-agents" data-preserve-attr="open"><summary>one-shot agents</summary><p>rows</p></details>"#
    );
    // The three fold sites emit through the helper.
    let costs_html = costs_all(&cost_report(vec![
        cost_view("a", 1, 1.0),
        cost_view("overspender-019f339c15767d70b8ffd6d7ca6b0a70", 1, 1.0),
    ]));
    assert!(
        costs_html.contains(r#"<details id="one-shot-agents" data-preserve-attr="open">"#),
        "got: {costs_html}"
    );
}

#[test]
fn page_carries_refresh_and_escaped_title() {
    let html = page("a<b", 7, "<p>x</p>");
    assert!(html.contains(r#"content="7""#));
    assert!(html.contains("a&lt;b"));
    assert!(html.contains("<p>x</p>"));
}

#[test]
fn unreachable_shows_last_seen_or_never() {
    let never = unreachable("127.0.0.1:9471", "refused", None, 1_000);
    assert!(never.contains("never seen"));
    let seen = unreachable("127.0.0.1:9471", "refused", Some(0), 30_000);
    assert!(seen.contains("last seen 30s ago"));
}

/// Tool output is attacker-influenced content: markup in a payload
/// must render as text, never as HTML.
#[test]
fn transcript_escapes_hostile_payloads() {
    let entries = vec![fq_ops::transcript::TranscriptEntry::ToolResult {
        timestamp_ms: 0,
        tool_call_id: "tc-1".into(),
        tool_name: "exec".into(),
        parameters: serde_json::json!({"cmd": "<img src=x onerror=alert(1)>"}),
        output: Some("<script>alert('pwned')</script>".into()),
        is_error: Some(true),
    }];
    let html = transcript(&entries, 1_000, false, "inv-1", None);
    assert!(!html.contains("<script>"), "raw script leaked: {html}");
    assert!(html.contains("&lt;script&gt;"), "got: {html}");
    assert!(!html.contains("<img"), "raw img leaked: {html}");
    // Error results are visually flagged.
    assert!(html.contains(r#"class="turn err""#), "got: {html}");
    // Truncated view links to the full one.
    assert!(
        html.contains("/invocations/inv-1/transcript?full=1"),
        "got: {html}"
    );
}

/// The scroll-panel contract: DOM order is newest-first (the
/// column-reverse panel flips it back visually), so the page opens
/// at — and stays pinned to — the latest turn.
#[test]
fn transcript_dom_holds_entries_newest_first() {
    use fq_ops::transcript::TranscriptEntry;
    let entries = vec![
        TranscriptEntry::Prompt {
            timestamp_ms: 0,
            system: None,
            user: Some("FIRST".into()),
        },
        TranscriptEntry::Outcome {
            timestamp_ms: 9,
            phase: "completed".to_string(),
        },
    ];
    let html = transcript(&entries, 10_000, true, "inv-1", None);
    let first = html.find("FIRST").expect("prompt rendered");
    let outcome = html.find("run completed").expect("outcome rendered");
    assert!(
        outcome < first,
        "newest entry must come first in the DOM (column-reverse flips it back)"
    );
}

#[test]
fn transcript_renders_all_entry_kinds() {
    use fq_ops::transcript::{AssistantToolCall, TranscriptEntry};
    let entries = vec![
        TranscriptEntry::Prompt {
            timestamp_ms: 0,
            system: Some("sys".into()),
            user: Some("do the thing".into()),
        },
        TranscriptEntry::Assistant {
            timestamp_ms: 1_000,
            model: "claude-opus-4-8".into(),
            content: Some("on it".into()),
            tool_calls: vec![AssistantToolCall {
                tool_call_id: "tc-1".into(),
                tool_name: "exec".into(),
                parameters: serde_json::json!({"command": "ls"}),
            }],
            cost_usd: Some(0.01),
            is_error: Some(false),
        },
    ];
    let html = transcript(&entries, 60_000, true, "inv-1", None);
    assert!(html.contains("system prompt (3 bytes)"), "got: {html}");
    assert!(html.contains("do the thing"));
    assert!(html.contains("assistant · claude-opus-4-8"));
    assert!(html.contains("tool call <b>exec</b>"));
    // Full view links back to the truncated one.
    assert!(html.contains(r#"href="/invocations/inv-1/transcript""#));
}

/// #216: the one-line summary renders (escaped) in both tables,
/// with a muted em-dash when absent.
#[test]
fn summary_column_renders_escaped_with_fallback() {
    let mut items = vec![fq_ops::views::InvocationSummaryView {
        invocation_id: "inv-s".into(),
        agent_id: Some("m0-issue-fix".into()),
        worker_id: "w".into(),
        status: "in_flight".into(),
        assigned_at_ms: 0,
        started_at_ms: 0,
        archived: false,
        summary: Some("Fixing #7: <script>alert(1)</script>".into()),
    }];
    let html = invocations(&items, InvocationFilters::default(), 1_000);
    assert!(html.contains("<th>summary</th>"), "got: {html}");
    assert!(
        html.contains("Fixing #7: &lt;script&gt;alert(1)&lt;/script&gt;"),
        "summary escaped: {html}"
    );
    items[0].summary = None;
    let html = invocations(&items, InvocationFilters::default(), 1_000);
    assert!(html.contains("—"), "fallback dash: {html}");

    let active_rows = [fq_ops::views::ActiveInvocationView {
        invocation_id: "inv-a".into(),
        agent_id: "m0-issue-fix".into(),
        phase: "reducing".into(),
        step_index: 1,
        started_at_ms: 0,
        updated_at_ms: 0,
        liveness: Liveness::Advancing,
        open_tools: vec![],
        open_llms: vec![],
        summary: Some("Editing widget.rs".into()),
    }];
    let html = active(&active_rows, 1_000);
    assert!(html.contains("Editing widget.rs"), "got: {html}");
}

/// The one-line summary (#216) renders on the invocation detail
/// page as a table row, and on the transcript page as a header
/// line — both escaped, both absent when there is no summary.
#[test]
fn summary_renders_on_detail_and_transcript_pages() {
    let detail = fq_ops::views::InvocationDetailView {
        invocation_id: "inv-1".into(),
        agent_id: Some("m0-issue-fix".into()),
        owner: None,
        archive: None,
        live: None,
        recent_events: vec![],
        has_transcript: false,
        summary: Some("Fixing #83: <b>ci</b> running".into()),
        cost: Some(fq_ops::views::InvocationCostView {
            invocation_id: "inv-1".into(),
            started_at_ms: 0,
            event_count: 52,
            total_cost: 2.2137,
            total_input_tokens: 6_723_812,
            total_output_tokens: 10_095,
            total_cache_read_tokens: 6_554_327,
            total_cache_write_tokens: 0,
        }),
    };
    let html = invocation_detail(&detail, 1_000);
    assert_eq!(
        html.matches("<th>summary</th>").count(),
        1,
        "exactly one summary row: {html}"
    );
    assert!(
        html.contains("Fixing #83: &lt;b&gt;ci&lt;/b&gt; running"),
        "summary must be escaped: {html}"
    );
    assert!(!html.contains("<b>ci</b>"), "raw markup leaked: {html}");

    // Cost so far renders with compact counts; absent when no
    // priced call has landed yet.
    assert!(
        html.contains(r#"<th>cost so far</th><td>$2.2137 <span class="muted">· 52 llm calls · 6.72M in / 10.1K out · 6.55M cache read</span></td>"#),
        "got: {html}"
    );
    let mut no_cost = detail.clone();
    no_cost.cost = None;
    assert!(!invocation_detail(&no_cost, 1_000).contains("cost so far"));

    let mut no_summary = detail.clone();
    no_summary.summary = None;
    assert!(!invocation_detail(&no_summary, 1_000).contains("<th>summary</th>"));

    // Failures remain useful without a transcript: the reason is inline and
    // the dead transcript link is omitted. Both provider text fields are escaped.
    let mut failed = detail.clone();
    failed.recent_events = vec![fq_ops::views::EventView {
        event_id: "event-1".into(),
        timestamp: "2026-07-18T00:00:00Z".into(),
        agent_id: "m0-issue-fix".into(),
        invocation_id: "inv-1".into(),
        event_type: "failed".into(),
        model: None,
        total_cost: None,
        error_kind: Some("llm_error".into()),
        error_message: Some("provider <429>".into()),
        duration_ms: Some(10),
    }];
    let html = invocation_detail(&failed, 1_000);
    assert!(
        html.contains("llm_error: provider &lt;429&gt;"),
        "got: {html}"
    );
    assert!(!html.contains("transcript →"), "got: {html}");
    failed.has_transcript = true;
    assert!(invocation_detail(&failed, 1_000).contains("transcript →"));

    let html = transcript(&[], 1_000, false, "inv-1", Some("Fixing #83: ci running"));
    assert!(
        html.contains(r#"<span class="muted">summary —</span> Fixing #83: ci running"#),
        "got: {html}"
    );
    let html = transcript(&[], 1_000, false, "inv-1", None);
    assert!(!html.contains("summary —"), "got: {html}");
}

#[test]
fn active_table_omitted_when_nothing_in_flight() {
    let items = [fq_ops::views::InvocationSummaryView {
        invocation_id: "abc".into(),
        agent_id: None,
        worker_id: "w".into(),
        status: "completed".into(),
        assigned_at_ms: 0,
        started_at_ms: 0,
        archived: false,
        summary: None,
    }];
    assert_eq!(active(&[], 1_000), "");
    // With no active rows the page is byte-identical to the plain list.
    assert_eq!(
        invocations_page(&[], &items, InvocationFilters::default(), 1_000),
        invocations(&items, InvocationFilters::default(), 1_000)
    );
}

#[test]
fn active_table_shows_live_work_above_the_list() {
    let active_rows = [fq_ops::views::ActiveInvocationView {
        invocation_id: "0123456789abcdef".into(),
        agent_id: "m0-issue-fix".into(),
        phase: "dispatching_tools".into(),
        step_index: 165,
        started_at_ms: 0,
        updated_at_ms: 540_000,
        liveness: Liveness::Working,
        open_tools: vec![fq_ops::views::OpenToolView {
            tool_name: "exec".into(),
            command: Some("gh issue view 86 --repo bricef/factor-q".into()),
        }],
        open_llms: vec![],
        summary: None,
    }];
    let html = invocations_page(&active_rows, &[], InvocationFilters::default(), 600_000);
    assert!(html.contains("Active now"), "got: {html}");
    assert!(html.contains(r#"<a href="/invocations/0123456789abcdef">01234567</a>"#));
    assert!(
        html.contains(
            r#"tool exec <span class="muted">— gh issue view 86 --repo bricef/factor-q</span>"#
        ),
        "got: {html}"
    );
    assert!(html.contains("<td>10m ago</td>"), "started age: {html}");
    assert!(html.contains("<td>1m ago</td>"), "advanced age: {html}");
    // The list below gains its heading only when active is present.
    assert!(html.contains("All invocations"), "got: {html}");
}

fn cost_view(agent: &str, calls: i64, cost: f64) -> CostView {
    CostView {
        agent_id: agent.to_string(),
        event_count: calls,
        total_cost: cost,
        total_input_tokens: 0,
        total_output_tokens: 0,
        total_cache_read_tokens: 0,
        total_cache_write_tokens: 0,
        invocation_count: 1,
        framework_cost: 0.0,
    }
}

fn cost_report(agents: Vec<CostView>) -> CostReport {
    CostReport {
        total_cost: agents.iter().map(|a| a.total_cost).sum(),
        total_input_tokens: agents.iter().map(|a| a.total_input_tokens).sum(),
        total_output_tokens: agents.iter().map(|a| a.total_output_tokens).sum(),
        total_cache_read_tokens: agents.iter().map(|a| a.total_cache_read_tokens).sum(),
        total_cache_write_tokens: agents.iter().map(|a| a.total_cache_write_tokens).sum(),
        framework_cost: agents.iter().map(|a| a.framework_cost).sum(),
        agents,
        buckets: vec![],
        models: vec![],
    }
}

/// The top-level costs page renders the report's per-model split
/// with shares against the grand total; no models, no section.
#[test]
fn costs_render_the_by_model_split() {
    let mut report = cost_report(vec![
        cost_view("m0-issue-fix", 100, 75.0),
        cost_view("m0-loop", 10, 25.0),
    ]);
    report.models = vec![
        ModelCostView {
            model: "claude-opus-4-8".to_string(),
            event_count: 80,
            total_cost: 90.0,
            total_input_tokens: 100_000_000,
            total_output_tokens: 500_000,
        },
        ModelCostView {
            model: "z-ai/glm-5.2".to_string(),
            event_count: 30,
            total_cost: 10.0,
            total_input_tokens: 8_000_000,
            total_output_tokens: 60_000,
        },
    ];
    let html = costs(&report, &CostReport::default(), Window::All, TEST_NOW_MS);
    assert!(html.contains("<h2>By agent</h2>"), "got: {html}");
    assert!(html.contains("<h2>By model</h2>"), "got: {html}");
    assert!(html.contains("claude-opus-4-8"), "got: {html}");
    assert!(html.contains("z-ai/glm-5.2"), "got: {html}");
    assert!(html.contains("90.0%"), "got: {html}");
    assert!(html.contains("10.0%"), "got: {html}");

    // Without model rows the section is absent entirely.
    let bare = costs_all(&cost_report(vec![cost_view("a", 1, 1.0)]));
    assert!(!bare.contains("By model"), "got: {bare}");
}

/// An id is a one-shot instance only when its last segment is
/// exactly 32 lowercase hex chars — named agents, short suffixes,
/// and uppercase all stay named.
#[test]
fn one_shot_family_matches_uuid_suffixed_ids_only() {
    assert_eq!(
        one_shot_family("overspender-019f339c15767d70b8ffd6d7ca6b0a70"),
        Some("overspender")
    );
    assert_eq!(
        one_shot_family("step4-tool-wal-019f339c178c74409c1552ce7ddf6ff8"),
        Some("step4-tool-wal")
    );
    assert_eq!(one_shot_family("m0-issue-fix"), None);
    assert_eq!(one_shot_family("deadbeef"), None);
    // 31 hex chars — not a uuid suffix.
    assert_eq!(
        one_shot_family("agent-019f339c15767d70b8ffd6d7ca6b0a7"),
        None
    );
    // Uppercase hex is not the uuid7 wire form.
    assert_eq!(
        one_shot_family("agent-019F339C15767D70B8FFD6D7CA6B0A70"),
        None
    );
    // A bare 32-hex id with no family prefix stays named.
    assert_eq!(one_shot_family("019f339c15767d70b8ffd6d7ca6b0a70"), None);
}

/// One-shot instances collapse into per-family rows under the fold;
/// named agents keep their own rows, and the totals line splits
/// named vs one-shot spend.
/// `costs()` with an unbounded window and an empty day report — the
/// shape most render assertions want.
fn costs_all(report: &CostReport) -> String {
    costs(report, &CostReport::default(), Window::All, TEST_NOW_MS)
}

/// 2026-07-16T12:00:00Z — a fixed clock for chart-slot tests.
const TEST_NOW_MS: i64 = 1_784_203_200_000;

#[test]
fn costs_collapse_one_shot_agents_into_families() {
    let html = costs_all(&cost_report(vec![
        cost_view("m0-issue-fix", 2474, 121.397646),
        cost_view("overspender-019f339c15767d70b8ffd6d7ca6b0a70", 1, 1.0),
        cost_view("overspender-019f339b43c47822bdff48bec821d815", 1, 1.0),
        cost_view("e2e-agent-019f339c10bd7200a1a72a3f07606067", 1, 0.0),
    ]));
    // Named row present; raw one-shot ids never rendered.
    assert!(html.contains("m0-issue-fix"), "got: {html}");
    assert!(!html.contains("019f339c15767d70"), "got: {html}");
    // Family rows fold the instances.
    assert!(html.contains("<td>overspender-*</td>"), "got: {html}");
    assert!(html.contains("<td>e2e-agent-*</td>"), "got: {html}");
    assert!(
        html.contains("one-shot agents — 3 ids · $2.0000"),
        "got: {html}"
    );
    // The totals line splits honest spend from synthetic e2e spend.
    assert!(html.contains("total $123.3976"), "got: {html}");
    assert!(html.contains("named $121.3976"), "got: {html}");
    assert!(html.contains("one-shot $2.0000"), "got: {html}");
}

/// Cache token sums are on the wire (`CostView`) and must reach the
/// page; token cells compact with the exact value in the hover.
#[test]
fn costs_render_cache_columns_and_share() {
    let mut a = cost_view("m0-issue-fix", 2474, 75.0);
    a.total_input_tokens = 171_392_966;
    a.total_cache_read_tokens = 26_118_676;
    let b = cost_view("m0-loop", 162, 25.0);
    let html = costs_all(&cost_report(vec![a, b]));
    assert!(
        html.contains("<th class=\"n\">cache read</th>"),
        "got: {html}"
    );
    assert!(
        html.contains(r#"<td class="n" title="26,118,676">26.12M</td>"#),
        "got: {html}"
    );
    assert!(
        html.contains(r#"<td class="n" title="171,392,966">171.39M</td>"#),
        "got: {html}"
    );
    // Share column: text carries the value, bar carries the glance.
    assert!(html.contains("75.0%"), "got: {html}");
    assert!(html.contains("25.0%"), "got: {html}");
    assert!(html.contains(r#"style="width:75%""#), "got: {html}");
    // No one-shot agents → no fold, no split in the total line.
    assert!(!html.contains("one-shot"), "got: {html}");
    assert!(html.contains("total $100.0000"), "got: {html}");
}

/// Agent ids are attacker-adjacent strings and stay escaped.
#[test]
fn costs_escape_agent_ids() {
    let html = costs_all(&cost_report(vec![cost_view("<agent>", 1, 0.5)]));
    assert!(html.contains("&lt;agent&gt;"), "got: {html}");
    assert!(!html.contains("<agent>"), "got: {html}");
}

/// The window selector: the current window is bold text, the other
/// two are links back to the page.
#[test]
fn costs_window_selector_marks_current_and_links_others() {
    let html = costs(
        &cost_report(vec![cost_view("a", 1, 1.0)]),
        &CostReport::default(),
        Window::Days7,
        TEST_NOW_MS,
    );
    assert!(html.contains("<b>7d</b>"), "got: {html}");
    assert!(html.contains(r#"<a href="/costs">all</a>"#), "got: {html}");
    assert!(
        html.contains(r#"<a href="/costs?window=24h">24h</a>"#),
        "got: {html}"
    );
    // An empty windowed report still renders the selector — the way
    // back out of a quiet window.
    let empty = costs(
        &CostReport::default(),
        &CostReport::default(),
        Window::Day,
        TEST_NOW_MS,
    );
    assert!(empty.contains("<b>24h</b>"), "got: {empty}");
    assert!(empty.contains("no cost events"), "got: {empty}");
}

/// The last-24h column reads from the day-bounded report; agents
/// with no day spend show a muted dash.
#[test]
fn costs_day_column_reads_from_the_day_report() {
    let report = cost_report(vec![
        cost_view("m0-issue-fix", 10, 121.0),
        cost_view("m0-loop", 5, 6.0),
    ]);
    let day = cost_report(vec![cost_view("m0-issue-fix", 2, 13.16)]);
    let html = costs(&report, &day, Window::All, TEST_NOW_MS);
    assert!(
        html.contains("<th class=\"n\">last 24h</th>"),
        "got: {html}"
    );
    assert!(html.contains("$13.16"), "got: {html}");
    assert!(
        html.contains(r#"<td class="n muted">—</td>"#),
        "got: {html}"
    );
}

/// Named agents link to their drill-down and carry the invocation
/// count; the folded family rows do not link (a family is not an
/// agent id).
#[test]
fn costs_link_named_agents_to_their_drilldown() {
    let mut a = cost_view("m0-issue-fix", 2474, 121.0);
    a.invocation_count = 43;
    let html = costs_all(&cost_report(vec![
        a,
        cost_view("overspender-019f339c15767d70b8ffd6d7ca6b0a70", 1, 1.0),
    ]));
    assert!(
        html.contains(r#"<a href="/costs/m0-issue-fix">m0-issue-fix</a>"#),
        "got: {html}"
    );
    assert!(
        html.contains("<th class=\"n\">invocations</th>"),
        "got: {html}"
    );
    assert!(html.contains(r#"<td class="n">43</td>"#), "got: {html}");
    assert!(!html.contains(r#"href="/costs/overspender"#), "got: {html}");
}

/// The drill-down page: totals strip, per-model split with share,
/// and per-invocation rows linking to the invocation detail page.
#[test]
fn agent_costs_render_models_and_linked_invocations() {
    use fq_ops::views::{InvocationCostView, ModelCostView};
    let mut totals = cost_view("m0-issue-fix", 1187, 101.38);
    totals.invocation_count = 43;
    let d = AgentCostDetailView {
        agent_id: "m0-issue-fix".to_string(),
        totals,
        models: vec![ModelCostView {
            model: "claude-opus-4-8".to_string(),
            event_count: 1187,
            total_cost: 101.38,
            total_input_tokens: 126_872_419,
            total_output_tokens: 702_313,
        }],
        invocations: vec![InvocationCostView {
            invocation_id: "019f6176-78c3-7cb3-9f0a-73c98b760b70".to_string(),
            started_at_ms: 0,
            event_count: 52,
            total_cost: 2.2137,
            total_input_tokens: 6_723_812,
            total_output_tokens: 10_095,
            total_cache_read_tokens: 6_554_327,
            total_cache_write_tokens: 0,
        }],
    };
    let html = agent_costs(&d, Window::All, 1_860_000);
    assert!(
        html.contains(r#"<a href="/costs">← all agents</a>"#),
        "got: {html}"
    );
    // Window links target this agent's own path.
    assert!(
        html.contains(r#"<a href="/costs/m0-issue-fix?window=7d">7d</a>"#),
        "got: {html}"
    );
    assert!(html.contains("By model"), "got: {html}");
    assert!(html.contains("claude-opus-4-8"), "got: {html}");
    assert!(html.contains("By invocation"), "got: {html}");
    assert!(
        html.contains(
            r#"<a href="/invocations/019f6176-78c3-7cb3-9f0a-73c98b760b70">019f6176</a>"#
        ),
        "got: {html}"
    );
    assert!(html.contains("<td>31m ago</td>"), "got: {html}");
    assert!(html.contains("$2.2137"), "got: {html}");
    assert!(html.contains("showing 1 of 43 invocations"), "got: {html}");
    // Nothing of this agent's spend is framework spend, so the page
    // says nothing about it.
    assert!(!html.contains("framework"), "got: {html}");
}

/// A summariser's spend is the engine's, charged to no invocation
/// (#466), so per-invocation figures fall short of the total by
/// construction. The fleet page states the identity under the total
/// instead of leaving the reader to subtract and file the gap as a bug.
#[test]
fn costs_state_the_framework_remainder_under_the_total() {
    let mut summary = cost_view("summary", 61, 0.913_4);
    summary.framework_cost = 0.913_4;
    summary.invocation_count = 0;
    let report = cost_report(vec![cost_view("m0-issue-fix", 100, 9.0), summary]);
    let html = costs_all(&report);
    assert!(
        html.contains("total = invocations <b>$9.0000</b> + framework <b>$0.9134</b>"),
        "got: {html}"
    );
    assert!(
        html.contains("framework is engine spend (invocation summaries), charged to no invocation"),
        "got: {html}"
    );
}

/// The zero case stays silent. `framework_cost` is zero for every agent
/// but the reserved one, and a caveat that is true of one page must not
/// ride on all of them.
#[test]
fn costs_say_nothing_about_framework_when_there_is_none() {
    let html = costs_all(&cost_report(vec![cost_view("m0-issue-fix", 100, 9.0)]));
    assert!(!html.contains("framework"), "got: {html}");
    assert!(!html.contains("invocations <b>"), "got: {html}");
}

/// `/costs/summary` is the page the allocation rule made strange: real
/// spend with no invocation rows under it at all. Correct, and
/// indistinguishable from data that failed to load unless the page
/// says why — so it does, instead of rendering an empty table.
#[test]
fn summary_agent_costs_explain_the_empty_invocation_table() {
    let mut totals = cost_view("summary", 61, 0.913_4);
    totals.framework_cost = 0.913_4;
    totals.invocation_count = 0;
    let d = AgentCostDetailView {
        agent_id: "summary".to_string(),
        totals,
        models: vec![],
        invocations: vec![],
    };
    let html = agent_costs(&d, Window::All, TEST_NOW_MS);
    // All of it is framework, and the identity says so without a
    // subtraction.
    assert!(
        html.contains("total = invocations <b>$0.0000</b> + framework <b>$0.9134</b>"),
        "got: {html}"
    );
    assert!(
        html.contains("No invocation rows, and none are missing"),
        "got: {html}"
    );
    assert!(
        html.contains(r#"counted in the fleet total on the <a href="/costs">costs page</a>"#),
        "got: {html}"
    );
    // The header-only table and its "showing 0 of 0" footer are what
    // read as missing data; neither is rendered.
    assert!(!html.contains("<th>invocation</th>"), "got: {html}");
    assert!(!html.contains("showing 0 of 0"), "got: {html}");
}

/// The agents list links each definition and surfaces registry
/// load errors loudly.
#[test]
fn agents_list_links_definitions_and_surfaces_load_errors() {
    use fq_ops::agent_view::{AgentSummaryView, AgentsView};
    let view = AgentsView {
        agents: vec![
            AgentSummaryView {
                agent_id: "m0-issue-fix".to_string(),
                model: "claude-opus-4-8".to_string(),
                budget: Some(12.0),
                trigger: Some("m0-issue-fix".to_string()),
                tool_count: 3,
                prompt_bytes: 4_212,
                path: "/agents/m0-issue-fix.md".to_string(),
            },
            AgentSummaryView {
                agent_id: "doc-drift".to_string(),
                model: "claude-sonnet-4-5".to_string(),
                budget: None,
                trigger: None,
                tool_count: 1,
                prompt_bytes: 900,
                path: "/agents/doc-drift.md".to_string(),
            },
        ],
        errors: vec!["failed to parse /agents/broken.md: missing model".to_string()],
    };
    let html = agents(&view);
    assert!(
        html.contains(r#"<a href="/agents/m0-issue-fix">m0-issue-fix</a>"#),
        "got: {html}"
    );
    assert!(html.contains("$12.00"), "got: {html}");
    assert!(html.contains("4,212 B"), "got: {html}");
    // Missing budget/trigger render as muted dashes, not blanks.
    assert!(
        html.contains(r#"<span class="muted">—</span>"#),
        "got: {html}"
    );
    assert!(
        html.contains("1 definition(s) failed to load"),
        "got: {html}"
    );
    assert!(html.contains("broken.md"), "got: {html}");
    // Empty registry has its own message.
    assert!(agents(&AgentsView::default()).contains("no agents loaded"));
}

/// The agent definition page: fields, cross-links, and the system
/// prompt inside a collapsed <details> — escaped, since a prompt is
/// arbitrary text.
#[test]
fn agent_detail_collapses_and_escapes_the_prompt() {
    use fq_ops::agent_view::AgentDetailView;
    let d = AgentDetailView {
        agent_id: "m0-issue-fix".to_string(),
        model: "claude-opus-4-8".to_string(),
        system_prompt: "Fix issues end-to-end. Never claim <b>unpersisted</b> work.".to_string(),
        tools: vec!["exec".to_string(), "file_read".to_string()],
        mcp_servers: vec!["github".to_string()],
        budget: Some(12.0),
        max_iterations: Some(200),
        effort: Some("high".to_string()),
        trigger: Some("m0-issue-fix".to_string()),
        path: "/home/fq/agents/m0-issue-fix.md".to_string(),
    };
    let html = agent_detail(&d);
    assert!(
        html.contains(r#"<details id="system-prompt" data-preserve-attr="open"><summary>system prompt (59 bytes)</summary>"#),
        "got: {html}"
    );
    assert!(
        !html.contains("<b>unpersisted</b>"),
        "prompt leaked markup: {html}"
    );
    assert!(
        html.contains("&lt;b&gt;unpersisted&lt;/b&gt;"),
        "got: {html}"
    );
    assert!(
        html.contains(r#"<a href="/costs/m0-issue-fix">costs</a>"#),
        "got: {html}"
    );
    assert!(
        html.contains(r#"<a href="/events?agent=m0-issue-fix">events</a>"#),
        "got: {html}"
    );
    assert!(html.contains("fq.trigger.m0-issue-fix"), "got: {html}");
    assert!(html.contains("exec, file_read"), "got: {html}");
    assert!(html.contains("m0-issue-fix.md"), "got: {html}");
}

/// Agent names across the invocation surfaces link to the agent
/// page; an unknown agent renders a muted placeholder, not a link.
#[test]
fn invocation_surfaces_link_agent_names() {
    let items = vec![
        fq_ops::views::InvocationSummaryView {
            invocation_id: "inv-1".into(),
            agent_id: Some("m0-loop".into()),
            worker_id: "w".into(),
            status: "completed".into(),
            assigned_at_ms: 0,
            started_at_ms: 0,
            archived: false,
            summary: None,
        },
        fq_ops::views::InvocationSummaryView {
            invocation_id: "inv-2".into(),
            agent_id: None,
            worker_id: "w".into(),
            status: "failed".into(),
            assigned_at_ms: 0,
            started_at_ms: 0,
            archived: false,
            summary: None,
        },
    ];
    let html = invocations(&items, InvocationFilters::default(), 1_000);
    assert!(
        html.contains(r#"<a href="/agents/m0-loop">m0-loop</a>"#),
        "got: {html}"
    );
    assert!(
        html.contains(r#"<span class="muted">?</span>"#),
        "got: {html}"
    );

    let active_rows = [fq_ops::views::ActiveInvocationView {
        invocation_id: "inv-3".into(),
        agent_id: "m0-issue-fix".into(),
        phase: "reducing".into(),
        step_index: 1,
        started_at_ms: 0,
        updated_at_ms: 0,
        liveness: Liveness::Advancing,
        open_tools: vec![],
        open_llms: vec![],
        summary: None,
    }];
    let html = active(&active_rows, 1_000);
    assert!(
        html.contains(r#"<a href="/agents/m0-issue-fix">m0-issue-fix</a>"#),
        "got: {html}"
    );
}

/// Terminal-status filters: hide/show completed and failed rows,
/// with toggle links that flip one flag while preserving the rest
/// (the live region polls the same query string, so state
/// survives ticks).
#[test]
fn invocation_filters_hide_terminal_rows_and_compose_links() {
    let mk = |id: &str, status: &str| fq_ops::views::InvocationSummaryView {
        invocation_id: id.into(),
        agent_id: Some("a".into()),
        worker_id: "w".into(),
        status: status.into(),
        assigned_at_ms: 0,
        started_at_ms: 0,
        archived: false,
        summary: None,
    };
    let items = vec![
        mk("inv-live", "in_flight"),
        mk("inv-done", "completed"),
        mk("inv-boom", "failed"),
    ];

    // Default: everything visible, both toggles say "hide".
    let html = invocations(&items, InvocationFilters::default(), 1_000);
    for id in ["inv-live", "inv-done", "inv-boom"] {
        assert!(html.contains(id), "default shows {id}: {html}");
    }
    assert!(
        html.contains(r#"<a href="/invocations?completed=0">hide completed</a>"#),
        "got: {html}"
    );
    assert!(
        html.contains(r#"<a href="/invocations?failed=0">hide failed</a>"#),
        "got: {html}"
    );

    // Hiding completed drops only those rows; its link flips to
    // "show" and the OTHER toggles carry the completed=0 state.
    let filters = InvocationFilters {
        show_completed: false,
        ..Default::default()
    };
    let html = invocations(&items, filters, 1_000);
    assert!(!html.contains("inv-done"), "got: {html}");
    assert!(
        html.contains("inv-live") && html.contains("inv-boom"),
        "got: {html}"
    );
    assert!(
        html.contains(r#"<a href="/invocations">show completed</a>"#),
        "got: {html}"
    );
    assert!(
        html.contains(r#"<a href="/invocations?completed=0&failed=0">hide failed</a>"#),
        "got: {html}"
    );
    assert!(
        html.contains(r#"<a href="/invocations?archived=1&completed=0">show archived</a>"#),
        "got: {html}"
    );

    // Everything hidden → the honest empty message.
    let filters = InvocationFilters {
        show_completed: false,
        show_failed: false,
        ..Default::default()
    };
    let html = invocations(&items[1..], filters, 1_000);
    assert!(
        html.contains("no invocations match the filters"),
        "got: {html}"
    );
}

/// The spend chart: continuous slots from the clock, sparse
/// buckets joined by key, quiet slots as zero-height gaps, the
/// max bar labeled, hourly granularity on the day window.
#[test]
fn cost_chart_fills_slots_and_labels_the_max() {
    let bucket = |key: &str, cost: f64| CostBucketView {
        bucket: key.to_string(),
        total_cost: cost,
    };
    // TEST_NOW_MS is 2026-07-16T12:00Z: 30 daily slots end there.
    let buckets = vec![
        bucket("2026-07-14", 4.0),
        bucket("2026-07-16", 8.0),
        // Outside the 30-day pane — must be ignored, not drawn.
        bucket("2020-01-01", 99.0),
    ];
    let html = cost_chart(&buckets, Window::All, TEST_NOW_MS);
    assert_eq!(html.matches("cslot").count(), 30, "got: {html}");
    // Max bar: full height and labeled; half-cost bar: half height.
    assert!(
        html.contains(r#"title="2026-07-16 · $8.0000"><b>$8.00</b><i style="height:72px""#),
        "got: {html}"
    );
    assert!(
        html.contains(r#"title="2026-07-14 · $4.0000"><i style="height:36px""#),
        "got: {html}"
    );
    // A quiet day renders a gap, not a bar.
    assert!(
        html.contains(r#"title="2026-07-15 · $0.0000"><i style="height:0px""#),
        "got: {html}"
    );
    assert!(!html.contains("2020-01-01"), "stale bucket drawn: {html}");

    // Day window: 24 hourly slots, keys carry the hour.
    let html = cost_chart(&[bucket("2026-07-16T09", 1.5)], Window::Day, TEST_NOW_MS);
    assert_eq!(html.matches("cslot").count(), 24, "got: {html}");
    assert!(
        html.contains(r#"title="2026-07-16T09 · $1.5000""#),
        "got: {html}"
    );

    // 7d window: 8 daily slots.
    let html = cost_chart(&[bucket("2026-07-12", 2.0)], Window::Days7, TEST_NOW_MS);
    assert_eq!(html.matches("cslot").count(), 8, "got: {html}");

    // No spend in the pane → no chart at all.
    assert_eq!(cost_chart(&[], Window::All, TEST_NOW_MS), "");
    assert_eq!(
        cost_chart(&[bucket("2020-01-01", 9.0)], Window::All, TEST_NOW_MS),
        ""
    );
}

/// The health page's palette lands on the row surfaces: liveness
/// badges on active rows, terminal-status colours on the list and
/// detail — same vocabulary everywhere.
#[test]
fn liveness_and_status_carry_the_health_palette() {
    let mk = |liveness| fq_ops::views::ActiveInvocationView {
        invocation_id: "inv-a".into(),
        agent_id: "a".into(),
        phase: "reducing".into(),
        step_index: 1,
        started_at_ms: 0,
        updated_at_ms: 0,
        liveness,
        open_tools: vec![],
        open_llms: vec![],
        summary: None,
    };
    let html = active(&[mk(Liveness::Working)], 1_000);
    assert!(
        html.contains(r#"<span class="ok">✓ working</span>"#),
        "got: {html}"
    );
    let html = active(&[mk(Liveness::Stuck)], 1_000);
    assert!(
        html.contains(r#"<span class="bad">✗ stuck</span>"#),
        "got: {html}"
    );
    let html = active(&[mk(Liveness::Advancing)], 1_000);
    assert!(
        html.contains(r#"<span class="muted">advancing</span>"#),
        "got: {html}"
    );

    // Terminal statuses colour the list; in_flight stays plain.
    assert_eq!(
        status_span("completed"),
        r#"<span class="ok">completed</span>"#
    );
    assert_eq!(status_span("failed"), r#"<span class="bad">failed</span>"#);
    assert_eq!(
        status_span("ambiguous"),
        r#"<span class="warn">ambiguous</span>"#
    );
    assert_eq!(status_span("in_flight"), "in_flight");

    // The detail page's live block carries the badge on the phase.
    let detail = fq_ops::views::InvocationDetailView {
        invocation_id: "inv-1".into(),
        agent_id: None,
        owner: None,
        archive: None,
        live: Some(fq_ops::views::LiveExecutionView {
            liveness: Liveness::Stuck,
            phase: "reducing".into(),
            step_index: 3,
            started_at_ms: 0,
            updated_at_ms: 0,
            terminal_at_ms: None,
            tools: vec![],
            llms: vec![],
        }),
        recent_events: vec![],
        summary: None,
        cost: None,
        has_transcript: false,
    };
    let html = invocation_detail(&detail, 1_000);
    assert!(
        html.contains(r#"<td>reducing · <span class="bad">✗ stuck</span></td>"#),
        "got: {html}"
    );
}

#[test]
fn window_parses_query_and_bounds() {
    assert_eq!(Window::from_query(None), Window::All);
    assert_eq!(Window::from_query(Some("7d")), Window::Days7);
    assert_eq!(Window::from_query(Some("24h")), Window::Day);
    assert_eq!(Window::from_query(Some("bogus")), Window::All);
    assert_eq!(Window::All.since_ms(), None);
    assert_eq!(Window::Day.since_ms(), Some(86_400_000));
    assert_eq!(Window::Days7.since_ms(), Some(604_800_000));
}

#[test]
fn token_cells_compact_with_exact_hover() {
    assert_eq!(fmt_grouped(1_597), "1,597");
    assert_eq!(fmt_grouped(171_392_966), "171,392,966");
    assert_eq!(fmt_grouped(420), "420");
    assert_eq!(token_cell(420), r#"<td class="n">420</td>"#);
    assert_eq!(
        token_cell(58_912),
        r#"<td class="n" title="58,912">58.9K</td>"#
    );
    assert_eq!(
        token_cell(7_409_042),
        r#"<td class="n" title="7,409,042">7.41M</td>"#
    );
}

#[test]
fn invocation_rows_escape_link_and_show_start() {
    let items = vec![fq_ops::views::InvocationSummaryView {
        invocation_id: "0123456789abcdef".into(),
        agent_id: Some("<agent>".into()),
        worker_id: "w".into(),
        status: "in_flight".into(),
        assigned_at_ms: 600_000,
        started_at_ms: 600_000,
        archived: false,
        summary: None,
    }];
    let html = invocations(&items, InvocationFilters::default(), 1_200_000);
    assert!(html.contains(r#"<a href="/invocations/0123456789abcdef">01234567</a>"#));
    assert!(html.contains("&lt;agent&gt;"));
    assert!(!html.contains("<agent>"));
    assert!(html.contains("<th>started</th>"));
    assert!(html.contains("<td>10m ago</td>"), "got: {html}");
}
