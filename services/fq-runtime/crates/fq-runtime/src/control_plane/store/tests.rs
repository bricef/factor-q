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

/// A control-plane file at schema `version`, built by the ladder's first
/// `version` rungs as a build of that version built it. A rung that is not
/// idempotent (the archive-state rung adds a column) cannot be replayed
/// over a file that has it, so an older file is built, not a newer one
/// stamped back.
async fn store_at_version(path: &Path, version: usize) -> Pool<Sqlite> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = schema::connect_store_pool(options, 1).await.unwrap();
    schema::bootstrap_versioned(&pool, SCHEMA_CLASS, version as u32, &MIGRATIONS[..version])
        .await
        .expect("a store at the older version");
    pool
}

#[tokio::test]
async fn control_plane_ladder_upgrades_populated_database() {
    // Pins the NeedsUpgrade dispatch, the version restamp, and row
    // survival, over a file the first rung built. Since a rung adds a column
    // (real DDL) the older file is built by the older ladder rather than
    // stamped back from a current one.
    let dir = tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    let pool = store_at_version(&path, 1).await;
    sqlx::query(
        "INSERT INTO coordination_worker \
         (worker_id, host, registered_at, last_heartbeat, status) \
         VALUES ('migration-worker', 'sim-host', 1000, 1000, 'alive')",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let store = ControlPlaneStore::open(&path)
        .await
        .expect("reopen a v1 store");
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

    store
        .upsert_invocation_ownership("inv-A", "w-1", 100, OwnerStatus::Ambiguous)
        .await
        .unwrap();
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
    assert!(!store.release_trigger_claim(key(42), "inv-1").await.unwrap());
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
    assert!(store.release_trigger_claim(key(43), "inv-c").await.unwrap());
    assert_eq!(
        store
            .claim_trigger(key(43), "worker-a", "inv-d", 101)
            .await
            .unwrap(),
        TriggerClaim::Won
    );
}

/// **A release gives back only the claim its invocation holds (issue
/// 1061).** The invocation holding the claim removes it. One that no longer
/// holds it, because the claim was taken again under another id, removes
/// nothing and says so. A claim written before claims named their run,
/// which no running dispatcher holds, is left too.
#[tokio::test]
async fn a_release_gives_back_only_the_claim_its_invocation_holds() {
    let (store, _dir) = open_fresh().await;
    store
        .claim_trigger(key(1), "worker-a", "inv-first", 100)
        .await
        .unwrap();
    assert!(
        store
            .retake_trigger_claim(key(1), "worker-a", "inv-first", "inv-again", 200)
            .await
            .unwrap()
    );
    assert!(
        !store
            .release_trigger_claim(key(1), "inv-first")
            .await
            .unwrap(),
        "the first invocation no longer holds the claim, so its release removes nothing"
    );
    assert_eq!(
        claimed_invocation(&store, key(1)).await.as_deref(),
        Some("inv-again"),
        "the claim taken again is left"
    );
    assert!(
        store
            .release_trigger_claim(key(1), "inv-again")
            .await
            .unwrap(),
        "the invocation holding the claim gives it back"
    );
    assert_eq!(store.trigger_claim(key(1)).await.unwrap(), None);

    sqlx::query(
        "INSERT INTO trigger_claim \
         (stream, stream_epoch, stream_seq, claimant, state, invocation_id, claimed_at) \
         VALUES (?, ?, ?, 'old-build', 'claimed', NULL, 100)",
    )
    .bind(key(2).stream)
    .bind(key(2).stream_epoch)
    .bind(key(2).stream_seq as i64)
    .execute(&store.pool)
    .await
    .unwrap();
    assert!(
        !store
            .release_trigger_claim(key(2), "inv-any")
            .await
            .unwrap(),
        "a claim that names no invocation is no copy's to give back"
    );
    assert!(
        store.trigger_claim(key(2)).await.unwrap().is_some(),
        "and is left"
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

/// **The dispatcher's mark of a durable start says whether it marked the
/// claim (issue 1046).** It marks the claim that names its run, and says
/// so again for a claim already marked for that run, as a retry of a mark
/// that landed finds it. A key with no row, or a row naming another
/// invocation, is reported as not marked, and the row is left as it is.
#[tokio::test]
async fn a_durable_start_mark_reports_whether_it_marked_the_claim_naming_its_run() {
    let (store, _dir) = open_fresh().await;
    store
        .claim_trigger(key(1), "worker-a", "inv-run", 100)
        .await
        .unwrap();
    assert!(
        store.mark_trigger_started(key(1), "inv-run").await.unwrap(),
        "the claim naming the run is marked"
    );
    assert!(
        store.mark_trigger_started(key(1), "inv-run").await.unwrap(),
        "a claim already marked for the run is reported marked, as a retry finds it"
    );
    assert_eq!(
        store
            .claim_trigger(key(1), "worker-b", "inv-copy", 101)
            .await
            .unwrap(),
        TriggerClaim::Started {
            invocation_id: "inv-run".to_string()
        },
        "a copy meets the run's start"
    );

    assert!(
        !store.mark_trigger_started(key(2), "inv-run").await.unwrap(),
        "a key with no row is reported as not marked"
    );
    assert_eq!(
        store.trigger_claim(key(2)).await.unwrap(),
        None,
        "and the mark writes no row"
    );

    store
        .claim_trigger(key(3), "worker-a", "inv-other", 100)
        .await
        .unwrap();
    assert!(
        !store.mark_trigger_started(key(3), "inv-run").await.unwrap(),
        "a claim naming another invocation is reported as not marked"
    );
    let other = store
        .trigger_claim(key(3))
        .await
        .unwrap()
        .expect("the claim is still there");
    assert!(
        other.state == ClaimState::Claimed && other.invocation_id.as_deref() == Some("inv-other"),
        "and is left as it is: {other:?}"
    );
}

/// **A start the mark did not find a claim for is recorded by what the
/// claim holds (issue 1046).** No row: one is written, started, naming the
/// run. Another invocation's claim, not started: the start is recorded on
/// it in that invocation's place, keeping its age. Another run's start: left
/// as it is. A row naming the run: marked. Every key ends with a start on
/// record, which is what refuses a copy of the trigger.
#[tokio::test]
async fn a_start_the_mark_did_not_find_is_recorded_by_what_the_claim_holds() {
    let (store, _dir) = open_fresh().await;

    assert_eq!(
        store
            .record_trigger_start(key(1), "worker-a", "inv-run", 500)
            .await
            .unwrap(),
        RecordedStart::Written,
        "no row: one is written"
    );
    let written = store.trigger_claim(key(1)).await.unwrap().unwrap();
    assert_eq!(
        (
            written.state == ClaimState::DurablyStarted,
            written.invocation_id.as_deref(),
            written.claimant.as_str(),
            written.claimed_at
        ),
        (true, Some("inv-run"), "worker-a", 500),
        "{written:?}"
    );

    store
        .claim_trigger(key(2), "worker-a", "inv-other", 100)
        .await
        .unwrap();
    assert_eq!(
        store
            .record_trigger_start(key(2), "worker-a", "inv-run", 500)
            .await
            .unwrap(),
        RecordedStart::WrittenOver {
            claimed_for: Some("inv-other".to_string())
        },
        "another invocation's claim, not started: the start is recorded on it"
    );
    let over = store.trigger_claim(key(2)).await.unwrap().unwrap();
    assert_eq!(
        (
            over.state == ClaimState::DurablyStarted,
            over.invocation_id.as_deref(),
            over.claimed_at
        ),
        (true, Some("inv-run"), 100),
        "named for the run, keeping the claim's age: {over:?}"
    );

    store
        .claim_trigger(key(3), "worker-a", "inv-other", 100)
        .await
        .unwrap();
    store
        .mark_trigger_started(key(3), "inv-other")
        .await
        .unwrap();
    assert_eq!(
        store
            .record_trigger_start(key(3), "worker-a", "inv-run", 500)
            .await
            .unwrap(),
        RecordedStart::AlreadyStarted {
            invocation_id: Some("inv-other".to_string())
        },
        "another run's start: left"
    );
    assert_eq!(
        store
            .claim_trigger(key(3), "worker-b", "inv-copy", 600)
            .await
            .unwrap(),
        TriggerClaim::Started {
            invocation_id: "inv-other".to_string()
        },
        "still naming the other run"
    );

    store
        .claim_trigger(key(4), "worker-a", "inv-run", 100)
        .await
        .unwrap();
    assert_eq!(
        store
            .record_trigger_start(key(4), "worker-a", "inv-run", 500)
            .await
            .unwrap(),
        RecordedStart::Marked,
        "a row naming the run: marked"
    );
    assert_eq!(
        store
            .claim_trigger(key(4), "worker-b", "inv-copy", 600)
            .await
            .unwrap(),
        TriggerClaim::Started {
            invocation_id: "inv-run".to_string()
        }
    );

    assert_eq!(
        store
            .record_trigger_start(key(4), "worker-a", "inv-run", 700)
            .await
            .unwrap(),
        RecordedStart::Marked,
        "a row already recording the run's own start: marked, not another run's start"
    );
}

/// How many keys [`the_record_of_a_start_holds_the_write_lock_against_racing_writers`]
/// races, and how long it may take.
const RACED_KEYS: u64 = 200;
const RACE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

/// **The record of a start holds the write lock from its read to its write
/// (issue 1046).** For each key the record races a claim, a release and a
/// re-take, on a pool of its own connections; half the keys start with
/// another invocation's claim. Every record must land and leave the key
/// recording the run's start. A record that read in a deferred transaction
/// and then wrote would be refused by SQLite with `SQLITE_BUSY` whenever a
/// racing writer committed between its read and its write, since a read
/// transaction cannot become a writer over a newer snapshot; `BEGIN
/// IMMEDIATE` takes the lock before the read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_record_of_a_start_holds_the_write_lock_against_racing_writers() {
    let (store, _dir) = open_fresh().await;
    let store = std::sync::Arc::new(store);
    let raced = async {
        for seq in 1..=RACED_KEYS {
            if seq % 2 == 1 {
                store
                    .claim_trigger(key(seq), "worker-a", "inv-other", 100)
                    .await
                    .unwrap();
            }
            let record = tokio::spawn({
                let store = std::sync::Arc::clone(&store);
                async move {
                    store
                        .record_trigger_start(key(seq), "worker-a", "inv-run", 300)
                        .await
                }
            });
            let claim = tokio::spawn({
                let store = std::sync::Arc::clone(&store);
                async move {
                    store
                        .claim_trigger(key(seq), "worker-a", "inv-copy", 200)
                        .await
                }
            });
            let release = tokio::spawn({
                let store = std::sync::Arc::clone(&store);
                async move { store.release_trigger_claim(key(seq), "inv-other").await }
            });
            let retake = tokio::spawn({
                let store = std::sync::Arc::clone(&store);
                async move {
                    store
                        .retake_trigger_claim(key(seq), "worker-a", "inv-other", "inv-again", 250)
                        .await
                }
            });
            let recorded = record.await.unwrap();
            // The racing claim's own result is not what this pins: a claim
            // that loses its insert reads the row back in a second
            // statement, and a release landing between the two leaves it
            // nothing to read.
            let _ = claim.await.unwrap();
            release.await.unwrap().expect("the racing release lands");
            retake.await.unwrap().expect("the racing re-take lands");
            let recorded = recorded.unwrap_or_else(|err| {
                panic!("key {seq}: the record was refused while writers raced it: {err}")
            });
            assert!(
                !matches!(recorded, RecordedStart::AlreadyStarted { .. }),
                "key {seq}: nothing else starts a run, yet the record found {recorded:?}"
            );
            let row = store
                .trigger_claim(key(seq))
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("key {seq}: no claim after {recorded:?}"));
            assert!(
                row.state == ClaimState::DurablyStarted
                    && row.invocation_id.as_deref() == Some("inv-run"),
                "key {seq}: {recorded:?} left {row:?}"
            );
        }
    };
    tokio::time::timeout(RACE_DEADLINE, raced)
        .await
        .expect("the race finishes inside its deadline");
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
            .sweep_settled_trigger_claims_batched(cutoff, 1)
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

    assert_eq!(store.sweep_settled_trigger_claims(1_000).await.unwrap(), 1);
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
    store_at_version(&path, 1).await.close().await;

    let upgraded = ControlPlaneStore::open(&path).await.unwrap();
    assert_eq!(
        upgraded.read_schema_version().await.unwrap(),
        Some(CONTROL_PLANE_SCHEMA_VERSION)
    );
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

/// **One claim reads back in whatever state it is (issue 967).** The
/// dead-letter watch reads a trigger's claim before it calls the trigger
/// exhausted: a `claimed` row and a `durably_started` row each come back
/// as they are, under their own key and no other, and an unclaimed key
/// reads as nothing.
#[tokio::test]
async fn one_claim_reads_back_in_whatever_state_it_is() {
    let (store, _dir) = open_fresh().await;
    store
        .claim_trigger(key(1), "worker-a", "inv-claimed", 100)
        .await
        .unwrap();
    store
        .claim_trigger(key(2), "worker-a", "inv-started", 101)
        .await
        .unwrap();
    store
        .mark_trigger_started(key(2), "inv-started")
        .await
        .unwrap();

    let claimed = store.trigger_claim(key(1)).await.unwrap().expect("claimed");
    assert_eq!(
        (
            claimed.stream_seq,
            claimed.state,
            claimed.invocation_id.as_deref(),
            claimed.claimant.as_str(),
            claimed.claimed_at,
        ),
        (1, ClaimState::Claimed, Some("inv-claimed"), "worker-a", 100)
    );
    let started = store.trigger_claim(key(2)).await.unwrap().expect("started");
    assert_eq!(
        (
            started.stream_seq,
            started.state,
            started.invocation_id.as_deref()
        ),
        (2, ClaimState::DurablyStarted, Some("inv-started"))
    );
    assert_eq!(store.trigger_claim(key(3)).await.unwrap(), None);
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

// ----- Which errors the store refuses for good (issue 1070) -----

/// **A write the store refuses by its own rules is `Refused`; every other
/// failure is `Backend`.** Against the real store and schema: a CHECK, a
/// key, a NOT NULL column and a trigger each refuse the same write again
/// on every try, so each comes back [`ControlPlaneStoreError::Refused`],
/// which the coordination consumer acknowledges rather than retries. A
/// locked database, a full one, and a statement that fails for another
/// reason (here a table that is not there) can pass, so each comes back
/// [`ControlPlaneStoreError::Backend`], which the consumer retries: taken
/// for a refusal, a locked file or a full disk would be acknowledged and
/// the event lost.
#[tokio::test]
async fn a_write_the_store_refuses_by_its_own_rules_is_refused_and_no_other_failure_is() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    let store = ControlPlaneStore::open(&path).await.unwrap();
    let raw = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    let refused = |err: Option<&ControlPlaneStoreError>| {
        matches!(err, Some(ControlPlaneStoreError::Refused(_)))
    };
    let backend = |err: Option<&ControlPlaneStoreError>| {
        matches!(err, Some(ControlPlaneStoreError::Backend(_)))
    };

    // A CHECK: a status the row's archive state does not go with.
    store.assign_invocation("checked", "w", 1).await.unwrap();
    let check = store
        .overwrite_owner_status_for_test("checked", "completed")
        .await;
    assert!(
        refused(check.as_ref().err()),
        "a CHECK violation: {check:?}"
    );

    // A key: the same invocation's row inserted twice.
    let key = store.assign_invocation("checked", "w", 1).await;
    assert!(refused(key.as_ref().err()), "a key violation: {key:?}");

    // A NOT NULL column: a row with no worker.
    let not_null = sqlx::query(
        "INSERT INTO coordination_invocation_owner \
         (invocation_id, worker_id, assigned_at, status, archive_state) \
         VALUES ('no-worker', NULL, 1, 'in_flight', 'not_due')",
    )
    .execute(&raw)
    .await
    .map(|_| ())
    .map_err(ControlPlaneStoreError::from);
    assert!(
        refused(not_null.as_ref().err()),
        "a NOT NULL violation: {not_null:?}"
    );

    // A trigger's RAISE.
    sqlx::query(
        "CREATE TRIGGER refuse_the_worker BEFORE INSERT ON coordination_worker \
         BEGIN SELECT RAISE(ABORT, 'the test refuses the worker'); END",
    )
    .execute(&raw)
    .await
    .unwrap();
    let trigger = store.register_worker("refused", "host", 1).await;
    assert!(
        refused(trigger.as_ref().err()),
        "a trigger's RAISE: {trigger:?}"
    );

    // A statement that fails for another reason: a table that is not there.
    sqlx::query("DROP TABLE schedule_entry")
        .execute(&raw)
        .await
        .unwrap();
    let missing = store.list_due_schedules(i64::MAX).await;
    assert!(
        backend(missing.as_ref().err()),
        "a table that is not there: {missing:?}"
    );

    // A full database: the file may grow no further than it is, so a write
    // that needs a new page fails as on a full disk (SQLITE_FULL, 13).
    let pages: i64 = sqlx::query_scalar("PRAGMA page_count")
        .fetch_one(&raw)
        .await
        .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "PRAGMA max_page_count = {pages}"
    )))
    .execute(&raw)
    .await
    .unwrap();
    let full = sqlx::query(
        "INSERT INTO invocation_archive \
         (invocation_id, agent_id, final_phase, final_state_blob, started_at, terminal_at, \
          archived_at) \
         VALUES ('full', 'a', 'completed', ?, 1, 2, 3)",
    )
    .bind(vec![0_u8; 1 << 20])
    .execute(&raw)
    .await
    .map(|_| ())
    .map_err(ControlPlaneStoreError::from);
    assert!(
        matches!(&full, Err(ControlPlaneStoreError::Backend(message)) if message.contains("(code: 13)")),
        "a full database: {full:?}"
    );

    // A locked database: another connection holds the write lock past the
    // store's busy timeout.
    let mut holder = raw.acquire().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *holder)
        .await
        .unwrap();
    let locked = store.assign_invocation("locked", "w", 1).await;
    sqlx::query("ROLLBACK").execute(&mut *holder).await.unwrap();
    assert!(
        backend(locked.as_ref().err()),
        "a locked database: {locked:?}"
    );
}

/// A claim this test writes and then consumes, as the dispatcher's
/// consuming answers will.
async fn consumed(store: &ControlPlaneStore, seq: u64, invocation_id: &str, why: ConsumedWhy) {
    store
        .claim_trigger(key(seq), "worker-a", invocation_id, 100)
        .await
        .unwrap();
    assert!(
        store
            .consume_trigger_claim(key(seq), invocation_id, &why)
            .await
            .unwrap(),
        "the claim is consumed"
    );
}

/// An operator's drop of `invocation_id`'s run recorded on its consumed
/// claim, as the drop's handler records it.
async fn recorded_drop(store: &ControlPlaneStore, invocation_id: &str) {
    let recorded = sqlx::query(
        "UPDATE trigger_claim SET operator_dropped_at = 700 \
         WHERE invocation_id = ? AND state = 'consumed'",
    )
    .bind(invocation_id)
    .execute(&store.pool)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(recorded, 1, "the drop is recorded on the consumed claim");
}

/// **The claim table's consumed state comes in by its own rung (issue
/// 1055).** A store at the version before that rung, with the table as it
/// stood then and a claim in each of its two states, is opened by this
/// build: the rung runs, every row keeps its state and its place in the
/// table, and the table now admits `consumed`, with `consumed_why` set
/// exactly when the state is `consumed`.
///
/// The version before the rung is read off the ladder by the rung's own
/// SQL, so a rung added before or after it leaves this test as it is. The
/// rows differ in every one of the table's seven columns, one is of
/// another incarnation of the stream and one names no invocation, and
/// every row comes through the rebuild whole. They are written out of key
/// order, and two name one invocation: the rebuilt table keeps their
/// order, which `trigger_claim_for_invocation` (the latest row naming a
/// run) and the retention sweep (old rows first) rely on. A lost
/// `claimed_at` would make every claim look older than the window, and
/// the next sweep would delete every started row.
///
/// The store is the one every rung before it left, the termination
/// record's included (issue 1070): its ownership table holds a row with a
/// whole record, which comes through this rung untouched, since the two
/// rungs rebuild different tables.
#[tokio::test]
async fn the_consumed_state_rung_upgrades_a_claim_table_keeping_its_rows() {
    type Row = (String, i64, i64, String, String, Option<String>, i64);
    async fn rows(store: &ControlPlaneStore) -> Vec<Row> {
        sqlx::query_as(
            "SELECT stream, stream_epoch, stream_seq, claimant, state, invocation_id, claimed_at \
             FROM trigger_claim ORDER BY rowid",
        )
        .fetch_all(&store.pool)
        .await
        .unwrap()
    }
    let rung = MIGRATIONS
        .iter()
        .find(|(_, sql)| *sql == trigger_claim::TRIGGER_CLAIM_CONSUMED_STATE_SQL)
        .map(|(version, _)| *version)
        .expect("the consumed-state rung is on the ladder");
    let dir = tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    let store = ControlPlaneStore::open(&path).await.unwrap();
    sqlx::query("DROP TABLE trigger_claim")
        .execute(&store.pool)
        .await
        .unwrap();
    sqlx::raw_sql(TRIGGER_CLAIM_TABLE_V2_SQL)
        .execute(&store.pool)
        .await
        .unwrap();
    let epoch = key(1).stream_epoch;
    for (stream_epoch, seq, claimant, state, invocation_id, claimed_at) in [
        (
            epoch,
            2,
            "worker-b",
            "durably_started",
            Some("inv-started"),
            200,
        ),
        (epoch, 1, "worker-a", "claimed", Some("inv-claimed"), 100),
        (epoch, 5, "worker-c", "claimed", Some("inv-twice"), 500),
        (epoch, 4, "worker-d", "claimed", Some("inv-twice"), 400),
        (epoch + 1, 3, "worker-e", "claimed", None, 300),
    ] {
        sqlx::query(
            "INSERT INTO trigger_claim \
             (stream, stream_epoch, stream_seq, claimant, state, invocation_id, claimed_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(key(seq).stream)
        .bind(stream_epoch)
        .bind(seq as i64)
        .bind(claimant)
        .bind(state)
        .bind(invocation_id)
        .bind(claimed_at)
        .execute(&store.pool)
        .await
        .unwrap();
    }
    let before = rows(&store).await;
    type Owner = (
        String,
        String,
        String,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<String>,
    );
    async fn owners(store: &ControlPlaneStore) -> Vec<Owner> {
        sqlx::query_as(
            "SELECT invocation_id, status, archive_state, ended_by, ended_at, \
             termination_requested_at, termination_reason \
             FROM coordination_invocation_owner ORDER BY invocation_id",
        )
        .fetch_all(&store.pool)
        .await
        .unwrap()
    }
    sqlx::query(
        "INSERT INTO coordination_invocation_owner \
         (invocation_id, worker_id, assigned_at, status, archive_state, ended_by, ended_at, \
          termination_requested_at, termination_requested_by, termination_reason) \
         VALUES ('inv-dropped', 'worker-a', 100, 'failed', 'archived', 'drop', 300, 250, \
          'operator', 'stuck')",
    )
    .execute(&store.pool)
    .await
    .unwrap();
    let owners_before = owners(&store).await;
    store.write_schema_version(rung - 1).await.unwrap();
    drop(store);

    let upgraded = ControlPlaneStore::open(&path).await.unwrap();
    assert_eq!(
        owners(&upgraded).await,
        owners_before,
        "the ownership table and its termination record come through untouched"
    );
    assert_eq!(
        upgraded.read_schema_version().await.unwrap(),
        Some(CONTROL_PLANE_SCHEMA_VERSION)
    );
    assert_eq!(
        rows(&upgraded).await,
        before,
        "every row comes through whole and in its place"
    );
    let dropped: Vec<Option<i64>> =
        sqlx::query_scalar("SELECT operator_dropped_at FROM trigger_claim ORDER BY rowid")
            .fetch_all(&upgraded.pool)
            .await
            .unwrap();
    assert_eq!(
        dropped,
        vec![None; before.len()],
        "no row from before the rung records an operator's drop"
    );
    let claimed = upgraded.trigger_claim(key(1)).await.unwrap().unwrap();
    let started = upgraded.trigger_claim(key(2)).await.unwrap().unwrap();
    assert_eq!(
        (claimed.state, claimed.invocation_id.as_deref()),
        (ClaimState::Claimed, Some("inv-claimed"))
    );
    assert_eq!(
        (started.state, started.invocation_id.as_deref()),
        (ClaimState::DurablyStarted, Some("inv-started"))
    );
    assert_eq!(
        upgraded
            .trigger_claim_for_invocation("inv-twice")
            .await
            .unwrap()
            .map(|row| row.stream_seq),
        Some(4),
        "the latest row naming a run is still the one written last"
    );

    assert!(
        upgraded
            .consume_trigger_claim(key(1), "inv-claimed", &ConsumedWhy::FailedForGood)
            .await
            .unwrap(),
        "the upgraded table admits a consumed claim"
    );
    for (state, why) in [("consumed", None), ("claimed", Some("failed_for_good"))] {
        let refused = sqlx::query(
            "INSERT INTO trigger_claim \
             (stream, stream_epoch, stream_seq, claimant, state, invocation_id, claimed_at, \
              consumed_why) VALUES ('fq-triggers', 0, 99, 'w', ?, 'inv', 0, ?)",
        )
        .bind(state)
        .bind(why)
        .execute(&upgraded.pool)
        .await;
        assert!(
            refused.is_err(),
            "`consumed_why` goes with `consumed` and nothing else: {state} with {why:?}"
        );
    }
}

/// **A claim is consumed only while it is still `claimed` under the
/// invocation consuming it (issue 1055),** as a release is scoped. A claim
/// taken again since, one already started, one already consumed and a key
/// with no row are all left, and reported as not consumed.
#[tokio::test]
async fn a_claim_is_consumed_only_while_its_invocation_holds_it() {
    let (store, _dir) = open_fresh().await;
    store
        .claim_trigger(key(1), "worker-a", "inv-first", 100)
        .await
        .unwrap();
    assert!(
        store
            .retake_trigger_claim(key(1), "worker-a", "inv-first", "inv-again", 200)
            .await
            .unwrap()
    );
    let why = ConsumedWhy::FailedForGood;
    assert!(
        !store
            .consume_trigger_claim(key(1), "inv-first", &why)
            .await
            .unwrap(),
        "a claim taken again is not the first invocation's to consume"
    );
    assert_eq!(
        store.trigger_claim(key(1)).await.unwrap().unwrap().state,
        ClaimState::Claimed
    );

    store
        .claim_trigger(key(2), "worker-a", "inv-run", 100)
        .await
        .unwrap();
    assert!(store.mark_trigger_started(key(2), "inv-run").await.unwrap());
    assert!(
        !store
            .consume_trigger_claim(key(2), "inv-run", &why)
            .await
            .unwrap(),
        "a started claim is not consumed"
    );

    consumed(&store, 3, "inv-done", ConsumedWhy::InvalidPayload).await;
    assert!(
        !store
            .consume_trigger_claim(key(3), "inv-done", &why)
            .await
            .unwrap(),
        "a consumed claim is not consumed again"
    );
    assert_eq!(
        store.trigger_claim(key(3)).await.unwrap().unwrap().state,
        ClaimState::Consumed {
            why: ConsumedWhy::InvalidPayload
        },
        "and keeps the reason it was consumed for"
    );

    assert!(
        !store
            .consume_trigger_claim(key(4), "inv-any", &why)
            .await
            .unwrap(),
        "a key with no row consumes nothing"
    );
    assert_eq!(store.trigger_claim(key(4)).await.unwrap(), None);
}

/// **A consumed claim is the trigger's record that it was dealt with, and
/// nothing that works on `claimed` rows touches it (issue 1055).** A copy
/// that claims the delivery is answered `Consumed`; adoption, re-take,
/// release, the mark and recovery's mark all leave it; and the unsettled
/// reads that recovery and `fq doctor` use do not list it.
#[tokio::test]
async fn a_consumed_claim_is_answered_as_consumed_and_left_by_every_claimed_row_write() {
    let (store, _dir) = open_fresh().await;
    consumed(&store, 1, "inv-done", ConsumedWhy::DeadLettered).await;

    assert_eq!(
        store
            .claim_trigger(key(1), "worker-b", "inv-copy", 200)
            .await
            .unwrap(),
        TriggerClaim::Consumed {
            invocation_id: Some("inv-done".to_string()),
            why: ConsumedWhy::DeadLettered,
        },
        "a copy meets the consumed claim"
    );
    assert!(
        !store
            .take_over_trigger_claim(key(1), "worker-a", "worker-b", "inv-adopter", 200)
            .await
            .unwrap(),
        "adoption leaves a consumed claim"
    );
    assert!(
        !store
            .retake_trigger_claim(key(1), "worker-a", "inv-done", "inv-again", 200)
            .await
            .unwrap(),
        "a re-take leaves a consumed claim"
    );
    assert!(
        !store
            .release_trigger_claim(key(1), "inv-done")
            .await
            .unwrap(),
        "a release leaves a consumed claim"
    );
    assert!(
        !store
            .mark_trigger_started(key(1), "inv-done")
            .await
            .unwrap(),
        "the mark leaves a consumed claim, for the record of a start to put right"
    );
    assert!(
        !store
            .mark_unsettled_trigger_started(key(1), "inv-done")
            .await
            .unwrap(),
        "recovery's mark leaves a consumed claim"
    );
    assert_eq!(store.unsettled_trigger_claim(key(1)).await.unwrap(), None);
    assert!(
        store.unsettled_trigger_claims().await.unwrap().is_empty(),
        "the unsettled claims recovery and `fq doctor` read do not list it"
    );
    assert_eq!(
        store.trigger_claim(key(1)).await.unwrap().unwrap().state,
        ClaimState::Consumed {
            why: ConsumedWhy::DeadLettered
        },
        "and it is still consumed"
    );
}

/// **A start found over a consumed claim is recorded over it (issue
/// 1055).** The run did start, and left consumed, the trigger's dead
/// letter could be requeued and run again; so the record of a start the
/// mark did not find takes the row, whatever invocation it names, and the
/// row is started for the run with no reason left on it.
#[tokio::test]
async fn a_start_found_over_a_consumed_claim_is_recorded_over_it() {
    let (store, _dir) = open_fresh().await;
    consumed(&store, 1, "inv-other", ConsumedWhy::FailedForGood).await;
    recorded_drop(&store, "inv-other").await;

    assert_eq!(
        store
            .record_trigger_start(key(1), "worker-a", "inv-run", 500)
            .await
            .unwrap(),
        RecordedStart::OverConsumed {
            consumed_by: Some("inv-other".to_string()),
            why: ConsumedWhy::FailedForGood,
        }
    );
    let row = store.trigger_claim(key(1)).await.unwrap().unwrap();
    assert_eq!(
        (row.state, row.invocation_id.as_deref(), row.claimed_at),
        (ClaimState::DurablyStarted, Some("inv-run"), 100),
        "started for the run, keeping the claim's age"
    );
    assert_eq!(
        row.operator_dropped_at, None,
        "the drop goes with the consumed state"
    );
}

/// **A consumed claim is marked started only while it is still consumed
/// for the run on record (issue 1055).** The dead-letter watch and requeue
/// mark it when that run has a start on record. A `claimed` row, a
/// consumed one naming another invocation, and a key with no row are all
/// left, and reported as not marked; the one it marks loses its reason.
#[tokio::test]
async fn a_consumed_claim_is_marked_started_only_for_the_run_it_names() {
    let (store, _dir) = open_fresh().await;
    store
        .claim_trigger(key(1), "worker-a", "inv-run", 100)
        .await
        .unwrap();
    assert!(
        !store
            .mark_consumed_trigger_started(key(1), "inv-run")
            .await
            .unwrap(),
        "a `claimed` row is recovery's and the dispatcher's to mark, not this"
    );
    consumed(&store, 2, "inv-other", ConsumedWhy::FailedForGood).await;
    assert!(
        !store
            .mark_consumed_trigger_started(key(2), "inv-run")
            .await
            .unwrap(),
        "a consumed claim naming another invocation is left"
    );
    assert!(
        !store
            .mark_consumed_trigger_started(key(3), "inv-run")
            .await
            .unwrap()
    );

    consumed(&store, 4, "inv-run", ConsumedWhy::FailedForGood).await;
    recorded_drop(&store, "inv-run").await;
    assert!(
        store
            .mark_consumed_trigger_started(key(4), "inv-run")
            .await
            .unwrap()
    );
    let row = store.trigger_claim(key(4)).await.unwrap().unwrap();
    assert_eq!(
        (row.state, row.invocation_id.as_deref(), row.claimed_at),
        (ClaimState::DurablyStarted, Some("inv-run"), 100),
        "started for the run, keeping the claim's age"
    );
    assert_eq!(
        row.operator_dropped_at, None,
        "the drop goes with the consumed state"
    );
    assert_eq!(
        store.trigger_claim(key(2)).await.unwrap().unwrap().state,
        ClaimState::Consumed {
            why: ConsumedWhy::FailedForGood
        }
    );
}

/// **The retention sweep takes consumed claims past the window, as it
/// takes started ones (issue 1055),** and leaves younger ones. The sweep of
/// claims an exited process left `claimed` does not take them: a consumed
/// claim is settled, and kept as long as a started one.
#[tokio::test]
async fn the_retention_sweep_takes_consumed_claims_past_the_window() {
    let (store, _dir) = open_fresh().await;
    consumed(&store, 1, "inv-old", ConsumedWhy::UnknownAgent).await;
    store
        .claim_trigger(key(2), "worker-a", "inv-young", 2_000)
        .await
        .unwrap();
    assert!(
        store
            .consume_trigger_claim(key(2), "inv-young", &ConsumedWhy::UnknownAgent)
            .await
            .unwrap()
    );

    assert_eq!(
        store
            .sweep_abandoned_trigger_claims(1_000, "worker-now")
            .await
            .unwrap(),
        0,
        "the abandoned-claim sweep leaves a consumed claim, however old"
    );
    assert!(store.trigger_claim(key(1)).await.unwrap().is_some());

    assert_eq!(store.sweep_settled_trigger_claims(1_000).await.unwrap(), 1);
    assert_eq!(store.trigger_claim(key(1)).await.unwrap(), None);
    assert!(store.trigger_claim(key(2)).await.unwrap().is_some());
}

/// **A claim's state reads in a log line as its column spells it, and a
/// consumed claim with its reason (issue 1055).** The line about a release
/// that gave nothing back names the claim's state this way.
#[test]
fn a_claim_state_is_named_as_its_column_spells_it() {
    assert_eq!(ClaimState::Claimed.to_string(), "claimed");
    assert_eq!(ClaimState::DurablyStarted.to_string(), "durably_started");
    assert_eq!(
        ClaimState::Consumed {
            why: ConsumedWhy::FailedForGood
        }
        .to_string(),
        "consumed (failed_for_good)"
    );
}

/// **A consumed claim's reason keeps its spelling (issue 1055).** Every
/// reason reads back as itself, and a spelling this build does not know,
/// which a newer build wrote, is kept as it is rather than refused.
#[test]
fn a_consumed_reason_keeps_its_spelling_and_an_unknown_one_is_kept_as_it_is() {
    for why in [
        ConsumedWhy::RefusedSubject,
        ConsumedWhy::InvalidAgentId,
        ConsumedWhy::UnknownAgent,
        ConsumedWhy::InvalidPayload,
        ConsumedWhy::RequeuedFromHold,
        ConsumedWhy::AgentRemoved,
        ConsumedWhy::FailedForGood,
        ConsumedWhy::HaltedInSetup,
        ConsumedWhy::ReturnedUnstarted,
        ConsumedWhy::DeadLettered,
    ] {
        assert_eq!(ConsumedWhy::of_column(why.as_str()), why);
    }
    assert_eq!(
        ConsumedWhy::of_column("a_reason_from_a_newer_build"),
        ConsumedWhy::Other("a_reason_from_a_newer_build".to_string())
    );
    assert_eq!(
        ConsumedWhy::Other("a_reason_from_a_newer_build".to_string()).as_str(),
        "a_reason_from_a_newer_build"
    );
}

/// **A claim state this build does not know is refused, never read as
/// `claimed` (issue 1055).** A row read as `claimed` that is not is the
/// hazard the state's enum exists to remove: a consumed trigger would be
/// adopted and run again. The mapping names every pair of `state` and
/// `consumed_why` the table admits and refuses any other, as the ownership
/// table's status is refused. The rows here are seeded past the table's
/// `CHECK`, as only a hand edit could write them.
#[tokio::test]
async fn a_claim_state_this_build_does_not_know_is_refused_not_read_as_claimed() {
    assert_eq!(
        ClaimState::parse("claimed", None),
        Some(ClaimState::Claimed)
    );
    assert_eq!(
        ClaimState::parse("durably_started", None),
        Some(ClaimState::DurablyStarted)
    );
    assert_eq!(
        ClaimState::parse("consumed", Some("failed_for_good")),
        Some(ClaimState::Consumed {
            why: ConsumedWhy::FailedForGood
        })
    );
    for (state, why) in [
        ("a_state_from_a_newer_build", None),
        ("consumed", None),
        ("claimed", Some("failed_for_good")),
        ("", None),
    ] {
        assert_eq!(
            ClaimState::parse(state, why),
            None,
            "{state:?} with {why:?}"
        );
    }

    let (store, _dir) = open_fresh().await;
    let mut conn = store.pool.acquire().await.unwrap();
    sqlx::query("PRAGMA ignore_check_constraints = ON")
        .execute(&mut *conn)
        .await
        .unwrap();
    for (seq, state, why) in [
        (1, "a_state_from_a_newer_build", None::<&str>),
        (2, "consumed", None),
    ] {
        sqlx::query(
            "INSERT INTO trigger_claim \
             (stream, stream_epoch, stream_seq, claimant, state, invocation_id, claimed_at, \
              consumed_why) VALUES (?, ?, ?, 'worker-a', ?, 'inv', 100, ?)",
        )
        .bind(key(seq).stream)
        .bind(key(seq).stream_epoch)
        .bind(seq as i64)
        .bind(state)
        .bind(why)
        .execute(&mut *conn)
        .await
        .unwrap();
    }
    sqlx::query("PRAGMA ignore_check_constraints = OFF")
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);

    for seq in [1, 2] {
        assert!(
            matches!(
                store.trigger_claim(key(seq)).await,
                Err(ControlPlaneStoreError::Malformed(_))
            ),
            "row {seq} is refused when read"
        );
        assert!(
            matches!(
                store
                    .claim_trigger(key(seq), "worker-b", "inv-copy", 200)
                    .await,
                Err(ControlPlaneStoreError::Malformed(_))
            ),
            "and a copy that meets row {seq} is refused, not handed a claim"
        );
    }
}

/// **The claim table admits only the three states, a reason exactly when
/// the state is `consumed`, and an operator's drop only on a consumed claim
/// (issue 1055).** The rung that adds the state rebuilds the table with a
/// `CHECK` on each, so nothing written with the checks on, by this build or
/// by hand, leaves a row the readers above refuse, or a drop recorded on a
/// claim whose readers do not look for one.
#[tokio::test]
async fn the_claim_table_admits_only_the_three_states_and_a_reason_only_when_consumed() {
    let (store, _dir) = open_fresh().await;
    for (seq, state, why, dropped, admitted) in [
        (1, "claimed", None::<&str>, None::<i64>, true),
        (2, "durably_started", None, None, true),
        (3, "consumed", Some("failed_for_good"), None, true),
        (4, "a_state_from_a_newer_build", None, None, false),
        (5, "consumed", None, None, false),
        (6, "claimed", Some("failed_for_good"), None, false),
        (7, "durably_started", Some("dead_lettered"), None, false),
        (8, "consumed", Some("dead_lettered"), Some(500), true),
        (9, "claimed", None, Some(500), false),
        (10, "durably_started", None, Some(500), false),
    ] {
        let written = sqlx::query(
            "INSERT INTO trigger_claim \
             (stream, stream_epoch, stream_seq, claimant, state, invocation_id, claimed_at, \
              consumed_why, operator_dropped_at) VALUES (?, ?, ?, 'worker-a', ?, 'inv', 100, ?, ?)",
        )
        .bind(key(seq).stream)
        .bind(key(seq).stream_epoch)
        .bind(seq as i64)
        .bind(state)
        .bind(why)
        .bind(dropped)
        .execute(&store.pool)
        .await;
        match written {
            Ok(_) => assert!(
                admitted,
                "{state:?} with {why:?} and a drop at {dropped:?} must be refused"
            ),
            Err(err) => assert!(
                !admitted && err.to_string().contains("CHECK constraint failed"),
                "{state:?} with {why:?} and a drop at {dropped:?}: {err}"
            ),
        }
    }
    assert_eq!(
        store
            .trigger_claim(key(8))
            .await
            .unwrap()
            .map(|row| (row.state, row.operator_dropped_at)),
        Some((
            ClaimState::Consumed {
                why: ConsumedWhy::DeadLettered
            },
            Some(500)
        )),
        "a consumed claim reads back with its reason and the drop beside it"
    );
}
