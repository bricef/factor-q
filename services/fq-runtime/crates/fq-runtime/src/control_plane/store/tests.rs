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
        "trigger_claim",
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

/// #970 on this store's ladder. Its rungs are all `CREATE ... IF NOT
/// EXISTS`, so two unserialised openers never failed here — but the
/// second still read "no version" and ran the ladder over the first's,
/// and the first rung of this kind to be added would have failed the way
/// the worker store's did. The rival opener is the real `open`.
#[tokio::test]
async fn two_openers_of_a_fresh_control_plane_db_serialise() {
    use crate::db::schema::test_seam::RivalAtVersionRead;

    let dir = tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    let rival_path = path.clone();
    let mut seam = RivalAtVersionRead::new(&path, async move {
        ControlPlaneStore::open(&rival_path)
            .await
            .map(drop)
            .map_err(|err| err.to_string())
    });
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = schema::connect_store_pool(options, 4).await.unwrap();

    let first = schema::bootstrap_observed(
        &pool,
        SCHEMA_CLASS,
        CONTROL_PLANE_SCHEMA_VERSION,
        MIGRATIONS,
        &mut seam,
    )
    .await;
    let rival = seam.rival_outcome().await;

    assert_eq!(
        first.expect("the first opener must succeed"),
        Compatibility::FreshInstall
    );
    rival.expect("the second opener must succeed");
    assert_eq!(
        seam.lock_held,
        Some(true),
        "the first opener read the version without holding the write lock"
    );
    let store = ControlPlaneStore::open(&path).await.unwrap();
    assert_eq!(
        store.read_schema_version().await.unwrap(),
        Some(CONTROL_PLANE_SCHEMA_VERSION)
    );
}

#[tokio::test]
async fn open_read_only_refuses_outdated_and_newer_schemas() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    std::fs::File::create(&path).unwrap();
    let err = ControlPlaneStore::open_read_only(&path)
        .await
        .expect_err("unstamped file must be reported as uninitialised");
    assert!(matches!(err, ControlPlaneStoreError::NotInitialised(_)));

    let store = ControlPlaneStore::open(&path).await.unwrap();
    store
        .write_schema_version(CONTROL_PLANE_SCHEMA_VERSION - 1)
        .await
        .unwrap();

    let err = ControlPlaneStore::open_read_only(&path)
        .await
        .expect_err("read-only open must refuse an outdated schema");
    assert!(matches!(
        err,
        ControlPlaneStoreError::SchemaOutdated {
            db_version,
            binary_version,
            ..
        } if db_version == CONTROL_PLANE_SCHEMA_VERSION - 1
            && binary_version == CONTROL_PLANE_SCHEMA_VERSION
    ));

    store
        .write_schema_version(CONTROL_PLANE_SCHEMA_VERSION + 1)
        .await
        .unwrap();
    drop(store);
    let err = ControlPlaneStore::open_read_only(&path)
        .await
        .expect_err("read-only open must refuse a newer schema");
    assert!(matches!(
        err,
        ControlPlaneStoreError::IncompatibleSchema {
            db_version,
            binary_version,
        } if db_version == CONTROL_PLANE_SCHEMA_VERSION + 1
            && binary_version == CONTROL_PLANE_SCHEMA_VERSION
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

/// `delete_worker` is the bare primitive the retention sweep drives: it
/// removes exactly the named row, reports whether one was there, and is a
/// no-op the second time. It carries no policy of its own — the sweep decides
/// *which* ids to hand it, and that decision is tested in
/// `control_plane::retention`.
#[tokio::test]
async fn delete_worker_removes_exactly_one_row_idempotently() {
    let (store, _dir) = open_fresh().await;
    store.register_worker("alive", "h", 1).await.unwrap();
    store.register_worker("stale", "h", 1).await.unwrap();
    store.mark_worker_stale("stale").await.unwrap();

    assert!(store.delete_worker("stale").await.unwrap());
    assert!(store.get_worker("stale").await.unwrap().is_none());
    assert!(
        store.get_worker("alive").await.unwrap().is_some(),
        "delete_worker must not touch any row but the one named"
    );
    // Second delete finds nothing, and says so rather than erroring.
    assert!(!store.delete_worker("stale").await.unwrap());
    assert!(!store.delete_worker("never-existed").await.unwrap());
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

/// One delivery's key on the live trigger stream, at the epoch every
/// claim in this test shares.
fn key(stream_seq: u64) -> TriggerKey<'static> {
    TriggerKey {
        stream: "fq-triggers",
        stream_epoch: 1_700_000_000_000_000_000,
        stream_seq,
    }
}

#[tokio::test]
async fn trigger_claim_state_machine_round_trip() {
    let (store, _dir) = open_fresh().await;
    assert_eq!(
        store
            .claim_trigger(key(42), "worker-a", "inv-a", 100)
            .await
            .unwrap(),
        TriggerClaim::Won
    );
    // A losing claim is told the row as it stands: the holder, the id the
    // holder named and when it claimed, never the loser's own.
    assert_eq!(
        store
            .claim_trigger(key(42), "worker-a", "inv-a2", 101)
            .await
            .unwrap(),
        TriggerClaim::Held {
            claimant: "worker-a".to_string(),
            invocation_id: Some("inv-a".to_string()),
            claimed_at: 100,
        }
    );
    assert!(
        store
            .take_over_trigger_claim(key(42), "worker-a", "worker-b", "inv-1", 102)
            .await
            .unwrap()
    );
    store.mark_trigger_started(key(42), "inv-1").await.unwrap();
    assert_eq!(
        store
            .claim_trigger(key(42), "worker-b", "inv-b", 103)
            .await
            .unwrap(),
        TriggerClaim::Started {
            invocation_id: "inv-1".to_string()
        }
    );
    // Started claims survive the release used by ordinary ACK paths.
    store.release_trigger_claim(key(42)).await.unwrap();
    assert!(matches!(
        store
            .claim_trigger(key(42), "worker-b", "inv-b2", 104)
            .await
            .unwrap(),
        TriggerClaim::Started { .. }
    ));

    store
        .claim_trigger(key(43), "worker-a", "inv-c", 100)
        .await
        .unwrap();
    store.release_trigger_claim(key(43)).await.unwrap();
    assert_eq!(
        store
            .claim_trigger(key(43), "worker-a", "inv-d", 101)
            .await
            .unwrap(),
        TriggerClaim::Won
    );
}

/// What a claim row names while it is still `claimed`. Not part of the
/// store's API: the dispatcher never needs to read it back, and startup
/// recovery finds a row *by* this column rather than reading it.
async fn claimed_invocation(store: &ControlPlaneStore, key: TriggerKey<'_>) -> Option<String> {
    sqlx::query_scalar(
        "SELECT invocation_id FROM trigger_claim \
         WHERE stream = ? AND stream_epoch = ? AND stream_seq = ?",
    )
    .bind(key.stream)
    .bind(key.stream_epoch)
    .bind(key.stream_seq as i64)
    .fetch_one(&store.pool)
    .await
    .unwrap()
}

/// **A claim names its invocation from claim time (#959).** The id is
/// written by the insert that wins the claim, not by the durable start,
/// so a process that meets an unfinished claim later can tell which
/// invocation it was for. A copy that loses the insert does not
/// overwrite it, and an adopter replaces it with its own in the same
/// compare-and-set that takes the claim over.
#[tokio::test]
async fn a_claim_names_its_invocation_from_claim_time() {
    let (store, _dir) = open_fresh().await;
    store
        .claim_trigger(key(7), "worker-a", "inv-first", 100)
        .await
        .unwrap();
    assert_eq!(
        claimed_invocation(&store, key(7)).await.as_deref(),
        Some("inv-first"),
        "the winning insert writes the id it was given"
    );

    store
        .claim_trigger(key(7), "worker-a", "inv-duplicate", 101)
        .await
        .unwrap();
    assert_eq!(
        claimed_invocation(&store, key(7)).await.as_deref(),
        Some("inv-first"),
        "a copy that loses the insert leaves the row's id alone"
    );

    assert!(
        !store
            .take_over_trigger_claim(key(7), "someone-else", "worker-b", "inv-wrong", 102)
            .await
            .unwrap(),
        "a take-over naming the wrong holder changes nothing"
    );
    assert_eq!(
        claimed_invocation(&store, key(7)).await.as_deref(),
        Some("inv-first")
    );
    assert!(
        store
            .take_over_trigger_claim(key(7), "worker-a", "worker-b", "inv-adopted", 103)
            .await
            .unwrap()
    );
    assert_eq!(
        claimed_invocation(&store, key(7)).await.as_deref(),
        Some("inv-adopted"),
        "the adopter's id replaces the previous holder's"
    );
}

/// **Startup recovery marks a claim by its key, and only while it still
/// names the run recovery found on record (#959, #967).** The mark turns
/// the `claimed` row into `durably_started`, once, keeping the id it
/// names, and touches nothing else: not a row naming another invocation
/// (adopted since it was read), not a row already started, not a key with
/// no row, not another trigger's claim.
#[tokio::test]
async fn a_claim_is_marked_started_by_its_key_while_it_names_the_run() {
    let (store, _dir) = open_fresh().await;
    store
        .claim_trigger(key(1), "dead-worker", "inv-on-record", 100)
        .await
        .unwrap();
    store
        .claim_trigger(key(2), "dead-worker", "inv-never-started", 100)
        .await
        .unwrap();

    assert!(
        !store
            .mark_unsettled_trigger_started(key(1), "inv-adopted-since")
            .await
            .unwrap(),
        "a claim that names another invocation is left as it is"
    );
    assert!(
        store
            .mark_unsettled_trigger_started(key(1), "inv-on-record")
            .await
            .unwrap(),
        "the claim naming the run is marked"
    );
    assert_eq!(
        store
            .claim_trigger(key(1), "worker-b", "inv-redelivered", 101)
            .await
            .unwrap(),
        TriggerClaim::Started {
            invocation_id: "inv-on-record".to_string()
        },
        "the redelivery now meets a started claim and is dropped"
    );
    assert!(
        !store
            .mark_unsettled_trigger_started(key(1), "inv-on-record")
            .await
            .unwrap(),
        "a row already started is left as it is"
    );
    assert!(
        !store
            .mark_unsettled_trigger_started(key(3), "inv-on-record")
            .await
            .unwrap(),
        "a key with no row changes nothing"
    );
    assert_eq!(
        store
            .claim_trigger(key(2), "worker-b", "inv-adopter", 101)
            .await
            .unwrap(),
        TriggerClaim::Held {
            claimant: "dead-worker".to_string(),
            invocation_id: Some("inv-never-started".to_string()),
            claimed_at: 100,
        },
        "another trigger's unfinished claim is untouched"
    );
}

/// **A recreated stream starts a new claim space.** JetStream restarts
/// sequences at 1 when a stream is rebuilt, so a claim table keyed on
/// the name and sequence alone would answer `Started` to the first N
/// triggers of the new stream and drop them — silently, because a
/// dropped duplicate starts nothing by design. Keyed on the stream's
/// creation time as well, the old incarnation's rows are inert and the
/// fresh delivery wins its claim, with no operator asked to remember to
/// clear a table.
#[tokio::test]
async fn a_claim_from_an_earlier_stream_incarnation_does_not_drop_a_fresh_delivery() {
    let (store, _dir) = open_fresh().await;
    let old = TriggerKey {
        stream: "fq-triggers",
        stream_epoch: 1_700_000_000_000_000_000,
        stream_seq: 1,
    };
    let recreated = TriggerKey {
        stream_epoch: old.stream_epoch + 1,
        ..old
    };
    assert_eq!(
        store
            .claim_trigger(old, "worker-a", "inv-old", 0)
            .await
            .unwrap(),
        TriggerClaim::Won
    );
    store.mark_trigger_started(old, "inv-old").await.unwrap();
    assert_eq!(
        store
            .claim_trigger(recreated, "worker-a", "inv-new", 1)
            .await
            .unwrap(),
        TriggerClaim::Won,
        "sequence 1 of a rebuilt stream is not the sequence 1 that already ran"
    );
    assert_eq!(
        store
            .claim_trigger(old, "worker-a", "inv-probe", 2)
            .await
            .unwrap(),
        TriggerClaim::Started {
            invocation_id: "inv-old".to_string()
        },
        "the old incarnation's record is kept, not overwritten"
    );
}

/// **The claim sweeps clear a backlog in batches.** The first sweep on a
/// long-lived instance meets every claim it has ever written, so each
/// sweep deletes a batch at a time until a short batch says the backlog
/// is gone. With a batch of one, every old row is its own round: the
/// loop must clear them all, count them, and leave the rest alone.
#[tokio::test]
async fn trigger_claim_sweeps_clear_a_backlog_in_batches() {
    let (store, _dir) = open_fresh().await;
    let cutoff = 1_000;
    for seq in 1..=3 {
        store
            .claim_trigger(key(seq), "worker-old", &format!("inv-{seq}"), 10)
            .await
            .unwrap();
        store
            .mark_trigger_started(key(seq), &format!("inv-{seq}"))
            .await
            .unwrap();
    }
    for seq in 4..=6 {
        store
            .claim_trigger(key(seq), "worker-old", &format!("inv-{seq}"), 10)
            .await
            .unwrap();
    }
    // Inside the cutoff, one of each.
    store
        .claim_trigger(key(7), "worker-old", "inv-7", cutoff)
        .await
        .unwrap();
    store.mark_trigger_started(key(7), "inv-7").await.unwrap();
    store
        .claim_trigger(key(8), "worker-old", "inv-8", cutoff)
        .await
        .unwrap();

    assert_eq!(
        store
            .sweep_started_trigger_claims_batched(cutoff, 1)
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        store
            .sweep_abandoned_trigger_claims_batched(cutoff, "worker-now", 1)
            .await
            .unwrap(),
        3
    );
    for seq in 1..=6 {
        assert_eq!(
            store
                .trigger_claim_for_invocation(&format!("inv-{seq}"))
                .await
                .unwrap(),
            None,
            "seq {seq} is older than the cutoff"
        );
    }
    for invocation in ["inv-7", "inv-8"] {
        assert!(
            store
                .trigger_claim_for_invocation(invocation)
                .await
                .unwrap()
                .is_some(),
            "{invocation} was claimed at the cutoff, not before it"
        );
    }
}

/// Each claim sweep keeps to its own state: the started sweep never
/// takes a `claimed` row, and the abandoned sweep never takes a started
/// one, whoever holds it.
#[tokio::test]
async fn each_trigger_claim_sweep_takes_only_its_own_state() {
    let (store, _dir) = open_fresh().await;
    store
        .claim_trigger(key(1), "worker-old", "inv-claimed", 0)
        .await
        .unwrap();
    store
        .claim_trigger(key(2), "worker-old", "inv-started", 0)
        .await
        .unwrap();
    store
        .mark_trigger_started(key(2), "inv-started")
        .await
        .unwrap();

    assert_eq!(store.sweep_started_trigger_claims(1_000).await.unwrap(), 1);
    assert!(
        store
            .trigger_claim_for_invocation("inv-claimed")
            .await
            .unwrap()
            .is_some()
    );

    store
        .claim_trigger(key(2), "worker-old", "inv-started-again", 0)
        .await
        .unwrap();
    store
        .mark_trigger_started(key(2), "inv-started-again")
        .await
        .unwrap();
    assert_eq!(
        store
            .sweep_abandoned_trigger_claims(1_000, "worker-now")
            .await
            .unwrap(),
        1
    );
    assert!(
        store
            .trigger_claim_for_invocation("inv-started-again")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn schema_v1_upgrades_to_trigger_claim_v2() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    let store = ControlPlaneStore::open(&path).await.unwrap();
    sqlx::query("DROP TABLE trigger_claim")
        .execute(&store.pool)
        .await
        .unwrap();
    store.write_schema_version(1).await.unwrap();
    drop(store);

    let upgraded = ControlPlaneStore::open(&path).await.unwrap();
    assert_eq!(upgraded.read_schema_version().await.unwrap(), Some(2));
    assert_eq!(
        upgraded
            .claim_trigger(key(1), "worker", "inv", 0)
            .await
            .unwrap(),
        TriggerClaim::Won
    );
}

/// **A claim of this daemon's own is taken again only while it still names
/// the abandoned run (issue 967).** The re-take is a compare-and-set on the
/// claimant, the `claimed` state and the invocation id the row names: two
/// copies that both found the claim abandoned claim under the same id, so
/// only the id in the condition lets exactly one of them win. It renames
/// the claim and restarts its age, and touches no other row.
#[tokio::test]
async fn a_claim_is_taken_again_only_while_it_names_the_abandoned_run() {
    let (store, _dir) = open_fresh().await;
    store
        .claim_trigger(key(9), "worker-a", "inv-abandoned", 100)
        .await
        .unwrap();
    store
        .claim_trigger(key(10), "worker-a", "inv-other", 100)
        .await
        .unwrap();

    assert!(
        !store
            .retake_trigger_claim(key(9), "worker-b", "inv-abandoned", "inv-new", 200)
            .await
            .unwrap(),
        "another claimant's re-take changes nothing"
    );
    assert!(
        !store
            .retake_trigger_claim(key(9), "worker-a", "inv-wrong", "inv-new", 200)
            .await
            .unwrap(),
        "a re-take naming another abandoned run changes nothing"
    );
    assert!(
        store
            .retake_trigger_claim(key(9), "worker-a", "inv-abandoned", "inv-first", 200)
            .await
            .unwrap(),
        "the first copy to find the claim abandoned takes it"
    );
    assert!(
        !store
            .retake_trigger_claim(key(9), "worker-a", "inv-abandoned", "inv-second", 201)
            .await
            .unwrap(),
        "a second copy that found it abandoned loses: the row names the first now"
    );
    assert_eq!(
        store
            .claim_trigger(key(9), "probe", "probe", 0)
            .await
            .unwrap(),
        TriggerClaim::Held {
            claimant: "worker-a".to_string(),
            invocation_id: Some("inv-first".to_string()),
            claimed_at: 200,
        },
        "renamed, and its age restarted"
    );
    assert_eq!(
        claimed_invocation(&store, key(10)).await.as_deref(),
        Some("inv-other"),
        "another trigger's claim is untouched"
    );

    store
        .mark_trigger_started(key(10), "inv-other")
        .await
        .unwrap();
    assert!(
        !store
            .retake_trigger_claim(key(10), "worker-a", "inv-other", "inv-new", 300)
            .await
            .unwrap(),
        "a claim marked started is never taken again"
    );
}

/// **One claim reads joined to its ownership row as every claim does (issue
/// 967).** The dispatcher reads a claim of its own through
/// `unsettled_trigger_claim`, which must return what
/// `unsettled_trigger_claims` returns for that row, the ownership status
/// included, and nothing for a row that is no longer `claimed`.
#[tokio::test]
async fn one_claim_reads_joined_as_every_claim_does() {
    let (store, _dir) = open_fresh().await;
    store
        .claim_trigger(key(1), "worker-a", "inv-owned", 100)
        .await
        .unwrap();
    store
        .claim_trigger(key(2), "worker-a", "inv-bare", 101)
        .await
        .unwrap();
    store
        .claim_trigger(key(3), "worker-a", "inv-started", 102)
        .await
        .unwrap();
    store
        .mark_trigger_started(key(3), "inv-started")
        .await
        .unwrap();
    store
        .upsert_invocation_ownership("inv-owned", "worker-a", 150, OwnerStatus::Completed)
        .await
        .unwrap();

    let every = store.unsettled_trigger_claims().await.unwrap();
    for seq in [1, 2] {
        let one = store.unsettled_trigger_claim(key(seq)).await.unwrap();
        let from_every = every.iter().find(|u| u.claim.stream_seq == seq).cloned();
        assert_eq!(one, from_every, "claim {seq}");
    }
    assert_eq!(
        store
            .unsettled_trigger_claim(key(1))
            .await
            .unwrap()
            .and_then(|u| u.owner_row_status),
        Some("completed".to_string())
    );
    assert_eq!(
        store.unsettled_trigger_claim(key(3)).await.unwrap(),
        None,
        "a started claim is not unsettled"
    );
    assert_eq!(store.unsettled_trigger_claim(key(4)).await.unwrap(), None);
}

/// **A start on record is either record holding the run (issue 967).** The
/// one definition, shared by startup recovery, `fq doctor` and the
/// dispatcher: a WAL row in flight or finished, or an ownership row of any
/// status, counts; neither does not.
#[test]
fn a_start_on_record_is_either_record_holding_the_run() {
    use crate::control_plane::store::RunRecords;
    use crate::worker::WalRun;
    let cases = [
        (None, None, false),
        (Some(WalRun::InFlight), None, true),
        (Some(WalRun::Finished), None, true),
        (None, Some("completed"), true),
        (None, Some("failed"), true),
        (None, Some("a status this build cannot read"), true),
        (Some(WalRun::InFlight), Some("ambiguous"), true),
    ];
    for (wal, owner, expected) in cases {
        let records = RunRecords {
            wal,
            owner_row_status: owner.map(str::to_string),
        };
        assert_eq!(records.start_on_record(), expected, "{records:?}");
        assert_eq!(
            records.where_on_record().is_some(),
            expected,
            "where_on_record agrees: {records:?}"
        );
    }
}
