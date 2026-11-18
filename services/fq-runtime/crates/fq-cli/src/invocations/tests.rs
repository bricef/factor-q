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
        archive_state: fq_ops::views::InvocationArchiveState::NotDue,
        summary: None,
    };
    let line = format_invocation_list_row_human(&item);
    assert!(line.starts_with("019e3b32"), "expected 8-char id prefix");
    assert!(line.contains("ambiguous"));
    assert!(line.contains("worker-42"));
    assert!(line.contains(" - "), "no archive due: {line:?}");
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
        archive_state: fq_ops::views::InvocationArchiveState::NotDue,
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
        archive_state: fq_ops::views::InvocationArchiveState::Archived,
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
        archive_state: fq_ops::views::InvocationArchiveState::NotDue,
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

/// The `trigger:` line (#954): the delivery that started the run, where
/// its claim stands, and who took it when. A run no claim names says so
/// rather than leaving the line out, and says only what an absent claim
/// shows: a run refused before its first durable write has events under
/// the id and no claim, though the trigger stream started it.
#[test]
fn format_trigger_delivery_names_the_delivery_or_its_absence() {
    let mut delivery = fq_ops::views::TriggerDeliveryView {
        stream: "fq-triggers".to_string(),
        stream_epoch: "1767323045000000000".to_string(),
        stream_seq: 42,
        state: fq_ops::views::TriggerClaimState::DurablyStarted,
        claimant: "worker-alpha".to_string(),
        claimed_at_ms: 1_767_323_045_000,
    };
    assert_eq!(
        format_trigger_delivery(Some(&delivery)),
        "fq-triggers seq 42 (durably started; claimed 2026-01-02T03:04:05Z by worker-alpha)"
    );
    delivery.state = fq_ops::views::TriggerClaimState::Claimed;
    assert_eq!(
        format_trigger_delivery(Some(&delivery)),
        "fq-triggers seq 42 (claimed, start not recorded; claimed 2026-01-02T03:04:05Z by \
         worker-alpha)"
    );
    assert_eq!(
        format_trigger_delivery(None),
        "none recorded: not started from the trigger stream, its claim was released or taken \
         over before the run's first durable write, or the claim is older than its retention \
         window"
    );
}

/// What an accepted `fq invocation resume` prints (#994, issue 1009): the
/// worker cap the run waits under, then the agent's cap it counts against
/// but is not held at, then where to follow it. Each its own line, in
/// that order.
#[test]
fn format_resume_accepted_names_the_worker_cap_and_the_agent_cap_it_is_not_held_at() {
    let id = "019a2b3c-0000-7000-8000-000000000001";
    assert_eq!(
        format_resume_accepted(id),
        "Resume accepted for invocation 019a2b3c-0000-7000-8000-000000000001.\n\
         It runs under [worker] max_concurrent_invocations like any other invocation: at once \
         if a worker is free, otherwise when one frees.\n\
         If its agent sets max_concurrent, this run counts against that limit but is not held \
         back by it, so the agent can be over its limit until one of its invocations ends.\n\
         Follow with `fq invocation show 019a2b3c-0000-7000-8000-000000000001` or \
         `fq invocation transcript 019a2b3c-0000-7000-8000-000000000001`.\n"
    );
}

/// The list's archive column, one label per state.
#[test]
fn the_archive_column_names_each_state() {
    use fq_ops::views::InvocationArchiveState as S;
    assert_eq!(
        [S::NotDue, S::Pending, S::Held, S::Archived].map(archive_label),
        ["-", "pending", "held", "yes"]
    );
}

fn detail_with(
    archive_state: fq_ops::views::InvocationArchiveState,
    archive_hold: Option<fq_ops::health::ArchiveHold>,
) -> fq_ops::views::InvocationDetailView {
    fq_ops::views::InvocationDetailView {
        invocation_id: "inv".to_string(),
        agent_id: None,
        owner: Some(fq_ops::views::InvocationSummaryView {
            invocation_id: "inv".to_string(),
            agent_id: None,
            worker_id: "w".to_string(),
            status: "failed".to_string(),
            assigned_at_ms: 0,
            started_at_ms: 0,
            archived: false,
            archive_state,
            summary: None,
        }),
        archive: None,
        live: None,
        recent_events: Vec::new(),
        has_transcript: false,
        summary: None,
        cost: None,
        trigger: None,
        archive_hold,
    }
}

/// `show` says when a finished run's archive has not arrived, and why
/// when the sweeper holds it; it says nothing for a run with no archive
/// due or one already archived.
#[test]
fn show_names_a_pending_or_held_archive() {
    use fq_ops::views::InvocationArchiveState as S;
    assert_eq!(format_archive_state(&detail_with(S::NotDue, None)), None);
    assert_eq!(format_archive_state(&detail_with(S::Archived, None)), None);
    assert_eq!(
        format_archive_state(&detail_with(S::Pending, None)).as_deref(),
        Some("pending: the run has ended and its archive has not reached the control plane")
    );
    let hold = fq_ops::health::ArchiveHold {
        invocation_id: "inv".to_string(),
        agent_id: "a".to_string(),
        reason: fq_ops::health::ArchiveHoldReason::PayloadTooLarge {
            event_type: "invocation_archived".to_string(),
            size_bytes: 2_000_000,
            limit_bytes: 1_048_576,
        },
        remedy: "raise the broker's max_payload".to_string(),
    };
    let held = format_archive_state(&detail_with(S::Held, Some(hold))).unwrap();
    assert!(
        held.starts_with("held by the worker's archive sweeper: "),
        "{held}"
    );
    assert!(
        held.ends_with("-> raise the broker's max_payload"),
        "{held}"
    );
}
