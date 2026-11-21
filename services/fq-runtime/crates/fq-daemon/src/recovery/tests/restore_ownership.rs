//! The restore of an ownership row before a resume, and a row this build
//! cannot read, at the next start (issue 1024).
//!
//! `restore_ownership` has two answers that decide whether a run resumes:
//! a restore refused on a row that records the run finished does not
//! resume it, and a row this build cannot read is left as it is and the
//! run resumed, since the WAL decides that. The first two tests drive
//! `classify_in_flight` alone, so each answer is reached through the
//! restore itself and not through `reconcile_terminal_owners` before it.
//! The third runs the two in the order the boot does, over a row this
//! build cannot read: the boot goes on.

use super::*;
use fq_runtime::control_plane::{ControlPlaneStoreError, OwnerStatus};

/// A run whose WAL row is in flight, under [`CAPPED_AGENT`].
async fn in_flight_run(worker_store: &WorkerStore) -> String {
    let invocation = Uuid::now_v7().to_string();
    let mut row = first_wal_row(&invocation);
    row.agent_id = CAPPED_AGENT.to_string();
    worker_store.upsert_invocation_state(&row).await.unwrap();
    invocation
}

/// What recovery hands the boot for one start of a daemon over `cp_store`
/// and `worker_store`.
async fn next_start(
    bus: &EventBus,
    cp_store: &Arc<ControlPlaneStore>,
    worker_store: &Arc<WorkerStore>,
) -> Recovered {
    let runtime_id = Uuid::now_v7();
    let worker_id = WorkerId::new(runtime_id.to_string()).unwrap();
    classify_in_flight(worker_store, cp_store, bus, runtime_id, &worker_id)
        .await
        .expect("startup recovery")
}

/// Whether `invocation`'s ownership row is one this build cannot read.
async fn unreadable(cp_store: &ControlPlaneStore, invocation: &str) -> bool {
    matches!(
        cp_store.get_invocation_owner(invocation).await,
        Err(ControlPlaneStoreError::Malformed(_))
    )
}

#[tokio::test]
async fn a_restore_refused_on_a_finished_row_does_not_resume_the_run() {
    let server = fq_test_support::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let scratch = tempfile::tempdir().unwrap();
    let (cp_store, worker_store) = stores_under(scratch.path()).await;
    let invocation = in_flight_run(&worker_store).await;
    cp_store
        .upsert_invocation_ownership(&invocation, "the-last-daemon", 2_000, OwnerStatus::Failed)
        .await
        .unwrap();

    let recovered = next_start(&bus, &cp_store, &worker_store).await;

    assert!(
        recovered.recoverable.is_empty(),
        "the row records the run finished, so the restore is refused and the run not resumed"
    );
    let row = cp_store
        .get_invocation_owner(&invocation)
        .await
        .unwrap()
        .expect("the row stays");
    assert_eq!(
        (row.status, row.worker_id.as_str()),
        (OwnerStatus::Failed, "the-last-daemon"),
        "the refused restore wrote nothing"
    );
}

/// **A run whose row its durable start wrote, left running by a daemon
/// that stopped, is re-owned by the next start and resumed (issue 721).**
/// The row reads `in_flight` under the stopped daemon's worker. Recovery
/// restores it under its own worker, from the run's start as the WAL
/// records it, and resumes the run.
#[tokio::test]
async fn a_run_whose_start_wrote_its_row_is_re_owned_and_resumed() {
    use fq_runtime::control_plane::ownership::{OwnerFacts, OwnershipEvent, apply};
    let server = fq_test_support::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let scratch = tempfile::tempdir().unwrap();
    let (cp_store, worker_store) = stores_under(scratch.path()).await;
    let invocation = in_flight_run(&worker_store).await;
    apply(
        cp_store.as_ref(),
        &invocation,
        OwnershipEvent::DurablyStarted,
        OwnerFacts {
            worker_id: "the-stopped-daemon",
            at_ms: 1_000,
            ended_at_ms: 1_000,
            request: None,
        },
    )
    .await
    .expect("the durable start writes the row");

    let recovered = next_start(&bus, &cp_store, &worker_store).await;

    assert_eq!(recovered.recoverable.len(), 1, "the run is resumed");
    let row = cp_store
        .get_invocation_owner(&invocation)
        .await
        .unwrap()
        .expect("the row stays");
    assert_eq!(row.status, OwnerStatus::InFlight);
    assert_ne!(
        row.worker_id, "the-stopped-daemon",
        "the next start's recovery owns the run now"
    );
    assert_eq!(row.assigned_at, 1_000, "from the run's own start");
}

#[tokio::test]
async fn a_row_this_build_cannot_read_is_left_as_it_is_and_its_run_resumed() {
    let server = fq_test_support::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let scratch = tempfile::tempdir().unwrap();
    let (cp_store, worker_store) = stores_under(scratch.path()).await;
    let invocation = in_flight_run(&worker_store).await;
    cp_store
        .upsert_invocation_ownership(&invocation, "the-last-daemon", 1_000, OwnerStatus::InFlight)
        .await
        .unwrap();
    assert!(
        cp_store
            .overwrite_owner_status_for_test(&invocation, "weird")
            .await
            .unwrap()
    );

    let recovered = next_start(&bus, &cp_store, &worker_store).await;

    assert_eq!(
        recovered
            .recoverable
            .iter()
            .map(|run| run.state.invocation_id.as_str())
            .collect::<Vec<_>>(),
        [invocation.as_str()],
        "the WAL, not the row, says the run can be resumed"
    );
    assert!(
        unreadable(&cp_store, &invocation).await,
        "the row is left as it is"
    );
}

#[tokio::test]
async fn the_boot_goes_on_past_a_row_this_build_cannot_read() {
    let server = fq_test_support::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let scratch = tempfile::tempdir().unwrap();
    let (cp_store, worker_store) = stores_under(scratch.path()).await;
    let invocation = in_flight_run(&worker_store).await;
    cp_store
        .upsert_invocation_ownership(&invocation, "the-last-daemon", 1_000, OwnerStatus::InFlight)
        .await
        .unwrap();
    assert!(
        cp_store
            .overwrite_owner_status_for_test(&invocation, "weird")
            .await
            .unwrap()
    );

    // The boot's order: reconcile, then classify.
    reconcile_terminal_owners(
        &worker_store,
        &cp_store,
        chrono::Utc::now().timestamp_millis(),
    )
    .await
    .expect("reconciliation logs the row and goes on");
    let recovered = next_start(&bus, &cp_store, &worker_store).await;

    assert_eq!(recovered.recoverable.len(), 1, "the run is resumed");
    let wal = worker_store
        .get_invocation_state(&invocation)
        .await
        .unwrap()
        .expect("the WAL row stays");
    assert!(
        wal.terminal_at.is_none(),
        "reconciliation found no ending to apply"
    );
    assert!(
        unreadable(&cp_store, &invocation).await,
        "the row is left as it is"
    );
}
