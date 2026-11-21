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
        ended_by: None,
        ended_at_ms: None,
        termination: None,
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
        ended_by: None,
        ended_at_ms: None,
        termination: None,
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
    assert!(line.chars().count() < 160, "bounded: {line}");

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
        ended_by: None,
        ended_at_ms: None,
        termination: None,
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
        ended_by: None,
        ended_at_ms: None,
        termination: None,
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
        consumed_why: None,
        operator_dropped_at_ms: None,
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
        [S::NotDue, S::Pending, S::Held, S::Archived, S::WillNotCome].map(archive_label),
        ["-", "pending", "held", "yes", "none"]
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
            ended_by: None,
            ended_at_ms: None,
            termination: None,
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
    assert_eq!(
        format_archive_state(&detail_with(S::WillNotCome, None)).as_deref(),
        Some("none: the run was dropped with no row on its worker, so no archive will come")
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

/// **The claim line names a consumed claim and how, and reads what a newer
/// daemon sends without failing (issue 1055).** A state this build does
/// not know decodes to `Unknown` rather than failing the whole answer, and
/// a reason it does not know is shown as it is.
#[test]
fn format_trigger_delivery_names_a_consumed_claim_and_reads_newer_values() {
    let decode = |state: &str, why: Option<&str>| -> fq_ops::views::TriggerDeliveryView {
        let mut wire = serde_json::json!({
            "stream": "fq-triggers",
            "stream_epoch": "1767323045000000000",
            "stream_seq": 42,
            "state": state,
            "claimant": "worker-alpha",
            "claimed_at_ms": 1_767_323_045_000_i64,
        });
        if let Some(why) = why {
            wire["consumed_why"] = serde_json::json!(why);
        }
        serde_json::from_value(wire).expect("the claim decodes")
    };
    assert_eq!(
        format_trigger_delivery(Some(&decode("consumed", Some("failed_for_good")))),
        "fq-triggers seq 42 (consumed without a run: failed_for_good; claimed \
         2026-01-02T03:04:05Z by worker-alpha)"
    );
    assert_eq!(
        format_trigger_delivery(Some(&decode(
            "consumed",
            Some("a_reason_from_a_newer_daemon")
        ))),
        "fq-triggers seq 42 (consumed without a run: a_reason_from_a_newer_daemon; claimed \
         2026-01-02T03:04:05Z by worker-alpha)"
    );
    let mut dropped = decode("consumed", Some("failed_for_good"));
    dropped.operator_dropped_at_ms = Some(1_767_323_046_000);
    assert_eq!(
        format_trigger_delivery(Some(&dropped)),
        "fq-triggers seq 42 (consumed without a run: failed_for_good; dropped by an operator, \
         recorded 2026-01-02T03:04:06Z; claimed 2026-01-02T03:04:05Z by worker-alpha)",
        "an operator's drop recorded on the claim is named beside its reason"
    );
    let newer = decode("a_state_from_a_newer_daemon", None);
    assert_eq!(newer.state, fq_ops::views::TriggerClaimState::Unknown);
    assert_eq!(
        format_trigger_delivery(Some(&newer)),
        "fq-triggers seq 42 (in a state this build does not know; claimed \
         2026-01-02T03:04:05Z by worker-alpha)"
    );
}

/// A row the run ended, `ended_at` and the request as given.
fn ended(
    status: &str,
    ended_by: Option<fq_ops::views::InvocationEnder>,
    ended_at_ms: Option<i64>,
    termination: Option<fq_ops::views::TerminationRequestView>,
) -> fq_ops::views::InvocationSummaryView {
    fq_ops::views::InvocationSummaryView {
        invocation_id: "inv".to_string(),
        agent_id: None,
        worker_id: "w".to_string(),
        status: status.to_string(),
        assigned_at_ms: 0,
        started_at_ms: 0,
        archived: false,
        archive_state: fq_ops::views::InvocationArchiveState::Pending,
        ended_by,
        ended_at_ms,
        termination,
        summary: None,
    }
}

/// 2026-01-02T03:04:05Z.
const T: i64 = 1_767_323_045_000;

fn request(at: i64) -> Option<fq_ops::views::TerminationRequestView> {
    Some(fq_ops::views::TerminationRequestView {
        requested_at_ms: at,
        requested_by: Some("on-call".to_string()),
        reason: Some("runaway".to_string()),
    })
}

/// The list's `ended by` column: `run` or `drop`, and a dash where the
/// row does not say, a run still going or one from before the record.
#[test]
fn the_ended_by_column_names_the_ender_or_nothing() {
    use fq_ops::views::InvocationEnder as E;
    assert_eq!(
        [
            ended("completed", Some(E::Run), Some(T), None),
            ended("failed", Some(E::Drop), Some(T), request(T)),
            ended("completed", None, None, None),
            ended("in_flight", None, None, None),
        ]
        .iter()
        .map(ended_by_label)
        .collect::<Vec<_>>(),
        ["run", "drop", "-", "-"]
    );
}

/// `show`'s `ended by:` line: who and when; `not recorded` for a run that
/// ended before its row recorded an ender; nothing for one still going.
#[test]
fn show_says_who_ended_the_run_and_when() {
    use fq_ops::views::InvocationEnder as E;
    assert_eq!(
        format_ended_by(&ended("completed", Some(E::Run), Some(T), None)).as_deref(),
        Some("run at 2026-01-02T03:04:05Z")
    );
    assert_eq!(
        format_ended_by(&ended("failed", Some(E::Drop), Some(T), request(T))).as_deref(),
        Some("drop at 2026-01-02T03:04:05Z")
    );
    assert_eq!(
        format_ended_by(&ended("failed", None, None, None)).as_deref(),
        Some("not recorded")
    );
    assert_eq!(
        format_ended_by(&ended("completed", None, None, None)).as_deref(),
        Some("not recorded"),
        "a completed run from before the record does not know who ended it either"
    );
    assert_eq!(format_ended_by(&ended("in_flight", None, None, None)), None);
}

/// `show`'s `termination:` line reads the ending beside the request in
/// each of the three ways, says only the request where the row records no
/// ender, and says nothing where no request is on record.
#[test]
fn show_reads_the_termination_three_ways() {
    use fq_ops::views::InvocationEnder as E;
    let said = "(requested 2026-01-02T03:04:05Z by on-call: runaway)";
    assert_eq!(
        format_termination(&ended("failed", Some(E::Drop), Some(T), request(T))),
        Some(format!("dropped {said}"))
    );
    assert_eq!(
        format_termination(&ended(
            "completed",
            Some(E::Run),
            Some(T - 1_000),
            request(T)
        )),
        Some(format!(
            "already ended when the termination was requested {said}"
        ))
    );
    assert_eq!(
        format_termination(&ended("completed", Some(E::Run), Some(T), request(T))),
        Some(format!(
            "already ended when the termination was requested {said}"
        )),
        "an ending at the request's own time had already happened"
    );
    assert_eq!(
        format_termination(&ended(
            "completed",
            Some(E::Run),
            Some(T + 1_000),
            request(T)
        )),
        Some(format!(
            "requested while running; it reached its own ending first {said}"
        ))
    );
    assert_eq!(
        format_termination(&ended("failed", None, None, request(T))).as_deref(),
        Some("requested 2026-01-02T03:04:05Z by on-call: runaway"),
        "a row that records no ender says only the request"
    );
    let bare = Some(fq_ops::views::TerminationRequestView {
        requested_at_ms: T,
        requested_by: None,
        reason: None,
    });
    assert_eq!(
        format_termination(&ended("failed", Some(E::Drop), Some(T), bare)).as_deref(),
        Some("dropped (requested 2026-01-02T03:04:05Z)"),
        "who asked and why, only where the record says"
    );
    assert_eq!(
        format_termination(&ended("completed", Some(E::Run), Some(T), None)),
        None
    );
}

/// A drop read back, as `invocation_drop` builds it.
fn read_back(
    case: Option<fq_ops::views::DropCase>,
    provisional: bool,
    expects_no_archive: bool,
    earlier: Option<fq_ops::views::TerminationRequestView>,
) -> InvocationDropResult {
    InvocationDropResult {
        invocation_id: "inv".to_string(),
        agent_id: Some("a".to_string()),
        event_id: "e".to_string(),
        reason: Some("second look".to_string()),
        case,
        provisional,
        earlier_request: earlier,
        read_back_failed: None,
        expects_no_archive,
    }
}

/// What `fq invocation drop` prints for one drop: the case this drop
/// found, read from its own time; that a provisional ending can still give
/// way, while it can; the earlier request the record keeps; and the reason
/// given, made safe. Nothing of a case where the row does not say.
#[test]
fn the_drop_says_what_this_drop_found() {
    use fq_ops::views::DropCase as C;
    let follow =
        "Reason given: second look\nFollow with `fq invocation show inv` to read the record.\n";
    let provisional = "That ending is provisional until the run's archive lands: if the run reached \
                       its own ending first, its own ending stands, and `fq invocation show` reads \
                       the final record.\n";
    let earlier = "An earlier request is on record (requested 2026-01-02T03:04:05Z by on-call: \
                   runaway); the record keeps the first.\n";
    assert_eq!(
        format_drop_recorded(&read_back(Some(C::Dropped), true, false, None)),
        format!(
            "Drop of invocation inv recorded (agent=a, event_id=e): dropped.\n{provisional}{follow}"
        )
    );
    assert_eq!(
        format_drop_recorded(&read_back(
            Some(C::EndedByAnEarlierDrop),
            true,
            false,
            request(T)
        )),
        format!(
            "Drop of invocation inv recorded (agent=a, event_id=e): already ended by an earlier \
             drop.\n{provisional}{earlier}{follow}"
        )
    );
    assert_eq!(
        format_drop_recorded(&read_back(Some(C::AlreadyEnded), false, false, None)),
        format!(
            "Drop of invocation inv recorded (agent=a, event_id=e): already ended when the \
             termination was requested.\n{follow}"
        )
    );
    assert_eq!(
        format_drop_recorded(&read_back(Some(C::AlreadyEnded), false, false, request(T))),
        format!(
            "Drop of invocation inv recorded (agent=a, event_id=e): already ended when the \
             termination was requested.\n{earlier}{follow}"
        )
    );
    assert_eq!(
        format_drop_recorded(&read_back(
            Some(C::ReachedItsOwnEndingFirst),
            false,
            false,
            None
        )),
        format!(
            "Drop of invocation inv recorded (agent=a, event_id=e): requested while running; it \
             reached its own ending first.\n{follow}"
        )
    );
    assert_eq!(
        format_drop_recorded(&read_back(None, false, false, None)),
        format!("Drop of invocation inv recorded (agent=a, event_id=e).\n{follow}")
    );
}

/// **The provisional sentence says what is true of the row.** Of a drop's
/// ending whose archive is still to come, that it gives way to the run's
/// own if that came first; of one whose archive will not come, that none
/// is expected and the run's own ending stands if it arrives; and where
/// the first line names no ending, a row from before the record, nothing
/// of an ending, only what can still come.
#[test]
fn the_provisional_sentence_is_true_of_the_row() {
    use fq_ops::views::DropCase as C;
    let line = |case, expects_no_archive| {
        format_drop_recorded(&read_back(case, true, expects_no_archive, None))
            .lines()
            .nth(1)
            .unwrap()
            .to_string()
    };
    assert_eq!(
        line(Some(C::Dropped), false),
        "That ending is provisional until the run's archive lands: if the run reached its own \
         ending first, its own ending stands, and `fq invocation show` reads the final record."
    );
    assert_eq!(
        line(Some(C::Dropped), true),
        "No archive of the run is expected; if its own ending arrives after all, that ending \
         stands, and `fq invocation show` reads the final record."
    );
    assert_eq!(
        line(None, true),
        "If the run's own ending arrives, it is recorded in place of what the row holds now, and \
         `fq invocation show` reads the final record."
    );
    assert!(!line(None, false).contains("That ending"));
}

/// **A drop whose reading back failed is reported as recorded**, with why
/// the reading failed and nothing of what it found; its JSON says the
/// same, the agent unknown.
#[test]
fn a_drop_whose_reading_failed_is_reported_as_recorded() {
    let result = InvocationDropResult::unread(
        "inv",
        "e".to_string(),
        Some("why"),
        "the invocation: malformed\nrow".to_string(),
    );
    assert_eq!(
        format_drop_recorded(&result),
        "Drop of invocation inv recorded (event_id=e), but reading it back failed, so what it \
         found is not known: the invocation: malformed\\nrow.\nReason given: why\nFollow with \
         `fq invocation show inv` to read the record.\n"
    );
    assert_eq!(
        serde_json::to_value(&result).unwrap(),
        serde_json::json!({
            "invocation_id": "inv",
            "agent_id": null,
            "event_id": "e",
            "reason": "why",
            "case": null,
            "provisional": false,
            "earlier_request": null,
            "read_back_failed": "the invocation: malformed\nrow"
        })
    );
}

/// The drop's JSON carries the case, whether the ending is provisional,
/// and the earlier request, exact, and nothing of a read that failed.
#[test]
fn the_drops_json_carries_the_case_and_the_earlier_request() {
    let mut result = read_back(
        Some(fq_ops::views::DropCase::EndedByAnEarlierDrop),
        true,
        true,
        request(T),
    );
    result.reason = Some("line one\nline two".to_string());
    assert_eq!(
        serde_json::to_value(&result).unwrap(),
        serde_json::json!({
            "invocation_id": "inv",
            "agent_id": "a",
            "event_id": "e",
            "reason": "line one\nline two",
            "case": "ended_by_an_earlier_drop",
            "provisional": true,
            "earlier_request": {
                "requested_at_ms": T,
                "requested_by": "on-call",
                "reason": "runaway"
            }
        })
    );
}

/// `show`'s `termination:` line and the drop's own lines put free text
/// through [`fq_ops::text::one_line`]: who asked and why, each one line,
/// who asked cut at 80 characters and why at 200.
#[test]
fn shows_free_text_is_made_safe() {
    use fq_ops::views::InvocationEnder as E;
    let nasty = Some(fq_ops::views::TerminationRequestView {
        requested_at_ms: T,
        requested_by: Some("on-call\u{1b}[31m".to_string()),
        reason: Some(format!("why\n  status:   completed{}", "y".repeat(300))),
    });
    let line = format_termination(&ended("failed", Some(E::Drop), Some(T), nasty.clone()))
        .expect("a termination line");
    assert!(!line.contains('\n') && !line.contains('\u{1b}'), "{line}");
    assert!(
        line.contains("by on-call\\u{1b}[31m: why\\n  status:"),
        "{line}"
    );
    assert!(line.ends_with("y…)"), "the reason is cut: {line}");
    let long_name = Some(fq_ops::views::TerminationRequestView {
        requested_at_ms: T,
        requested_by: Some("w".repeat(100)),
        reason: None,
    });
    let line = format_termination(&ended("failed", Some(E::Drop), Some(T), long_name))
        .expect("a termination line");
    assert!(
        line.contains(&format!(" by {}…)", "w".repeat(80))),
        "who asked, cut at 80: {line}"
    );
    let dropped = format_drop_recorded(&InvocationDropResult {
        reason: Some("one\ntwo\u{1b}[2J".to_string()),
        ..read_back(
            Some(fq_ops::views::DropCase::AlreadyEnded),
            false,
            false,
            nasty,
        )
    });
    assert_eq!(
        dropped.lines().count(),
        4,
        "four lines, none forged: {dropped}"
    );
    assert!(
        dropped.contains("Reason given: one\\ntwo\\u{1b}[2J\n"),
        "{dropped}"
    );
    assert!(!dropped.contains('\u{1b}'), "{dropped}");
}
