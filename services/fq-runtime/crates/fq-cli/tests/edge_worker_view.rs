//! The Worker view over the edge (plan Phase 4, verbs 21/22): Get
//! answers with the fold — roster row plus every invocation the worker
//! owns — and List answers with the view's index rows, narrowed by a
//! typed filter the DAEMON applies.
//!
//! That last part is the behaviour this file exists for. `fq workers
//! list --stale-only/--alive-only` used to pull the whole roster and
//! sieve it in the client, so the filter had no wire contract to get
//! wrong and nothing to test. Now it is part of the surface: an
//! unrecognised status is a verdict on the request (`InvalidInput`
//! naming the accepted set), never an empty list the caller would
//! misread as "no such workers".
//!
//! The assertions are chosen to be independent of the daemon's
//! stale-worker sweep, which runs on its own tick and would make any
//! claim about an `alive` seeded worker a race: `shutdown` rows are
//! never swept, the daemon's own worker is sweep-exempt by
//! construction, and "the filter never widens the answer" holds
//! whatever the sweep has done.

#![cfg(unix)]

use std::process::{Command, Stdio};
use std::time::Duration;

use fq_ops::{Domain, OpId};
use serde_json::json;

/// Epoch for the seeded rows: fixed, and far enough in the past that
/// nothing here depends on wall-clock drift.
const BASE_MS: i64 = 1_767_323_045_000;

const OWNED_INVOCATION: &str = "5b000000-0000-7000-8000-000000000005";

fn unique_scratch() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("edge-worker-{}-{}", std::process::id(), nanos));
    std::fs::create_dir_all(dir.join("cache")).unwrap();
    std::fs::create_dir_all(dir.join("agents")).unwrap();
    std::fs::write(dir.join("fq.toml"), "[edge]\nbind = \"127.0.0.1:0\"\n").unwrap();
    dir
}

fn suffix_of<'a>(log: &'a str, prefix: &str) -> &'a str {
    log.lines()
        .find_map(|l| l.trim().strip_prefix(prefix))
        .unwrap_or_else(|| panic!("log lacks prefix {prefix:?}\n--- log ---\n{log}"))
        .trim()
}

fn parse_fingerprint(hex: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex fingerprint");
    }
    out
}

/// The roster the daemon will serve: one stale worker, one shut-down
/// worker owning one completed invocation. Written before the daemon
/// starts, so it is opening stores that already hold these rows.
async fn seed_roster(cache: &std::path::Path) {
    use fq_runtime::control_plane::store::{ControlPlaneStore, OwnerStatus};

    let paths = fq_runtime::db::RuntimeDbPaths::under(cache);
    let cp = ControlPlaneStore::open(&paths.control_plane)
        .await
        .expect("open control plane");
    cp.register_worker("worker-alpha", "probe-host", BASE_MS)
        .await
        .expect("register alpha");
    assert!(
        cp.mark_worker_stale("worker-alpha").await.expect("mark"),
        "alpha must transition alive→stale"
    );
    cp.register_worker("worker-omega", "probe-host", BASE_MS + 1_000)
        .await
        .expect("register omega");
    cp.mark_worker_shutdown("worker-omega")
        .await
        .expect("shutdown omega");
    // Terminal ownership: the sweep and startup recovery both leave
    // it alone, so the Get fold below is stable.
    cp.upsert_invocation_ownership(
        OWNED_INVOCATION,
        "worker-omega",
        BASE_MS + 2_000,
        OwnerStatus::Completed,
    )
    .await
    .expect("assign ownership");
}

fn ids_of(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .expect("List answers with an array")
        .iter()
        .map(|row| row["worker_id"].as_str().expect("worker_id").to_string())
        .collect()
}

fn statuses_of(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .expect("List answers with an array")
        .iter()
        .map(|row| row["status"].as_str().expect("status").to_string())
        .collect()
}

#[tokio::test]
async fn the_worker_view_folds_and_filters_daemon_side() {
    let server = fq_test_support::NatsServer::start();
    let scratch = unique_scratch();
    seed_roster(&scratch.join("cache")).await;

    let log_path = scratch.join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let log_err = log.try_clone().expect("clone log handle");
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_fqd"))
        .env("FQ_CONFIG", scratch.join("fq.toml"))
        .env("FQ_NATS_URL", server.url())
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .env("RUST_LOG", "off")
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .expect("spawn fqd");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let text = loop {
        if let Some(status) = daemon.try_wait().expect("poll fqd") {
            let text = std::fs::read_to_string(&log_path).unwrap_or_default();
            panic!("fqd exited during startup with {status:?}\n--- log ---\n{text}");
        }
        let text = std::fs::read_to_string(&log_path).unwrap_or_default();
        if text.contains("Runtime ready") {
            break text;
        }
        assert!(tokio::time::Instant::now() < deadline, "fqd never ready");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let fingerprint = parse_fingerprint(suffix_of(
        &text,
        "edge: certificate fingerprint (clients pin this): ",
    ));
    let token = {
        let mut lines = text.lines();
        lines.find(|l| l.contains("edge: admin token")).unwrap();
        lines.next().unwrap().trim().to_string()
    };
    let addr = suffix_of(&text, "- edge is listening on ").to_string();
    // The daemon self-registers its own worker at startup and the
    // sweep skips it, so it is the one row guaranteed `alive`.
    let self_worker = suffix_of(&text, "worker:")
        .split_whitespace()
        .next()
        .expect("worker id in the banner")
        .to_string();

    let client = fq_edge::EdgeClient::connect(&addr, fingerprint, &token)
        .await
        .expect("connect edge");

    let list = |filter: serde_json::Value| {
        let client = &client;
        async move {
            client
                .rpc
                .invoke(
                    tarpc::context::current(),
                    fq_edge::InvokeRequest {
                        op: OpId::List(Domain::Worker),
                        version: 1,
                        input: filter,
                        min_seq: None,
                    },
                )
                .await
                .expect("rpc")
        }
    };

    // Unfiltered: the whole roster, seeded rows and the daemon's own.
    let all = list(json!({})).await.expect("worker.list").output;
    let all_ids = ids_of(&all);
    for expected in ["worker-alpha", "worker-omega", self_worker.as_str()] {
        assert!(
            all_ids.iter().any(|id| id == expected),
            "unfiltered list must carry {expected}: {all_ids:?}"
        );
    }

    // The narrowing invariant, independent of the sweep: a filtered
    // answer is a subset of the unfiltered one, and every row in it
    // carries the status that was asked for.
    for status in ["alive", "stale", "shutdown"] {
        let narrowed = list(json!({ "status": status }))
            .await
            .expect("filtered worker.list")
            .output;
        assert!(
            statuses_of(&narrowed).iter().all(|s| s == status),
            "worker.list({status}) answered with foreign statuses: {:?}",
            statuses_of(&narrowed)
        );
        assert!(
            ids_of(&narrowed).iter().all(|id| all_ids.contains(id)),
            "a filter may only narrow: {:?} ⊄ {all_ids:?}",
            ids_of(&narrowed)
        );
    }

    // Exactly one shut-down worker, and shutdown rows are never
    // swept — so this is an equality, not a containment.
    let shutdown = list(json!({ "status": "shutdown" }))
        .await
        .expect("worker.list(shutdown)")
        .output;
    assert_eq!(ids_of(&shutdown), vec!["worker-omega".to_string()]);

    // The daemon's own row is sweep-exempt, so `alive` always has it
    // and never has the two seeded rows.
    let alive_ids = ids_of(
        &list(json!({ "status": "alive" }))
            .await
            .expect("alive")
            .output,
    );
    assert!(
        alive_ids.contains(&self_worker),
        "the daemon's own worker is alive: {alive_ids:?}"
    );
    assert!(
        !alive_ids.iter().any(|id| id == "worker-omega"),
        "a shut-down worker is not alive: {alive_ids:?}"
    );

    // An unrecognised status is a verdict on the request, not an
    // empty answer.
    let err = list(json!({ "status": "sleepy" }))
        .await
        .expect_err("unknown status must be refused");
    let message = err.to_string();
    assert!(
        message.contains("sleepy") && message.contains("alive | stale | shutdown"),
        "the refusal must name the value and the accepted set: {message}"
    );

    // Get answers with the fold: the roster row plus what it owns.
    let detail = client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: OpId::Get(Domain::Worker),
                version: 1,
                input: json!({ "worker_id": "worker-omega" }),
                min_seq: None,
            },
        )
        .await
        .expect("rpc")
        .expect("worker.get")
        .output;
    assert_eq!(detail["worker"]["worker_id"], "worker-omega");
    assert_eq!(detail["worker"]["status"], "shutdown");
    assert_eq!(detail["worker"]["registered_at_ms"], BASE_MS + 1_000);
    // Terminal ownership counts as owned but not as in-flight.
    assert_eq!(detail["worker"]["in_flight_count"], 0);
    assert_eq!(detail["owned"][0]["invocation_id"], OWNED_INVOCATION);
    assert_eq!(detail["owned"][0]["status"], "completed");

    // An unknown id is NotFound, which is what `fq workers show`
    // turns into its exit-1 message.
    let err = client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: OpId::Get(Domain::Worker),
                version: 1,
                input: json!({ "worker_id": "no-such-worker" }),
                min_seq: None,
            },
        )
        .await
        .expect("rpc")
        .expect_err("unknown worker must be NotFound");
    assert!(
        matches!(err, fq_edge::wire::WireError::NotFound { .. }),
        "expected NotFound, got {err:?}"
    );

    unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
    let _ = daemon.wait();
    let _ = std::fs::remove_dir_all(&scratch);
}
