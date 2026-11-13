//! The real `fqd` holds an archive too large for its broker, archives the
//! rows behind it, and says so in `control.doctor` and `control.status`
//! (<https://github.com/bricef/factor-q/issues/981>).
//!
//! The private broker is a stock `nats-server`, whose `max_payload` is
//! 1 MiB, and the bus refuses any event over it before it reaches the
//! wire. `invocation.archived` carries the run's state as a JSON array of
//! integers, four bytes on the wire for each byte of state written `200`,
//! so a terminal row with 300 KB of state has an archive of about 1.2 MB.
//! Before this change the sweeper's pass ended at that row, every pass,
//! and the row written behind it was never archived.
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
/// Bytes of state in the oversized row.
const STATE: usize = 300_000;
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
    assert!(size > 4 * STATE as u64, "the state's integer array: {hold}");
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
