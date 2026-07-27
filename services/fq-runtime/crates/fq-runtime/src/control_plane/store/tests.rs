//! Unit tests for [`super`]. Extracted from the parent module so the
//! file that ships is the file you read (#390); `super::*` keeps the
//! same access it had inline.

use super::*;
use tempfile::tempdir;

// ----- Unit -----

#[test]
fn is_stale_within_threshold_returns_false() {
    // last_heartbeat 1000ms ago, threshold 5s
    assert!(!is_stale(9_000, 10_000, 5_000));
}

#[test]
fn is_stale_past_threshold_returns_true() {
    // last_heartbeat 6s ago, threshold 5s
    assert!(is_stale(4_000, 10_000, 5_000));
}

#[test]
fn is_stale_handles_zero_and_negative_clock_skew() {
    assert!(!is_stale(10_000, 10_000, 5_000)); // exactly now
    assert!(!is_stale(10_000, 10_000, 0)); // threshold zero, not stale at exactly now
    assert!(!is_stale(11_000, 10_000, 5_000)); // future heartbeat: not stale
}

#[test]
fn is_due_when_fire_at_is_past_or_now() {
    assert!(is_due(99, 100));
    assert!(is_due(100, 100));
    assert!(!is_due(101, 100));
}

#[test]
fn retention_cutoff_subtracts_days() {
    let now = 86_400_000 * 10; // day 10
    assert_eq!(retention_cutoff_ms(now, 7), 86_400_000 * 3); // day 3
    assert_eq!(retention_cutoff_ms(now, 0), now);
}

#[test]
fn check_compatibility_classifies_correctly() {
    assert_eq!(check_compatibility(None, 1), Compatibility::FreshInstall);
    assert_eq!(check_compatibility(Some(1), 1), Compatibility::Current);
    assert_eq!(
        check_compatibility(Some(1), 2),
        Compatibility::NeedsUpgrade { from: 1 }
    );
    assert_eq!(
        check_compatibility(Some(3), 2),
        Compatibility::BinaryTooOld { db_version: 3 }
    );
}

#[test]
fn worker_status_round_trip() {
    for s in [
        WorkerStatus::Alive,
        WorkerStatus::Stale,
        WorkerStatus::Shutdown,
    ] {
        assert_eq!(WorkerStatus::parse(s.as_str()), Some(s));
    }
    assert!(WorkerStatus::parse("garbage").is_none());
}

#[test]
fn owner_status_round_trip() {
    for s in [
        OwnerStatus::InFlight,
        OwnerStatus::Completed,
        OwnerStatus::Failed,
        OwnerStatus::Ambiguous,
    ] {
        assert_eq!(OwnerStatus::parse(s.as_str()), Some(s));
    }
}

// ----- Integration -----

async fn open_fresh() -> (ControlPlaneStore, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    let store = ControlPlaneStore::open(&path).await.expect("open fresh");
    (store, dir)
}

#[tokio::test]
async fn control_plane_ladder_upgrades_populated_database() {
    // v1 is currently the only historical schema, and its DDL is all
    // CREATE TABLE IF NOT EXISTS — so stamping v0 and reopening replays
    // a no-op ladder. This pins the NeedsUpgrade dispatch, the version
    // restamp, and row survival; when v2 adds real DDL, grow this into
    // the per-step harness the worker store has.
    let dir = tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    let store = ControlPlaneStore::open(&path).await.unwrap();
    store
        .register_worker("migration-worker", "sim-host", 1_000)
        .await
        .unwrap();
    store.write_schema_version(0).await.unwrap();
    drop(store);

    let store = ControlPlaneStore::open(&path)
        .await
        .expect("reopen a v0-stamped store");
    let worker = store
        .get_worker("migration-worker")
        .await
        .unwrap()
        .expect("row preserved");
    assert_eq!(worker.host, "sim-host");
    assert_eq!(
        store.read_schema_version().await.unwrap(),
        Some(CONTROL_PLANE_SCHEMA_VERSION)
    );
    let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_eq!(integrity, "ok");

    // Current must not re-run the ladder. Future ALTER migrations are not
    // necessarily idempotent, so this remains an explicit guard.
    drop(store);
    let reopened = ControlPlaneStore::open(&path)
        .await
        .expect("reopen current");
    assert!(
        reopened
            .get_worker("migration-worker")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn open_creates_tables_and_records_version() {
    let (store, _dir) = open_fresh().await;
    assert_eq!(
        store.read_schema_version().await.unwrap(),
        Some(CONTROL_PLANE_SCHEMA_VERSION)
    );

    for table in [
        "coordination_worker",
        "coordination_invocation_owner",
        "pending_wait",
        "schedule_entry",
        "invocation_archive",
    ] {
        let row = sqlx::query("SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?")
            .bind(table)
            .fetch_optional(&store.pool)
            .await
            .unwrap();
        assert!(row.is_some(), "missing table {table}");
    }
}

#[tokio::test]
async fn open_refuses_when_db_version_higher_than_binary() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    let store = ControlPlaneStore::open(&path).await.unwrap();
    store
        .write_schema_version(CONTROL_PLANE_SCHEMA_VERSION + 1)
        .await
        .unwrap();
    drop(store);
    let err = ControlPlaneStore::open(&path)
        .await
        .expect_err("should refuse newer DB");
    assert!(matches!(
        err,
        ControlPlaneStoreError::IncompatibleSchema { .. }
    ));
}

#[tokio::test]
async fn worker_registration_round_trip() {
    let (store, _dir) = open_fresh().await;
    store
        .register_worker("w-001", "prod-1", 1_000)
        .await
        .unwrap();
    let w = store.get_worker("w-001").await.unwrap().unwrap();
    assert_eq!(w.worker_id, "w-001");
    assert_eq!(w.host, "prod-1");
    assert_eq!(w.registered_at, 1_000);
    assert_eq!(w.last_heartbeat, 1_000);
    assert_eq!(w.status, WorkerStatus::Alive);

    // Re-register with a different host: row updated in place.
    store
        .register_worker("w-001", "prod-2", 2_000)
        .await
        .unwrap();
    let w = store.get_worker("w-001").await.unwrap().unwrap();
    assert_eq!(w.host, "prod-2");
    assert_eq!(w.registered_at, 2_000);
}

#[tokio::test]
async fn worker_heartbeat_updates_last_heartbeat() {
    let (store, _dir) = open_fresh().await;
    store.register_worker("w-002", "host", 100).await.unwrap();
    store.heartbeat_worker("w-002", 200).await.unwrap();
    let w = store.get_worker("w-002").await.unwrap().unwrap();
    assert_eq!(w.last_heartbeat, 200);
    store.heartbeat_worker("w-002", 300).await.unwrap();
    let w = store.get_worker("w-002").await.unwrap().unwrap();
    assert_eq!(w.last_heartbeat, 300);
}

#[tokio::test]
async fn worker_marked_stale_after_heartbeat_lapse() {
    let (store, _dir) = open_fresh().await;
    store.register_worker("alive", "h", 10_000).await.unwrap();
    store.register_worker("stale", "h", 1_000).await.unwrap();
    store.register_worker("gone", "h", 500).await.unwrap();
    store.mark_worker_shutdown("gone").await.unwrap();

    let now = 12_000;
    let threshold = 5_000; // 5s
    let stale = store.list_stale_workers(now, threshold).await.unwrap();
    let ids: Vec<_> = stale.iter().map(|w| w.worker_id.as_str()).collect();
    // alive is within threshold, gone is shutdown — both excluded.
    assert_eq!(ids, vec!["stale"]);
}

#[tokio::test]
async fn mark_worker_stale_consumes_transition_exactly_once() {
    let (store, _dir) = open_fresh().await;
    store.register_worker("w-once", "h", 1_000).await.unwrap();

    // The alive→stale flip is claimed by exactly one caller —
    // the once-per-transition guarantee the orphan event
    // publisher (#64) relies on.
    assert!(store.mark_worker_stale("w-once").await.unwrap());
    assert!(!store.mark_worker_stale("w-once").await.unwrap());

    // Unknown workers and shutdown workers are never a transition.
    assert!(!store.mark_worker_stale("w-missing").await.unwrap());
    store.register_worker("w-down", "h", 1_000).await.unwrap();
    store.mark_worker_shutdown("w-down").await.unwrap();
    assert!(!store.mark_worker_stale("w-down").await.unwrap());
}

#[tokio::test]
async fn prune_stale_workers_removes_only_stale_rows_idempotently() {
    let (store, _dir) = open_fresh().await;
    store.register_worker("alive", "h", 1).await.unwrap();
    store.register_worker("stale", "h", 1).await.unwrap();
    store.register_worker("shutdown", "h", 1).await.unwrap();
    store.mark_worker_stale("stale").await.unwrap();
    store.mark_worker_shutdown("shutdown").await.unwrap();

    assert_eq!(store.prune_stale_workers().await.unwrap(), vec!["stale"]);
    assert!(store.get_worker("alive").await.unwrap().is_some());
    assert!(store.get_worker("shutdown").await.unwrap().is_some());
    assert!(store.get_worker("stale").await.unwrap().is_none());
    assert!(store.prune_stale_workers().await.unwrap().is_empty());
}

#[tokio::test]
async fn invocation_ownership_round_trip() {
    let (store, _dir) = open_fresh().await;
    store.register_worker("w-1", "h", 1).await.unwrap();
    store.assign_invocation("inv-A", "w-1", 100).await.unwrap();
    store.assign_invocation("inv-B", "w-1", 200).await.unwrap();

    let owner = store.get_invocation_owner("inv-A").await.unwrap().unwrap();
    assert_eq!(owner.worker_id, "w-1");
    assert_eq!(owner.assigned_at, 100);
    assert_eq!(owner.status, OwnerStatus::InFlight);

    let listed = store.list_invocations_for_worker("w-1").await.unwrap();
    let ids: Vec<_> = listed.iter().map(|o| o.invocation_id.as_str()).collect();
    assert_eq!(ids, vec!["inv-A", "inv-B"]);

    let updated = store
        .update_invocation_status("inv-A", OwnerStatus::Ambiguous)
        .await
        .unwrap();
    assert_eq!(updated, 1);
    let amb = store
        .list_invocations_with_status(OwnerStatus::Ambiguous)
        .await
        .unwrap();
    assert_eq!(amb.len(), 1);
    assert_eq!(amb[0].invocation_id, "inv-A");
}

#[tokio::test]
async fn pending_wait_insert_and_signal() {
    let (store, _dir) = open_fresh().await;
    let w = PendingWaitRow {
        invocation_id: "inv-x".to_string(),
        kind: "approval".to_string(),
        descriptor: r#"{"approver":"alice"}"#.to_string(),
        expires_at: Some(2_000),
        created_at: 1_000,
    };
    store.insert_wait(&w).await.unwrap();

    let back = store.get_wait("inv-x").await.unwrap().unwrap();
    assert_eq!(back, w);

    let n = store.signal_wait("inv-x").await.unwrap();
    assert_eq!(n, 1);
    assert!(store.get_wait("inv-x").await.unwrap().is_none());

    // Signalling a non-existent wait returns 0.
    let n = store.signal_wait("inv-x").await.unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn pending_wait_list_expired() {
    let (store, _dir) = open_fresh().await;
    let mk = |id: &str, expires: Option<i64>| PendingWaitRow {
        invocation_id: id.to_string(),
        kind: "time".to_string(),
        descriptor: "{}".to_string(),
        expires_at: expires,
        created_at: 0,
    };
    store.insert_wait(&mk("expired", Some(50))).await.unwrap();
    store.insert_wait(&mk("future", Some(150))).await.unwrap();
    store.insert_wait(&mk("no-expiry", None)).await.unwrap();

    let now = 100;
    let expired = store.list_expired_waits(now).await.unwrap();
    let ids: Vec<_> = expired.iter().map(|w| w.invocation_id.as_str()).collect();
    assert_eq!(ids, vec!["expired"]);
}

#[tokio::test]
async fn schedule_entry_due_query() {
    let (store, _dir) = open_fresh().await;
    let mk = |id: &str, fire_at: i64| ScheduleEntryRow {
        id: id.to_string(),
        kind: "trigger".to_string(),
        fire_at,
        payload: "{}".to_string(),
    };
    store.insert_schedule(&mk("a", 100)).await.unwrap();
    store.insert_schedule(&mk("b", 200)).await.unwrap();
    store.insert_schedule(&mk("c", 50)).await.unwrap();

    let due = store.list_due_schedules(150).await.unwrap();
    let ids: Vec<_> = due.iter().map(|s| s.id.as_str()).collect();
    // Sorted by fire_at ascending.
    assert_eq!(ids, vec!["c", "a"]);

    let n = store.delete_schedule("a").await.unwrap();
    assert_eq!(n, 1);
    assert!(store.get_schedule("a").await.unwrap().is_none());
}

#[tokio::test]
async fn archive_insert_and_retention_query() {
    let (store, _dir) = open_fresh().await;
    let mk = |id: &str, archived_at: i64| InvocationArchiveRow {
        invocation_id: id.to_string(),
        agent_id: "agent-a".to_string(),
        final_phase: "completed".to_string(),
        final_state_blob: vec![1, 2, 3],
        started_at: 0,
        terminal_at: archived_at - 1,
        archived_at,
    };
    store.insert_archive(&mk("old1", 1_000)).await.unwrap();
    store.insert_archive(&mk("old2", 2_000)).await.unwrap();
    store.insert_archive(&mk("recent", 5_000)).await.unwrap();

    // Cutoff at 3_000 — old1 and old2 should be swept.
    let n = store.sweep_archive(3_000).await.unwrap();
    assert_eq!(n, 2);

    assert!(store.get_archive("old1").await.unwrap().is_none());
    assert!(store.get_archive("old2").await.unwrap().is_none());
    assert!(store.get_archive("recent").await.unwrap().is_some());

    let by_agent = store.list_archive_for_agent("agent-a").await.unwrap();
    assert_eq!(by_agent.len(), 1);
    assert_eq!(by_agent[0].invocation_id, "recent");
}

#[tokio::test]
async fn archive_insert_is_idempotent_on_invocation_id() {
    let (store, _dir) = open_fresh().await;
    let row = InvocationArchiveRow {
        invocation_id: "dup".to_string(),
        agent_id: "a".to_string(),
        final_phase: "completed".to_string(),
        final_state_blob: vec![],
        started_at: 0,
        terminal_at: 1,
        archived_at: 2,
    };
    store.insert_archive(&row).await.unwrap();

    // Second insert for the same invocation_id is a no-op
    // (DO NOTHING). A redelivered `invocation.archived`
    // event must not produce a duplicate row or fail.
    let mut second = row.clone();
    second.archived_at = 999;
    store.insert_archive(&second).await.unwrap();

    // Stored row is the original, not the second.
    let back = store.get_archive("dup").await.unwrap().unwrap();
    assert_eq!(back.archived_at, 2);
}

#[tokio::test]
async fn per_file_bootstrap_isolates_schema_meta() {
    // The split layout (#262): each store bootstraps its own
    // file, and each file's `schema_meta` carries only that
    // store's class row — the version handshake is per-file.
    let dir = tempdir().unwrap();
    let paths = crate::db::RuntimeDbPaths::under(dir.path());

    let cp = ControlPlaneStore::open(&paths.control_plane).await.unwrap();
    let worker = crate::worker::WorkerStore::open(&paths.worker)
        .await
        .unwrap();

    cp.register_worker("w-split", "h", 1).await.unwrap();
    worker
        .write_tool_intent("inv-split", "tc", "echo", "{}", 100)
        .await
        .unwrap();

    assert!(cp.get_worker("w-split").await.unwrap().is_some());
    assert!(
        worker
            .get_tool_dispatch("inv-split", "tc")
            .await
            .unwrap()
            .is_some()
    );

    // Each file records exactly its own schema class.
    for (path, class) in [
        (&paths.control_plane, SCHEMA_CLASS),
        (&paths.worker, crate::worker::store::SCHEMA_CLASS),
    ] {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        let classes: Vec<String> = sqlx::query_scalar("SELECT class FROM schema_meta")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(classes, vec![class.to_string()]);
    }
}
