use super::*;
use fq_runtime::views::{ExecutionsView, FailureView, WorkerView};

fn worker(id: &str, status: &str, last_heartbeat: i64) -> WorkerView {
    WorkerView {
        worker_id: id.to_string(),
        host: "h".to_string(),
        registered_at_ms: 0,
        last_heartbeat_ms: last_heartbeat,
        status: status.to_string(),
        in_flight_count: 0,
    }
}

/// The in-flight/stuck determination itself (threshold, clock skew)
/// is `views::Views::executions`' job and is covered by its tests;
/// doctor receives the finished counts.
fn executions(in_flight: i64, stuck_ids: &[&str]) -> ExecutionsView {
    ExecutionsView {
        in_flight,
        working: 0,
        working_ids: vec![],
        stuck: stuck_ids.len() as i64,
        stuck_ids: stuck_ids.iter().map(|s| s.to_string()).collect(),
    }
}

const NOW: i64 = 1_000_000;

#[test]
fn all_clear_when_everything_healthy() {
    let workers = vec![worker("w1", "alive", NOW)];
    let report = build_doctor_report(&workers, &ExecutionsView::default(), 0, &[]);

    assert!(!report.has_issues());
    assert_eq!(report.workers.alive, 1);
    assert_eq!(report.workers.stale, 0);
    assert_eq!(report.executions.in_flight, 0);
    assert_eq!(report.failure_total(), 0);
    assert_eq!(
        report.dead_letters,
        DoctorDeadLetters {
            exhausted_triggers: 0
        }
    );

    let out = render_doctor_report_human(&report);
    assert!(out.contains("All clear."), "got: {out}");
    // Dead-letter section is always shown.
    assert!(out.contains("Dead-letters: none"), "got: {out}");
}

#[test]
fn running_in_flight_work_is_not_an_issue() {
    // In-flight but not stuck is healthy.
    let report = build_doctor_report(&[], &executions(1, &[]), 0, &[]);
    assert_eq!(report.executions.in_flight, 1);
    assert_eq!(report.executions.stuck, 0);
    assert!(!report.has_issues());
}

#[test]
fn stale_workers_flagged_with_ids() {
    let workers = vec![
        worker("alive-1", "alive", NOW),
        worker("stale-1", "stale", NOW - 60_000),
        worker("gone-1", "shutdown", 0),
    ];
    let report = build_doctor_report(&workers, &ExecutionsView::default(), 0, &[]);

    assert_eq!(report.workers.alive, 1);
    assert_eq!(report.workers.stale, 1);
    assert_eq!(report.workers.shutdown, 1);
    assert_eq!(report.workers.stale_ids, vec!["stale-1".to_string()]);
    assert!(report.has_issues());

    let out = render_doctor_report_human(&report);
    assert!(out.contains("1 alive, 1 stale, 1 shutdown"), "got: {out}");
    assert!(out.contains("fq workers list --stale-only"), "got: {out}");
    assert!(!out.contains("All clear."), "got: {out}");
}

#[test]
fn stuck_in_flight_flagged() {
    let report = build_doctor_report(&[], &executions(2, &["stuck-abcdef01"]), 0, &[]);

    assert_eq!(report.executions.in_flight, 2);
    assert_eq!(report.executions.stuck, 1);
    // Short id (8 chars) recorded for triage.
    assert_eq!(report.executions.stuck_ids, vec!["stuck-ab".to_string()]);
    assert!(report.has_issues());

    let out = render_doctor_report_human(&report);
    assert!(
        out.contains("2 in-flight (0 working, 1 stuck)"),
        "got: {out}"
    );
    assert!(out.contains("fq invocation drop"), "got: {out}");
}

/// Working invocations (fresh open dispatch, #130) surface in the human
/// report but are healthy — no issue, no remediation hint.
#[test]
fn working_in_flight_shown_but_not_an_issue() {
    let ex = ExecutionsView {
        in_flight: 2,
        working: 1,
        working_ids: vec!["019f5b3f-31fb-7ae0-b130-3d65ccf40375".to_string()],
        stuck: 0,
        stuck_ids: vec![],
    };
    let report = build_doctor_report(&[], &ex, 0, &[]);

    assert!(!report.has_issues());
    // Short id (8 chars), same convention as stuck_ids.
    assert_eq!(report.executions.working_ids, vec!["019f5b3f".to_string()]);

    let out = render_doctor_report_human(&report);
    assert!(
        out.contains("2 in-flight (1 working, 0 stuck)"),
        "got: {out}"
    );
    assert!(!out.contains("fq invocation drop"), "got: {out}");
}

/// #49: dead-lettered triggers surface as their own doctor line,
/// counted from the `trigger_exhausted` failures bucket.
#[test]
fn dead_lettered_triggers_are_counted_and_rendered() {
    let failures = vec![
        FailureView {
            error_kind: "trigger_exhausted".to_string(),
            count: 2,
        },
        FailureView {
            error_kind: "tool_error".to_string(),
            count: 1,
        },
    ];
    let report = build_doctor_report(&[], &ExecutionsView::default(), 0, &failures);
    assert_eq!(
        report.dead_letters,
        DoctorDeadLetters {
            exhausted_triggers: 2
        }
    );
    assert!(report.has_issues());

    let out = render_doctor_report_human(&report);
    assert!(
        out.contains("Dead-letters: 2 exhausted trigger(s)"),
        "got: {out}"
    );
    assert!(out.contains("fq dead-letters list"), "got: {out}");
    assert!(out.contains("fq dead-letters requeue"), "got: {out}");
}

#[test]
fn ambiguous_flagged() {
    let report = build_doctor_report(&[], &ExecutionsView::default(), 3, &[]);
    assert_eq!(report.ambiguous, 3);
    assert!(report.has_issues());

    let out = render_doctor_report_human(&report);
    assert!(out.contains("Ambiguous invocations: 3"), "got: {out}");
    assert!(
        out.contains("fq invocation list --status=ambiguous"),
        "got: {out}"
    );
}

#[test]
fn permanent_failures_grouped_by_kind() {
    let failures = vec![
        FailureView {
            error_kind: "budget_exceeded".to_string(),
            count: 2,
        },
        FailureView {
            error_kind: "tool_error".to_string(),
            count: 1,
        },
    ];
    let report = build_doctor_report(&[], &ExecutionsView::default(), 0, &failures);

    assert_eq!(report.failure_total(), 3);
    assert!(report.has_issues());

    let out = render_doctor_report_human(&report);
    assert!(out.contains("Permanent failures: 3"), "got: {out}");
    assert!(out.contains("budget_exceeded: 2"), "got: {out}");
    assert!(out.contains("tool_error: 1"), "got: {out}");
    assert!(
        out.contains("fq invocation list --status=failed"),
        "got: {out}"
    );
}

#[test]
fn report_serialises_to_stable_json_shape() {
    let report = build_doctor_report(
        &[worker("w1", "alive", NOW)],
        &executions(1, &[]),
        1,
        &[FailureView {
            error_kind: "runtimeerror".to_string(),
            count: 4,
        }],
    );
    let v = serde_json::to_value(&report).unwrap();
    assert_eq!(v["workers"]["alive"], 1);
    assert_eq!(v["executions"]["in_flight"], 1);
    assert_eq!(v["ambiguous"], 1);
    assert_eq!(v["failures"][0]["error_kind"], "runtimeerror");
    assert_eq!(v["failures"][0]["count"], 4);
    assert_eq!(v["dead_letters"]["exhausted_triggers"], 0);
}

/// The count derives only from the `triggerexhausted` bucket —
/// other failure kinds never inflate it.
#[test]
fn dead_letters_never_fabricates_a_count() {
    let report = build_doctor_report(
        &[],
        &ExecutionsView::default(),
        0,
        &[FailureView {
            error_kind: "runtimeerror".to_string(),
            count: 7,
        }],
    );
    assert_eq!(report.dead_letters.exhausted_triggers, 0);
    let out = render_doctor_report_human(&report);
    assert!(out.contains("Dead-letters: none"), "got: {out}");
}
