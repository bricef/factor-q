//! What an operator reads. The composite's own arithmetic — which
//! counts land where, and what counts as an issue — is asserted in
//! `doctor_report/tests.rs`; these are its rendering siblings, reached
//! through the same builder so a report that cannot be produced cannot
//! be pinned here either.

use super::*;
use fq_ops::surface::build_doctor_report;
use fq_ops::views::{ExecutionsView, FailureView, WorkerView};

/// The derived threshold a daemon would have handed the report. An
/// arbitrary value on purpose: the renderer must print what it is
/// given, not a number it knows.
const THRESHOLD_MS: i64 = 4_210_000;

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
    let report = build_doctor_report(
        &[worker("w1", "alive")],
        &ExecutionsView::default(),
        THRESHOLD_MS,
        0,
        &[],
        Vec::new(),
        Vec::new(),
    );
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
    let report = build_doctor_report(
        &workers,
        &ExecutionsView::default(),
        THRESHOLD_MS,
        0,
        &[],
        Vec::new(),
        Vec::new(),
    );

    let out = render_doctor_report_human(&report);
    assert!(out.contains("1 alive, 1 stale, 1 shutdown"), "got: {out}");
    assert!(out.contains("fq workers list --stale-only"), "got: {out}");
    assert!(!out.contains("All clear."), "got: {out}");
}

#[test]
fn stuck_in_flight_renders_with_its_remediation() {
    let report = build_doctor_report(
        &[],
        &executions(2, &["stuck-abcdef01"]),
        THRESHOLD_MS,
        0,
        &[],
        Vec::new(),
        Vec::new(),
    );

    let out = render_doctor_report_human(&report);
    assert!(
        out.contains("2 in-flight (0 working, 1 stuck after 4210s)"),
        "got: {out}"
    );
    assert!(out.contains("fq invocation drop"), "got: {out}");
}

/// The stuck line names the threshold the daemon applied — and takes it
/// off the report rather than out of a constant of its own (#37). The
/// number is derived from the daemon's call deadlines, so across the
/// `fq`/`fqd` split a client that quoted its own build would print a
/// threshold the daemon it is talking to never used.
#[test]
fn the_stuck_line_names_the_threshold_the_daemon_reported() {
    let report = build_doctor_report(
        &[],
        &executions(1, &["stuck-abcdef01"]),
        THRESHOLD_MS,
        0,
        &[],
        Vec::new(),
        Vec::new(),
    );
    let out = render_doctor_report_human(&report);
    assert!(
        out.contains(&format!("not advanced in >{}s", THRESHOLD_MS / 1000)),
        "got: {out}"
    );
    assert!(
        out.contains("fq events query --event-type invocation_stuck"),
        "the report points at the event that fired: {out}"
    );
}

/// The same number appears on the executions line whether or not
/// anything is stuck, so an operator can read the daemon's threshold
/// out of a clean report.
#[test]
fn a_clean_executions_line_still_names_the_threshold() {
    let report = build_doctor_report(
        &[],
        &ExecutionsView::default(),
        THRESHOLD_MS,
        0,
        &[],
        Vec::new(),
        Vec::new(),
    );
    let out = render_doctor_report_human(&report);
    assert!(
        out.contains("0 in-flight (0 working, 0 stuck after 4210s)"),
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
    let report = build_doctor_report(&[], &ex, THRESHOLD_MS, 0, &[], Vec::new(), Vec::new());

    let out = render_doctor_report_human(&report);
    assert!(
        out.contains("2 in-flight (1 working, 0 stuck after 4210s)"),
        "got: {out}"
    );
    assert!(!out.contains("fq invocation drop"), "got: {out}");
}

#[test]
fn dead_lettered_triggers_render_with_both_next_steps() {
    let failures = vec![failure("trigger_exhausted", 2), failure("tool_error", 1)];
    let report = build_doctor_report(
        &[],
        &ExecutionsView::default(),
        THRESHOLD_MS,
        0,
        &failures,
        Vec::new(),
        Vec::new(),
    );

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
    let report = build_doctor_report(
        &[],
        &ExecutionsView::default(),
        THRESHOLD_MS,
        3,
        &[],
        Vec::new(),
        Vec::new(),
    );

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
    let report = build_doctor_report(
        &[],
        &ExecutionsView::default(),
        THRESHOLD_MS,
        0,
        &failures,
        Vec::new(),
        Vec::new(),
    );

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
        THRESHOLD_MS,
        0,
        &[failure("runtimeerror", 7)],
        Vec::new(),
        Vec::new(),
    );
    let out = render_doctor_report_human(&report);
    assert!(out.contains("Dead-letters: none"), "got: {out}");
}

// ------------------------------------------------------------------
// Consumers (#549). The Phase 1 exit criterion, verbatim: "`fq doctor`
// reports every consumer".
// ------------------------------------------------------------------

fn active(name: &str, stuck: bool, redeliveries: u64) -> fq_ops::health::ConsumerHealth {
    fq_ops::health::ConsumerHealth::Active {
        name: name.to_string(),
        delivered: 100,
        lag: if stuck { 42 } else { 0 },
        ack_pending: u64::from(stuck),
        num_pending: if stuck { 42 } else { 0 },
        num_redelivered: u64::from(stuck),
        redeliveries,
        stuck,
    }
}

/// Every consumer is named, healthy ones included: an operator reading
/// this during an incident needs to know which durables were *checked*,
/// not only which complained.
#[test]
fn every_consumer_is_named_in_the_report() {
    let consumers = vec![
        active("fq-projector", false, 0),
        active("fq-coordination", false, 0),
        active("fq-heartbeat", false, 0),
        active("fq-dispatcher", false, 0),
        fq_ops::health::ConsumerHealth::Missing {
            name: "fq-advisory-watch".to_string(),
        },
    ];
    let report = build_doctor_report(
        &[],
        &ExecutionsView::default(),
        THRESHOLD_MS,
        0,
        &[],
        consumers,
        Vec::new(),
    );
    let out = render_doctor_report_human(&report);

    for name in [
        "fq-projector",
        "fq-coordination",
        "fq-heartbeat",
        "fq-dispatcher",
        "fq-advisory-watch",
    ] {
        assert!(
            out.contains(name),
            "consumer {name} is missing from:\n{out}"
        );
    }
    assert!(
        out.contains("Consumers: 5 checked, 1 unhealthy"),
        "got:\n{out}"
    );
}

/// The wedge of finding B4, as an operator reads it: the stuck consumer
/// is named, its redelivery count is shown, and the report is an issue.
#[test]
fn a_stuck_consumer_is_named_counted_and_makes_the_report_an_issue() {
    let report = build_doctor_report(
        &[],
        &ExecutionsView::default(),
        THRESHOLD_MS,
        0,
        &[],
        vec![
            active("fq-projector", false, 0),
            active("fq-coordination", true, 37),
        ],
        Vec::new(),
    );
    assert!(
        report.has_issues(),
        "a consumer that stopped making progress is an issue"
    );
    let out = render_doctor_report_human(&report);
    assert!(out.contains("Verdict: issues found"), "got:\n{out}");
    assert!(out.contains("fq-coordination: ✗ stuck"), "got:\n{out}");
    assert!(out.contains("37 redeliveries"), "got:\n{out}");
    assert!(
        out.contains("consumer=fq-coordination"),
        "the next step has to be greppable: {out}"
    );
}

/// Lag alone is not a fault. A consumer catching up after a restart is
/// working, and a health report that called it broken would train an
/// operator to ignore the line.
#[test]
fn a_lagging_but_progressing_consumer_is_not_an_issue() {
    let behind = fq_ops::health::ConsumerHealth::Active {
        name: "fq-projector".to_string(),
        delivered: 10,
        lag: 9_000,
        ack_pending: 1,
        num_pending: 9_000,
        num_redelivered: 0,
        redeliveries: 0,
        stuck: false,
    };
    let report = build_doctor_report(
        &[],
        &ExecutionsView::default(),
        THRESHOLD_MS,
        0,
        &[],
        vec![behind],
        Vec::new(),
    );
    assert!(!report.has_issues(), "catching up is not a fault");
    let out = render_doctor_report_human(&report);
    assert!(out.contains("fq-projector: ok (lag 9000)"), "got:\n{out}");
}
