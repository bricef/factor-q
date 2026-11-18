//! The real `fqd` holds an archive too large for its broker, archives the
//! rows behind it, and says so in `control.doctor` and `control.status`
//! (<https://github.com/bricef/factor-q/issues/981>).
//!
//! The private broker is a stock `nats-server`, whose `max_payload` is
//! 1 MiB, and the bus refuses any event over it before it reaches the
//! wire. `invocation.archived` carries the run's whole state, so a terminal
//! row with 1.1 MB of state has an archive over that limit however the
//! state is written: as a JSON array of integers (about 4.4 MB) or as
//! base64 (about 1.5 MB). The test does not depend on the encoding. Before
//! this change the sweeper's pass ended at that row, every pass, and the
//! row written behind it was never archived.
//!
//! Isolation follows `trigger_claim_retention.rs`: a private broker, a
//! scratch config and stores under the scratch cache directory, and no
//! agent, so nothing but the two seeded rows is ever swept.

#![cfg(unix)]

use std::process::Stdio;
use std::time::{Duration, Instant};

use fq_ops::{ControlReport, OpId, ReportId};
use fq_runtime::worker::InvocationStateRow;
use fq_runtime::{RuntimeDbPaths, WorkerStore};
use fq_test_support::TestChild;

/// The row whose archive the broker refuses: never archived, so the store
/// lists it first.
const OVERSIZED: &str = "019a0000-0000-7000-8000-000000000981";
/// The row behind it: archived once already, awaiting its ack.
const HEALTHY: &str = "019a0000-0000-7000-8000-000000000982";
/// Bytes of state in the oversized row: over a stock `max_payload` even
/// one byte for one, so its archive is over it whether the state travels
/// as an integer array or as base64.
const STATE: usize = 1_100_000;
/// A stock broker's `max_payload`.
const STOCK_MAX_PAYLOAD: u64 = 1024 * 1024;
/// The start of the line the daemon logs when it holds a row for size.
const HELD_LINE: &str = "an event this row must publish is larger than the broker's max_payload";

const POLL: Duration = Duration::from_millis(100);
const DEADLINE: Duration = Duration::from_secs(60);

/// The scratch layout `fqd` runs on, with the archive sweep every 200 ms
/// so several passes run inside the test.
fn scratch() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("fq-archive-holds-")
        .tempdir()
        .expect("create scratch directory");
    for sub in ["cache", "state", "agents"] {
        std::fs::create_dir_all(dir.path().join(sub)).unwrap();
    }
    std::fs::write(
        dir.path().join("fqd.toml"),
        "[edge]\nbind = \"127.0.0.1:0\"\n\n[worker]\narchive_retry_interval_ms = 200\n",
    )
    .unwrap();
    dir
}

/// A terminal row as the run loop leaves one, with `state` as its state.
fn terminal_row(invocation_id: &str, state: Vec<u8>) -> InvocationStateRow {
    InvocationStateRow {
        invocation_id: invocation_id.to_string(),
        agent_id: "researcher".to_string(),
        schema_version: 1,
        phase: "completed".to_string(),
        state_blob: state,
        step_index: 4,
        started_at: 1_000,
        updated_at: 2_000,
        terminal_at: Some(2_000),
        workspace_ref: None,
        archive_status: None,
        archive_published_at: None,
        trigger_source: None,
        trigger_subject: None,
        trigger_payload: None,
        terminal_outcome: None,
        terminal_published_at: None,
    }
}

/// The running daemon, and what to say when it is not doing what the test
/// waits for.
struct Daemon {
    child: TestChild,
    log: std::path::PathBuf,
}

impl Daemon {
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Poll until `done` answers, failing if the daemon exits or the
    /// deadline passes first.
    async fn wait_for<T, F, Fut>(&mut self, what: &str, mut done: F) -> T
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Option<T>>,
    {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some(answer) = done().await {
                return answer;
            }
            if let Some(status) = self.child.try_wait().expect("poll the daemon") {
                panic!(
                    "fqd exited ({status:?}) while waiting for {what}\n--- daemon log ---\n{}",
                    self.log()
                );
            }
            assert!(
                Instant::now() < deadline,
                "timed out after {DEADLINE:?} waiting for {what}\n--- daemon log ---\n{}",
                self.log()
            );
            tokio::time::sleep(POLL).await;
        }
    }
}

fn parse_fingerprint(hex: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex fingerprint");
    }
    out
}

async fn report(client: &fq_edge::EdgeClient, which: ControlReport) -> serde_json::Value {
    client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: OpId::Report(ReportId::Control(which)),
                version: 1,
                input: serde_json::json!({}),
                min_seq: None,
            },
        )
        .await
        .expect("rpc")
        .map(|r| r.output)
        .expect("the report answers")
}

/// **An archive too large for the broker holds its row and the sweep goes
/// on; `control.doctor` and `control.status` list the hold, and the log
/// says it once.**
#[tokio::test(flavor = "multi_thread")]
async fn an_archive_too_large_for_the_broker_is_held_and_reported() {
    let server = fq_test_support::NatsServer::start();
    let scratch = scratch();
    let paths = RuntimeDbPaths::under(&scratch.path().join("cache"));
    let store = WorkerStore::open(&paths.worker)
        .await
        .expect("open the worker store");
    store
        .upsert_invocation_state(&terminal_row(OVERSIZED, vec![200; STATE]))
        .await
        .unwrap();
    store
        .upsert_invocation_state(&terminal_row(HEALTHY, vec![1, 2, 3]))
        .await
        .unwrap();
    store.set_archive_pending(HEALTHY, 2_000).await.unwrap();

    let log = scratch.path().join("fqd.log");
    let log_file = std::fs::File::create(&log).expect("create daemon log");
    let child = TestChild::builder(env!("CARGO_BIN_EXE_fqd"))
        .env("FQ_DAEMON_CONFIG", scratch.path().join("fqd.toml"))
        .env("FQ_NATS_URL", server.url())
        .env("FQ_CACHE_DIR", scratch.path().join("cache"))
        .env("FQ_STATE_DIR", scratch.path().join("state"))
        .env("FQ_AGENTS_DIR", scratch.path().join("agents"))
        .stdout(Stdio::from(log_file.try_clone().unwrap()))
        .stderr(Stdio::from(log_file))
        .spawn();
    let mut daemon = Daemon { child, log };

    // ---- The pass goes on past the held row. ----
    let store = &store;
    daemon
        .wait_for(
            "the row behind the held one to be archived and acked",
            move || async move {
                let row = store.get_invocation_state(HEALTHY).await.unwrap();
                row.is_none().then_some(())
            },
        )
        .await;

    // ---- What an operator reads. ----
    let log_path = &daemon.log.clone();
    let addr = daemon
        .wait_for("the edge to listen", move || async move {
            let log = std::fs::read_to_string(log_path).ok()?;
            log.lines()
                .find_map(|l| l.trim().strip_prefix("- edge is listening on "))
                .map(|a| a.trim().to_string())
        })
        .await;
    let state = scratch.path().join("state");
    let client = fq_edge::EdgeClient::connect(
        &addr,
        parse_fingerprint(&fq_test_support::edge_fingerprint(&state)),
        &fq_test_support::admin_token(&state),
    )
    .await
    .expect("connect edge");

    // The pass that archived the row behind has finished, or finishes the
    // instant after; the report says when.
    let client = &client;
    let doctor = daemon
        .wait_for("a finished sweep in control.doctor", move || async move {
            let doctor = report(client, ControlReport::Doctor).await;
            doctor["archive_sweep"]["last_pass_at_ms"]
                .is_i64()
                .then_some(doctor)
        })
        .await;
    let holds = doctor["archive_sweep"]["holds"]
        .as_array()
        .expect("archive_sweep.holds");
    assert_eq!(holds.len(), 1, "the oversized row alone: {doctor}");
    let hold = &holds[0];
    assert_eq!(hold["invocation_id"], OVERSIZED);
    assert_eq!(hold["agent_id"], "researcher");
    let too_large = &hold["reason"]["payload_too_large"];
    assert_eq!(too_large["event_type"], "invocation_archived", "{hold}");
    assert_eq!(too_large["limit_bytes"], STOCK_MAX_PAYLOAD, "{hold}");
    let size = too_large["size_bytes"].as_u64().expect("a size");
    assert!(
        size > STATE as u64 && size > STOCK_MAX_PAYLOAD,
        "the archive carries the whole state, over the limit: {hold}"
    );
    let remedy = hold["remedy"].as_str().expect("a remedy");
    assert!(
        remedy.contains(&format!(
            "raise the broker's max_payload above {size} bytes"
        )) && remedy.contains("restart fqd"),
        "got: {remedy}"
    );
    let parsed: fq_ops::surface::DoctorReport =
        serde_json::from_value(doctor.clone()).expect("the declared shape");
    assert!(parsed.has_issues(), "a held row is an issue: {doctor}");

    let status = report(client, ControlReport::Status).await;
    assert_eq!(
        status["archive_sweep"]["holds"], doctor["archive_sweep"]["holds"],
        "the same rows, without a verdict"
    );
    assert!(
        status["archive_sweep"]["last_pass_at_ms"].is_i64(),
        "{status}"
    );

    // ---- The row is kept whole. ----
    let row = store.get_invocation_state(OVERSIZED).await.unwrap();
    let row = row.expect("held, not deleted");
    assert_eq!(row.state_blob, vec![200; STATE], "not truncated");
    assert_eq!(row.archive_status, None, "not marked archived");

    // ---- Said once, though the sweep keeps running. ----
    tokio::time::sleep(Duration::from_secs(1)).await;
    let text = daemon.log();
    let said: Vec<&str> = text
        .lines()
        .filter(|l| l.contains(HELD_LINE) && l.contains(OVERSIZED))
        .collect();
    assert_eq!(
        said.len(),
        1,
        "one error line per run\n--- daemon log ---\n{text}"
    );
}

async fn invoke(
    client: &fq_edge::EdgeClient,
    op: OpId,
    input: serde_json::Value,
) -> serde_json::Value {
    client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op,
                version: 1,
                input,
                min_seq: None,
            },
        )
        .await
        .expect("rpc")
        .map(|r| r.output)
        .expect("the op answers")
}

/// **A run whose ending reached the control plane and whose archive is
/// held reads finished, with its archive held, in `invocation.list` and
/// `invocation.get`.** The row stores a `completed` the run never got
/// out: the sweeper publishes it first, the coordination consumer moves
/// the ownership row to `completed` with its archive pending, and the
/// archive behind it is too large for the broker. The list and the
/// detail read the hold from the sweeper where they are built; the
/// detail names the reason and what lifts it.
#[tokio::test(flavor = "multi_thread")]
async fn a_held_run_reads_finished_with_its_archive_held() {
    let server = fq_test_support::NatsServer::start();
    let scratch = scratch();
    let paths = RuntimeDbPaths::under(&scratch.path().join("cache"));
    let store = WorkerStore::open(&paths.worker)
        .await
        .expect("open the worker store");
    let completed = fq_ops::events::Event::new(
        fq_ops::agent::AgentId::new("researcher").unwrap(),
        uuid::Uuid::parse_str(OVERSIZED).unwrap(),
        fq_ops::events::EventPayload::Completed(fq_ops::events::CompletedPayload {
            task_status: Default::default(),
            result_summary: Some("done".to_string()),
            total_llm_calls: 1,
            total_tool_calls: 0,
            total_cost: 0.0,
            total_duration_ms: 10,
        }),
    );
    let mut row = terminal_row(OVERSIZED, vec![200; STATE]);
    row.terminal_outcome = Some(serde_json::to_string(&completed).unwrap());
    store.upsert_invocation_state(&row).await.unwrap();

    let log = scratch.path().join("fqd.log");
    let log_file = std::fs::File::create(&log).expect("create daemon log");
    let child = TestChild::builder(env!("CARGO_BIN_EXE_fqd"))
        .env("FQ_DAEMON_CONFIG", scratch.path().join("fqd.toml"))
        .env("FQ_NATS_URL", server.url())
        .env("FQ_CACHE_DIR", scratch.path().join("cache"))
        .env("FQ_STATE_DIR", scratch.path().join("state"))
        .env("FQ_AGENTS_DIR", scratch.path().join("agents"))
        .stdout(Stdio::from(log_file.try_clone().unwrap()))
        .stderr(Stdio::from(log_file))
        .spawn();
    let mut daemon = Daemon { child, log };

    let log_path = &daemon.log.clone();
    let addr = daemon
        .wait_for("the edge to listen", move || async move {
            let log = std::fs::read_to_string(log_path).ok()?;
            log.lines()
                .find_map(|l| l.trim().strip_prefix("- edge is listening on "))
                .map(|a| a.trim().to_string())
        })
        .await;
    let state = scratch.path().join("state");
    let client = fq_edge::EdgeClient::connect(
        &addr,
        parse_fingerprint(&fq_test_support::edge_fingerprint(&state)),
        &fq_test_support::admin_token(&state),
    )
    .await
    .expect("connect edge");
    let client = &client;

    let listed = daemon
        .wait_for("the run listed with its archive held", move || async move {
            let items = invoke(
                client,
                OpId::List(fq_ops::Domain::Invocation),
                serde_json::json!({ "include_archived": false, "limit": 50 }),
            )
            .await;
            let item = items
                .as_array()?
                .iter()
                .find(|item| item["invocation_id"] == OVERSIZED)?
                .clone();
            (item["archive_state"] == "held").then_some(item)
        })
        .await;
    assert_eq!(listed["status"], "completed", "{listed}");

    let detail = invoke(
        client,
        OpId::Get(fq_ops::Domain::Invocation),
        serde_json::json!({ "invocation_id": OVERSIZED }),
    )
    .await;
    assert_eq!(detail["owner"]["status"], "completed", "{detail}");
    assert_eq!(detail["owner"]["archive_state"], "held", "{detail}");
    let hold = &detail["archive_hold"];
    assert_eq!(hold["invocation_id"], OVERSIZED, "{detail}");
    assert_eq!(
        hold["reason"]["payload_too_large"]["event_type"], "invocation_archived",
        "{detail}"
    );
    assert!(
        hold["remedy"]
            .as_str()
            .is_some_and(|remedy| remedy.contains("restart fqd")),
        "{detail}"
    );
}

/// **The worker view reads `held` as the invocation list does.** A row
/// startup recovery restored sits under the worker's own id, so the run is
/// among that worker's owned rows; when the run's archive is held, the
/// owned row reads `held` too, not `pending`. Seeded as a restore leaves a
/// row (in flight, under a registered worker), over a terminal WAL row
/// that stores a `completed` and holds too much state for the broker, so
/// no model is needed.
#[tokio::test(flavor = "multi_thread")]
async fn the_worker_view_reads_held_for_a_row_its_worker_owns() {
    use fq_runtime::control_plane::store::{ControlPlaneStore, OwnerStatus};
    const WORKER: &str = "worker-omega";
    let server = fq_test_support::NatsServer::start();
    let scratch = scratch();
    let paths = RuntimeDbPaths::under(&scratch.path().join("cache"));
    {
        let store = WorkerStore::open(&paths.worker)
            .await
            .expect("open the worker store");
        let completed = fq_ops::events::Event::new(
            fq_ops::agent::AgentId::new("researcher").unwrap(),
            uuid::Uuid::parse_str(OVERSIZED).unwrap(),
            fq_ops::events::EventPayload::Completed(fq_ops::events::CompletedPayload {
                task_status: Default::default(),
                result_summary: Some("done".to_string()),
                total_llm_calls: 1,
                total_tool_calls: 0,
                total_cost: 0.0,
                total_duration_ms: 10,
            }),
        );
        let mut row = terminal_row(OVERSIZED, vec![200; STATE]);
        row.terminal_outcome = Some(serde_json::to_string(&completed).unwrap());
        store.upsert_invocation_state(&row).await.unwrap();
        let control_plane = ControlPlaneStore::open(&paths.control_plane)
            .await
            .expect("open the control-plane store");
        control_plane
            .register_worker(WORKER, "probe-host", 1_000)
            .await
            .unwrap();
        control_plane.mark_worker_shutdown(WORKER).await.unwrap();
        control_plane
            .upsert_invocation_ownership(OVERSIZED, WORKER, 1_000, OwnerStatus::InFlight)
            .await
            .unwrap();
    }

    let log = scratch.path().join("fqd.log");
    let log_file = std::fs::File::create(&log).expect("create daemon log");
    let child = TestChild::builder(env!("CARGO_BIN_EXE_fqd"))
        .env("FQ_DAEMON_CONFIG", scratch.path().join("fqd.toml"))
        .env("FQ_NATS_URL", server.url())
        .env("FQ_CACHE_DIR", scratch.path().join("cache"))
        .env("FQ_STATE_DIR", scratch.path().join("state"))
        .env("FQ_AGENTS_DIR", scratch.path().join("agents"))
        .stdout(Stdio::from(log_file.try_clone().unwrap()))
        .stderr(Stdio::from(log_file))
        .spawn();
    let mut daemon = Daemon { child, log };

    let log_path = &daemon.log.clone();
    let addr = daemon
        .wait_for("the edge to listen", move || async move {
            let log = std::fs::read_to_string(log_path).ok()?;
            log.lines()
                .find_map(|l| l.trim().strip_prefix("- edge is listening on "))
                .map(|a| a.trim().to_string())
        })
        .await;
    let state = scratch.path().join("state");
    let client = fq_edge::EdgeClient::connect(
        &addr,
        parse_fingerprint(&fq_test_support::edge_fingerprint(&state)),
        &fq_test_support::admin_token(&state),
    )
    .await
    .expect("connect edge");
    let client = &client;

    // Once the list says held, the hold is in the sweeper's memory, where
    // the worker view reads it too.
    daemon
        .wait_for("the run listed with its archive held", move || async move {
            let items = invoke(
                client,
                OpId::List(fq_ops::Domain::Invocation),
                serde_json::json!({ "include_archived": false, "limit": 50 }),
            )
            .await;
            items
                .as_array()?
                .iter()
                .any(|item| item["invocation_id"] == OVERSIZED && item["archive_state"] == "held")
                .then_some(())
        })
        .await;
    let detail = invoke(
        client,
        OpId::Get(fq_ops::Domain::Worker),
        serde_json::json!({ "worker_id": WORKER }),
    )
    .await;
    let owned = &detail["owned"][0];
    assert_eq!(owned["invocation_id"], OVERSIZED, "{detail}");
    assert_eq!(owned["status"], "completed", "{detail}");
    assert_eq!(
        owned["archive_state"], "held",
        "the worker view reads held as the list does: {detail}"
    );
}
