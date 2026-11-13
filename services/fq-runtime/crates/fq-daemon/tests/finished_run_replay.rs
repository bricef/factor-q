//! A trigger whose run finished while its claim still read `claimed`,
//! replayed to the real `fqd` from a recreated durable
//! (<https://github.com/bricef/factor-q/issues/967>).
//!
//! What the daemon that stopped left is made here, with no daemon running
//! and in the order a daemon makes it: the trigger is pulled off the
//! dispatcher's own durable; the claim row is written under the stopped
//! daemon's id, naming invocation X; X ran to its end and the claim was
//! never marked, so the dispatcher acked the delivery and kept the claim,
//! as it does for a mark that fails after its invocation has returned; and
//! the coordination consumer archived X, which leaves the archive row and
//! the ownership row and takes X out of the worker's WAL.
//!
//! Then the dispatcher's durable is deleted, as an operator recreating it
//! does, or a broker restore that loses its state, and the real `fqd`
//! starts. Its dispatcher creates the durable again, and a durable with no
//! deliver policy starts from the oldest message the trigger stream holds:
//! the stream keeps a message for a day, acked or not, so X's trigger is
//! handed to the new daemon. No acknowledgement was lost on the way. This
//! is the route the review of PR 1021 reproduced.
//!
//! The replayed copy must be dropped: startup recovery marks the claim
//! from the control plane's record of X before the dispatcher consumes
//! anything. Before that, the copy met a `claimed` row under an exited
//! daemon's id, which the dispatcher reads as a delivery that never
//! started, adopted it and ran the trigger a second time. The unit tests
//! beside the recovery code build the restarted dispatcher themselves;
//! only the binary decides when recovery runs relative to its dispatcher.
//!
//! Isolation follows `crash_before_claim_mark.rs`: a private broker, a
//! mock model and a scratch config per test.

#![cfg(unix)]

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fq_runtime::agent::AgentId;
use fq_runtime::bus::{NATS_DEFAULT_MAX_ACK_PENDING, TRIGGER_STREAM_NAME};
use fq_runtime::control_plane::dispatcher::CONSUMER_NAME;
use fq_runtime::control_plane::{InvocationArchiveRow, OwnerStatus, TriggerClaim, TriggerKey};
use fq_runtime::events::EventPayload;
use fq_runtime::test_support::mock_anthropic::{MockAnthropicServer, MockResponse};
use fq_runtime::{ControlPlaneStore, EventBus, RuntimeDbPaths, WorkerStore};
use fq_test_support::TestChild;
use futures::StreamExt;
use serde_json::json;
use uuid::Uuid;

const AGENT: &str = "replay-probe";
const MODEL: &str = "claude-haiku-4-5";
const STOPPED_DAEMON: &str = "daemon-that-stopped";

/// Polls run every 100 ms, at most this many times (ninety seconds'
/// worth), each under a wall-clock deadline of the same length.
const MAX_POLLS: u32 = 900;
const POLL: Duration = Duration::from_millis(100);

fn fqd_binary() -> &'static str {
    env!("CARGO_BIN_EXE_fqd")
}

/// The invocation id of every `triggered` event for [`AGENT`] since the
/// test subscribed.
#[derive(Clone, Default)]
struct Triggered(Arc<Mutex<Vec<Uuid>>>);

impl Triggered {
    async fn record(bus: &EventBus) -> Self {
        let triggered = Self::default();
        let mut stream = bus
            .subscribe(format!("fq.agent.{AGENT}.>"))
            .await
            .expect("subscribe to the agent's events");
        let sink = triggered.clone();
        tokio::spawn(async move {
            while let Some(event) = stream.next().await {
                let Ok(event) = event else { continue };
                if let EventPayload::Triggered(_) = event.payload {
                    sink.0.lock().unwrap().push(event.envelope.invocation_id);
                }
            }
        });
        triggered
    }

    fn ids(&self) -> Vec<Uuid> {
        self.0.lock().unwrap().clone()
    }
}

/// The scratch layout `fqd` runs on: config, agents and the cache
/// directory its three stores live in.
fn scratch(mock_base_url: &str) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("fq-finished-run-replay-")
        .tempdir()
        .expect("create scratch directory");
    for sub in ["cache", "state", "agents"] {
        std::fs::create_dir_all(dir.path().join(sub)).unwrap();
    }
    std::fs::write(
        dir.path().join("agents").join(format!("{AGENT}.md")),
        format!("---\nname: {AGENT}\nmodel: {MODEL}\nbudget: 1.00\n---\n\nReport your outcome.\n"),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("fq.toml"),
        format!(
            "[edge]\nbind = \"127.0.0.1:0\"\n\n\
             [providers.anthropic]\nmodels = [\"{MODEL}\"]\nbase_url = \"{mock_base_url}\"\n"
        ),
    )
    .unwrap();
    dir
}

/// Wait until the recreated durable has delivered the trigger at
/// `stream_seq` and nothing is unacked: the replayed copy has been
/// answered, whichever way. A start publishes `triggered` before its
/// durable start acks the copy, so by then any such event is out.
async fn wait_until_the_replay_is_settled(bus: &EventBus, stream_seq: u64, log: &std::path::Path) {
    let stream = bus
        .jetstream()
        .get_stream(TRIGGER_STREAM_NAME)
        .await
        .expect("the trigger stream");
    let deadline = Instant::now() + POLL * MAX_POLLS;
    let mut last = None;
    for _ in 0..MAX_POLLS {
        // Absent until the daemon's dispatcher has created it again.
        if let Ok(info) = stream.consumer_info(CONSUMER_NAME).await {
            if info.delivered.stream_sequence >= stream_seq && info.num_ack_pending == 0 {
                return;
            }
            last = Some(info);
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(POLL).await;
    }
    panic!(
        "the replayed trigger was not settled in time: {last:?}\n--- daemon log ---\n{}",
        std::fs::read_to_string(log).unwrap_or_default()
    );
}

/// **The real daemon, started after a run finished under a claim that was
/// never marked, does not run its trigger again when a recreated durable
/// replays it (#967).**
///
/// No `triggered` event for the agent, the startup log names the repair,
/// and the claim reads `durably_started` with X.
#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_daemon_drops_the_replay_of_a_trigger_whose_run_finished() {
    let server = fq_test_support::NatsServer::start();
    let nats_url = server.url().to_string();
    let mock = MockAnthropicServer::start().await;
    let scratch = scratch(mock.base_url());
    let paths = RuntimeDbPaths::under(&scratch.path().join("cache"));

    let bus = EventBus::connect(&nats_url).await.expect("connect");
    let triggered = Triggered::record(&bus).await;
    let mut consumer = bus
        .trigger_consumer(CONSUMER_NAME, NATS_DEFAULT_MAX_ACK_PENDING)
        .await
        .expect("open the dispatcher's durable");
    let published = bus
        .publish_trigger(&AgentId::new(AGENT).unwrap(), &json!("run the probe"))
        .await
        .expect("publish the trigger");
    let delivery = consumer
        .batch()
        .max_messages(1)
        .expires(Duration::from_secs(5))
        .messages()
        .await
        .expect("pull")
        .next()
        .await
        .expect("the trigger within 5s")
        .expect("message ok");
    let key = TriggerKey {
        stream: TRIGGER_STREAM_NAME,
        stream_epoch: bus.trigger_stream_epoch(),
        stream_seq: published.stream_seq,
    };

    // ---- The daemon that stopped. ----
    let invocation = Uuid::now_v7().to_string();
    {
        let claims = ControlPlaneStore::open(&paths.control_plane)
            .await
            .expect("open the control-plane store");
        assert_eq!(
            claims
                .claim_trigger(key, STOPPED_DAEMON, &invocation, 0)
                .await
                .unwrap(),
            TriggerClaim::Won
        );
        // X ran to its end and its claim was never marked; the delivery
        // was acked with the claim kept.
        delivery.ack().await.expect("ack the delivery");
        // The archive hand-off: the coordination consumer's two writes. The
        // worker's WAL row went on the acknowledgement, so the WAL is left
        // without one.
        claims
            .insert_archive(&InvocationArchiveRow {
                invocation_id: invocation.clone(),
                agent_id: AGENT.to_string(),
                final_phase: "completed".to_string(),
                final_state_blob: b"{}".to_vec(),
                started_at: 1_000,
                terminal_at: 2_000,
                archived_at: 3_000,
            })
            .await
            .unwrap();
        claims
            .upsert_invocation_ownership(&invocation, STOPPED_DAEMON, 3_000, OwnerStatus::Completed)
            .await
            .unwrap();
        let wal = WorkerStore::open(&paths.worker)
            .await
            .expect("open the worker store");
        assert!(
            wal.get_invocation_state(&invocation)
                .await
                .unwrap()
                .is_none()
        );
    }
    let deadline = Instant::now() + POLL * MAX_POLLS;
    loop {
        let info = consumer.info().await.expect("consumer info").clone();
        if info.num_ack_pending == 0 {
            break;
        }
        assert!(Instant::now() < deadline, "the ack never landed: {info:?}");
        tokio::time::sleep(POLL).await;
    }

    // ---- The durable is lost. ----
    assert!(
        bus.jetstream()
            .get_stream(TRIGGER_STREAM_NAME)
            .await
            .expect("the trigger stream")
            .delete_consumer(CONSUMER_NAME)
            .await
            .expect("delete the dispatcher's durable")
            .success
    );

    // An answer for a second run, so one shows up as `triggered` and runs
    // to its end rather than failing on an empty script.
    mock.push_response(MockResponse::tool_use(
        "toolu_outcome_0",
        "report_outcome",
        json!({ "status": "success", "summary": "done" }),
        10,
        5,
    ));

    // ---- The real daemon, started on what the stopped one left. ----
    let log_path = scratch.path().join("fqd.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let mut daemon = TestChild::builder(fqd_binary())
        .env("FQ_DAEMON_CONFIG", scratch.path().join("fq.toml"))
        .env("FQ_NATS_URL", &nats_url)
        .env("FQ_CACHE_DIR", scratch.path().join("cache"))
        .env("FQ_STATE_DIR", scratch.path().join("state"))
        .env("FQ_AGENTS_DIR", scratch.path().join("agents"))
        .env("ANTHROPIC_API_KEY", "test-key-unused-by-mock")
        .env("FQ_LOG_FORMAT", "json")
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn();
    let daemon_log = || std::fs::read_to_string(&log_path).unwrap_or_default();

    wait_until_the_replay_is_settled(&bus, published.stream_seq, &log_path).await;
    if let Some(status) = daemon.try_wait().expect("poll the daemon") {
        panic!(
            "fqd exited ({status:?})\n--- daemon log ---\n{}",
            daemon_log()
        );
    }
    // Margin for an event still on its way to the subscription.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let _ = daemon.signal(libc::SIGTERM);
    let _ = daemon.wait_timeout(Duration::from_secs(15));

    assert_eq!(
        triggered.ids(),
        Vec::<Uuid>::new(),
        "the replayed trigger started its work a second time: its run had finished under a claim \
         the restarted daemon adopted (#967)\n--- daemon log ---\n{}",
        daemon_log()
    );
    assert!(
        daemon_log().contains("recorded a durable start missing from its trigger's claim"),
        "the daemon's startup recovery repairs the claim, and says so\n--- daemon log ---\n{}",
        daemon_log()
    );
    let claims = ControlPlaneStore::open(&paths.control_plane)
        .await
        .expect("reopen the control-plane store");
    assert_eq!(
        claims
            .claim_trigger(key, "probe", "probe", 0)
            .await
            .unwrap(),
        TriggerClaim::Started {
            invocation_id: invocation
        },
        "the claim reads durably started, with X"
    );

    mock.shutdown().await;
}
