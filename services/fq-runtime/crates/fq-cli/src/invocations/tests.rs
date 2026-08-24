use super::*;

#[test]
fn format_invocation_list_row_human_renders_short_id_and_truncated_fields() {
    let item = fq_ops::views::InvocationSummaryView {
        invocation_id: "019e3b328fd47de1aae0bb91bb24528d".to_string(),
        agent_id: Some("a".repeat(40)),
        worker_id: "worker-42".to_string(),
        status: "ambiguous".to_string(),
        assigned_at_ms: 1_700_000_000_000,
        started_at_ms: 1_700_000_000_000,
        archived: false,
        summary: None,
    };
    let line = format_invocation_list_row_human(&item);
    assert!(line.starts_with("019e3b32"), "expected 8-char id prefix");
    assert!(line.contains("ambiguous"));
    assert!(line.contains("worker-42"));
    assert!(line.contains("no"));
    // Agent string was truncated to 22 chars.
    assert!(line.contains(&"a".repeat(22)));
    assert!(!line.contains(&"a".repeat(23)));
}

/// #216: the summary line rides last, truncated char-safe; absent
/// renders an em-dash.
#[test]
fn format_invocation_list_row_human_renders_summary_last() {
    let mut item = fq_ops::views::InvocationSummaryView {
        invocation_id: "019e3b328fd47de1aae0bb91bb24528d".to_string(),
        agent_id: Some("m0-issue-fix".to_string()),
        worker_id: "w".to_string(),
        status: "in_flight".to_string(),
        assigned_at_ms: 0,
        started_at_ms: 0,
        archived: false,
        summary: Some("Fixing #7: editing widget.rs".to_string()),
    };
    let line = format_invocation_list_row_human(&item);
    assert!(
        line.ends_with("Fixing #7: editing widget.rs"),
        "got: {line}"
    );

    item.summary = Some("x".repeat(200));
    let line = format_invocation_list_row_human(&item);
    assert!(line.ends_with('…'), "truncated: {line}");
    assert!(line.chars().count() < 150, "bounded: {line}");

    item.summary = None;
    let line = format_invocation_list_row_human(&item);
    assert!(line.ends_with('—'), "fallback dash: {line}");
}

#[test]
fn format_invocation_list_row_human_marks_archived() {
    let item = fq_ops::views::InvocationSummaryView {
        invocation_id: "inv".to_string(),
        agent_id: Some("a".to_string()),
        worker_id: String::new(),
        status: "completed".to_string(),
        assigned_at_ms: 0,
        started_at_ms: 0,
        archived: true,
        summary: None,
    };
    let line = format_invocation_list_row_human(&item);
    // The archived flag sits before the (now trailing) summary
    // column (#216).
    assert!(
        line.contains(" yes "),
        "archived flag should be 'yes', got: {line:?}"
    );
}

/// The `--json` list shape is an operator contract: the swap from the
/// CLI-local struct to `views::InvocationSummaryView` (#105 layer 1)
/// must not move these fields.
#[test]
fn invocation_summary_view_serialises_to_stable_json_shape() {
    let item = fq_ops::views::InvocationSummaryView {
        invocation_id: "inv-1".to_string(),
        agent_id: Some("agent-1".to_string()),
        worker_id: "worker-1".to_string(),
        status: "in_flight".to_string(),
        assigned_at_ms: 42,
        started_at_ms: 41,
        archived: false,
        summary: None,
    };
    let v = serde_json::to_value(&item).unwrap();
    assert_eq!(v["invocation_id"], "inv-1");
    assert_eq!(v["agent_id"], "agent-1");
    assert_eq!(v["worker_id"], "worker-1");
    assert_eq!(v["status"], "in_flight");
    assert_eq!(v["assigned_at_ms"], 42);
    assert_eq!(v["started_at_ms"], 41);
    assert_eq!(v["archived"], false);
}
