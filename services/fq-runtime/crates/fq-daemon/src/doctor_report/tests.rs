//! What the composite makes of its inputs. The rendering half of the
//! same checks lives in `doctor/tests.rs` — these two files were one
//! before `control.doctor` split the verb from the report, and each
//! test here has a sibling there asserting what an operator reads.

use super::*;
use fq_ops::surface::DoctorDeadLetters;
use fq_runtime::views::{ExecutionsView, FailureView, WorkerView};

pub(crate) fn worker(id: &str, status: &str, last_heartbeat: i64) -> WorkerView {
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
pub(crate) fn executions(in_flight: i64, stuck_ids: &[&str]) -> ExecutionsView {
    ExecutionsView {
        in_flight,
        working: 0,
        working_ids: vec![],
        stuck: stuck_ids.len() as i64,
        stuck_ids: stuck_ids.iter().map(|s| s.to_string()).collect(),
    }
}

pub(crate) const NOW: i64 = 1_000_000;

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
}

/// A status the control-plane does not record is a store/view drift.
/// It counts as stale so it surfaces as an issue rather than vanishing
/// between the three known buckets.
#[test]
fn an_unknown_worker_status_surfaces_rather_than_vanishing() {
    let workers = vec![worker("odd-1", "quiescent", NOW)];
    let report = build_doctor_report(&workers, &ExecutionsView::default(), 0, &[]);

    assert_eq!(report.workers.alive, 0);
    assert_eq!(report.workers.stale, 1);
    assert_eq!(report.workers.stale_ids, vec!["odd-1".to_string()]);
    assert!(report.has_issues());
}

#[test]
fn stuck_in_flight_flagged() {
    let report = build_doctor_report(&[], &executions(2, &["stuck-abcdef01"]), 0, &[]);

    assert_eq!(report.executions.in_flight, 2);
    assert_eq!(report.executions.stuck, 1);
    // The id survives whole. It was shortened here once, to match the
    // human report, and a shortened id is not one: `invocation.get`
    // matches exactly, so nothing took it back. Shorten for display,
    // never on the wire.
    assert_eq!(
        report.executions.stuck_ids,
        vec!["stuck-abcdef01".to_string()]
    );
    assert!(report.has_issues());
}

/// Working invocations (fresh open dispatch, #130) are healthy — they
/// are counted, and they are not an issue.
#[test]
fn working_in_flight_counted_but_not_an_issue() {
    let ex = ExecutionsView {
        in_flight: 2,
        working: 1,
        working_ids: vec!["019f5b3f-31fb-7ae0-b130-3d65ccf40375".to_string()],
        stuck: 0,
        stuck_ids: vec![],
    };
    let report = build_doctor_report(&[], &ex, 0, &[]);

    assert!(!report.has_issues());
    // Whole, same convention as stuck_ids — this is the id the
    // dashboard links to and `fq invocation show` is handed.
    assert_eq!(
        report.executions.working_ids,
        vec!["019f5b3f-31fb-7ae0-b130-3d65ccf40375".to_string()]
    );
}

/// #49: dead-lettered triggers are counted from the
/// `trigger_exhausted` failures bucket, so the report needs no extra
/// query.
#[test]
fn dead_lettered_triggers_are_counted() {
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
}

#[test]
fn ambiguous_flagged() {
    let report = build_doctor_report(&[], &ExecutionsView::default(), 3, &[]);
    assert_eq!(report.ambiguous, 3);
    assert!(report.has_issues());
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

/// The report crosses a wire now, so the shape has to survive the
/// round trip the verb actually performs — the daemon serialises it,
/// the client deserialises it, and `--json` prints the client's copy.
#[test]
fn the_report_survives_the_wire_round_trip() {
    let report = build_doctor_report(
        &[worker("w1", "alive", NOW), worker("w2", "stale", 0)],
        &executions(2, &["stuck-abcdef01"]),
        1,
        &[FailureView {
            error_kind: "trigger_exhausted".to_string(),
            count: 3,
        }],
    );
    let wire = serde_json::to_value(&report).unwrap();
    let back: DoctorReport = serde_json::from_value(wire).unwrap();
    assert_eq!(back, report);
}

/// The count derives only from the `trigger_exhausted` bucket —
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
}
