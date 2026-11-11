//! A daemon that died between an invocation's first WAL write and its
//! trigger claim being marked started, restarted as the real `fqd`
//! (<https://github.com/bricef/factor-q/issues/959>).
//!
//! The dead daemon's leavings are made here, with no daemon running and in
//! the order a daemon makes them: the trigger is pulled off the
//! dispatcher's own durable and not acked; the claim row is written under
//! the dead daemon's id, naming invocation X; and the real `ReducerRunner`
//! runs X under that id until its durable-start signal fires, which is the
//! moment its first WAL write has landed, and is dropped there. That WAL row
//! is the runner's own step-0 state with its trigger columns filled. The
//! delivery is then NAKed, standing in for the end of its ack window.
//!
//! Then the real `fqd` starts on those stores. It must resume X from the
//! WAL, and drop the redelivered trigger instead of starting it again. Only
//! the binary decides when startup recovery records X's durable start on
//! the claim relative to when its dispatcher consumes, so this is the test
//! that notices if that order changes. The unit tests beside the
//! recovery code build the restarted dispatcher themselves.
//!
//! Isolation follows `invocation_resume.rs`: a private broker, a mock model
//! and a scratch config per test.

#![cfg(unix)]

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_nats::jetstream::AckKind;
use fq_runtime::agent::{AgentId, AgentRegistry};
use fq_runtime::bus::{NATS_DEFAULT_MAX_ACK_PENDING, TRIGGER_STREAM_NAME};
use fq_runtime::control_plane::dispatcher::CONSUMER_NAME;
use fq_runtime::control_plane::{TriggerClaim, TriggerKey};
use fq_runtime::events::EventPayload;
use fq_runtime::llm::fixture::FixtureClient;
use fq_runtime::test_support::mock_anthropic::{MockAnthropicServer, MockResponse};
use fq_runtime::worker::{DurableStart, WorkerId};
use fq_runtime::{
    ControlPlaneStore, EventBus, Harness, ModelPricing, PricingTable, ReducerContext,
    ReducerRunner, RunnerConfig, RuntimeDbPaths, ToolRegistry, WorkerStore,
};
use fq_test_support::TestChild;
use futures::StreamExt;
use serde_json::json;
use uuid::Uuid;

const AGENT: &str = "crash-probe";
const MODEL: &str = "claude-haiku-4-5";
const DEAD_DAEMON: &str = "daemon-that-died";

/// Polls run every 100 ms, at most this many times (ninety seconds'
/// worth), each under a wall-clock deadline of the same length.
const MAX_POLLS: u32 = 900;
const POLL: Duration = Duration::from_millis(100);

fn fqd_binary() -> &'static str {
    env!("CARGO_BIN_EXE_fqd")
}

/// What one event said, kept for the assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    kind: &'static str,
    invocation_id: Uuid,
}

/// Every `triggered`, `completed` and `failed` event for [`AGENT`] since
/// the test subscribed.
#[derive(Clone, Default)]
struct Events(Arc<Mutex<Vec<Seen>>>);

impl Events {
    async fn record(bus: &EventBus) -> Self {
        let events = Self::default();
        let mut stream = bus
            .subscribe(format!("fq.agent.{AGENT}.>"))
            .await
            .expect("subscribe to the agent's events");
        let sink = events.clone();
        tokio::spawn(async move {
            while let Some(event) = stream.next().await {
                let Ok(event) = event else { continue };
                let kind = match event.payload {
                    EventPayload::Triggered(_) => "triggered",
                    EventPayload::Completed(_) => "completed",
                    EventPayload::Failed(_) => "failed",
                    _ => continue,
                };
                sink.0.lock().unwrap().push(Seen {
                    kind,
                    invocation_id: event.envelope.invocation_id,
                });
            }
        });
        events
    }

    fn of(&self, kind: &str) -> Vec<Uuid> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.kind == kind)
            .map(|seen| seen.invocation_id)
            .collect()
    }
}

/// The scratch layout `fqd` runs on: config, agents and the cache
/// directory its three stores live in.
fn scratch(mock_base_url: &str) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("fq-crash-before-claim-mark-")
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

/// Run invocation `invocation_id` on the real runner until its first WAL
/// write has landed, then drop it: the daemon dies there, before anything
/// marks the claim. Returns once the drop has happened.
async fn first_wal_write_then_die(
    bus: &EventBus,
    wal: Arc<WorkerStore>,
    registry: &AgentRegistry,
    delivery: &async_nats::jetstream::Message,
    invocation_id: Uuid,
) {
    let mut prices = HashMap::new();
    prices.insert(
        MODEL.to_string(),
        ModelPricing {
            input_per_million: 1.0,
            output_per_million: 5.0,
            cache_read_per_million: None,
            cache_write_per_million: None,
            cache_write_1h_per_million: None,
        },
    );
    let runner = ReducerRunner::new(
        Arc::new(
            ReducerContext::builder()
                .tools(Arc::new(ToolRegistry::with_builtins()))
                .build(),
        ),
        Arc::new(
            RunnerConfig::builder()
                .bus(bus.clone())
                .pricing(Arc::new(PricingTable::from_map(prices)))
                .store(wal)
                .worker_id(WorkerId::new(DEAD_DAEMON).unwrap())
                .build(),
        ),
        Harness::new(),
    );
    let agent = registry
        .get_loaded(&AgentId::new(AGENT).unwrap())
        .expect("the probe agent is loaded")
        .agent
        .clone();
    let payload: serde_json::Value =
        serde_json::from_slice(&delivery.payload).expect("the trigger body is JSON");
    let trigger = fq_runtime::trigger::delivered(delivery, payload);
    // Never asked: the run is dropped before its first model call.
    let llm = FixtureClient::new();

    let (durable_start, fired) = DurableStart::channel(invocation_id);
    let run = runner.run_signalling(&agent, &llm, trigger, Some(1), durable_start);
    tokio::pin!(run);
    // `fired` first: once it is ready the run is not polled again, so it
    // gets no further than whatever it started in the poll that fired.
    tokio::select! {
        biased;
        signal = fired => signal.expect("the durable start fires"),
        outcome = &mut run => panic!("the run ended before its first WAL write: {outcome:?}"),
    }
}

/// Wait until the broker has delivered the trigger a second time and
/// nothing is unacked. A duplicate start publishes `triggered` before its
/// durable start acks the message, so by then any such event is out.
async fn wait_until_the_redelivery_is_settled(
    consumer: &mut async_nats::jetstream::consumer::PullConsumer,
    log: &std::path::Path,
) {
    let deadline = Instant::now() + POLL * MAX_POLLS;
    for _ in 0..MAX_POLLS {
        let info = consumer.info().await.expect("consumer info").clone();
        if info.delivered.consumer_sequence >= 2 && info.num_ack_pending == 0 {
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(POLL).await;
    }
    let info = consumer.info().await.expect("consumer info").clone();
    panic!(
        "the redelivered trigger was not settled in time: {info:?}\n--- daemon log ---\n{}",
        std::fs::read_to_string(log).unwrap_or_default()
    );
}

/// **The real daemon, restarted after a crash between an invocation's
/// first WAL write and its claim's update, runs the trigger once (#959).**
///
/// Exactly one `completed`, for X, resumed from the WAL; no `triggered`
/// under any other invocation id; and the claim reads `durably_started`
/// with X.
#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_daemon_resumes_the_crashed_invocation_and_drops_its_redelivery() {
    let server = fq_test_support::NatsServer::start();
    let nats_url = server.url().to_string();
    let mock = MockAnthropicServer::start().await;
    let scratch = scratch(mock.base_url());
    let paths = RuntimeDbPaths::under(&scratch.path().join("cache"));

    let bus = EventBus::connect(&nats_url).await.expect("connect");
    let events = Events::record(&bus).await;
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

    // ---- The daemon that died. ----
    let invocation = Uuid::now_v7();
    {
        let claims = ControlPlaneStore::open(&paths.control_plane)
            .await
            .expect("open the control-plane store");
        let key = TriggerKey {
            stream: TRIGGER_STREAM_NAME,
            stream_epoch: bus.trigger_stream_epoch(),
            stream_seq: published.stream_seq,
        };
        assert_eq!(
            claims
                .claim_trigger(key, DEAD_DAEMON, &invocation.to_string(), 0)
                .await
                .unwrap(),
            TriggerClaim::Won
        );
        let wal = Arc::new(
            WorkerStore::open(&paths.worker)
                .await
                .expect("open the worker store"),
        );
        let mut registry = AgentRegistry::new();
        registry.load_file(&scratch.path().join("agents").join(format!("{AGENT}.md")));
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());
        first_wal_write_then_die(&bus, wal.clone(), &registry, &delivery, invocation).await;
        let row = wal
            .get_invocation_state(&invocation.to_string())
            .await
            .unwrap()
            .expect("the first WAL write landed under the claim's id");
        assert!(row.terminal_at.is_none(), "{row:?}");
        assert!(row.trigger_payload.is_some(), "{row:?}");
    }
    // The broker's redelivery, brought forward from the end of the ack
    // window.
    delivery
        .ack_with(AckKind::Nak(None))
        .await
        .expect("redeliver the trigger");

    // One answer for the resumed run. A second start of the trigger would
    // get one too, so it shows up as a second `completed` as well as a
    // second `triggered`.
    for n in 0..2 {
        mock.push_response(MockResponse::tool_use(
            format!("toolu_outcome_{n}"),
            "report_outcome",
            json!({ "status": "success", "summary": "done" }),
            10,
            5,
        ));
    }

    // ---- The real daemon, restarted on what the dead one left. ----
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

    let deadline = Instant::now() + POLL * MAX_POLLS;
    for _ in 0..MAX_POLLS {
        if !events.of("completed").is_empty() || Instant::now() >= deadline {
            break;
        }
        if let Some(status) = daemon.try_wait().expect("poll the daemon") {
            panic!(
                "fqd exited ({status:?})\n--- daemon log ---\n{}",
                daemon_log()
            );
        }
        tokio::time::sleep(POLL).await;
    }
    assert!(
        !events.of("completed").is_empty(),
        "no invocation completed\n--- daemon log ---\n{}",
        daemon_log()
    );
    wait_until_the_redelivery_is_settled(&mut consumer, &log_path).await;
    // Margin for an event still on its way to the subscription.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let _ = daemon.signal(libc::SIGTERM);
    let _ = daemon.wait_timeout(Duration::from_secs(15));

    assert_eq!(
        events.of("triggered"),
        vec![invocation],
        "the only `triggered` is the dead daemon's, for X: any other is the trigger started a \
         second time (#959)\n--- daemon log ---\n{}",
        daemon_log()
    );
    assert_eq!(
        events.of("completed"),
        vec![invocation],
        "exactly one invocation completes, and it is X, resumed from the WAL\n--- daemon log ---\n{}",
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
            .claim_trigger(
                TriggerKey {
                    stream: TRIGGER_STREAM_NAME,
                    stream_epoch: bus.trigger_stream_epoch(),
                    stream_seq: published.stream_seq,
                },
                "probe",
                "probe",
                0,
            )
            .await
            .unwrap(),
        TriggerClaim::Started {
            invocation_id: invocation.to_string()
        },
        "the claim reads durably started, with X"
    );

    mock.shutdown().await;
}
