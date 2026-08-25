//! What an operator reads. The composite's own arithmetic — which
//! counts land where, and what counts as an issue — is asserted in
//! `doctor_report/tests.rs`; these are its rendering siblings, reached
//! through the same builder so a report that cannot be produced cannot
//! be pinned here either.

use super::*;
use fq_ops::surface::build_doctor_report;
use fq_ops::views::{ExecutionsView, FailureView, WorkerView};

fn worker(id: &str, status: &str) -> WorkerView {
    WorkerView {
        worker_id: id.to_string(),
        host: "h".to_string(),
        registered_at_ms: 0,
        last_heartbeat_ms: 0,
        status: status.to_string(),
        in_flight_count: 0,
    }
}

fn executions(in_flight: i64, stuck_ids: &[&str]) -> ExecutionsView {
    ExecutionsView {
        in_flight,
        working: 0,
        working_ids: vec![],
        stuck: stuck_ids.len() as i64,
        stuck_ids: stuck_ids.iter().map(|s| s.to_string()).collect(),
    }
}

fn failure(kind: &str, count: i64) -> FailureView {
    FailureView {
        error_kind: kind.to_string(),
        count,
    }
}

#[test]
fn all_clear_renders_a_verdict_and_still_shows_dead_letters() {
    let report = build_doctor_report(&[worker("w1", "alive")], &ExecutionsView::default(), 0, &[]);
    let out = render_doctor_report_human(&report);
    assert!(out.contains("All clear."), "got: {out}");
    // Dead-letter section is always shown.
    assert!(out.contains("Dead-letters: none"), "got: {out}");
}

#[test]
fn stale_workers_render_with_their_remediation() {
    let workers = vec![
        worker("alive-1", "alive"),
        worker("stale-1", "stale"),
        worker("gone-1", "shutdown"),
    ];
    let report = build_doctor_report(&workers, &ExecutionsView::default(), 0, &[]);

    let out = render_doctor_report_human(&report);
    assert!(out.contains("1 alive, 1 stale, 1 shutdown"), "got: {out}");
    assert!(out.contains("fq workers list --stale-only"), "got: {out}");
    assert!(!out.contains("All clear."), "got: {out}");
}

#[test]
fn stuck_in_flight_renders_with_its_remediation() {
    let report = build_doctor_report(&[], &executions(2, &["stuck-abcdef01"]), 0, &[]);

    let out = render_doctor_report_human(&report);
    assert!(
        out.contains("2 in-flight (0 working, 1 stuck)"),
        "got: {out}"
    );
    assert!(out.contains("fq invocation drop"), "got: {out}");
}

/// The stuck line names the threshold the daemon applied. It reads it
/// from the shared constant today; when Phase 5 splits the binaries,
/// either the number travels in the report or this line stops naming
/// one.
#[test]
fn the_stuck_line_names_the_threshold_in_seconds() {
    let report = build_doctor_report(&[], &executions(1, &["stuck-abcdef01"]), 0, &[]);
    let out = render_doctor_report_human(&report);
    assert!(
        out.contains(&format!(
            "not advanced in >{}s",
            DOCTOR_STUCK_THRESHOLD_MS / 1000
        )),
        "got: {out}"
    );
}

/// Working invocations (#130) surface in the human report but carry no
/// remediation hint — they are healthy.
#[test]
fn working_in_flight_shown_but_offered_no_remedy() {
    let ex = ExecutionsView {
        in_flight: 2,
        working: 1,
        working_ids: vec!["019f5b3f-31fb-7ae0-b130-3d65ccf40375".to_string()],
        stuck: 0,
        stuck_ids: vec![],
    };
    let report = build_doctor_report(&[], &ex, 0, &[]);

    let out = render_doctor_report_human(&report);
    assert!(
        out.contains("2 in-flight (1 working, 0 stuck)"),
        "got: {out}"
    );
    assert!(!out.contains("fq invocation drop"), "got: {out}");
}

#[test]
fn dead_lettered_triggers_render_with_both_next_steps() {
    let failures = vec![failure("trigger_exhausted", 2), failure("tool_error", 1)];
    let report = build_doctor_report(&[], &ExecutionsView::default(), 0, &failures);

    let out = render_doctor_report_human(&report);
    assert!(
        out.contains("Dead-letters: 2 exhausted trigger(s)"),
        "got: {out}"
    );
    assert!(out.contains("fq dead-letters list"), "got: {out}");
    assert!(out.contains("fq dead-letters requeue"), "got: {out}");
}

#[test]
fn ambiguous_renders_with_its_remediation() {
    let report = build_doctor_report(&[], &ExecutionsView::default(), 3, &[]);

    let out = render_doctor_report_human(&report);
    assert!(out.contains("Ambiguous invocations: 3"), "got: {out}");
    assert!(
        out.contains("fq invocation list --status=ambiguous"),
        "got: {out}"
    );
}

#[test]
fn permanent_failures_render_per_kind() {
    let failures = vec![failure("budget_exceeded", 2), failure("tool_error", 1)];
    let report = build_doctor_report(&[], &ExecutionsView::default(), 0, &failures);

    let out = render_doctor_report_human(&report);
    assert!(out.contains("Permanent failures: 3"), "got: {out}");
    assert!(out.contains("budget_exceeded: 2"), "got: {out}");
    assert!(out.contains("tool_error: 1"), "got: {out}");
    assert!(
        out.contains("fq invocation list --status=failed"),
        "got: {out}"
    );
}

/// A failure kind that is not a dead letter renders the "none" line —
/// the counterpart to `dead_letters_never_fabricates_a_count`.
#[test]
fn a_non_dead_letter_failure_still_renders_dead_letters_none() {
    let report = build_doctor_report(
        &[],
        &ExecutionsView::default(),
        0,
        &[failure("runtimeerror", 7)],
    );
    let out = render_doctor_report_human(&report);
    assert!(out.contains("Dead-letters: none"), "got: {out}");
}
