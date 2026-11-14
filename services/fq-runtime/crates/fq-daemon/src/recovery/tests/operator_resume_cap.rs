//! An operator's resume and its agent's `max_concurrent` (issue 1009).
//!
//! A trigger takes its agent's slot through the gated entry and waits while
//! the agent is full. `fq invocation resume` counts its run through the
//! ungated one, so it is not held at the cap: an ambiguous invocation holds
//! no slot while it waits for triage, and resuming it can run it beside the
//! runs the agent has admitted since. That was decided rather than fixed —
//! the operator asked for the run by id, and holding it would need a
//! priority rule among the waiters for a slot — and this pins it, through
//! the daemon's own operator surface, so the guide and the CLI are not the
//! only places that say it.

use super::*;

/// **Issue 1009: an operator's resume waits for a worker permit, not for
/// its agent's slot.** Two worker permits, and [`CAPPED_AGENT`]
/// (`max_concurrent: 1`) already running one invocation, admitted the way
/// the dispatcher admits a trigger: its slot taken through the gated entry,
/// `try_enter`, with a worker permit beside it. Another agent's run holds
/// the second permit.
///
/// The resume is accepted and counted against the agent at once, two
/// against a cap of one, and waits while every worker permit is held. When
/// the other agent's run gives its permit back, the resume starts on it
/// while the run holding the agent's only slot is still running: the
/// agent's cap did not hold it.
#[tokio::test]
async fn an_operator_resume_waits_for_a_worker_permit_but_not_for_its_agents_slot() {
    let start = NextStart::over(Seed::Ambiguous).await;
    let admission = RunAdmission::new(start.runner.clone(), 2);
    let surface = start.operator_surface(&admission).await;
    let agent = capped_agent();

    // The agent's run, admitted as a trigger is.
    let agents_run_slot = admission
        .agents()
        .try_enter(&agent, Some(1))
        .expect("the agent's only slot is free");
    let agents_run_permit = admission
        .workers()
        .queue(Waiter::Trigger(None))
        .await
        .permit()
        .await
        .expect("a worker permit is free");
    assert!(
        admission.agents().try_enter(&agent, Some(1)).is_none(),
        "the agent is at its cap: a trigger for it would be held now"
    );
    // Another agent's run, on the other permit.
    let other_run_permit = admission
        .workers()
        .queue(Waiter::Trigger(None))
        .await
        .permit()
        .await
        .expect("the second worker permit is free");

    let resumed = invoke(
        &surface,
        fq_ops::Invocation::Resume,
        serde_json::json!({ "invocation_id": start.invocation.to_string(), "reason": "test" }),
    )
    .await;
    assert!(resumed.is_ok(), "the resume is accepted: {resumed:?}");
    assert_eq!(
        admission.agents().in_flight(&agent),
        2,
        "the re-drive counts against its agent at once, past `max_concurrent: 1`"
    );

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        start.model.calls(),
        0,
        "every worker permit is held, so the re-drive waits for one"
    );
    assert!(!start.runner.is_active(&start.invocation));

    drop(other_run_permit);
    start.model.entered().await;
    assert!(
        start.runner.is_active(&start.invocation),
        "the re-drive runs on the freed worker permit"
    );
    assert_eq!(
        admission.agents().in_flight(&agent),
        2,
        "beside the run holding its agent's only slot: two of a `max_concurrent: 1` agent"
    );
    assert_eq!(
        admission.workers().in_flight(),
        2,
        "and within the worker cap, which is the bound it waited for"
    );

    start.model.release();
    await_in_flight(&admission, 1).await;
    assert!(!start.runner.is_active(&start.invocation));
    drop((agents_run_slot, agents_run_permit));
    assert_eq!(admission.agents().in_flight(&agent), 0);
}
