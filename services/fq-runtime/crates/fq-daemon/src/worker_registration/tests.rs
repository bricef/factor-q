//! The guard fires, and it fires once.
//!
//! One test per error path would be a test per `?` in `run_daemon`,
//! renewed every time someone adds a step — which is the shape the
//! structural guard exists to avoid. These prove the guard's contract
//! instead: an unaccounted-for exit deregisters the worker, a teardown
//! that already spoke is not overruled, and settling twice cannot
//! double-write. The end-to-end proof that a real post-registration
//! failure takes this path is in `tests/instance_lock.rs`.

use super::*;

async fn store() -> (tempfile::TempDir, Arc<ControlPlaneStore>) {
    let dir = tempfile::tempdir().unwrap();
    let store = ControlPlaneStore::open(&dir.path().join("control-plane.db"))
        .await
        .expect("open control-plane store");
    (dir, Arc::new(store))
}

async fn status_of(store: &ControlPlaneStore, worker_id: &str) -> String {
    store
        .list_workers()
        .await
        .expect("list workers")
        .into_iter()
        .find(|w| w.worker_id == worker_id)
        .unwrap_or_else(|| panic!("no worker row for {worker_id}"))
        .status
        .as_str()
        .to_string()
}

fn worker_id(name: &str) -> WorkerId {
    WorkerId::new(name.to_string()).expect("valid worker id")
}

/// The path B5 names: a `?` between registration and the teardown.
/// Nothing else accounts for the row, so the guard does.
#[tokio::test]
async fn an_exit_that_reached_no_teardown_deregisters_the_worker() {
    let (_dir, store) = store().await;
    let id = worker_id("guard-unclaimed");
    let registration = WorkerRegistration::register(store.clone(), id.clone(), "host", 1_000)
        .await
        .expect("register");
    assert_eq!(status_of(&store, id.as_str()).await, "alive");

    registration.settle_unclaimed().await;

    assert_eq!(
        status_of(&store, id.as_str()).await,
        "shutdown",
        "a post-registration failure must leave the row `shutdown`, not `alive` to \
         age into `stale`"
    );
}

/// The teardown speaks first and the guard defers: a clean stop is
/// already recorded, and the catch-all must not rewrite it.
#[tokio::test]
async fn a_teardown_that_settled_cleanly_is_not_overruled() {
    let (_dir, store) = store().await;
    let id = worker_id("guard-clean");
    let registration = WorkerRegistration::register(store.clone(), id.clone(), "host", 1_000)
        .await
        .expect("register");

    registration.settle(true).await;
    registration.settle_unclaimed().await;

    assert_eq!(status_of(&store, id.as_str()).await, "shutdown");
}

/// A hosted task died and took the runtime with it. That is exactly
/// the case the stale sweep exists to report, so the row is left
/// alone — and the catch-all must not undo that decision by marking it
/// `shutdown` behind the teardown's back.
#[tokio::test]
async fn an_unclean_teardown_leaves_the_row_for_the_stale_sweep() {
    let (_dir, store) = store().await;
    let id = worker_id("guard-unclean");
    let registration = WorkerRegistration::register(store.clone(), id.clone(), "host", 1_000)
        .await
        .expect("register");

    registration.settle(false).await;
    registration.settle_unclaimed().await;

    assert_eq!(
        status_of(&store, id.as_str()).await,
        "alive",
        "a crash-shaped exit must stay visible to the stale sweep"
    );
}

/// The teardown's copy and `run_daemon`'s copy are the same
/// registration: whichever settles first wins, and the other is inert.
#[tokio::test]
async fn a_clone_settles_the_same_registration() {
    let (_dir, store) = store().await;
    let id = worker_id("guard-clone");
    let registration = WorkerRegistration::register(store.clone(), id.clone(), "host", 1_000)
        .await
        .expect("register");
    let held_by_the_teardown = registration.clone();

    held_by_the_teardown.settle(false).await;
    registration.settle_unclaimed().await;

    assert_eq!(status_of(&store, id.as_str()).await, "alive");
}
