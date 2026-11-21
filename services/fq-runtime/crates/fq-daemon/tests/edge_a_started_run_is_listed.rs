//! A run started from a trigger is on the invocation list while it runs
//! (issue 721).
//!
//! `fq invocation list --status in_flight` reads the control plane's
//! ownership rows, and before this change nothing wrote one for a run
//! dispatched from the trigger stream until the run ended: the list
//! answered "nothing running" while an agent was mid-tool, and an
//! unattended deploy took that for idle. The dispatcher now writes the
//! row at the run's durable start, after it marks the trigger's claim.
//!
//! The test runs a real `fqd` against a private broker and a mock model
//! that holds the run's first model call open, so the run is mid-step for
//! as long as the test looks at it. Nothing here waits on a clock but the
//! bounded polls, whose expiry is a failure.

use std::process::Stdio;
use std::time::Duration;

use fq_ops::views::{InvocationDetailView, InvocationSummaryView};
use fq_ops::{Domain, Invocation, OpId, VerbId};
use fq_runtime::test_support::fault::MockFault;
use fq_runtime::test_support::mock_anthropic::MockAnthropicServer;
use fq_test_support::TestChild;
use serde_json::json;

const RUNNER: &str = "held-runner";
const MODEL: &str = "claude-haiku-4-5";

/// Ceiling on every poll. A deadline reached means the row never came.
const PATIENCE: Duration = Duration::from_secs(20);

fn unique_scratch() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("edge-listed-{}-{}", std::process::id(), nanos));
    std::fs::create_dir_all(dir.join("cache")).unwrap();
    std::fs::create_dir_all(dir.join("agents")).unwrap();
    dir
}

fn suffix_of<'a>(log: &'a str, prefix: &str) -> &'a str {
    log.lines()
        .find_map(|l| l.trim().strip_prefix(prefix))
        .unwrap_or_else(|| panic!("log lacks prefix {prefix:?}"))
        .trim()
}

fn parse_fingerprint(hex: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex fingerprint");
    }
    out
}

/// A live `fqd` that runs [`RUNNER`] on the mock model at `model_url`, an
/// edge client pinned to its certificate, and a bus connection to the same
/// broker.
struct Daemon {
    scratch: std::path::PathBuf,
    child: TestChild,
    client: fq_edge::EdgeClient,
    bus: fq_runtime::EventBus,
}

impl Daemon {
    async fn start(nats_url: &str, model_url: &str) -> Daemon {
        let scratch = unique_scratch();
        std::fs::write(
            scratch.join("agents").join(format!("{RUNNER}.md")),
            format!("---\nname: {RUNNER}\nmodel: {MODEL}\nbudget: 1.00\n---\n\nAnswer.\n"),
        )
        .unwrap();
        // The model is declared with its price, so the daemon's pricing
        // guarantee is met from config rather than from a network fetch.
        std::fs::write(
            scratch.join("fq.toml"),
            format!(
                "[edge]\nbind = \"127.0.0.1:0\"\n\n\
                 [providers.anthropic]\nmodels = [\"{MODEL}\"]\nbase_url = \"{model_url}\"\n\n\
                 [providers.anthropic.pricing.\"{MODEL}\"]\n\
                 input_per_mtok = 1.0\noutput_per_mtok = 5.0\n"
            ),
        )
        .unwrap();
        let log_path = scratch.join("daemon.log");
        let log = std::fs::File::create(&log_path).expect("create daemon log");
        let log_err = log.try_clone().expect("clone log handle");
        let mut child = TestChild::builder(env!("CARGO_BIN_EXE_fqd"))
            .env("FQ_DAEMON_CONFIG", scratch.join("fq.toml"))
            .env("FQ_NATS_URL", nats_url)
            .env("FQ_CACHE_DIR", scratch.join("cache"))
            .env("FQ_STATE_DIR", scratch.join("state"))
            .env("FQ_AGENTS_DIR", scratch.join("agents"))
            .env("ANTHROPIC_API_KEY", "test-key-unused-by-mock")
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .spawn();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let text = loop {
            if let Some(status) = child.try_wait().expect("poll fqd") {
                panic!("fqd exited during startup with {status:?}");
            }
            let text = std::fs::read_to_string(&log_path).unwrap_or_default();
            if text.contains("- edge is listening on ") {
                break text;
            }
            assert!(tokio::time::Instant::now() < deadline, "fqd never ready");
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let fingerprint = parse_fingerprint(suffix_of(
            &text,
            "edge: certificate fingerprint (clients pin this): ",
        ));
        let token = fq_test_support::admin_token(&scratch.join("state"));
        let addr = suffix_of(&text, "- edge is listening on ").to_string();
        let client = fq_edge::EdgeClient::connect(&addr, fingerprint, &token)
            .await
            .expect("connect edge");
        let bus = fq_runtime::EventBus::connect(nats_url)
            .await
            .expect("connect bus");
        Daemon {
            scratch,
            child,
            client,
            bus,
        }
    }

    async fn invoke(&self, op: OpId, input: serde_json::Value) -> serde_json::Value {
        let named = format!("{op:?}");
        self.client
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
            .unwrap_or_else(|e| panic!("{named}: {e}"))
            .output
    }

    /// The runs `invocation.list` names as in flight.
    async fn in_flight(&self) -> Vec<InvocationSummaryView> {
        let listed = self
            .invoke(
                OpId::List(Domain::Invocation),
                json!({ "status": "in_flight" }),
            )
            .await;
        serde_json::from_value(listed).expect("an invocation list")
    }

    /// A daemon holding a run open would wait out its drain for the held
    /// model call, so it is killed.
    fn kill(mut self) {
        self.child.signal(libc::SIGKILL).expect("kill(SIGKILL)");
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

/// **A run started from a trigger is listed in flight while it runs, shown
/// with its row, and ended under the same worker.** The run's first model
/// call is held open, so it is mid-step. `invocation.list` with
/// `in_flight` lists it, under the daemon's worker id rather than the
/// agent's; and `invocation.get` shows the row beside the run's live
/// execution, from the run's own start. An operator's drop then ends it,
/// and the row reads `failed` under the worker it named: before the row
/// was written at the start, the drop's fold created it with the agent's
/// id in the worker column.
#[tokio::test]
async fn a_run_started_from_a_trigger_is_listed_in_flight_while_it_runs() {
    let server = fq_test_support::NatsServer::start();
    let model = MockAnthropicServer::start().await;
    model.push_fault(MockFault::stall(Duration::from_secs(600)));
    let daemon = Daemon::start(server.url(), model.base_url()).await;

    daemon
        .bus
        .publish_trigger(
            &fq_runtime::AgentId::new(RUNNER).unwrap(),
            &json!({ "work": "held" }),
        )
        .await
        .expect("publish the trigger");

    let deadline = tokio::time::Instant::now() + PATIENCE;
    let listed = loop {
        if let Some(row) = daemon.in_flight().await.into_iter().next() {
            break row;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the run started from the trigger was never listed in flight"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(listed.status, "in_flight");
    assert!(
        !listed.worker_id.is_empty() && listed.worker_id != RUNNER,
        "the row names the daemon's worker, not the agent standing in for it: {listed:?}"
    );

    let shown: InvocationDetailView = serde_json::from_value(
        daemon
            .invoke(
                OpId::Get(Domain::Invocation),
                json!({ "invocation_id": listed.invocation_id }),
            )
            .await,
    )
    .expect("an invocation's detail");
    let owner = shown.owner.expect("the run has its coordination row");
    let live = shown.live.expect("the run is live in the worker's WAL");
    assert_eq!(owner.status, "in_flight");
    assert_eq!(
        owner.started_at_ms, live.started_at_ms,
        "the row's start is the run's own, as its WAL records it"
    );

    daemon
        .invoke(
            OpId::Verb(VerbId::Invocation(Invocation::Drop)),
            json!({
                "invocation_id": listed.invocation_id,
                "reason": "the test is done with it",
                "live": true,
            }),
        )
        .await;
    let deadline = tokio::time::Instant::now() + PATIENCE;
    let ended = loop {
        let shown: InvocationDetailView = serde_json::from_value(
            daemon
                .invoke(
                    OpId::Get(Domain::Invocation),
                    json!({ "invocation_id": listed.invocation_id }),
                )
                .await,
        )
        .expect("an invocation's detail");
        let owner = shown.owner.expect("the run keeps its coordination row");
        if owner.status != "in_flight" {
            break owner;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the dropped run's row never left in_flight"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(ended.status, "failed");
    assert_eq!(
        ended.worker_id, listed.worker_id,
        "the run's ending keeps the worker its start wrote"
    );

    daemon.kill();
}
