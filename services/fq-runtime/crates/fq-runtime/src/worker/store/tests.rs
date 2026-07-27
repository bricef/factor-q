//! Tiered tests:
//! - **unit**: pure functions over `Compatibility` and
//!   `DispatchStatus`. No I/O.
//! - **integration**: in-memory or tempdir SQLite. Fast,
//!   no env vars required.
//!
//! Live `fq run` acceptance — the daemon coming up cleanly
//! against an empty cache dir — is exercised by the existing
//! NATS-gated startup tests once the daemon construction in
//! `fq-cli/src/main.rs` is updated to call `WorkerStore::open`
//! (a step 3/4 follow-up).

use super::*;
use tempfile::tempdir;

/// Guard for the parallel-workers concurrency invariant (H4):
/// concurrent invocations may interleave WAL writes only because
/// every row is keyed by `invocation_id`. Tables are enumerated
/// from `sqlite_master`, not hardcoded, so a *new* table added
/// without either an invocation-id-led PK or an explicit exemption
/// below fails here loudly before it can cross-contaminate
/// invocations.
#[tokio::test]
async fn wal_tables_are_keyed_by_invocation_id() {
    // Tables that hold no per-invocation rows. Adding a table here
    // is an explicit classification decision — the point of the
    // test is that it cannot happen by omission.
    const NOT_PER_INVOCATION: &[&str] = &["schema_meta"];

    let dir = tempdir().unwrap();
    let store = WorkerStore::open(&dir.path().join("keyed.db"))
        .await
        .unwrap();
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master \
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(&store.pool)
    .await
    .unwrap();
    assert!(
        tables.len() > NOT_PER_INVOCATION.len(),
        "expected per-invocation tables beyond the exemptions; got {tables:?}"
    );

    for table in tables {
        if NOT_PER_INVOCATION.contains(&table.as_str()) {
            continue;
        }
        let columns: Vec<(String, i64)> = sqlx::query_as(&format!(
            "SELECT name, pk FROM pragma_table_info('{table}')"
        ))
        .fetch_all(&store.pool)
        .await
        .unwrap();
        let first_pk = columns
            .iter()
            .find(|(_, pk)| *pk == 1)
            .unwrap_or_else(|| panic!("{table} has no primary key"));
        assert_eq!(
            first_pk.0, "invocation_id",
            "{table}'s primary key must lead with invocation_id \
             (or be explicitly exempted as not per-invocation)"
        );
    }
}

// ----- Unit -----

#[test]
fn check_compatibility_fresh_install_when_no_row() {
    assert_eq!(check_compatibility(None, 1), Compatibility::FreshInstall);
}

#[test]
fn check_compatibility_current_when_matched() {
    assert_eq!(check_compatibility(Some(1), 1), Compatibility::Current);
    assert_eq!(check_compatibility(Some(7), 7), Compatibility::Current);
}

#[test]
fn check_compatibility_needs_upgrade_when_db_older() {
    assert_eq!(
        check_compatibility(Some(1), 3),
        Compatibility::NeedsUpgrade { from: 1 }
    );
}

#[test]
fn check_compatibility_binary_too_old_when_db_newer() {
    assert_eq!(
        check_compatibility(Some(2), 1),
        Compatibility::BinaryTooOld { db_version: 2 }
    );
}

#[test]
fn dispatch_status_round_trip() {
    for s in [
        DispatchStatus::Intent,
        DispatchStatus::Dispatched,
        DispatchStatus::Completed,
    ] {
        assert_eq!(DispatchStatus::parse(s.as_str()), Some(s));
    }
    assert_eq!(DispatchStatus::parse("garbage"), None);
}

// ----- Integration -----

async fn open_fresh() -> (WorkerStore, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("worker.db");
    let store = WorkerStore::open(&path).await.expect("open fresh");
    (store, dir)
}

/// A worker database down-projected to one historical schema version,
/// populated from a HEAD SimWorld run with all three WAL shapes #44
/// names (completed, crashed mid-flight, budget-failed).
struct PopulatedDb {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
    fixture: crate::test_support::sim::MigrationFixture,
    /// Rows projected per table, so the ladder test can assert
    /// nothing the old schema could hold is lost.
    source_counts: Vec<(&'static str, i64)>,
}

/// Copy the SimWorld-generated HEAD rows into the columns present at `version`.
/// This is deliberately mechanical: the migration history remains the schema fixture.
/// The meta bootstrap below mirrors `open()`'s fresh-install path
/// (`SCHEMA_META_SQL`, then the ladder) — keep the two in sync.
async fn populated_db_at(version: u32) -> PopulatedDb {
    use crate::test_support::sim::{MIGRATION_FIXTURE_BUDGET, SimWorld, migration_fixture_pricing};

    let world = SimWorld::with_pricing(
        44 + version as u64,
        MIGRATION_FIXTURE_BUDGET,
        migration_fixture_pricing(),
    )
    .await;
    let fixture = world.populate_for_migration_test().await;
    let source = world.worker_db_path();

    let dir = tempdir().unwrap();
    let path = dir.path().join("worker.db");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let store = WorkerStore { pool };
    for stmt in split_sql(SCHEMA_META_SQL) {
        sqlx::query(&stmt).execute(&store.pool).await.unwrap();
    }
    store.run_migrations(0, version).await.unwrap();
    store.write_schema_version(version).await.unwrap();
    sqlx::query("ATTACH DATABASE ? AS sim")
        .bind(source.to_string_lossy().as_ref())
        .execute(&store.pool)
        .await
        .unwrap();

    let mut source_counts: Vec<(&'static str, i64)> = Vec::new();
    for table in [
        "invocation_state",
        "tool_dispatch",
        "llm_dispatch",
        "host_notice",
    ] {
        let exists: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM main.sqlite_master WHERE type='table' AND name=?")
                .bind(table)
                .fetch_optional(&store.pool)
                .await
                .unwrap();
        if exists.is_none() {
            continue;
        }
        let old: Vec<String> =
            sqlx::query_scalar(&format!("SELECT name FROM pragma_table_info('{table}')"))
                .fetch_all(&store.pool)
                .await
                .unwrap();
        let current: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT name FROM pragma_table_info('{table}', 'sim')"
        ))
        .fetch_all(&store.pool)
        .await
        .unwrap();
        let common: Vec<&str> = old
            .iter()
            .map(String::as_str)
            .filter(|c| current.iter().any(|n| n == c))
            .collect();
        if !common.is_empty() {
            let columns = common.join(", ");
            sqlx::query(&format!(
                "INSERT INTO main.{table} ({columns}) SELECT {columns} FROM sim.{table}"
            ))
            .execute(&store.pool)
            .await
            .unwrap();
            let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM main.{table}"))
                .fetch_one(&store.pool)
                .await
                .unwrap();
            source_counts.push((table, count));
        }
    }
    // Preserve the renamed counter when projecting HEAD back before v6.
    if version < 6 {
        sqlx::query("UPDATE invocation_state SET iteration = (SELECT step_index FROM sim.invocation_state WHERE sim.invocation_state.invocation_id = invocation_state.invocation_id)")
            .execute(&store.pool).await.unwrap();
    }
    sqlx::query("DETACH DATABASE sim")
        .execute(&store.pool)
        .await
        .unwrap();
    store.pool.close().await;
    PopulatedDb {
        _dir: dir,
        path,
        fixture,
        source_counts,
    }
}

#[tokio::test]
async fn every_worker_migration_upgrades_populated_sim_data() {
    for from in 1..WORKER_SCHEMA_VERSION {
        let db = populated_db_at(from).await;
        let store = WorkerStore::open(&db.path)
            .await
            .unwrap_or_else(|e| panic!("v{from} migration failed: {e}"));
        assert_eq!(
            store.read_schema_version().await.unwrap(),
            Some(WORKER_SCHEMA_VERSION)
        );

        // Row preservation: nothing the old schema could hold is lost.
        for (table, expected) in &db.source_counts {
            let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(&store.pool)
                .await
                .unwrap();
            assert_eq!(count, *expected, "v{from} lost rows in {table}");
        }

        // Every WAL shape stays readable through the current readers:
        // terminal state for the completed and budget-failed rows,
        // recoverable in-flight state (with its finished LLM span)
        // for the crashed row, and the completed row's tool span.
        let ids = &db.fixture;
        for id in [&ids.completed, &ids.crashed, &ids.budget_failed] {
            assert!(
                store.get_invocation_state(id).await.unwrap().is_some(),
                "v{from}: state row {id} unreadable after migration"
            );
        }
        let in_flight: Vec<String> = store
            .find_in_flight_invocations()
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.invocation_id)
            .collect();
        assert!(
            in_flight.contains(&ids.crashed),
            "v{from}: crashed row no longer recoverable"
        );
        assert!(
            !in_flight.contains(&ids.completed),
            "v{from}: completed row regressed to in-flight"
        );
        assert!(
            !in_flight.contains(&ids.budget_failed),
            "v{from}: budget-failed row regressed to in-flight"
        );
        assert!(
            !store
                .list_tool_dispatches_for_invocation(&ids.completed)
                .await
                .unwrap()
                .is_empty(),
            "v{from}: completed tool WAL unreadable"
        );
        assert!(
            !store
                .list_llm_dispatches_for_invocation(&ids.crashed)
                .await
                .unwrap()
                .is_empty(),
            "v{from}: crashed LLM WAL unreadable"
        );

        let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(integrity, "ok", "v{from} integrity check");
        let fk_violations = sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&store.pool)
            .await
            .unwrap();
        assert!(
            fk_violations.is_empty(),
            "v{from}: {} foreign key violation(s)",
            fk_violations.len()
        );
    }
}

#[tokio::test]
async fn full_worker_ladder_preserves_populated_database() {
    // At v0 none of the worker tables exist yet, so "populated" for
    // the 0→current ladder means a foreign table the migrations must
    // leave untouched.
    let legacy_value = "pre-ladder value";
    let dir = tempdir().unwrap();
    let path = dir.path().join("worker.db");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE legacy_data (value TEXT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO legacy_data VALUES (?)")
        .bind(legacy_value)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let store = WorkerStore::open(&path).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT value FROM legacy_data")
            .fetch_one(&store.pool)
            .await
            .unwrap(),
        legacy_value
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("PRAGMA integrity_check")
            .fetch_one(&store.pool)
            .await
            .unwrap(),
        "ok"
    );
}

#[tokio::test]
async fn open_creates_tables_and_records_version() {
    let (store, _dir) = open_fresh().await;

    let v = store.read_schema_version().await.expect("read version");
    assert_eq!(v, Some(WORKER_SCHEMA_VERSION));

    // Verify each expected table exists by selecting its
    // column list (sqlite_master is the metadata table).
    for table in ["invocation_state", "tool_dispatch", "llm_dispatch"] {
        let row = sqlx::query("SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?")
            .bind(table)
            .fetch_optional(&store.pool)
            .await
            .unwrap();
        assert!(row.is_some(), "missing table {table}");
    }
}

#[tokio::test]
async fn open_against_existing_db_is_idempotent() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("worker.db");

    let _ = WorkerStore::open(&path).await.expect("first open");
    // Second open should not fail and should not re-run migrations.
    let store = WorkerStore::open(&path).await.expect("second open");
    let v = store.read_schema_version().await.unwrap();
    assert_eq!(v, Some(WORKER_SCHEMA_VERSION));
}

#[tokio::test]
async fn open_refuses_when_db_version_higher_than_binary() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("worker.db");

    // Bring the DB up to current version, then bump it
    // beyond what the binary supports.
    let store = WorkerStore::open(&path).await.unwrap();
    let future_version = WORKER_SCHEMA_VERSION + 1;
    store.write_schema_version(future_version).await.unwrap();
    drop(store);

    let err = WorkerStore::open(&path)
        .await
        .expect_err("should refuse newer DB");
    match err {
        WorkerStoreError::IncompatibleSchema {
            db_version,
            binary_version,
        } => {
            assert_eq!(db_version, future_version);
            assert_eq!(binary_version, WORKER_SCHEMA_VERSION);
        }
        other => panic!("expected IncompatibleSchema, got {other:?}"),
    }
}

#[tokio::test]
async fn open_against_v0_db_applies_migration_without_disturbing_other_tables() {
    // Simulate a pre-Step-2 database: only the projection
    // tables exist and there's no schema_meta row for
    // `worker`. Opening WorkerStore should add the worker
    // tables and stamp the version row, leaving the
    // projection tables intact.
    let dir = tempdir().unwrap();
    let path = dir.path().join("worker.db");

    // Create an existing-but-empty SQLite file with a
    // pre-existing unrelated table to stand in for a
    // pre-Step-2 layout.
    {
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let opts = SqliteConnectOptions::from_str(&url).unwrap();
        let pool = SqlitePoolOptions::new().connect_with(opts).await.unwrap();
        sqlx::query("CREATE TABLE pretend_projection (id INTEGER PRIMARY KEY, note TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pretend_projection (note) VALUES ('preserved')")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
    }

    let store = WorkerStore::open(&path).await.expect("migrate v0 -> v1");
    assert_eq!(
        store.read_schema_version().await.unwrap(),
        Some(WORKER_SCHEMA_VERSION)
    );

    // The pretend-projection table is untouched.
    let row = sqlx::query("SELECT note FROM pretend_projection")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>(0), "preserved");
}

#[tokio::test]
async fn wal_intent_dispatched_completed_round_trip() {
    let (store, _dir) = open_fresh().await;
    let inv = "inv_test_1";
    let call = "tc_a";

    store
        .write_tool_intent(inv, call, "echo", r#"{"x":1}"#, 100)
        .await
        .unwrap();
    let r = store.get_tool_dispatch(inv, call).await.unwrap().unwrap();
    assert_eq!(r.status, DispatchStatus::Intent);
    assert_eq!(r.intent_at, 100);
    assert!(r.dispatched_at.is_none());
    assert!(r.completed_at.is_none());
    assert!(r.result.is_none());

    store.write_tool_dispatched(inv, call, 200).await.unwrap();
    let r = store.get_tool_dispatch(inv, call).await.unwrap().unwrap();
    assert_eq!(r.status, DispatchStatus::Dispatched);
    assert_eq!(r.dispatched_at, Some(200));
    assert!(r.completed_at.is_none());

    store
        .write_tool_completed(inv, call, r#"{"out":"ok"}"#, false, 300)
        .await
        .unwrap();
    let r = store.get_tool_dispatch(inv, call).await.unwrap().unwrap();
    assert_eq!(r.status, DispatchStatus::Completed);
    assert_eq!(r.completed_at, Some(300));
    assert_eq!(r.is_error, Some(false));
    assert_eq!(r.result.as_deref(), Some(r#"{"out":"ok"}"#));
}

#[tokio::test]
async fn interrupted_injection_is_durable_and_idempotent() {
    let (store, _dir) = open_fresh().await;
    store
        .write_tool_intent("inv", "call", "exec", "{}", 100)
        .await
        .unwrap();
    store
        .write_tool_dispatched("inv", "call", 1_700_000_000_000)
        .await
        .unwrap();

    assert_eq!(
        store.inject_interrupted_results("inv").await.unwrap(),
        vec!["call"]
    );
    let first = store
        .get_tool_dispatch("inv", "call")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.status, DispatchStatus::Completed);
    let payload = first.result.unwrap();
    assert!(payload.contains(r#""interrupted":true"#));
    assert!(payload.contains("2023-11-14T22:13:20+00:00"));

    assert!(
        store
            .inject_interrupted_results("inv")
            .await
            .unwrap()
            .is_empty()
    );
    let replay = store
        .get_tool_dispatch("inv", "call")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replay.result.as_deref(), Some(payload.as_str()));
}

#[tokio::test]
async fn wal_dispatched_without_intent_fails() {
    let (store, _dir) = open_fresh().await;
    let err = store
        .write_tool_dispatched("missing_inv", "missing_call", 1)
        .await
        .expect_err("transition should fail");
    assert!(matches!(err, WorkerStoreError::WalTransitionFailed { .. }));
}

#[tokio::test]
async fn wal_completed_without_dispatched_fails() {
    let (store, _dir) = open_fresh().await;
    store
        .write_tool_intent("inv1", "tc1", "shell", "{}", 1)
        .await
        .unwrap();
    // Skip the `dispatched` step.
    let err = store
        .write_tool_completed("inv1", "tc1", "{}", false, 5)
        .await
        .expect_err("must fail without dispatched");
    assert!(matches!(err, WorkerStoreError::WalTransitionFailed { .. }));
}

#[tokio::test]
async fn find_ambiguous_returns_only_dispatched() {
    let (store, _dir) = open_fresh().await;

    // intent only — not ambiguous (safe-resume).
    store
        .write_tool_intent("inv1", "a", "shell", "{}", 1)
        .await
        .unwrap();

    // dispatched without completed — ambiguous.
    store
        .write_tool_intent("inv2", "b", "shell", "{}", 2)
        .await
        .unwrap();
    store.write_tool_dispatched("inv2", "b", 3).await.unwrap();

    // fully completed — safe-replay.
    store
        .write_tool_intent("inv3", "c", "shell", "{}", 4)
        .await
        .unwrap();
    store.write_tool_dispatched("inv3", "c", 5).await.unwrap();
    store
        .write_tool_completed("inv3", "c", "{}", false, 6)
        .await
        .unwrap();

    let ambiguous = store.find_ambiguous_tool_dispatches().await.unwrap();
    assert_eq!(ambiguous.len(), 1);
    assert_eq!(ambiguous[0].invocation_id, "inv2");
    assert_eq!(ambiguous[0].tool_call_id, "b");
}

#[tokio::test]
async fn invocation_state_upsert_round_trip() {
    let (store, _dir) = open_fresh().await;
    let row = InvocationStateRow {
        invocation_id: "inv-x".to_string(),
        agent_id: "agent-y".to_string(),
        schema_version: 1,
        phase: "awaiting_model".to_string(),
        state_blob: b"{\"phase\":\"awaiting_model\"}".to_vec(),
        step_index: 2,
        started_at: 1_000,
        updated_at: 1_010,
        terminal_at: None,
        workspace_ref: None,
        archive_status: None,
        archive_published_at: None,
        trigger_source: Some("subject".to_string()),
        trigger_subject: Some("fq.agent.agent-y.trigger".to_string()),
        trigger_payload: Some("{\"ask\":\"review the docs\"}".to_string()),
    };
    store.upsert_invocation_state(&row).await.unwrap();
    let back = store.get_invocation_state("inv-x").await.unwrap().unwrap();
    assert_eq!(back, row);

    // Update — same key, different phase + updated_at.
    let mut updated = row.clone();
    updated.phase = "dispatching_tools".to_string();
    updated.step_index = 3;
    updated.updated_at = 1_050;
    store.upsert_invocation_state(&updated).await.unwrap();
    let back2 = store.get_invocation_state("inv-x").await.unwrap().unwrap();
    assert_eq!(back2, updated);
}

#[tokio::test]
async fn mark_ambiguous_reported_claims_exactly_once() {
    let (store, _dir) = open_fresh().await;
    let row = InvocationStateRow {
        invocation_id: "inv-amb".to_string(),
        agent_id: "agent-y".to_string(),
        schema_version: 1,
        phase: "awaiting_model".to_string(),
        state_blob: b"{}".to_vec(),
        step_index: 0,
        started_at: 1_000,
        updated_at: 1_000,
        terminal_at: None,
        workspace_ref: None,
        archive_status: None,
        archive_published_at: None,
        trigger_source: None,
        trigger_subject: None,
        trigger_payload: None,
    };
    store.upsert_invocation_state(&row).await.unwrap();

    // First claim wins; the second (a restart re-classifying the
    // same invocation) must not re-fire.
    assert!(
        store
            .mark_ambiguous_reported("inv-amb", 2_000)
            .await
            .unwrap()
    );
    assert!(
        !store
            .mark_ambiguous_reported("inv-amb", 3_000)
            .await
            .unwrap()
    );

    // No row → nothing in recovery limbo → no claim.
    assert!(
        !store
            .mark_ambiguous_reported("inv-gone", 2_000)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn find_in_flight_excludes_terminal_rows() {
    let (store, _dir) = open_fresh().await;
    let alive = InvocationStateRow {
        invocation_id: "alive".to_string(),
        agent_id: "a".to_string(),
        schema_version: 1,
        phase: "awaiting_model".to_string(),
        state_blob: vec![],
        step_index: 0,
        started_at: 1,
        updated_at: 1,
        terminal_at: None,
        workspace_ref: None,
        archive_status: None,
        archive_published_at: None,
        trigger_source: None,
        trigger_subject: None,
        trigger_payload: None,
    };
    let mut done = alive.clone();
    done.invocation_id = "done".to_string();
    done.phase = "done".to_string();
    done.terminal_at = Some(2);

    store.upsert_invocation_state(&alive).await.unwrap();
    store.upsert_invocation_state(&done).await.unwrap();

    let in_flight = store.find_in_flight_invocations().await.unwrap();
    let ids: Vec<_> = in_flight.iter().map(|r| r.invocation_id.as_str()).collect();
    assert_eq!(ids, vec!["alive"]);
}

#[tokio::test]
async fn operator_terminal_marker_is_sticky_against_late_upsert() {
    let (store, _dir) = open_fresh().await;
    let row = InvocationStateRow {
        invocation_id: "dropped".to_string(),
        agent_id: "a".to_string(),
        schema_version: 1,
        phase: "dispatching_tools".to_string(),
        state_blob: vec![],
        step_index: 25,
        started_at: 1,
        updated_at: 2,
        terminal_at: None,
        workspace_ref: None,
        archive_status: None,
        archive_published_at: None,
        trigger_source: None,
        trigger_subject: None,
        trigger_payload: None,
    };
    store.upsert_invocation_state(&row).await.unwrap();
    assert_eq!(
        store
            .mark_invocation_operator_terminal("dropped", "failed", 3)
            .await
            .unwrap(),
        1
    );

    let mut late_worker_write = row;
    late_worker_write.updated_at = 4;
    store
        .upsert_invocation_state(&late_worker_write)
        .await
        .unwrap();
    let stored = store
        .get_invocation_state("dropped")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.phase, "failed");
    assert_eq!(stored.terminal_at, Some(3));
    assert!(store.find_in_flight_invocations().await.unwrap().is_empty());
}

#[tokio::test]
async fn delete_invocation_state_removes_row() {
    let (store, _dir) = open_fresh().await;
    let row = InvocationStateRow {
        invocation_id: "to-delete".to_string(),
        agent_id: "a".to_string(),
        schema_version: 1,
        phase: "awaiting_model".to_string(),
        state_blob: vec![],
        step_index: 0,
        started_at: 1,
        updated_at: 1,
        terminal_at: Some(2),
        workspace_ref: None,
        archive_status: None,
        archive_published_at: None,
        trigger_source: None,
        trigger_subject: None,
        trigger_payload: None,
    };
    store.upsert_invocation_state(&row).await.unwrap();
    let n = store.delete_invocation_state("to-delete").await.unwrap();
    assert_eq!(n, 1);
    assert!(
        store
            .get_invocation_state("to-delete")
            .await
            .unwrap()
            .is_none()
    );
}

fn terminal_state_row(id: &str, terminal_at_ms: i64) -> InvocationStateRow {
    InvocationStateRow {
        invocation_id: id.to_string(),
        agent_id: "a".to_string(),
        schema_version: 1,
        phase: "completed".to_string(),
        state_blob: vec![],
        step_index: 0,
        started_at: 1,
        updated_at: terminal_at_ms,
        terminal_at: Some(terminal_at_ms),
        workspace_ref: None,
        archive_status: None,
        archive_published_at: None,
        trigger_source: None,
        trigger_subject: None,
        trigger_payload: None,
    }
}

#[tokio::test]
async fn set_archive_pending_marks_terminal_row_pending() {
    let (store, _dir) = open_fresh().await;
    store
        .upsert_invocation_state(&terminal_state_row("inv-1", 100))
        .await
        .unwrap();

    let updated = store.set_archive_pending("inv-1", 200).await.unwrap();
    assert_eq!(updated, 1);

    let back = store.get_invocation_state("inv-1").await.unwrap().unwrap();
    assert_eq!(back.archive_status.as_deref(), Some("pending"));
    assert_eq!(back.archive_published_at, Some(200));
}

#[tokio::test]
async fn set_archive_pending_no_op_on_non_terminal_row() {
    // Guards the `terminal_at IS NOT NULL` WHERE clause: an
    // archive flow only makes sense after terminal. If a
    // non-terminal row somehow reaches this path it must
    // not be marked pending.
    let (store, _dir) = open_fresh().await;
    let mut row = terminal_state_row("inv-still-going", 100);
    row.terminal_at = None;
    row.phase = "awaiting_model".to_string();
    store.upsert_invocation_state(&row).await.unwrap();

    let updated = store
        .set_archive_pending("inv-still-going", 200)
        .await
        .unwrap();
    assert_eq!(updated, 0);

    let back = store
        .get_invocation_state("inv-still-going")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(back.archive_status, None);
    assert_eq!(back.archive_published_at, None);
}

#[tokio::test]
async fn set_archive_pending_bumps_published_at_on_retry() {
    // Re-calling is the retry sweeper's primary action; it
    // should leave the row pending and bump the published-at
    // so the next retry-window check measures from now.
    let (store, _dir) = open_fresh().await;
    store
        .upsert_invocation_state(&terminal_state_row("inv-1", 100))
        .await
        .unwrap();

    store.set_archive_pending("inv-1", 200).await.unwrap();
    store.set_archive_pending("inv-1", 250).await.unwrap();

    let back = store.get_invocation_state("inv-1").await.unwrap().unwrap();
    assert_eq!(back.archive_status.as_deref(), Some("pending"));
    assert_eq!(back.archive_published_at, Some(250));
}

#[tokio::test]
async fn list_archive_pending_returns_terminal_rows_in_published_order() {
    let (store, _dir) = open_fresh().await;

    // One in-flight row — must not appear.
    let mut alive = terminal_state_row("alive", 100);
    alive.terminal_at = None;
    alive.phase = "awaiting_model".to_string();
    store.upsert_invocation_state(&alive).await.unwrap();

    // Two terminal-pending rows with different publish times.
    store
        .upsert_invocation_state(&terminal_state_row("older", 100))
        .await
        .unwrap();
    store
        .upsert_invocation_state(&terminal_state_row("newer", 100))
        .await
        .unwrap();
    store.set_archive_pending("newer", 250).await.unwrap();
    store.set_archive_pending("older", 200).await.unwrap();

    // One terminal row that has not yet been published — the
    // transient "terminal but pre-publish" sliver. The
    // sweeper should see it and republish, so it ranks
    // before pending rows.
    store
        .upsert_invocation_state(&terminal_state_row("no-publish-yet", 100))
        .await
        .unwrap();

    let pending = store.list_archive_pending().await.unwrap();
    let ids: Vec<_> = pending.iter().map(|r| r.invocation_id.as_str()).collect();
    assert_eq!(ids, vec!["no-publish-yet", "older", "newer"]);
}

#[tokio::test]
async fn llm_wal_intent_dispatched_completed_round_trip() {
    let (store, _dir) = open_fresh().await;
    let inv = "inv_llm_1";
    let req = "req_a";

    store
        .write_llm_intent(inv, req, "claude-haiku", r#"{"messages":[]}"#, 100)
        .await
        .unwrap();
    let r = store.get_llm_dispatch(inv, req).await.unwrap().unwrap();
    assert_eq!(r.status, DispatchStatus::Intent);
    assert_eq!(r.model, "claude-haiku");
    assert!(r.dispatched_at.is_none());
    assert!(r.response.is_none());

    store.write_llm_dispatched(inv, req, 200).await.unwrap();
    let r = store.get_llm_dispatch(inv, req).await.unwrap().unwrap();
    assert_eq!(r.status, DispatchStatus::Dispatched);

    store
        .write_llm_completed(inv, req, r#"{"content":"hi"}"#, false, 0.0011, 300)
        .await
        .unwrap();
    let r = store.get_llm_dispatch(inv, req).await.unwrap().unwrap();
    assert_eq!(r.status, DispatchStatus::Completed);
    assert_eq!(r.cost_usd, Some(0.0011));
    assert_eq!(r.is_error, Some(false));
    assert_eq!(r.response.as_deref(), Some(r#"{"content":"hi"}"#));
}

#[tokio::test]
async fn llm_completed_with_error_round_trip() {
    let (store, _dir) = open_fresh().await;
    store
        .write_llm_intent("inv-err", "r-err", "haiku", "{}", 1)
        .await
        .unwrap();
    store
        .write_llm_dispatched("inv-err", "r-err", 2)
        .await
        .unwrap();
    store
        .write_llm_completed("inv-err", "r-err", "rate limited", true, 0.0, 3)
        .await
        .unwrap();
    let r = store
        .get_llm_dispatch("inv-err", "r-err")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r.status, DispatchStatus::Completed);
    assert_eq!(r.is_error, Some(true));
    assert_eq!(r.cost_usd, Some(0.0));
}

#[tokio::test]
async fn v1_to_v2_migration_adds_is_error_column() {
    // Build a DB at schema v1 (the worker tables without
    // the `is_error` column on `llm_dispatch`), then open
    // it with the current binary and verify the migration
    // adds the column without disturbing existing rows.
    let dir = tempdir().unwrap();
    let path = dir.path().join("worker.db");

    // Manually construct a v1 DB.
    {
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let opts = SqliteConnectOptions::from_str(&url).unwrap();
        let pool = SqlitePoolOptions::new().connect_with(opts).await.unwrap();
        for stmt in split_sql(SCHEMA_META_SQL) {
            sqlx::query(&stmt).execute(&pool).await.unwrap();
        }
        for stmt in split_sql(WORKER_TABLES_V1_SQL) {
            sqlx::query(&stmt).execute(&pool).await.unwrap();
        }
        sqlx::query("INSERT INTO schema_meta (class, version, updated_at) VALUES (?, ?, ?)")
            .bind(SCHEMA_CLASS)
            .bind(1_i64)
            .bind(0_i64)
            .execute(&pool)
            .await
            .unwrap();
        // Pre-existing v1 row to ensure migration preserves data.
        sqlx::query(
            "INSERT INTO llm_dispatch (invocation_id, request_id, model, status, request_payload, intent_at) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind("legacy-inv")
        .bind("legacy-req")
        .bind("claude-haiku")
        .bind("intent")
        .bind("{}")
        .bind(1_i64)
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
    }

    // Open with current binary — runs v1 → v2 migration.
    let store = WorkerStore::open(&path).await.expect("migrate v1 -> v2");
    assert_eq!(
        store.read_schema_version().await.unwrap(),
        Some(WORKER_SCHEMA_VERSION)
    );

    // Existing row preserved.
    let pre = store
        .get_llm_dispatch("legacy-inv", "legacy-req")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pre.status, DispatchStatus::Intent);
    assert_eq!(pre.is_error, None);

    // New writes can use the is_error column.
    store
        .write_llm_dispatched("legacy-inv", "legacy-req", 10)
        .await
        .unwrap();
    store
        .write_llm_completed("legacy-inv", "legacy-req", "ok", false, 0.001, 20)
        .await
        .unwrap();
    let post = store
        .get_llm_dispatch("legacy-inv", "legacy-req")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(post.is_error, Some(false));
}

#[tokio::test]
async fn find_ambiguous_llm_returns_only_dispatched() {
    let (store, _dir) = open_fresh().await;

    // intent only — safe-resume.
    store
        .write_llm_intent("inv1", "r1", "haiku", "{}", 1)
        .await
        .unwrap();

    // dispatched without completed — ambiguous.
    store
        .write_llm_intent("inv2", "r2", "haiku", "{}", 2)
        .await
        .unwrap();
    store.write_llm_dispatched("inv2", "r2", 3).await.unwrap();

    // fully completed — safe-replay.
    store
        .write_llm_intent("inv3", "r3", "haiku", "{}", 4)
        .await
        .unwrap();
    store.write_llm_dispatched("inv3", "r3", 5).await.unwrap();
    store
        .write_llm_completed("inv3", "r3", "{}", false, 0.0, 6)
        .await
        .unwrap();

    let ambiguous = store.find_ambiguous_llm_dispatches().await.unwrap();
    assert_eq!(ambiguous.len(), 1);
    assert_eq!(ambiguous[0].invocation_id, "inv2");
    assert_eq!(ambiguous[0].request_id, "r2");
}

#[tokio::test]
async fn v2_to_v3_migration_adds_workspace_ref_column() {
    // Pre-populate a v2 DB (initial tables + the v2
    // is_error column on llm_dispatch, but no workspace_ref
    // on invocation_state). Open with current binary;
    // verify the v3 migration adds workspace_ref without
    // disturbing existing rows.
    let dir = tempdir().unwrap();
    let path = dir.path().join("worker.db");

    {
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let opts = SqliteConnectOptions::from_str(&url).unwrap();
        let pool = SqlitePoolOptions::new().connect_with(opts).await.unwrap();
        for stmt in split_sql(SCHEMA_META_SQL) {
            sqlx::query(&stmt).execute(&pool).await.unwrap();
        }
        for stmt in split_sql(WORKER_TABLES_V1_SQL) {
            sqlx::query(&stmt).execute(&pool).await.unwrap();
        }
        for stmt in split_sql(WORKER_MIGRATION_V2_SQL) {
            sqlx::query(&stmt).execute(&pool).await.unwrap();
        }
        sqlx::query("INSERT INTO schema_meta (class, version, updated_at) VALUES (?, ?, ?)")
            .bind(SCHEMA_CLASS)
            .bind(2_i64)
            .bind(0_i64)
            .execute(&pool)
            .await
            .unwrap();
        // Pre-existing v2 row.
        sqlx::query(
            "INSERT INTO invocation_state (invocation_id, agent_id, schema_version, phase, state_blob, iteration, started_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("legacy-inv")
        .bind("a")
        .bind(1_i64)
        .bind("awaiting_model")
        .bind(b"".as_slice())
        .bind(0_i64)
        .bind(1_i64)
        .bind(1_i64)
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
    }

    let store = WorkerStore::open(&path).await.expect("migrate v2 -> v3");
    assert_eq!(
        store.read_schema_version().await.unwrap(),
        Some(WORKER_SCHEMA_VERSION)
    );

    // Existing row preserved; workspace_ref reads as None.
    let pre = store
        .get_invocation_state("legacy-inv")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pre.workspace_ref, None);

    // Future writes can populate workspace_ref.
    let mut updated = pre.clone();
    updated.workspace_ref = Some("placeholder-ref".to_string());
    store.upsert_invocation_state(&updated).await.unwrap();
    let post = store
        .get_invocation_state("legacy-inv")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(post.workspace_ref, Some("placeholder-ref".to_string()));
}

#[tokio::test]
async fn v3_to_v4_migration_adds_archive_columns() {
    // Pre-populate a v3 DB (initial tables + v2 is_error
    // + v3 workspace_ref, but no archive_status /
    // archive_published_at). Open with current binary;
    // verify the v4 migration adds the archive columns
    // without disturbing existing rows.
    let dir = tempdir().unwrap();
    let path = dir.path().join("worker.db");

    {
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let opts = SqliteConnectOptions::from_str(&url).unwrap();
        let pool = SqlitePoolOptions::new().connect_with(opts).await.unwrap();
        for stmt in split_sql(SCHEMA_META_SQL) {
            sqlx::query(&stmt).execute(&pool).await.unwrap();
        }
        for stmt in split_sql(WORKER_TABLES_V1_SQL) {
            sqlx::query(&stmt).execute(&pool).await.unwrap();
        }
        for stmt in split_sql(WORKER_MIGRATION_V2_SQL) {
            sqlx::query(&stmt).execute(&pool).await.unwrap();
        }
        for stmt in split_sql(WORKER_MIGRATION_V3_SQL) {
            sqlx::query(&stmt).execute(&pool).await.unwrap();
        }
        sqlx::query("INSERT INTO schema_meta (class, version, updated_at) VALUES (?, ?, ?)")
            .bind(SCHEMA_CLASS)
            .bind(3_i64)
            .bind(0_i64)
            .execute(&pool)
            .await
            .unwrap();
        // Pre-existing v3 terminal row.
        sqlx::query(
            "INSERT INTO invocation_state (invocation_id, agent_id, schema_version, phase, state_blob, iteration, started_at, updated_at, terminal_at, workspace_ref) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("legacy-terminal")
        .bind("a")
        .bind(1_i64)
        .bind("completed")
        .bind(b"".as_slice())
        .bind(0_i64)
        .bind(1_i64)
        .bind(2_i64)
        .bind(2_i64)
        .bind::<Option<String>>(None)
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
    }

    let store = WorkerStore::open(&path).await.expect("migrate v3 -> v4");
    assert_eq!(
        store.read_schema_version().await.unwrap(),
        Some(WORKER_SCHEMA_VERSION)
    );

    // Existing terminal row preserved; archive columns
    // read as None — pre-existing rows weren't part of the
    // hand-off flow and stay that way.
    let pre = store
        .get_invocation_state("legacy-terminal")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pre.archive_status, None);
    assert_eq!(pre.archive_published_at, None);

    // The new write path can flip the legacy terminal row
    // into archive-pending, exercising the migrated
    // columns.
    store
        .set_archive_pending("legacy-terminal", 999)
        .await
        .unwrap();
    let post = store
        .get_invocation_state("legacy-terminal")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(post.archive_status.as_deref(), Some("pending"));
    assert_eq!(post.archive_published_at, Some(999));
}

#[tokio::test]
async fn v6_to_v7_migration_adds_host_notice_table() {
    // Pre-populate a v6 DB (initial tables + every migration
    // through v6, no host_notice table). Open with the current
    // binary; verify the v7 migration creates the table without
    // disturbing existing rows.
    let dir = tempdir().unwrap();
    let path = dir.path().join("worker.db");

    {
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let opts = SqliteConnectOptions::from_str(&url).unwrap();
        let pool = SqlitePoolOptions::new().connect_with(opts).await.unwrap();
        for sql in [
            SCHEMA_META_SQL,
            WORKER_TABLES_V1_SQL,
            WORKER_MIGRATION_V2_SQL,
            WORKER_MIGRATION_V3_SQL,
            WORKER_MIGRATION_V4_SQL,
            WORKER_MIGRATION_V5_SQL,
            WORKER_MIGRATION_V6_SQL,
        ] {
            for stmt in split_sql(sql) {
                sqlx::query(&stmt).execute(&pool).await.unwrap();
            }
        }
        sqlx::query("INSERT INTO schema_meta (class, version, updated_at) VALUES (?, ?, ?)")
            .bind(SCHEMA_CLASS)
            .bind(6_i64)
            .bind(0_i64)
            .execute(&pool)
            .await
            .unwrap();
        // Pre-existing v6 row (post-rename: `step_index`).
        sqlx::query(
            "INSERT INTO invocation_state (invocation_id, agent_id, schema_version, phase, state_blob, step_index, started_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("legacy-inv")
        .bind("a")
        .bind(1_i64)
        .bind("awaiting_model")
        .bind(b"".as_slice())
        .bind(0_i64)
        .bind(1_i64)
        .bind(1_i64)
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
    }

    let store = WorkerStore::open(&path).await.expect("migrate v6 -> v7");
    assert_eq!(
        store.read_schema_version().await.unwrap(),
        Some(WORKER_SCHEMA_VERSION)
    );

    // Existing row preserved.
    assert!(
        store
            .get_invocation_state("legacy-inv")
            .await
            .unwrap()
            .is_some()
    );

    // The migrated table serves the new write path.
    store
        .insert_host_notice(
            "legacy-inv",
            0,
            0,
            "resume",
            "<host-notice>hello</host-notice>",
            5,
        )
        .await
        .unwrap();
    let rows = store.list_host_notices("legacy-inv").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].body, "<host-notice>hello</host-notice>");
}

/// Insert/list round-trip: rows come back ordered by
/// `(step_index, seq)` regardless of insertion order, with every
/// column intact — the order a replay re-injects them in.
#[tokio::test]
async fn host_notice_round_trip_orders_by_step_then_seq() {
    let dir = tempdir().unwrap();
    let store = WorkerStore::open(&dir.path().join("worker.db"))
        .await
        .unwrap();

    // Deliberately inserted out of order.
    for (step, seq, kind, body) in [
        (
            3_u32,
            1_u32,
            "context_pressure",
            "<host-notice>c</host-notice>",
        ),
        (0, 0, "resume", "<host-notice>a</host-notice>"),
        (3, 0, "tools_changed", "<host-notice>b</host-notice>"),
    ] {
        store
            .insert_host_notice("inv-1", step, seq, kind, body, 42)
            .await
            .unwrap();
    }
    // A different invocation's rows must not bleed in.
    store
        .insert_host_notice("inv-2", 0, 0, "resume", "<host-notice>x</host-notice>", 42)
        .await
        .unwrap();

    let rows = store.list_host_notices("inv-1").await.unwrap();
    let summary: Vec<(u32, u32, &str, &str)> = rows
        .iter()
        .map(|r| (r.step_index, r.seq, r.kind.as_str(), r.body.as_str()))
        .collect();
    assert_eq!(
        summary,
        vec![
            (0, 0, "resume", "<host-notice>a</host-notice>"),
            (3, 0, "tools_changed", "<host-notice>b</host-notice>"),
            (3, 1, "context_pressure", "<host-notice>c</host-notice>"),
        ]
    );
    assert!(rows.iter().all(|r| r.invocation_id == "inv-1"));
    assert!(rows.iter().all(|r| r.created_at == 42));

    // The composite key is a real constraint: re-inserting an
    // existing (invocation, step, seq) fails loudly rather than
    // silently rewriting history.
    let dup = store
        .insert_host_notice(
            "inv-1",
            0,
            0,
            "resume",
            "<host-notice>dup</host-notice>",
            43,
        )
        .await;
    assert!(dup.is_err(), "duplicate key must be rejected");
}

#[tokio::test]
async fn open_read_only_refuses_missing_file() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("does-not-exist.db");
    let err = WorkerStore::open_read_only(&path)
        .await
        .expect_err("missing file");
    assert!(matches!(err, WorkerStoreError::NotInitialised(_)));
}
#[tokio::test]
async fn v8_to_v9_migration_preserves_rows_and_adds_shared_sequence() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("worker-v8.db");
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let opts = SqliteConnectOptions::from_str(&url).unwrap();
    let pool = SqlitePoolOptions::new().connect_with(opts).await.unwrap();
    for stmt in split_sql(SCHEMA_META_SQL) {
        sqlx::query(&stmt).execute(&pool).await.unwrap();
    }
    for stmt in split_sql(WORKER_TABLES_V1_SQL) {
        sqlx::query(&stmt).execute(&pool).await.unwrap();
    }
    for migration in [
        WORKER_MIGRATION_V2_SQL,
        WORKER_MIGRATION_V3_SQL,
        WORKER_MIGRATION_V4_SQL,
        WORKER_MIGRATION_V5_SQL,
        WORKER_MIGRATION_V6_SQL,
        WORKER_MIGRATION_V7_SQL,
        WORKER_MIGRATION_V8_SQL,
    ] {
        for stmt in split_sql(migration) {
            sqlx::query(&stmt).execute(&pool).await.unwrap();
        }
    }
    sqlx::query("INSERT INTO schema_meta (class, version, updated_at) VALUES (?, 8, 0)")
        .bind(SCHEMA_CLASS)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO tool_dispatch (invocation_id, tool_call_id, tool_name, status, parameters, intent_at) VALUES ('inv', 'old', 't', 'intent', '{}', 1)")
        .execute(&pool).await.unwrap();
    pool.close().await;

    let store = WorkerStore::open(&path).await.unwrap();
    let old = store
        .get_tool_dispatch("inv", "old")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.seq, None, "pre-v9 rows use timestamp fallback");
    store
        .write_llm_intent("inv", "llm", "m", "{}", 2)
        .await
        .unwrap();
    store.write_llm_dispatched("inv", "llm", 3).await.unwrap();
    store
        .write_llm_completed("inv", "llm", "{}", false, 0.0, 4)
        .await
        .unwrap();
    store.write_tool_dispatched("inv", "old", 5).await.unwrap();
    store
        .write_tool_completed("inv", "old", "{}", false, 4)
        .await
        .unwrap();
    assert_eq!(
        store
            .get_llm_dispatch("inv", "llm")
            .await
            .unwrap()
            .unwrap()
            .seq,
        Some(1)
    );
    assert_eq!(
        store
            .get_tool_dispatch("inv", "old")
            .await
            .unwrap()
            .unwrap()
            .seq,
        Some(2)
    );
}
