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
        ack_pending: u64::from(stuck),
        num_pending: if stuck { 42 } else { 0 },
        num_redelivered: u64::from(stuck),
        redeliveries,
        stuck,
        malformed_acked: 0,
        duplicate_dropped: 0,
        held_duplicate_dropped: 0,
    }
}

#[test]
fn stuck_dispatcher_with_held_triggers_explains_duplicate_dispatch() {
    let report = build_doctor_report(
        &[],
        &ExecutionsView::default(),
        THRESHOLD_MS,
        0,
        &[],
        vec![active("fq-dispatcher", true, 8)],
        Vec::new(),
    )
    .with_agents_at_cap(vec![fq_ops::health::AgentAtCap {
        agent: "m0-issue-fix".to_string(),
        in_flight: 2,
        cap: 2,
        held: 1,
    }]);

    let out = render_doctor_report_human(&report);
    assert!(
        out.contains("fq-dispatcher: ✗ stuck — up to 8 redeliveries past its acked floor"),
        "got:\n{out}"
    );
    assert!(
        out.contains("redeliveries on the trigger consumer while triggers are held mean a trigger may have been delivered twice"),
        "got:\n{out}"
    );
    assert!(out.contains("transcripts' attempt: field"), "got:\n{out}");
}

#[test]
fn stuck_dispatcher_with_dropped_duplicates_explains_duplicate_dispatch() {
    let mut dispatcher = active("fq-dispatcher", true, 5);
    if let fq_ops::health::ConsumerHealth::Active {
        duplicate_dropped, ..
    } = &mut dispatcher
    {
        *duplicate_dropped = 2;
    }
    let report = build_doctor_report(
        &[],
        &ExecutionsView::default(),
        THRESHOLD_MS,
        0,
        &[],
        vec![dispatcher],
        Vec::new(),
    );

    let out = render_doctor_report_human(&report);
    assert!(
        out.contains("duplicates dropped since this daemon started: 2 after their run started"),
        "got:\n{out}"
    );
    assert!(
        out.contains("redeliveries on the trigger consumer while triggers are held mean a trigger may have been delivered twice"),
        "got:\n{out}"
    );
}

/// The held-copy count does not choose a stuck dispatcher's next step.
/// It only grows while the daemon runs: a slow start that was
/// redelivered hours ago leaves it at 1 when the worker store fails and
/// every trigger is NAKed before its first WAL write. With no trigger
/// held at a cap and no copy of a started run, the step stays the one
/// that points at the handler and the store, and the count's own note
/// follows it, saying what the count cannot tell.
#[test]
fn stuck_dispatcher_with_only_held_duplicates_keeps_the_handler_step() {
    let mut dispatcher = active("fq-dispatcher", true, 5);
    if let fq_ops::health::ConsumerHealth::Active {
        held_duplicate_dropped,
        ..
    } = &mut dispatcher
    {
        *held_duplicate_dropped = 1;
    }
    let report = build_doctor_report(
        &[],
        &ExecutionsView::default(),
        THRESHOLD_MS,
        0,
        &[],
        vec![dispatcher],
        Vec::new(),
    );

    let out = render_doctor_report_human(&report);
    assert!(
        out.contains("duplicates dropped since this daemon started: 1 while held\n"),
        "got:\n{out}"
    );
    assert!(
        out.contains(
            "  -> its handler keeps failing; check the daemon log for `consumer=fq-dispatcher` \
             and free whatever it is blocked on (disk, store, broker)\n  -> \"while held\" is a \
             copy"
        ),
        "the handler step, then the count's note:\n{out}"
    );
    assert!(
        !out.contains("may have been delivered twice"),
        "a count that only grows must not name the cause of a stall:\n{out}"
    );
}

#[test]
fn stuck_non_dispatcher_keeps_handler_failure_remediation() {
    let report = build_doctor_report(
        &[],
        &ExecutionsView::default(),
        THRESHOLD_MS,
        0,
        &[],
        vec![active("fq-projector", true, 3)],
        Vec::new(),
    );

    let out = render_doctor_report_human(&report);
    assert!(
        out.contains("fq-projector: ✗ stuck — up to 3 redeliveries past its acked floor"),
        "got:\n{out}"
    );
    assert!(
        out.contains("its handler keeps failing; check the daemon log for `consumer=fq-projector`"),
        "got:\n{out}"
    );
    assert!(
        !out.contains("redeliveries on the trigger consumer"),
        "got:\n{out}"
    );
}

/// A consumer that stopped on a v2 event, with `malformed_acked`
/// malformed messages acked before it did.
fn halted(name: &str, malformed_acked: u64) -> fq_ops::health::ConsumerHealth {
    fq_ops::health::ConsumerHealth::Halted {
        name: name.to_string(),
        halted_on: fq_ops::health::UnsupportedEvent {
            schema_version: 2,
            supported: vec![3],
            event_id: Some("01990000-0000-7000-8000-000000000002".to_string()),
            subject: "fq.agent.researcher.triggered".to_string(),
            stream_seq: Some(4242),
        },
        malformed_acked,
    }
}

/// A consumer halted on a version this build cannot read is named, the
/// version found and the versions this build reads are both quoted,
/// the event is located, and the report is an issue: the loss class
/// the silent-ack path used to hide, made something an operator finds
/// on the first screen.
#[test]
fn a_halted_consumer_is_named_with_the_version_found_and_the_versions_read() {
    let report = build_doctor_report(
        &[],
        &ExecutionsView::default(),
        THRESHOLD_MS,
        0,
        &[],
        vec![
            active("fq-coordination", false, 0),
            halted("fq-projector", 0),
        ],
        Vec::new(),
    );
    assert!(report.has_issues(), "a halted consumer is an issue");
    let out = render_doctor_report_human(&report);
    assert!(out.contains("Verdict: issues found"), "got:\n{out}");
    assert!(
        out.contains("Consumers: 2 checked, 1 unhealthy"),
        "got:\n{out}"
    );
    assert!(out.contains("fq-projector: ✗ halted"), "got:\n{out}");
    assert!(
        out.contains("declares schema_version 2"),
        "the version found: {out}"
    );
    assert!(
        out.contains("this build reads [3]"),
        "the versions this build reads: {out}"
    );
    assert!(
        out.contains("01990000-0000-7000-8000-000000000002"),
        "the event id: {out}"
    );
    assert!(
        out.contains("fq.agent.researcher.triggered"),
        "the subject: {out}"
    );
    assert!(out.contains("(seq 4242)"), "the stream position: {out}");
    assert!(
        out.contains("consumer=fq-projector"),
        "the next step has to be greppable: {out}"
    );
    assert!(
        !out.contains("malformed"),
        "nothing malformed was acked, so nothing says so: {out}"
    );
}

/// "Unparseable" and "a version this build cannot read" are two facts,
/// and the report keeps them apart: the malformed count rides its
/// consumer's line, halted or not, and on its own makes nothing an
/// issue — poison acked and counted is the policy working.
#[test]
fn malformed_acks_are_counted_beside_the_halt_and_are_not_an_issue_alone() {
    let mut counted = active("fq-coordination", false, 0);
    if let fq_ops::health::ConsumerHealth::Active {
        malformed_acked, ..
    } = &mut counted
    {
        *malformed_acked = 3;
    }
    let report = build_doctor_report(
        &[],
        &ExecutionsView::default(),
        THRESHOLD_MS,
        0,
        &[],
        vec![counted],
        Vec::new(),
    );
    assert!(
        !report.has_issues(),
        "poison acked and counted is not a fault"
    );
    let out = render_doctor_report_human(&report);
    assert!(
        out.contains("fq-coordination: ok (pending 0, 3 malformed acked)"),
        "got:\n{out}"
    );

    let report = build_doctor_report(
        &[],
        &ExecutionsView::default(),
        THRESHOLD_MS,
        0,
        &[],
        vec![halted("fq-projector", 2)],
        Vec::new(),
    );
    let out = render_doctor_report_human(&report);
    assert!(out.contains("✗ halted"), "got:\n{out}");
    assert!(
        out.contains("2 malformed acked"),
        "the count survives the halt, on the same line: {out}"
    );
}

/// A redelivery refused by the durable claim (#809) starts nothing and
/// mints no invocation id, so this line is the only trace it leaves.
/// Like the malformed count it rides the consumer's line and is not on
/// its own a fault: dropping a duplicate is the guard working, and the
/// #327 incident is what happens when nothing counts them.
#[test]
fn duplicate_deliveries_dropped_are_counted_beside_the_consumer() {
    let mut counted = active("fq-dispatcher", false, 0);
    if let fq_ops::health::ConsumerHealth::Active {
        duplicate_dropped, ..
    } = &mut counted
    {
        *duplicate_dropped = 5;
    }
    let report = build_doctor_report(
        &[],
        &ExecutionsView::default(),
        THRESHOLD_MS,
        0,
        &[],
        vec![counted],
        Vec::new(),
    );
    assert!(
        !report.has_issues(),
        "a duplicate refused before it could start anything is not a fault"
    );
    let out = render_doctor_report_human(&report);
    assert!(
        out.contains(
            "fq-dispatcher: ok (pending 0, duplicates dropped since this daemon started: \
             5 after their run started)\n"
        ),
        "got:\n{out}"
    );
    assert!(
        !out.contains("while held"),
        "copies of a started run are not copies dropped while held:\n{out}"
    );
}

/// The two drops are told apart on the consumer's line (#968): a copy
/// of a run that has started, and a copy that met a claim this daemon
/// holds with no start recorded. The second gets a line saying what it
/// can and cannot tell, because nothing on the claim says whether the
/// first copy is still coming. Neither is a fault, and the line under
/// the held count names no verdict: it says what is known and where
/// the daemon logged each one.
#[test]
fn held_duplicates_are_counted_apart_and_explained_without_a_verdict() {
    let mut counted = active("fq-dispatcher", false, 0);
    if let fq_ops::health::ConsumerHealth::Active {
        duplicate_dropped,
        held_duplicate_dropped,
        ..
    } = &mut counted
    {
        *duplicate_dropped = 5;
        *held_duplicate_dropped = 2;
    }
    let report = build_doctor_report(
        &[],
        &ExecutionsView::default(),
        THRESHOLD_MS,
        0,
        &[],
        vec![counted],
        Vec::new(),
    );
    assert!(
        !report.has_issues(),
        "a refused copy, held or started, is not a fault"
    );
    let out = render_doctor_report_human(&report);
    assert!(
        out.contains(
            "  fq-dispatcher: ok (pending 0, duplicates dropped since this daemon started: \
             5 after their run started, 2 while held)\n  -> \"while held\" is a copy that met \
             a claim this daemon holds with no start recorded, left unacked: its first copy was \
             still waiting or starting, or this daemon will never settle that claim, and the \
             claim does not say which; the daemon log has a warn line for each with its \
             sequence, attempt and the claim's age\n"
        ),
        "got:\n{out}"
    );
    assert!(
        out.contains("Consumers: 1 checked, 0 unhealthy"),
        "got:\n{out}"
    );
}

/// Nothing dropped, nothing said: a line that always reported zero
/// would be noise on every healthy consumer.
#[test]
fn a_consumer_that_dropped_no_duplicates_says_nothing_about_them() {
    let report = build_doctor_report(
        &[],
        &ExecutionsView::default(),
        THRESHOLD_MS,
        0,
        &[],
        vec![active("fq-dispatcher", false, 0)],
        Vec::new(),
    );
    let out = render_doctor_report_human(&report);
    assert!(!out.contains("duplicate"), "got:\n{out}");
    assert!(!out.contains("while held"), "got:\n{out}");
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

/// A backlog alone is not a fault. A consumer catching up after a
/// restart is working, and a health report that called it broken would
/// train an operator to ignore the line.
#[test]
fn a_lagging_but_progressing_consumer_is_not_an_issue() {
    let behind = fq_ops::health::ConsumerHealth::Active {
        name: "fq-projector".to_string(),
        delivered: 10,
        ack_pending: 1,
        num_pending: 9_000,
        num_redelivered: 0,
        redeliveries: 0,
        stuck: false,
        malformed_acked: 0,
        duplicate_dropped: 0,
        held_duplicate_dropped: 0,
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
    assert!(
        out.contains("fq-projector: ok (pending 9000)"),
        "got:\n{out}"
    );
}

/// The per-agent cap (#718): an agent at its `max_concurrent` is
/// listed with the invocations against the cap and the triggers waiting
/// on them, and is **not** an issue — a full agent is the runtime
/// honouring the definition. A fleet holding nothing says "none" so the
/// block is always on the page.
#[test]
fn agents_at_cap_are_listed_without_becoming_an_issue() {
    let quiet = build_doctor_report(
        &[],
        &ExecutionsView::default(),
        THRESHOLD_MS,
        0,
        &[],
        Vec::new(),
        Vec::new(),
    );
    let out = render_doctor_report_human(&quiet);
    assert!(out.contains("Agents at cap: none\n"), "got:\n{out}");

    let report = quiet.with_agents_at_cap(vec![fq_ops::health::AgentAtCap {
        agent: "m0-issue-fix".to_string(),
        in_flight: 2,
        cap: 2,
        held: 1,
    }]);
    assert!(!report.has_issues(), "an agent at its cap is not an issue");
    let out = render_doctor_report_human(&report);
    assert!(out.contains("Verdict: All clear."), "got:\n{out}");
    assert!(
        out.contains("Agents at cap: 1 (1 trigger(s) held)"),
        "got:\n{out}"
    );
    assert!(
        out.contains("  m0-issue-fix: at cap; 2/2 in flight, 1 trigger(s) held"),
        "the line names the agent, its state, the arithmetic and what is waiting: {out}"
    );
    assert!(
        out.contains("still its first delivery"),
        "the next-step line says a held trigger has not been redelivered: {out}"
    );
}

/// The window between an invocation ending and the held trigger's next
/// poll: the agent is listed because something is still waiting, but it
/// is *not* at its cap, and neither the header count nor the line may
/// say it is. Before `is_at_cap()` was asked rather than re-derived, this
/// rendered as "Agents at cap: 1" over "0/1 in flight".
#[test]
fn an_agent_with_a_freed_slot_is_not_counted_as_at_cap() {
    let quiet = build_doctor_report(
        &[],
        &ExecutionsView::default(),
        THRESHOLD_MS,
        0,
        &[],
        Vec::new(),
        Vec::new(),
    );
    let report = quiet.with_agents_at_cap(vec![fq_ops::health::AgentAtCap {
        agent: "m0-issue-fix".to_string(),
        in_flight: 0,
        cap: 1,
        held: 1,
    }]);
    let out = render_doctor_report_human(&report);
    assert!(
        out.contains("Agents at cap: 0 (1 trigger(s) held)"),
        "no agent is at its cap, and one trigger is waiting: {out}"
    );
    assert!(
        out.contains("  m0-issue-fix: a slot has freed; starting; 0/1 in flight"),
        "the line says what is actually true of it: {out}"
    );
    assert!(!report.has_issues(), "still not an issue");
}

/// The provider throttle (#278): a throttled model is listed with its
/// pause and permits, points at the deferral events, and is **not** an
/// issue — the runtime absorbing a provider's backpressure is the
/// runtime working. A quiet fleet says "none" so the block is always
/// on the page.
#[test]
fn throttled_models_are_listed_without_becoming_an_issue() {
    let quiet = build_doctor_report(
        &[],
        &ExecutionsView::default(),
        THRESHOLD_MS,
        0,
        &[],
        Vec::new(),
        Vec::new(),
    );
    let out = render_doctor_report_human(&quiet);
    assert!(out.contains("Throttled models: none\n"), "got:\n{out}");

    let report = quiet.with_throttled_models(vec![
        fq_ops::health::ThrottledModel {
            model: "moonshotai/kimi-k3".to_string(),
            paused_until_ms: Some(chrono::Utc::now().timestamp_millis() + 45_000),
            cap: 2,
            ceiling: 4,
            in_flight: 1,
            rate_limited_in_window: 3,
            waves: 1,
        },
        fq_ops::health::ThrottledModel {
            model: "claude-haiku".to_string(),
            paused_until_ms: None,
            cap: 1,
            ceiling: 4,
            in_flight: 0,
            rate_limited_in_window: 0,
            waves: 0,
        },
    ]);
    assert!(!report.has_issues(), "a throttled model is not an issue");
    let out = render_doctor_report_human(&report);
    assert!(out.contains("Verdict: All clear."), "got:\n{out}");
    assert!(
        out.contains("Throttled models: 2 (1 paused)"),
        "got:\n{out}"
    );
    assert!(
        out.contains("  moonshotai/kimi-k3: paused, resumes in 4"),
        "a countdown, like the MCP retry line: {out}"
    );
    assert!(
        out.contains("2 of 4 permits, 1 in flight; 3 rate-limited this window (wave 1)"),
        "got:\n{out}"
    );
    assert!(
        out.contains("  claude-haiku: not paused; 1 of 4 permits"),
        "got:\n{out}"
    );
    assert!(
        out.contains("fq events query --event-type invocation_deferred"),
        "the next step names the deferral events: {out}"
    );
}

/// One stranded claim, claimed at 2026-01-02T03:04:05Z plus `seq`
/// minutes, on the current stream unless the message says otherwise.
fn stranded(
    seq: u64,
    holder: fq_ops::health::ClaimHolder,
    message: fq_ops::health::ClaimedMessage,
    finding: fq_ops::health::StrandedClaimFinding,
    invocation_status: Option<&str>,
) -> fq_ops::health::StrandedTriggerClaim {
    let claimant = match holder {
        fq_ops::health::ClaimHolder::ThisDaemon => "019a0000-0000-7000-8000-00000000d0d0",
        fq_ops::health::ClaimHolder::ExitedProcess => "019a0000-0000-7000-8000-00000000dead",
    };
    let stream_epoch = match message {
        // 2025-12-29T09:20:00Z.
        fq_ops::health::ClaimedMessage::EarlierIncarnation => "1767000000000000000",
        _ => "1767323045000000000",
    };
    fq_ops::health::StrandedTriggerClaim {
        delivery: fq_ops::views::TriggerDeliveryView {
            stream: "fq-triggers".to_string(),
            stream_epoch: stream_epoch.to_string(),
            stream_seq: seq,
            state: fq_ops::views::TriggerClaimState::Claimed,
            claimant: claimant.to_string(),
            claimed_at_ms: 1_767_323_045_000 + seq as i64 * 60_000,
        },
        invocation_id: (finding != fq_ops::health::StrandedClaimFinding::StartUnknown)
            .then(|| format!("1c000000-0000-7000-8000-0000000000{seq:02}")),
        holder,
        message,
        finding,
        invocation_status: invocation_status.map(str::to_string),
    }
}

fn with_stranded_horizon(
    claims: Vec<fq_ops::health::StrandedTriggerClaim>,
    horizon_ms: Option<i64>,
) -> DoctorReport {
    build_doctor_report(
        &[],
        &ExecutionsView::default(),
        THRESHOLD_MS,
        0,
        &[],
        Vec::new(),
        Vec::new(),
    )
    .with_trigger_claims(fq_ops::surface::DoctorTriggerClaims {
        redelivery_window_ms: 1_170_000,
        stream_max_age_ms: 86_400_000,
        horizon_ms,
        stranded: claims,
    })
}

fn with_stranded(claims: Vec<fq_ops::health::StrandedTriggerClaim>) -> DoctorReport {
    with_stranded_horizon(claims, Some(30 * 86_400_000))
}

/// The trigger-claim block (#954), whole: one line per stranded claim
/// naming the delivery, who claimed it and when, and the invocation id it
/// names — labelled as reserved where no run is known to exist — and
/// under it what the finding means. Every finding, and every reason a
/// message is gone, asserted byte for byte like the daemon's doctor
/// goldens, because each line is a claim the report has to be able to
/// stand behind.
#[test]
fn stranded_trigger_claims_render_each_finding_in_its_own_words() {
    use fq_ops::health::ClaimHolder::{ExitedProcess, ThisDaemon};
    use fq_ops::health::ClaimedMessage::{AgedOut, Deliverable, EarlierIncarnation};
    use fq_ops::health::StrandedClaimFinding::{
        AwaitingRedelivery, NeverStarted, StartUnknown, StartedUnmarked,
    };

    let report = with_stranded(vec![
        stranded(1, ExitedProcess, Deliverable, AwaitingRedelivery, None),
        stranded(2, ExitedProcess, Deliverable, NeverStarted, None),
        stranded(3, ExitedProcess, EarlierIncarnation, NeverStarted, None),
        stranded(4, ExitedProcess, AgedOut, NeverStarted, None),
        stranded(
            5,
            ExitedProcess,
            Deliverable,
            StartedUnmarked,
            Some("completed"),
        ),
        stranded(
            6,
            ThisDaemon,
            Deliverable,
            StartedUnmarked,
            Some("in_flight"),
        ),
        stranded(7, ExitedProcess, Deliverable, StartUnknown, None),
    ]);
    assert!(report.has_issues());
    let out = render_doctor_report_human(&report);
    let block = "\
Stranded trigger claims: 7 (5 issues) — claimed trigger deliveries that have not settled
  fq-triggers seq 1: claimed 2026-01-02T03:05:05Z by 019a0000-0000-7000-8000-00000000dead, a process that has exited; reserved invocation id 1c000000-0000-7000-8000-000000000001
  -> not started; if its message is still deliverable, this daemon adopts it and starts a run when the broker redelivers it, which is due within 19m30s of this daemon starting
  fq-triggers seq 2: claimed 2026-01-02T03:06:05Z by 019a0000-0000-7000-8000-00000000dead, a process that has exited; reserved invocation id 1c000000-0000-7000-8000-000000000002
  -> this delivery never started a run and will not be redelivered: it has not been redelivered within 19m30s of this daemon starting, so its last delivery was most likely dead-lettered (`fq dead-letters list`)
  -> work requeued or published again runs under a new sequence this claim cannot show, so this line stays until the claim is 30d old
  fq-triggers seq 3 (an earlier incarnation of the stream, created 2025-12-29T09:20:00Z): claimed 2026-01-02T03:07:05Z by 019a0000-0000-7000-8000-00000000dead, a process that has exited; reserved invocation id 1c000000-0000-7000-8000-000000000003
  -> this delivery never started a run and will not be redelivered: its message belonged to an earlier incarnation of the stream
  -> work requeued or published again runs under a new sequence this claim cannot show, so this line stays until the claim is 30d old
  fq-triggers seq 4: claimed 2026-01-02T03:08:05Z by 019a0000-0000-7000-8000-00000000dead, a process that has exited; reserved invocation id 1c000000-0000-7000-8000-000000000004
  -> this delivery never started a run and will not be redelivered: its message is older than the stream keeps messages (1d)
  -> work requeued or published again runs under a new sequence this claim cannot show, so this line stays until the claim is 30d old
  fq-triggers seq 5: claimed 2026-01-02T03:09:05Z by 019a0000-0000-7000-8000-00000000dead, a process that has exited; invocation 1c000000-0000-7000-8000-000000000005
  -> the run started (completed) but its claim was never marked started: a redelivery would be adopted and start the work again, and a false `trigger_exhausted` dead letter may already exist — `fq invocation show 1c000000-0000-7000-8000-000000000005`
  fq-triggers seq 6: claimed 2026-01-02T03:10:05Z by this daemon; invocation 1c000000-0000-7000-8000-000000000006
  -> the run started (in_flight) but its claim was never marked started: this daemon drops its redeliveries, and the trigger will be dead-lettered as exhausted although its run started; do not requeue that dead letter — `fq invocation show 1c000000-0000-7000-8000-000000000006`
  fq-triggers seq 7: claimed 2026-01-02T03:11:05Z by 019a0000-0000-7000-8000-00000000dead, a process that has exited; no invocation id on the claim
  -> the claim was written before claims named their run, so this report cannot tell whether it started one; not an issue
";
    assert!(out.contains(block), "got:\n{out}");
    assert!(
        out.find("Stranded trigger claims").unwrap() < out.find("Dead-letters").unwrap(),
        "the block sits above the dead letters it points at:\n{out}"
    );
}

/// With retention off there is no horizon, and the line says it stays
/// while the row does; a started run whose status the report did not
/// learn is still named as started, without an empty status.
#[test]
fn stranded_trigger_claims_without_a_horizon_or_a_status() {
    use fq_ops::health::ClaimHolder::ExitedProcess;
    use fq_ops::health::ClaimedMessage::Deliverable;
    use fq_ops::health::StrandedClaimFinding::{NeverStarted, StartedUnmarked};

    let report = with_stranded_horizon(
        vec![
            stranded(1, ExitedProcess, Deliverable, NeverStarted, None),
            stranded(2, ExitedProcess, Deliverable, StartedUnmarked, None),
        ],
        None,
    );
    let out = render_doctor_report_human(&report);
    assert!(
        out.contains("so this line stays while the claim row exists\n"),
        "got:\n{out}"
    );
    assert!(
        out.contains("  -> the run started but its claim was never marked started"),
        "got:\n{out}"
    );
}

/// A healthy table says so; a claim still inside its redelivery window,
/// and one the report cannot judge, are listed without making the report
/// one with issues. A claim this daemon holds for a run that has not
/// started never reaches the report at all — that is the daemon's
/// judgement, pinned in `fq_runtime::views` — so there is nothing for the
/// renderer to quiet.
#[test]
fn claims_that_are_not_issues_are_listed_without_becoming_one() {
    use fq_ops::health::ClaimHolder::ExitedProcess;
    use fq_ops::health::ClaimedMessage::Deliverable;
    use fq_ops::health::StrandedClaimFinding::{AwaitingRedelivery, StartUnknown};

    let quiet = with_stranded(Vec::new());
    assert!(!quiet.has_issues());
    let out = render_doctor_report_human(&quiet);
    assert!(out.contains("Verdict: All clear."), "got:\n{out}");
    assert!(
        out.contains("Stranded trigger claims: none\n"),
        "got:\n{out}"
    );

    let listed = with_stranded(vec![
        stranded(8, ExitedProcess, Deliverable, AwaitingRedelivery, None),
        stranded(9, ExitedProcess, Deliverable, StartUnknown, None),
    ]);
    assert!(!listed.has_issues(), "neither is an issue");
    let out = render_doctor_report_human(&listed);
    assert!(out.contains("Verdict: All clear."), "got:\n{out}");
    assert!(
        out.contains("Stranded trigger claims: 2 (0 issues)"),
        "got:\n{out}"
    );
}

#[test]
fn spans_print_in_the_largest_unit_that_states_them_exactly() {
    use super::trigger_claims::format_span;
    assert_eq!(format_span(1_170_000), "19m30s");
    assert_eq!(format_span(1_200_000), "20m");
    assert_eq!(format_span(86_400_000), "1d");
    assert_eq!(format_span(30 * 86_400_000), "30d");
    assert_eq!(format_span(7_200_000), "2h");
}
