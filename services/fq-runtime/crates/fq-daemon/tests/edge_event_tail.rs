//! `fq events tail` — the oracle (plan Phase 4, verb 11).
//!
//! The verb has no goldens, so this suite was written against the
//! **pre-flip** core-NATS path and made to pass there before anything
//! moved: it records what the verb promised, not what a flip produced
//! (cohort 4.1's lesson, made a rule).
//!
//! A golden file is the wrong instrument for a command that never
//! terminates. Instead the test drives the real binary, publishes
//! events whose every rendered field is pinned (fixed ids, fixed
//! timestamps), and asserts the lines that come back — the same oracle
//! a golden would be, taken from a live stream.
//!
//! Readiness is not slept on. A tail can only see what is published
//! after it is listening, so the test publishes a *warm-up* event on a
//! separate invocation until a line appears, and asserts only on the
//! lines carrying the fixture invocation. That makes the suite immune
//! to how long a subscription (or, after the flip, a daemon dial and a
//! tail seek) takes to establish.

#![cfg(unix)]

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use fq_runtime::events::{
    CostMetadata, Event, EventPayload, InvocationArchiveAckedPayload, LlmCallOrigin,
    LlmResponsePayload, StopReason, SystemRecoveryPayload, TokenUsage, ToolCallId,
    ToolResultPayload, TriggerSource, TriggeredPayload, WorkerHeartbeatPayload,
    WorkerOrphanedPayload,
};
use fq_runtime::worker::WorkerId;
use fq_runtime::{AgentId, EventBus};
use uuid::Uuid;

// ------------------------------------------------------------------
// Fixed identities: every field the renderer prints is pinned, so the
// expected lines below are literals rather than patterns.
// ------------------------------------------------------------------

/// 2026-01-02 03:04:05 UTC.
const BASE_MS: i64 = 1_767_323_045_000;
const AGENT: &str = "researcher";
const OTHER_AGENT: &str = "fixer";
const INV: &str = "1c000000-0000-7000-8000-000000000001";
/// The warm-up invocation — a different id so its lines are never
/// confused with the fixture's.
const WARMUP_INV: &str = "9e000000-0000-7000-8000-000000000009";
/// How a rendered warm-up line is recognised, human or `--json`.
const WARMUP_MARK: &str = "9e000000";
/// How a rendered fixture line is recognised.
const FIXTURE_MARK: &str = "1c000000";

/// The first line of the human preamble. **This is the one line the
/// flip changed**, and it is a constant so the change is visible
/// rather than absorbed: the verb used to announce the NATS
/// connection it opened for itself (`Connecting to NATS at
/// nats://…`), and now announces the daemon edge it asks. The three
/// lines after it are unchanged.
const PREAMBLE_CONNECTING: &str = "Connecting to the edge at ";

/// The second line of the human preamble, unfiltered. A constant for
/// the same reason as the one above: it used to echo the raw NATS
/// subject the verb subscribed to (`Subscribing to fq.>`), and now
/// says the typed filter back in domain terms, because the subject
/// argument retired with D8.
const PREAMBLE_SCOPE_ALL: &str = "Tailing all events";

fn uuid_at(n: u32) -> Uuid {
    Uuid::parse_str(&format!("00000000-0000-7000-8000-0000000020{n:02}")).unwrap()
}

fn stamp(mut event: Event, n: u32, at_ms: i64) -> Event {
    event.envelope.event_id = uuid_at(n);
    event.envelope.timestamp = chrono::DateTime::from_timestamp_millis(at_ms).unwrap();
    event
}

fn snapshot_for(agent: &str) -> fq_runtime::events::ConfigSnapshot {
    fq_runtime::Agent::builder()
        .id(agent)
        .model("claude-haiku")
        .system_prompt("You are a deterministic fixture.")
        .build()
        .unwrap()
        .to_snapshot()
}

fn triggered(agent: &str, invocation: &str, n: u32, at_ms: i64) -> Event {
    stamp(
        Event::new(
            AgentId::new(agent).unwrap(),
            Uuid::parse_str(invocation).unwrap(),
            EventPayload::Triggered(TriggeredPayload {
                trigger_id: None,
                trigger_source: TriggerSource::Manual,
                trigger_subject: None,
                trigger_payload: serde_json::Value::String("tail fixture".into()),
                config_snapshot: snapshot_for(agent),
            }),
        ),
        n,
        at_ms,
    )
}

/// The fixture conversation: a trigger, a priced assistant turn, and a
/// tool result — three payload shapes, three renderer branches
/// (plain, cost-bearing, boolean).
fn fixture_events() -> Vec<Event> {
    let assistant = stamp(
        Event::new(
            AgentId::new(AGENT).unwrap(),
            Uuid::parse_str(INV).unwrap(),
            EventPayload::LlmResponse(LlmResponsePayload {
                parts: fq_runtime::events::assistant_parts(
                    Some("Reading the fixture file first.".into()),
                    Vec::new(),
                ),
                round: 1,
                call_id: uuid_at(20),
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage {
                    input_tokens: 1_200,
                    output_tokens: 340,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
                origin: LlmCallOrigin::AgentTurn,
            }),
        ),
        2,
        BASE_MS + 1_000,
    )
    .with_cost(CostMetadata {
        call_id: uuid_at(20),
        model: "claude-haiku".into(),
        input_tokens: 1_200,
        output_tokens: 340,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        input_cost: 0.00875,
        output_cost: 0.00375,
        total_cost: 0.0125,
        cumulative_invocation_cost: 0.0125,
        cumulative_agent_cost: 0.0125,
        origin: LlmCallOrigin::AgentTurn,
    });
    let tool = stamp(
        Event::new(
            AgentId::new(AGENT).unwrap(),
            Uuid::parse_str(INV).unwrap(),
            EventPayload::ToolResult(ToolResultPayload {
                round: 1,
                tool_name: "read_file".into(),
                tool_call_id: ToolCallId::new("tc-1").unwrap(),
                output: "{\"bytes\":42}".into(),
                is_error: false,
                error_kind: None,
                duration_ms: 500,
            }),
        ),
        3,
        BASE_MS + 2_500,
    );
    vec![triggered(AGENT, INV, 1, BASE_MS), assistant, tool]
}

/// What the renderer must print for [`fixture_events`], in order.
/// These literals are the oracle: they were captured from the verb
/// before it was flipped and are unchanged by it.
const EXPECTED_LINES: &[&str] = &[
    "2026-01-02T03:04:05.000Z [1c000000] researcher: triggered source=Manual",
    "2026-01-02T03:04:06.000Z [1c000000] researcher: llm.response tokens=1200/340 stop=ToolUse \
     cost=$0.012500 cumulative=$0.012500",
    "2026-01-02T03:04:07.500Z [1c000000] researcher: tool.result ok",
];

// ------------------------------------------------------------------
// Harness
// ------------------------------------------------------------------

fn scratch() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("fq-events-tail-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(dir.join("cache")).unwrap();
    std::fs::create_dir_all(dir.join("agents")).unwrap();
    std::fs::write(dir.join("fqd.toml"), "[edge]\nbind = \"127.0.0.1:0\"\n").unwrap();
    dir
}

fn parse_fingerprint(hex: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex fingerprint");
    }
    out
}

fn suffix_of<'a>(log: &'a str, prefix: &str) -> &'a str {
    log.lines()
        .find_map(|l| l.trim().strip_prefix(prefix))
        .unwrap_or_else(|| panic!("daemon log lacks {prefix:?}"))
        .trim()
}

/// A running `fq events tail`, with its stdout drained by a reader
/// thread so the child can never block on a full pipe, and its stderr
/// on disk so a death can say why.
///
/// The stderr capture is not decoration. This suite's one flake was a
/// tail that exited mid-test on `edge rpc failed: DeadlineExceeded`,
/// and with stderr discarded all the harness could report was "0 of 3
/// lines" — true, uninformative, and indistinguishable from a slow
/// machine. A streaming verb's test has to be able to tell "it died"
/// from "it is still thinking".
struct Tail {
    child: Child,
    lines: mpsc::Receiver<String>,
    stderr_path: std::path::PathBuf,
}

impl Tail {
    fn next_line(&self, within: Duration) -> Option<String> {
        self.lines.recv_timeout(within).ok()
    }

    /// The tail must still be running. Separates "it died" from "it
    /// rendered the wrong thing", which are different bugs and used to
    /// arrive as the same assertion failure.
    fn assert_alive(&mut self, when: &str) {
        if let Ok(Some(status)) = self.child.try_wait() {
            let stderr = std::fs::read_to_string(&self.stderr_path).unwrap_or_default();
            panic!("the tail died {when} with {status}; its stderr was:\n{stderr}");
        }
    }

    /// Why the tail is not answering: dead (with its status and
    /// stderr) or simply quiet.
    fn diagnosis(&mut self) -> String {
        let stderr = std::fs::read_to_string(&self.stderr_path).unwrap_or_default();
        match self.child.try_wait() {
            Ok(Some(status)) => format!(
                "the tail EXITED with {status}; its stderr was:\n{}",
                if stderr.trim().is_empty() {
                    "<empty>".into()
                } else {
                    stderr
                }
            ),
            Ok(None) => format!(
                "the tail is still running but emitted nothing; stderr so far:\n{}",
                if stderr.trim().is_empty() {
                    "<empty>".into()
                } else {
                    stderr
                }
            ),
            Err(e) => format!("could not poll the tail: {e}"),
        }
    }

    /// Publish a warm-up event until the tail *renders* one, proving
    /// it is listening — no sleep, no guess. The proof has to be a
    /// rendered warm-up line specifically: the human preamble is
    /// printed before the subscription exists, so treating any line as
    /// readiness would publish the fixture into a window nothing was
    /// listening to.
    fn wait_until_listening(&mut self, bus: &EventBus, rt: &tokio::runtime::Runtime) {
        self.wait_until_listening_for(bus, rt, || triggered(AGENT, WARMUP_INV, 99, BASE_MS));
    }

    /// [`Self::wait_until_listening`] for a tail whose filter the
    /// default warm-up would not survive: readiness has to be proven
    /// with an event the tail under test actually admits.
    fn wait_until_listening_for(
        &mut self,
        bus: &EventBus,
        rt: &tokio::runtime::Runtime,
        warmup: impl Fn() -> Event,
    ) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            rt.block_on(bus.publish(&warmup()))
                .expect("publish warm-up");
            while let Some(line) = self.next_line(Duration::from_millis(250)) {
                if line.contains(WARMUP_MARK) {
                    return;
                }
            }
            if Instant::now() >= deadline {
                panic!(
                    "`fq events tail` never rendered a warm-up event — {}",
                    self.diagnosis()
                );
            }
        }
    }

    /// Collect the next `n` lines carrying the fixture invocation,
    /// ignoring warm-up traffic still in flight.
    fn fixture_lines(&mut self, n: usize) -> Vec<String> {
        let mut out = Vec::new();
        while out.len() < n {
            let Some(line) = self.next_line(Duration::from_secs(30)) else {
                panic!(
                    "tail produced {} of {n} fixture lines ({out:?}) — {}",
                    out.len(),
                    self.diagnosis()
                );
            };
            if line.contains(FIXTURE_MARK) {
                out.push(line);
            }
        }
        out
    }
}

impl Drop for Tail {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let _ = self.child.wait();
    }
}

/// The world the tail runs in: a broker, the daemon that serves the
/// edge, and the pairing the client dials it with. **Only this half
/// moved when the verb was flipped** — before it, a broker and an
/// `FQ_NATS_URL` were the whole world, because the verb held its own
/// subscription. The assertions above are unchanged by that move,
/// which is what makes them an oracle rather than a description.
struct World {
    daemon: Option<Child>,
    broker: fq_test_support::NatsServer,
    dir: std::path::PathBuf,
    xdg: tempfile::TempDir,
    rt: tokio::runtime::Runtime,
    bus: EventBus,
    addr: String,
    token: String,
    fingerprint: [u8; 32],
}

impl World {
    fn start() -> Self {
        let broker = fq_test_support::NatsServer::start();
        let dir = scratch();
        let rt = tokio::runtime::Runtime::new().expect("test runtime");
        // Connecting provisions the event stream, so a publish has
        // somewhere to land before anything else starts.
        let bus = rt
            .block_on(EventBus::connect(broker.url()))
            .expect("connect bus");

        let log_path = dir.join("daemon.log");
        let log = std::fs::File::create(&log_path).expect("daemon log");
        let log_err = log.try_clone().expect("log handle");
        let mut daemon = Command::new(env!("CARGO_BIN_EXE_fqd"))
            .env("FQ_DAEMON_CONFIG", dir.join("fqd.toml"))
            .env("FQ_NATS_URL", broker.url())
            .env("FQ_CACHE_DIR", dir.join("cache"))
            .env("FQ_STATE_DIR", dir.join("state"))
            .env("FQ_AGENTS_DIR", dir.join("agents"))
            .env("RUST_LOG", "off")
            .env("NO_COLOR", "1")
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .spawn()
            .expect("spawn fqd");

        let deadline = Instant::now() + Duration::from_secs(30);
        let text = loop {
            if let Some(status) = daemon.try_wait().expect("poll fqd") {
                let text = std::fs::read_to_string(&log_path).unwrap_or_default();
                panic!("fqd exited during startup with {status:?}\n--- log ---\n{text}");
            }
            let text = std::fs::read_to_string(&log_path).unwrap_or_default();
            if text.contains("Runtime ready") {
                break text;
            }
            assert!(
                Instant::now() < deadline,
                "fqd never reached 'Runtime ready'\n--- log ---\n{text}"
            );
            std::thread::sleep(Duration::from_millis(100));
        };
        let addr = suffix_of(&text, "- edge is listening on ").to_string();
        let fingerprint = parse_fingerprint(suffix_of(
            &text,
            "edge: certificate fingerprint (clients pin this): ",
        ));
        let token = {
            let mut lines = text.lines();
            lines
                .find(|l| l.contains("edge: admin token"))
                .expect("admin token marker");
            lines.next().expect("token line").trim().to_string()
        };

        // The client's config names the daemon's actual address, so
        // the flipped verb dials it with no flags.
        std::fs::write(dir.join("fq.toml"), format!("[edge]\nbind = \"{addr}\"\n"))
            .expect("client fq.toml");

        let xdg = tempfile::tempdir().expect("xdg dir");
        let connect = Command::new(fq_client_binary())
            .args(["connect", &addr, "--token", &token])
            .env("FQ_CLI_CONFIG", dir.join("fq.toml"))
            .env("XDG_CONFIG_HOME", xdg.path())
            .env("RUST_LOG", "off")
            .stdin(Stdio::piped())
            .output()
            .expect("run fq connect");
        assert!(
            connect.status.success(),
            "fq connect failed:\n{}",
            String::from_utf8_lossy(&connect.stderr)
        );

        World {
            daemon: Some(daemon),
            broker,
            dir,
            xdg,
            rt,
            bus,
            addr,
            token,
            fingerprint,
        }
    }

    /// A direct edge client, for the atom verbs no CLI verb reaches
    /// yet — `event.stream`'s resume cursor, `event.list`'s window,
    /// `event.get`'s identity.
    fn edge_client(&self) -> fq_edge::EdgeClient {
        self.rt
            .block_on(fq_edge::EdgeClient::connect(
                &self.addr,
                self.fingerprint,
                &self.token,
            ))
            .expect("connect edge")
    }

    /// One `event.stream` batch, straight at the op.
    fn next_batch(
        &self,
        filter: &fq_edge::EdgeClient,
        selection: serde_json::Value,
        from_seq: u64,
        max_wait_ms: u64,
    ) -> fq_edge::wire::StreamBatch {
        self.rt
            .block_on(filter.rpc.next_batch(
                tarpc::context::current(),
                fq_edge::NextBatchRequest {
                    op: fq_ops::OpId::Stream(fq_ops::Domain::Event),
                    version: 1,
                    filter: selection,
                    from_seq,
                    max_wait_ms,
                },
            ))
            .expect("rpc")
            .expect("event.stream")
    }

    /// One read op, straight at the op.
    fn invoke(
        &self,
        client: &fq_edge::EdgeClient,
        op: fq_ops::OpId,
        input: serde_json::Value,
    ) -> serde_json::Value {
        self.invoke_gated(client, op, input, None)
    }

    /// [`Self::invoke`], watermarked. `event.list` reads the
    /// projection, so "publish then list" is a read-your-writes
    /// question: without the gate the test would race the fold and
    /// flake, and sleeping instead would only hide it.
    fn invoke_gated(
        &self,
        client: &fq_edge::EdgeClient,
        op: fq_ops::OpId,
        input: serde_json::Value,
        min_seq: Option<u64>,
    ) -> serde_json::Value {
        self.invoke_result(client, op, input, min_seq)
            .expect("read op")
    }

    /// [`Self::invoke_gated`], with the op's verdict left intact — for
    /// the requests the daemon is supposed to refuse, where the error
    /// *is* the answer under test.
    fn invoke_result(
        &self,
        client: &fq_edge::EdgeClient,
        op: fq_ops::OpId,
        input: serde_json::Value,
        min_seq: Option<u64>,
    ) -> Result<serde_json::Value, fq_edge::wire::WireError> {
        self.rt
            .block_on(client.rpc.invoke(
                tarpc::context::current(),
                fq_edge::InvokeRequest {
                    op,
                    version: 1,
                    input,
                    min_seq,
                },
            ))
            .expect("rpc")
            .map(|response| response.output)
    }

    fn tail(&self, args: &[&str]) -> Tail {
        // One log per tail: a test may run more than one.
        let stderr_path = self.dir.join(format!(
            "tail-{}.err",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut child = Command::new(fq_client_binary())
            .arg("events")
            .arg("tail")
            .args(args)
            .env("FQ_CLI_CONFIG", self.dir.join("fq.toml"))
            .env("FQ_NATS_URL", self.broker.url())
            .env("FQ_CACHE_DIR", self.dir.join("cache"))
            .env("FQ_STATE_DIR", self.dir.join("state"))
            .env("FQ_AGENTS_DIR", self.dir.join("agents"))
            .env("XDG_CONFIG_HOME", self.xdg.path())
            .env("RUST_LOG", "off")
            .env("NO_COLOR", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::from(
                std::fs::File::create(&stderr_path).expect("tail stderr log"),
            ))
            .spawn()
            .expect("spawn fq events tail");
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        Tail {
            child,
            lines,
            stderr_path,
        }
    }

    /// Run one terminating `fq` verb against this world's daemon,
    /// with the pairing `fq connect` stored — the operator's own route
    /// in, flags and all.
    ///
    /// [`Self::tail`] spawns instead of running, because a tail never
    /// exits. Every other verb on this atom does, and the thing under
    /// test for those is the whole run: what it printed, and what it
    /// exited with.
    fn run_fq(&self, args: &[&str]) -> (Option<i32>, String, String) {
        let out = Command::new(fq_client_binary())
            .args(args)
            .env("FQ_CLI_CONFIG", self.dir.join("fq.toml"))
            .env("FQ_NATS_URL", self.broker.url())
            .env("FQ_CACHE_DIR", self.dir.join("cache"))
            .env("FQ_STATE_DIR", self.dir.join("state"))
            .env("FQ_AGENTS_DIR", self.dir.join("agents"))
            .env("XDG_CONFIG_HOME", self.xdg.path())
            .env("RUST_LOG", "off")
            .env("NO_COLOR", "1")
            .output()
            .expect("run fq");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// `fq events get <id>`, as an operator types it: the identity
    /// goes across as a string, unexamined and unmodified.
    fn get_from_the_command_line(&self, event_id: &str) -> (Option<i32>, String, String) {
        self.run_fq(&["events", "get", event_id])
    }

    /// Every `event.list` row's identity for one filter, read at the
    /// log's current tip so the projection is certain to have caught
    /// up with everything published so far.
    fn listed_ids(&self, client: &fq_edge::EdgeClient, filter: &serde_json::Value) -> Ids {
        let tip = self
            .rt
            .block_on(self.bus.last_event_seq())
            .expect("the log's tip");
        self.invoke_gated(
            client,
            fq_ops::OpId::List(fq_ops::Domain::Event),
            filter.clone(),
            Some(tip),
        )
        .as_array()
        .expect("a list of index rows")
        .iter()
        .map(|row| {
            row["event_id"]
                .as_str()
                .expect("a row identity")
                .to_string()
        })
        .collect()
    }

    /// Every identity `event.stream` serves for one filter, from the
    /// start of the log to wherever it currently ends. Drains rather
    /// than reads one batch: a batch is capped, and the question here
    /// is what the whole log answers.
    fn streamed_ids(&self, client: &fq_edge::EdgeClient, filter: &serde_json::Value) -> Ids {
        let mut cursor = 1;
        let mut ids = BTreeSet::new();
        // An empty batch means the poll waited out its window with
        // nothing matching — the end of the log, not a pause.
        for _ in 0..64 {
            let batch = self.next_batch(client, filter.clone(), cursor, 500);
            if batch.items.is_empty() {
                break;
            }
            for item in &batch.items {
                ids.insert(
                    item.item["event"]["envelope"]["event_id"]
                        .as_str()
                        .expect("a streamed event's identity")
                        .to_string(),
                );
            }
            cursor = batch.next_from_seq;
        }
        ids
    }

    /// Publish, returning the last event's log sequence — the
    /// watermark a following projection read gates on.
    fn publish(&self, events: &[Event]) -> u64 {
        let mut last = 0;
        for event in events {
            last = self.rt.block_on(self.bus.publish(event)).expect("publish");
        }
        last
    }

    /// Index one event at `position` **without publishing it** — the
    /// row exists, the log does not know about it.
    ///
    /// The unavailable cases `event.get` names arise in production
    /// from a stream recreated under a projection that outlived it,
    /// and from a cost-bearing row kept past the log's 30-day
    /// retention. Neither is something a test can wait for, and both
    /// are ordinary states of a real system — so the row is written
    /// the way the projection would have written it, with the
    /// position the situation leaves behind.
    fn index_only(&self, event: &Event, position: Option<u64>) {
        let paths = fq_runtime::db::RuntimeDbPaths::under(&self.dir.join("cache"));
        let store = self
            .rt
            .block_on(
                fq_runtime::control_plane::projection::ProjectionStore::open(&paths.projection),
            )
            .expect("open the daemon's projection");
        self.rt
            .block_on(store.insert_event(event, position))
            .expect("index event");
    }

    /// Drop one message from the event log, leaving its index row
    /// behind — retention, without the thirty days.
    fn drop_from_log(&self, seq: u64) {
        let js = self.bus.jetstream();
        let dropped = self.rt.block_on(async {
            js.get_stream(fq_runtime::bus::STREAM_NAME)
                .await
                .expect("the event stream")
                .delete_message(seq)
                .await
                .expect("delete message from the event log")
        });
        assert!(dropped, "the log must have held sequence {seq}");
    }
}

impl Drop for World {
    fn drop(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            unsafe {
                libc::kill(daemon.id() as i32, libc::SIGTERM);
            }
            let _ = daemon.wait();
        }
    }
}

// ------------------------------------------------------------------
// One population, two verbs
// ------------------------------------------------------------------

/// A set of event identities — what both verbs answer with, once the
/// shape of the answer is set aside.
type Ids = BTreeSet<String>;

/// The worker the corpus below names. Not the daemon's own: the
/// events are fixtures, and nothing in the control plane should move
/// because a test published one.
const CORPUS_WORKER: &str = "w-population";
/// The corpus's system-scoped invocation.
const SYS_INV: &str = "5c000000-0000-7000-8000-000000000005";
/// [`OTHER_AGENT`]'s invocation in the corpus.
const OTHER_INV: &str = "2f000000-0000-7000-8000-000000000002";

/// One selection, in the two spellings this test needs: the JSON that
/// travels to both verbs, and the predicate the corpus is scored
/// against.
struct Selection {
    agent: Option<&'static str>,
    event_type: Option<&'static str>,
}

impl Selection {
    fn json(&self, since: &str) -> serde_json::Value {
        let mut filter = serde_json::json!({ "since": since, "limit": 500 });
        if let Some(agent) = self.agent {
            filter["agent"] = agent.into();
        }
        if let Some(event_type) = self.event_type {
            filter["event_type"] = event_type.into();
        }
        filter
    }

    /// Whether this selection admits one corpus event — the test's
    /// own reading of the contract, written out rather than borrowed
    /// from the code under test. `agent` is the **envelope's**, and a
    /// heartbeat is admitted by nothing: a predicate that called
    /// `is_transient` would agree with the implementation by
    /// construction and prove nothing about what it contains.
    fn admits(&self, event: &Event) -> bool {
        event.payload.event_type() != "worker_heartbeat"
            && self
                .agent
                .is_none_or(|a| event.envelope.agent_id.as_str() == a)
            && self
                .event_type
                .is_none_or(|t| event.payload.event_type() == t)
    }

    fn describe(&self) -> String {
        format!("agent={:?} event_type={:?}", self.agent, self.event_type)
    }
}

/// Every shape the two verbs have to agree about, stamped from
/// `base_ms` onwards.
///
/// The interesting property of this corpus is the relationship
/// between an event's **envelope agent** and the **subject** it is
/// published on, because that is exactly where the two verbs used to
/// part company:
///
/// | event                       | envelope agent | subject          |
/// |-----------------------------|----------------|------------------|
/// | `triggered`                 | `researcher`   | `fq.agent.…`     |
/// | `invocation_archive_acked`  | `researcher`   | `fq.worker.…`    |
/// | `system_recovery`           | `system`       | `fq.system.…`    |
/// | `worker_orphaned`           | `system`       | `fq.worker.…`    |
/// | `worker_heartbeat`          | `system`       | `fq.worker.…`    |
///
/// Only the first row is agent-partitioned. The last is transient.
fn population_corpus(base_ms: i64) -> Vec<Event> {
    let worker = || WorkerId::new(CORPUS_WORKER.to_string()).unwrap();
    let system = |n: u32, at_ms: i64, payload: EventPayload| {
        stamp(
            Event::system(Uuid::parse_str(SYS_INV).unwrap(), payload),
            n,
            at_ms,
        )
    };
    vec![
        triggered(AGENT, INV, 60, base_ms),
        triggered(OTHER_AGENT, OTHER_INV, 61, base_ms + 100),
        // An agent's own event, published on a worker's subject. The
        // subject-scoped stream could never deliver this one, while
        // `event.list` returned it for the same `--agent researcher`.
        stamp(
            Event::new(
                AgentId::new(AGENT).unwrap(),
                Uuid::parse_str(INV).unwrap(),
                EventPayload::InvocationArchiveAcked(InvocationArchiveAckedPayload {
                    worker_id: worker(),
                }),
            ),
            62,
            base_ms + 200,
        ),
        // Agent `system`, which has no `fq.agent.*` subject at all —
        // so `--agent system` listed rows that could not be tailed.
        system(
            63,
            base_ms + 300,
            EventPayload::SystemRecovery(SystemRecoveryPayload {
                runtime_id: Uuid::parse_str(SYS_INV).unwrap(),
                worker_id: CORPUS_WORKER.into(),
                safe_resume: 0,
                safe_replay: 0,
                ambiguous: 0,
                total: 0,
            }),
        ),
        system(
            64,
            base_ms + 400,
            EventPayload::WorkerOrphaned(WorkerOrphanedPayload {
                worker_id: worker(),
                last_heartbeat_ms: base_ms,
            }),
        ),
        // The transient. Neither verb serves it.
        system(
            65,
            base_ms + 500,
            EventPayload::WorkerHeartbeat(WorkerHeartbeatPayload {
                worker_id: worker(),
            }),
        ),
    ]
}

/// The selections swept below: each narrowing on its own, the two
/// composed, and the ones that must answer with nothing.
fn population_selections() -> Vec<Selection> {
    let sel = |agent, event_type| Selection { agent, event_type };
    vec![
        sel(None, None),
        sel(Some(AGENT), None),
        sel(Some(OTHER_AGENT), None),
        sel(Some("system"), None),
        // An agent with nothing of its own: both answer empty, and
        // neither leaks somebody else's rows.
        sel(Some("nobody"), None),
        sel(None, Some("triggered")),
        sel(None, Some("invocation_archive_acked")),
        sel(None, Some("system_recovery")),
        sel(None, Some("worker_orphaned")),
        sel(None, Some("worker_heartbeat")),
        sel(Some(AGENT), Some("invocation_archive_acked")),
        sel(Some("system"), Some("worker_heartbeat")),
    ]
}

/// **The test both findings on this atom exist for.** `event.list`
/// and `event.stream` are two reads over one substrate, and the same
/// filter must select the same events from either.
///
/// It did not. The two disagreed twice over, and each disagreement was
/// invisible from the other side:
///
/// - **Population.** List answers from the projection, which never
///   indexed a heartbeat; Stream answers from the log, which holds
///   every one. The declared description said "one row per event".
/// - **What `agent` means.** List narrowed by the envelope's
///   `agent_id`; Stream narrowed by the consumer subject
///   `fq.agent.<id>.>`. Those are the same question only for events
///   that happen to be agent-partitioned — so an archive ack, and
///   every event of agent `system`, listed and could not be tailed.
///
/// So the assertion is not three cases: it is every selection the
/// filter can express over a corpus built to span the shapes, scored
/// against what the corpus itself says the answer is. Both findings
/// fail it, and so would the next one of the same kind.
#[test]
fn list_and_stream_answer_the_same_population_for_every_filter() {
    let world = World::start();
    let client = world.edge_client();

    // `since` bounds both reads to the corpus's own window, so the
    // events the daemon minted while booting are out of scope for
    // both. Millisecond precision on the bound and a second of
    // clearance below it: the projection compares stored text and the
    // log compares parsed instants, and only a bound no event sits
    // exactly on is certainly the same bound to both.
    let base_ms = chrono::Utc::now().timestamp_millis();
    let since = chrono::DateTime::from_timestamp_millis(base_ms)
        .expect("a valid instant")
        .to_rfc3339();
    let corpus = population_corpus(base_ms + 1_000);
    world.publish(&corpus);

    let universe: Ids = corpus
        .iter()
        .map(|e| e.envelope.event_id.to_string())
        .collect();

    for selection in population_selections() {
        let filter = selection.json(&since);
        let expected: Ids = corpus
            .iter()
            .filter(|e| selection.admits(e))
            .map(|e| e.envelope.event_id.to_string())
            .collect();

        // Re-read on disagreement rather than assert on the first
        // pass: an event the daemon published between the two reads
        // is in one answer and not the other for reasons that have
        // nothing to do with the contract, and it is in both on the
        // next pass. A real disagreement never converges.
        let mut attempts = 0;
        let (listed, streamed) = loop {
            let listed = world.listed_ids(&client, &filter);
            let streamed = world.streamed_ids(&client, &filter);
            attempts += 1;
            if listed == streamed || attempts == 3 {
                break (listed, streamed);
            }
        };

        assert_eq!(
            listed,
            streamed,
            "event.list and event.stream disagree for {} — \
             only in list: {:?}; only in stream: {:?}",
            selection.describe(),
            &listed - &streamed,
            &streamed - &listed,
        );
        // …and they agree on the right answer, not merely with each
        // other: both must serve exactly the corpus events the
        // selection admits. Restricted to the corpus because a live
        // daemon keeps minting events of its own, which is not what
        // this is measuring.
        assert_eq!(
            &listed & &universe,
            expected,
            "event.list answered the wrong corpus events for {}",
            selection.describe()
        );
        assert_eq!(
            &streamed & &universe,
            expected,
            "event.stream answered the wrong corpus events for {}",
            selection.describe()
        );
    }
}

/// The largest page `event.list` will serve, as a consumer reads it:
/// off the declared surface, not out of the source.
fn declared_list_cap(surface: &serde_json::Value) -> u64 {
    surface
        .as_array()
        .expect("the surface describes itself as a list of entries")
        .iter()
        .filter_map(|entry| entry.get("atom"))
        .find(|atom| atom["domain"] == "event")
        .expect("the Event atom is on the surface")["filter_schema"]["properties"]["limit"]
        ["maximum"]
        .as_u64()
        .expect("the declared filter says how large a page may be")
}

/// **A List page is bounded, the bound is declared, and an ask above it
/// is refused rather than quietly shortened.**
///
/// `event.list` served whatever `limit` it was handed. That was
/// harmless while `fq events query` read `projection.db` itself, and
/// stopped being harmless when the read moved into the daemon:
/// `--limit -1` arrived as `u32::MAX`, so `LIMIT 4294967295`
/// materialised the whole projection table as one `Vec<EventView>` in
/// daemon memory and then failed to encode, because one List answer is
/// one frame and the edge's codec stops at 8 MiB. The operator paid for
/// the scan and got a transport error for it, and any paired client
/// could ask for that.
///
/// Clamping would have fixed the allocation and broken the answer.
/// List hands back a bare array of index rows — no envelope, no cursor,
/// nowhere to say "there is more" — so a page the daemon shortened is
/// indistinguishable from a listing that ended, and the operator reads
/// a partial answer as the whole one. This is the same failure shape as
/// the population and identity findings on this atom: a surface saying
/// less than it means.
///
/// So the three properties that make a cap honest, asserted end to end:
///
/// 1. **The cap is on the surface.** It is read here off
///    `List(Operation)`, the way a consumer would, rather than
///    hardcoded — and since the refusal and the served page are both
///    measured against that number, a declared cap that disagreed with
///    the enforced one fails this test from either side.
/// 2. **Above it is a refusal**, naming the cap and the cursored read
///    that serves more than a page, so the caller's next request is an
///    edit rather than a guess.
/// 3. **Below it the page is the caller's own number** — which is what
///    makes a row count readable at all: fewer rows than you asked for
///    means there are no more.
#[test]
fn a_list_page_is_bounded_by_a_cap_the_surface_declares() {
    let world = World::start();
    let client = world.edge_client();

    let cap = declared_list_cap(&world.invoke(
        &client,
        fq_ops::OpId::List(fq_ops::Domain::Operation),
        serde_json::json!({}),
    ));

    // A corpus only this test can have published: one agent, one event
    // type, and a `since` below the first of them. The daemon mints
    // events of its own throughout, and the assertions here are about
    // exact counts rather than set membership, so the window has to
    // admit nothing else.
    const CORPUS: u32 = 6;
    let base_ms = chrono::Utc::now().timestamp_millis() + 1_000;
    let since = chrono::DateTime::from_timestamp_millis(base_ms - 500)
        .expect("a valid instant")
        .to_rfc3339();
    let corpus: Vec<Event> = (0..CORPUS)
        .map(|i| triggered(AGENT, INV, 80 + i, base_ms + i64::from(i)))
        .collect();
    let tip = world.publish(&corpus);

    let page = |limit: u64| {
        world.invoke_result(
            &client,
            fq_ops::OpId::List(fq_ops::Domain::Event),
            serde_json::json!({
                "agent": AGENT,
                "event_type": "triggered",
                "since": since,
                "limit": limit,
            }),
            // `event.list` reads the projection: without the watermark
            // the counts below would race the fold.
            Some(tip),
        )
    };
    let rows = |limit: u64| {
        page(limit)
            .unwrap_or_else(|e| panic!("event.list must serve a page of {limit}; got {e:?}"))
            .as_array()
            .expect("a list of index rows")
            .len()
    };

    let err = page(cap + 1).expect_err("a page over the cap must be refused, not served short");
    assert!(
        matches!(&err, fq_edge::wire::WireError::InvalidInput { op, message }
            if op == "event.list"
                && message.contains(&cap.to_string())
                && message.contains("event.stream")),
        "the refusal must name the cap and the cursored read; got {err:?}"
    );

    // The declared cap is servable, not one the daemon trips over —
    // and asking for it over a six-event window answers with six, so
    // "fewer rows than I asked for" is a complete listing rather than
    // a shortened one.
    assert_eq!(
        rows(cap),
        CORPUS as usize,
        "a page at the declared cap must be served, and must not be padded to it"
    );

    // Under the cap, `limit` is the caller's own bound in both
    // directions: never rounded up to the cap, never rounded down to
    // some number the daemon preferred.
    let all = u64::from(CORPUS);
    assert_eq!(
        rows(all + 5),
        CORPUS as usize,
        "asking for more than exists must answer with everything"
    );
    assert_eq!(
        rows(all - 1),
        (CORPUS - 1) as usize,
        "asking for fewer than exists must answer with exactly that many"
    );
}

/// The surface stops serving a transient; the machinery that consumes
/// one must not notice.
///
/// A heartbeat leaves the operator surface because it is operational
/// signal rather than part of the external interface — not because it
/// stopped mattering. It still has to reach the daemon's heartbeat
/// consumer and land as worker liveness, which is where an operator
/// reads it: `worker.list`'s `last_heartbeat_ms`. A change that
/// achieved the population fix by dropping heartbeats earlier — at the
/// producer, or out of the stream's subjects — would pass every
/// assertion above and break the roster, silently.
#[test]
fn a_transient_still_reaches_the_daemons_own_consumers() {
    let world = World::start();
    let client = world.edge_client();
    let workers = fq_ops::OpId::List(fq_ops::Domain::Worker);

    let roster = world.invoke(&client, workers.clone(), serde_json::json!({}));
    // The daemon self-registers its own worker at startup — the row
    // whose heartbeat is a live signal rather than fixture data.
    let worker_id = roster
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["status"] == "alive"))
        .and_then(|row| row["worker_id"].as_str())
        .unwrap_or_else(|| panic!("the daemon runs a worker of its own: {roster}"))
        .to_string();

    // A minute ahead, so nothing but this heartbeat can produce it —
    // the daemon's own are stamped `now` and land every 10s.
    let marker_ms = chrono::Utc::now().timestamp_millis() + 60_000;
    let heartbeat = stamp(
        Event::system(
            Uuid::parse_str(SYS_INV).unwrap(),
            EventPayload::WorkerHeartbeat(WorkerHeartbeatPayload {
                worker_id: WorkerId::new(worker_id.clone()).unwrap(),
            }),
        ),
        70,
        marker_ms,
    );
    world.publish(std::slice::from_ref(&heartbeat));

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let roster = world.invoke(&client, workers.clone(), serde_json::json!({}));
        let seen = roster
            .as_array()
            .expect("a roster")
            .iter()
            .find(|row| row["worker_id"].as_str() == Some(worker_id.as_str()))
            .and_then(|row| row["last_heartbeat_ms"].as_i64())
            .unwrap_or_default();
        if seen >= marker_ms {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the heartbeat never reached the control plane — `worker.list` still \
             reports {seen} for {worker_id}, and the marker was {marker_ms}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // The same event is on neither operator read, which is the whole
    // arrangement: consumed by the machinery, absent from the surface.
    let filter = serde_json::json!({ "event_type": "worker_heartbeat", "limit": 500 });
    let id = heartbeat.envelope.event_id.to_string();
    assert!(!world.listed_ids(&client, &filter).contains(&id));
    assert!(!world.streamed_ids(&client, &filter).contains(&id));
}

// ------------------------------------------------------------------
// The oracle
// ------------------------------------------------------------------

/// The verb's product: one rendered line per event, payload summary
/// included. This is what an operator watches, and it is unchanged by
/// the flip.
#[test]
fn tail_renders_every_published_event() {
    let world = World::start();
    let mut tail = world.tail(&[]);

    // The preamble comes first, before any event.
    let first = tail.next_line(Duration::from_secs(30)).expect("preamble");
    assert!(
        first.starts_with(PREAMBLE_CONNECTING),
        "preamble line 1 should start with {PREAMBLE_CONNECTING:?}; got {first:?}"
    );
    assert_eq!(
        tail.next_line(Duration::from_secs(5)).as_deref(),
        Some(PREAMBLE_SCOPE_ALL)
    );
    assert_eq!(
        tail.next_line(Duration::from_secs(5)).as_deref(),
        Some("Press Ctrl-C to exit.")
    );
    assert_eq!(tail.next_line(Duration::from_secs(5)).as_deref(), Some(""));

    tail.wait_until_listening(&world.bus, &world.rt);
    world.publish(&fixture_events());

    assert_eq!(tail.fixture_lines(EXPECTED_LINES.len()), EXPECTED_LINES);
}

/// `--json` is the machine contract: one whole event per line, and
/// nothing else on stdout — no preamble to strip.
#[test]
fn tail_json_emits_one_whole_event_per_line() {
    let world = World::start();
    let mut tail = world.tail(&["--json"]);

    tail.wait_until_listening(&world.bus, &world.rt);
    world.publish(&fixture_events());

    let lines = tail.fixture_lines(3);
    let events: Vec<Event> = lines
        .iter()
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| panic!("line is not an Event: {e}\n{l}"))
        })
        .collect();
    let ids: Vec<Uuid> = events.iter().map(|e| e.envelope.event_id).collect();
    assert_eq!(ids, vec![uuid_at(1), uuid_at(2), uuid_at(3)]);
    assert!(
        matches!(events[1].payload, EventPayload::LlmResponse(_)),
        "the whole payload rides the json line, not a summary of it"
    );
    assert_eq!(
        events[1].envelope.cost.as_ref().map(|c| c.total_cost),
        Some(0.0125),
        "the envelope's cost survives the trip"
    );
}

/// `--agent` narrows the tail. This is the narrowing the retired
/// `--subject fq.agent.<id>.>` expressed, and the assertion is
/// unchanged by the swap — what changed is where the narrowing
/// happens: the typed filter travels, so the other agent's event never
/// leaves the daemon rather than being sieved out at the terminal.
#[test]
fn tail_honours_an_agent_filter() {
    let world = World::start();
    let mut tail = world.tail(&["--agent", AGENT]);

    tail.wait_until_listening(&world.bus, &world.rt);
    // The other agent's event is published FIRST, so if it were going
    // to be rendered it would arrive before the fixture's.
    world.publish(&[triggered(
        OTHER_AGENT,
        "2f000000-0000-7000-8000-000000000002",
        50,
        BASE_MS,
    )]);
    world.publish(&fixture_events());

    let lines = tail.fixture_lines(EXPECTED_LINES.len());
    assert_eq!(lines, EXPECTED_LINES);
    assert!(
        !lines.iter().any(|l| l.contains(OTHER_AGENT)),
        "an agent-scoped tail must not render another agent's events: {lines:?}"
    );
}

/// `--event-type` narrows to one event type across every agent — the
/// other half of the typed filter, and the replacement for the subject
/// patterns that named a leaf (`fq.agent.*.triggered`).
#[test]
fn tail_honours_an_event_type_filter() {
    let world = World::start();
    let mut tail = world.tail(&["--json", "--event-type", "tool_result"]);

    // A trigger cannot prove readiness for this tail — the filter
    // excludes it — so the warm-up is a tool result on the warm-up
    // invocation, which the filter admits.
    tail.wait_until_listening_for(&world.bus, &world.rt, || {
        let mut warmup = fixture_events().pop().expect("the fixture's tool result");
        warmup.envelope.invocation_id = Uuid::parse_str(WARMUP_INV).unwrap();
        warmup
    });

    world.publish(&fixture_events());

    // The fixture is a trigger, a priced llm.response and a tool
    // result; only the last is this tail's, so the *first* fixture
    // line it renders is the assertion.
    let line = tail.fixture_lines(1).pop().expect("one fixture line");
    let event: Event = serde_json::from_str(&line).expect("a whole event per line");
    assert!(
        matches!(event.payload, EventPayload::ToolResult(_)),
        "a type-filtered tail renders only that type; got {:?}",
        event.payload
    );
    assert_eq!(event.envelope.event_id, uuid_at(3));
}

// ------------------------------------------------------------------
// The Event atom itself: what the flip bought, and the two verbs no
// CLI verb reaches yet.
// ------------------------------------------------------------------

/// **The behaviour the flip changes.** A core-NATS subscription is
/// live-only and lossy: an event published while the consumer is away
/// (or merely behind) is gone, silently. `event.stream` is positional,
/// so a caller that comes back with its cursor is handed every event
/// after it — including the ones published while nothing was listening.
///
/// This is not a refactor of the old behaviour. It is a different
/// promise, and it is the reason verb 11 was flipped at all.
#[test]
fn a_stream_resumed_at_its_cursor_loses_nothing() {
    let world = World::start();
    let client = world.edge_client();
    let all = serde_json::json!({});

    // Seek the tail: no items, a concrete cursor.
    let seek = world.next_batch(&client, all.clone(), u64::MAX, 0);
    assert!(seek.items.is_empty(), "a tail seek consumes nothing");
    assert!(seek.next_from_seq < u64::MAX, "a concrete resume cursor");

    // Published with NO reader on the stream at all — the case the
    // old subscription could not survive.
    world.publish(&fixture_events());

    let batch = world.next_batch(&client, all.clone(), seek.next_from_seq, 10_000);
    let ids: Vec<String> = batch
        .items
        .iter()
        .map(|i| {
            i.item["event"]["envelope"]["event_id"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(
        ids,
        vec![
            uuid_at(1).to_string(),
            uuid_at(2).to_string(),
            uuid_at(3).to_string()
        ],
        "every event published while away is delivered on resume: {batch:?}"
    );
    // Each item is addressed by its log sequence, and the state
    // carries the same number — the universal cursor (P5).
    for item in &batch.items {
        assert_eq!(item.item["seq"].as_u64(), Some(item.seq));
    }

    // And resuming a second time picks up exactly where this batch
    // ended, with nothing repeated and nothing skipped.
    world.publish(&[triggered(AGENT, INV, 4, BASE_MS + 9_000)]);
    let next = world.next_batch(&client, all, batch.next_from_seq, 10_000);
    let ids: Vec<String> = next
        .items
        .iter()
        .map(|i| {
            i.item["event"]["envelope"]["event_id"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(ids, vec![uuid_at(4).to_string()], "no gap, no repeat");
}

/// `event.list` answers with the most recent window of the projection
/// index, and its filter is typed: agent, event type, since, limit —
/// the same narrowing `fq events query` offers, moved onto the wire.
///
/// The rows are index rows, not events (plan Phase 4, cohort 4.2):
/// extracted fields, no payload. What keeps that from being a lossy
/// answer is the `event_id` on every row, and
/// [`a_list_row_walks_to_its_whole_event`] is that half of the
/// contract.
#[test]
fn list_answers_the_recent_window_through_a_typed_filter() {
    let world = World::start();
    let client = world.edge_client();
    let list = fq_ops::OpId::List(fq_ops::Domain::Event);

    world.publish(&fixture_events());
    let watermark = world.publish(&[triggered(
        OTHER_AGENT,
        "2f000000-0000-7000-8000-000000000002",
        50,
        BASE_MS + 60_000,
    )]);

    let event_ids = |value: &serde_json::Value| -> Vec<String> {
        value
            .as_array()
            .expect("a list of index rows")
            .iter()
            .map(|e| e["event_id"].as_str().unwrap().to_string())
            .collect()
    };
    let listed = |input: serde_json::Value| {
        world.invoke_gated(&client, list.clone(), input, Some(watermark))
    };

    // Unfiltered: everything the fixture published, plus whatever the
    // daemon's own boot wrote — so assert on the fixture's own ids.
    let all = listed(serde_json::json!({}));
    let all_ids = event_ids(&all);
    for n in [1u32, 2, 3, 50] {
        assert!(
            all_ids.contains(&uuid_at(n).to_string()),
            "event {n} missing from the unfiltered list: {all_ids:?}"
        );
    }

    // The declared contract, asserted: a row is the index's fields,
    // and the payload is not among them. A regression that started
    // serving whole events here would be a silent performance cliff on
    // the verb an operator reaches for when the log is largest, so it
    // is pinned rather than left to the schema snapshot.
    let row = &all.as_array().unwrap()[0];
    assert!(
        row.get("event").is_none() && row.get("payload").is_none(),
        "an index row must not carry the payload: {row}"
    );
    for field in ["event_id", "timestamp", "agent_id", "event_type"] {
        assert!(row.get(field).is_some(), "index row lacks `{field}`: {row}");
    }
    // And the log position is NOT among them: it is an internal
    // locator `event.get` resolves through, not something a caller
    // holds. A row that handed one back would be inviting a consumer
    // to store a transport coordinate as an identity, which is the
    // habit this atom was corrected out of.
    assert!(
        row.get("seq").is_none(),
        "an index row must not hand back the log position: {row}"
    );

    // By agent: the query is narrowed at the daemon, so the other
    // agent's event never leaves it.
    let mine = event_ids(&listed(serde_json::json!({"agent": AGENT})));
    assert!(
        !mine.contains(&uuid_at(50).to_string()),
        "an agent filter must exclude other agents: {mine:?}"
    );

    // By type.
    let responses = event_ids(&listed(
        serde_json::json!({"agent": AGENT, "event_type": "llm_response"}),
    ));
    assert_eq!(responses, vec![uuid_at(2).to_string()]);

    // By instant: `since` is inclusive, so the tool result at
    // +2500ms is in and the trigger at +0 is out. The `Z` spelling is
    // deliberate — the index stores `+00:00`, and the two are the same
    // instant only because the filter is parsed rather than pasted.
    let recent = event_ids(&listed(
        serde_json::json!({"agent": AGENT, "since": "2026-01-02T03:04:07.500Z"}),
    ));
    assert_eq!(recent, vec![uuid_at(3).to_string()]);

    // By limit: the *most recent* N, not the first N.
    let last_one = event_ids(&listed(serde_json::json!({"agent": AGENT, "limit": 1})));
    assert_eq!(last_one, vec![uuid_at(3).to_string()]);

    // An unparseable instant is a verdict on the request, not an
    // empty answer the caller would read as "no such events". The
    // index compares text, so without the parse this would have been
    // the quietest possible wrong answer.
    let refused = world.rt.block_on(client.rpc.invoke(
        tarpc::context::current(),
        fq_edge::InvokeRequest {
            op: list,
            version: 1,
            input: serde_json::json!({"since": "yesterday"}),
            min_seq: None,
        },
    ));
    assert!(
        matches!(
            refused.expect("rpc"),
            Err(fq_edge::wire::WireError::InvalidInput { .. })
        ),
        "an unparseable `since` must be refused, not silently matched"
    );
}

/// The condition that makes a payload-free `event.list` legitimate:
/// **every row names the identity `event.get` takes**, so a consumer
/// walks from any listing to the whole event without constructing a
/// key of its own.
///
/// This is the hard half of the contract. `event.list` excludes
/// payloads on the strength of it, so the walk is executed here — a
/// row is listed, its `event_id` is handed straight back to
/// `event.get`, and the event that comes out has to be the one the
/// row described. A projection that stopped recording log positions
/// would still list perfectly well and would fail exactly here.
#[test]
fn a_list_row_walks_to_its_whole_event() {
    let world = World::start();
    let client = world.edge_client();

    let watermark = world.publish(&fixture_events());
    let listed = world.invoke_gated(
        &client,
        fq_ops::OpId::List(fq_ops::Domain::Event),
        serde_json::json!({"agent": AGENT, "event_type": "llm_response"}),
        Some(watermark),
    );
    let rows = listed.as_array().expect("index rows");
    assert_eq!(
        rows.len(),
        1,
        "one priced response in the fixture: {listed}"
    );
    let row = &rows[0];
    let event_id = row["event_id"]
        .as_str()
        .unwrap_or_else(|| panic!("a listed row must carry its `event.get` identity: {row}"));

    // The walk itself — nothing constructed, the row's own identity,
    // in the domain's terms rather than the transport's.
    let whole = world.invoke(
        &client,
        fq_ops::OpId::Get(fq_ops::Domain::Event),
        serde_json::json!({ "event_id": event_id }),
    );
    assert_eq!(
        whole["event"]["envelope"]["event_id"].as_str(),
        Some(event_id),
        "the walk must land on the event the row described"
    );
    // …and what the walk buys is the payload the row does not carry.
    // `parts[0].text` rather than `content`: schema v3 made an assistant
    // turn an ordered part list (ADR-0034), so the turn's text is a part.
    assert_eq!(
        whole["event"]["payload"]["payload"]["parts"][0]["text"].as_str(),
        Some("Reading the fixture file first."),
        "event.get answers with the whole event: {whole}"
    );
}

/// **The same walk, from a terminal.** The atom's half of this is
/// pinned above; this is the half an operator has.
///
/// It was unreachable. `event.get` worked, was declared, and was
/// exercised over the edge — and no CLI verb called it, while the
/// human `fq events query` table printed timestamp / agent / event /
/// cost / an eight-character invocation prefix and no identity at
/// all. So the walk the atom's declared description promises existed
/// only for a consumer willing to pipe `--json` through `jq`, which
/// is not the operator surface saying what it does.
///
/// The test takes the walk the way an operator takes it: **the
/// identity is read off the rendered table** and handed to `fq events
/// get` unchanged. Nothing is constructed here and nothing is looked
/// up out of band, which is what makes this a test of the surface
/// rather than of the atom underneath it.
///
/// That is also what pins the constraint the column design turns on:
/// an identity truncated to fit the table — the tempting fix, and the
/// precedent the `invocation` column set — passes every assertion
/// about which columns exist and fails here, because `event.get`
/// resolves an exact `event_id` and has no prefix search. A walk that
/// looks reachable and is not would be worse than the honest absence
/// this replaced.
#[test]
fn a_listed_event_walks_to_its_whole_event_from_the_command_line() {
    let world = World::start();
    let client = world.edge_client();

    let watermark = world.publish(&fixture_events());
    // `fq events query` reads the projection, so wait for the fold
    // rather than racing it — the same read-your-writes gate the
    // atom-level walk uses, taken here so the CLI run is deterministic.
    world.invoke_gated(
        &client,
        fq_ops::OpId::List(fq_ops::Domain::Event),
        serde_json::json!({"agent": AGENT, "event_type": "llm_response"}),
        Some(watermark),
    );

    let (exit, listing, stderr) = world.run_fq(&[
        "events",
        "query",
        "--agent",
        AGENT,
        "--event-type",
        "llm_response",
    ]);
    assert_eq!(exit, Some(0), "`fq events query` failed:\n{stderr}");
    let mut rows = listing.lines();
    let header = rows.next().expect("a header line");
    assert!(
        header.ends_with("event-id"),
        "the listing must name the identity column it prints; got {header:?}"
    );
    let row = rows
        .next()
        .unwrap_or_else(|| panic!("one priced response in the fixture, so one row:\n{listing}"));
    assert_eq!(rows.next(), None, "exactly one row:\n{listing}");

    // What an operator would copy: the last field of the row.
    let printed = row.split_whitespace().last().expect("a last column");
    // Whole, not a prefix — asserted here as well as walked below,
    // because a truncation that happened to still resolve (it cannot,
    // but a future prefix search might) would otherwise hide the
    // change in contract.
    assert_eq!(
        printed,
        uuid_at(2).to_string(),
        "the listing must print the identity in full:\n{listing}"
    );

    // The walk itself, through the verb.
    let (exit, whole, stderr) = world.get_from_the_command_line(printed);
    assert_eq!(exit, Some(0), "`fq events get {printed}` failed:\n{stderr}");
    // The detail is the row's event, said back whole. Every line is a
    // literal because every field it renders is pinned by the fixture.
    let expected = format!(
        "Event: {id}\n  \
         time:        2026-01-02T03:04:06.000Z\n  \
         agent:       researcher\n  \
         invocation:  {INV}\n  \
         type:        llm_response\n  \
         summary:     llm.response tokens=1200/340 stop=ToolUse cost=$0.012500 \
         cumulative=$0.012500\n",
        id = uuid_at(2)
    );
    assert!(
        whole.starts_with(&expected),
        "expected the detail to open:\n{expected}\n…but it was:\n{whole}"
    );
    // …and the invocation the listing no longer has room for is here,
    // in full, which is what makes dropping that column a move rather
    // than a loss.
    assert!(whole.contains(INV), "the whole event names its invocation");
    // What the walk buys: the payload an index row does not carry.
    assert!(
        whole.contains("Reading the fixture file first."),
        "`fq events get` must answer with the payload:\n{whole}"
    );

    // The machine route agrees, and emits the same shape `fq events
    // tail --json` does, so one parser reads either verb.
    let (exit, json, stderr) = world.run_fq(&["events", "get", printed, "--json"]);
    assert_eq!(exit, Some(0), "`fq events get --json` failed:\n{stderr}");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("one JSON event");
    assert_eq!(parsed["envelope"]["event_id"].as_str(), Some(printed));
    assert_eq!(
        parsed["payload"]["payload"]["parts"][0]["text"].as_str(),
        Some("Reading the fixture file first.")
    );
}

/// `event.get` addresses one event by the identity the event stamps
/// on itself, not by where the transport happened to put it.
#[test]
fn get_answers_by_the_events_own_identity() {
    let world = World::start();
    let client = world.edge_client();
    let get = fq_ops::OpId::Get(fq_ops::Domain::Event);

    let seq = world
        .rt
        .block_on(world.bus.publish(&triggered(AGENT, INV, 1, BASE_MS)))
        .expect("publish");
    // The index is the first hop, so the read is only answerable once
    // the fold has seen the event.
    world.invoke_gated(
        &client,
        fq_ops::OpId::List(fq_ops::Domain::Event),
        serde_json::json!({"agent": AGENT}),
        Some(seq),
    );

    let got = world.invoke(
        &client,
        get.clone(),
        serde_json::json!({"event_id": uuid_at(1).to_string()}),
    );
    assert_eq!(
        got["event"]["envelope"]["event_id"].as_str(),
        Some(uuid_at(1).to_string().as_str())
    );

    let refused = |input: serde_json::Value| {
        world
            .rt
            .block_on(client.rpc.invoke(
                tarpc::context::current(),
                fq_edge::InvokeRequest {
                    op: get.clone(),
                    version: 1,
                    input,
                    min_seq: None,
                },
            ))
            .expect("rpc")
            .expect_err("must not answer")
    };

    // An identity nothing has ever carried is a plain miss — the
    // request was fine, the entity isn't there.
    assert!(
        matches!(
            refused(serde_json::json!({"event_id": Uuid::now_v7().to_string()})),
            fq_edge::wire::WireError::NotFound { .. }
        ),
        "an unknown identity is NotFound"
    );

    // A string that is not a UUID cannot name any event — every id in
    // the index was written from one — so it is a verdict on the
    // request rather than an empty answer.
    let err = refused(serde_json::json!({"event_id": "the-second-one"}));
    assert!(
        matches!(&err, fq_edge::wire::WireError::InvalidInput { op, message }
            if op == "event.get" && message.contains("the-second-one")),
        "expected an InvalidInput naming what was written; got {err:?}"
    );
}

/// **The test this change exists for.** A stored log position is a
/// transport coordinate held across a boundary the transport makes no
/// promise about: recreate `fq-events` and sequence 42 belongs to a
/// different event, under an index that survived. `event.get` must
/// notice, because the alternative is not an error — it is a
/// confident answer with somebody else's payload in it.
///
/// The stale locator is arranged the way production produces one: an
/// index row whose recorded position addresses an event that is
/// genuinely there, and is genuinely not the one asked for. The
/// neighbour is proven to be sitting at that position, readable by
/// its own identity — so what stops it being returned is the
/// identity check inside `event.get` and nothing else. Remove that
/// check and this test does not error differently; it passes back
/// the wrong event.
#[test]
fn a_stale_locator_is_caught_rather_than_answered_with_a_neighbour() {
    let world = World::start();
    let client = world.edge_client();
    let get = fq_ops::OpId::Get(fq_ops::Domain::Event);

    // The event that really is in the log, at a real position.
    let neighbour = triggered(AGENT, INV, 1, BASE_MS);
    let occupied = world.publish(std::slice::from_ref(&neighbour));
    world.invoke_gated(
        &client,
        fq_ops::OpId::List(fq_ops::Domain::Event),
        serde_json::json!({"agent": AGENT}),
        Some(occupied),
    );

    // The event whose index row lies about where its payload is —
    // never published, indexed at the neighbour's position, exactly
    // as a surviving projection describes a recreated log.
    let stale = triggered(AGENT, INV, 7, BASE_MS + 7_000);
    world.index_only(&stale, Some(occupied));

    // The neighbour is right there to be handed back by mistake.
    let its_own = world.invoke(
        &client,
        get.clone(),
        serde_json::json!({"event_id": neighbour.envelope.event_id.to_string()}),
    );
    assert_eq!(
        its_own["event"]["envelope"]["event_id"].as_str(),
        Some(neighbour.envelope.event_id.to_string().as_str()),
        "the position genuinely holds a readable event"
    );

    let err = world
        .rt
        .block_on(client.rpc.invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: get,
                version: 1,
                input: serde_json::json!({
                    "event_id": stale.envelope.event_id.to_string(),
                }),
                min_seq: None,
            },
        ))
        .expect("rpc")
        .expect_err("a stale locator must not be answered");
    assert!(
        matches!(&err, fq_edge::wire::WireError::Gone { op, message }
            if op == "event.get"
                && message.contains(&stale.envelope.event_id.to_string())
                && message.contains(&neighbour.envelope.event_id.to_string())),
        "the failure must name both the event asked for and the one found at that \
         position, or an operator cannot tell a rewound log from a missing event; \
         got {err:?}"
    );
}

/// The first unavailable case: **the locator is unknown.** The row
/// predates the `seq` column, or the delivery's JetStream metadata
/// could not be read — the event is indexed, and where its payload
/// sits is not.
///
/// Not a `NotFound`: this daemon has seen the event and says so.
#[test]
fn an_indexed_event_with_no_recorded_position_says_which_half_is_missing() {
    let world = World::start();
    let client = world.edge_client();

    let unlocated = triggered(AGENT, INV, 3, BASE_MS + 3_000);
    world.index_only(&unlocated, None);

    // It lists — the row is whole as an index row.
    let listed = world.invoke(
        &client,
        fq_ops::OpId::List(fq_ops::Domain::Event),
        serde_json::json!({"agent": AGENT}),
    );
    let ids: Vec<&str> = listed
        .as_array()
        .expect("index rows")
        .iter()
        .filter_map(|row| row["event_id"].as_str())
        .collect();
    assert!(
        ids.contains(&unlocated.envelope.event_id.to_string().as_str()),
        "an unlocated row still lists: {ids:?}"
    );

    let err = world
        .rt
        .block_on(client.rpc.invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: fq_ops::OpId::Get(fq_ops::Domain::Event),
                version: 1,
                input: serde_json::json!({
                    "event_id": unlocated.envelope.event_id.to_string(),
                }),
                min_seq: None,
            },
        ))
        .expect("rpc")
        .expect_err("an unlocated row cannot be read whole");
    assert!(
        matches!(&err, fq_edge::wire::WireError::Unlocatable { op, .. } if op == "event.get"),
        "expected Unlocatable, not a miss — the event is known; got {err:?}"
    );
}

/// The second unavailable case: **the payload is gone.** The position
/// is known and the log no longer holds it.
///
/// This is not hypothetical arithmetic. Cost-bearing rows are exempt
/// from the retention sweep and kept indefinitely, while the event
/// log keeps thirty days — so every retained row eventually reaches
/// this state, and `Gone` is the true answer about an old fact rather
/// than a fault. The thirty days are compressed here into one
/// deleted message; the row is untouched, which is the point.
#[test]
fn a_row_the_log_has_dropped_says_the_payload_is_gone_not_the_event() {
    let world = World::start();
    let client = world.edge_client();

    let aged = triggered(AGENT, INV, 5, BASE_MS + 5_000);
    let seq = world.publish(std::slice::from_ref(&aged));
    // Publish after it, so the read finds a live log that simply no
    // longer has this message — not an empty one.
    let tip = world.publish(&[triggered(OTHER_AGENT, INV, 6, BASE_MS + 6_000)]);
    // Let the fold record the position before the message goes.
    world.invoke_gated(
        &client,
        fq_ops::OpId::List(fq_ops::Domain::Event),
        serde_json::json!({}),
        Some(tip),
    );
    world.drop_from_log(seq);

    let err = world
        .rt
        .block_on(client.rpc.invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: fq_ops::OpId::Get(fq_ops::Domain::Event),
                version: 1,
                input: serde_json::json!({
                    "event_id": aged.envelope.event_id.to_string(),
                }),
                min_seq: None,
            },
        ))
        .expect("rpc")
        .expect_err("the payload is no longer readable");
    assert!(
        matches!(&err, fq_edge::wire::WireError::Gone { op, .. } if op == "event.get"),
        "expected Gone — the row is retained by policy, the payload is not; got {err:?}"
    );
}

/// **The three states, told apart at a terminal.**
///
/// `EventLocation` answers `Unindexed`, `At(seq)` or `Unlocated`, and
/// the wire carries `NotFound`, `Unlocatable` and `Gone`. Those names
/// exist because these are three different facts about a real system:
/// no row at all; a row whose payload we cannot locate; a payload the
/// log has passed. The two tests above prove the daemon distinguishes
/// them. This one proves the *operator* can, which is a separate
/// claim — every one of the three renders through `WireError`'s
/// `Display`, and all three of those impls are the same
/// `` `{op}`: {message} `` string, so a verb that simply surfaced the
/// error would hand back three sentences of prose that differ only in
/// their middle.
///
/// So the assertion is not that each errors. It is that each names
/// **its own** state and **neither of the other two** — which is what
/// a collapse would break, and what a test asserting "it failed"
/// would sail straight past. The verdicts are scored against each
/// other rather than checked one at a time, so any two of them
/// becoming one word fails here.
#[test]
fn the_three_unavailable_states_read_differently_from_the_command_line() {
    let world = World::start();
    let client = world.edge_client();

    // 1. Unindexed — an identity this daemon has never seen.
    let missing = Uuid::now_v7();

    // 2. Unlocated — a row, and no position. Exactly what a row
    //    projected before the index recorded log positions looks like.
    let unlocated = triggered(AGENT, INV, 3, BASE_MS + 3_000);
    world.index_only(&unlocated, None);

    // 3. Gone — a position the log has since passed. Publish after it
    //    so the log is alive and simply no longer holds this message,
    //    and gate on the tip so the fold has recorded the position
    //    before it is deleted.
    let aged = triggered(AGENT, INV, 5, BASE_MS + 5_000);
    let seq = world.publish(std::slice::from_ref(&aged));
    let tip = world.publish(&[triggered(OTHER_AGENT, INV, 6, BASE_MS + 6_000)]);
    world.invoke_gated(
        &client,
        fq_ops::OpId::List(fq_ops::Domain::Event),
        serde_json::json!({}),
        Some(tip),
    );
    world.drop_from_log(seq);

    // The word each state must own, and — by construction — must not
    // share. `gone` is a substring of nothing else here, and `not
    // found` and `unlocatable` share no word, so "names mine, names
    // neither of theirs" is a total scoring of the three.
    let cases = [
        ("not found", missing.to_string()),
        ("unlocatable", unlocated.envelope.event_id.to_string()),
        ("gone", aged.envelope.event_id.to_string()),
    ];
    for (verdict, event_id) in &cases {
        let (exit, stdout, stderr) = world.get_from_the_command_line(event_id);
        assert_eq!(
            exit,
            Some(1),
            "an unreadable event must exit non-zero; got stdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            stdout.is_empty(),
            "nothing may reach stdout when there is no event to print; got:\n{stdout}"
        );
        assert!(
            stderr.starts_with(&format!("{verdict}:")),
            "the verdict must open with the state's own name `{verdict}`; got:\n{stderr}"
        );
        // The event's identity is echoed, so an operator reading a
        // scrollback knows which of several reads this verdict is
        // about.
        assert!(
            stderr.contains(event_id),
            "the verdict must name the event asked for; got:\n{stderr}"
        );
        // …and not one of the others' names. This is the assertion
        // that bites when three states become one message.
        for (other, _) in &cases {
            assert!(
                other == verdict || !stderr.contains(other),
                "`{verdict}` must not also read as `{other}` — the three states are three \
                 facts, not three spellings of one; got:\n{stderr}"
            );
        }
    }

    // A fourth outcome, kept outside the three: an id that is not a
    // UUID names no event and never could, so it is a verdict on the
    // request rather than a state of the system. It must not be
    // absorbed into any of the three above — an operator who typed the
    // id wrongly is not being told their event aged out of the log.
    let (exit, _, stderr) = world.get_from_the_command_line("the-second-one");
    assert_eq!(exit, Some(1), "a malformed id is refused");
    assert!(
        stderr.contains("the-second-one")
            && cases.iter().all(|(verdict, _)| !stderr.contains(verdict)),
        "a malformed id must quote what was written and claim none of the three states; \
         got:\n{stderr}"
    );
}

/// A regression this cohort found in the **Turn** atom, which has the
/// same shape as the Event atom's List.
///
/// Both walk the log until they see the stream's last sequence — but
/// the walk only ever sees messages matching its own filter subject,
/// so when the tip belongs to something else (another agent, a system
/// event, a heartbeat) the walk waits for a message it will never be
/// handed. `turn.list` had been surviving on timing: a transcript read
/// usually follows that invocation's own events closely enough that
/// the tip is still one of them. Here the tip is deliberately another
/// agent's event, which before the fix hung until the caller's
/// deadline expired.
#[test]
fn a_list_scan_ends_when_the_stream_tip_is_not_its_own() {
    let world = World::start();
    let client = world.edge_client();

    // The invocation under test, established by its trigger — the
    // event `turn.list` resolves the agent from.
    let mine = world
        .rt
        .block_on(world.bus.publish(&triggered(AGENT, INV, 1, BASE_MS)))
        .expect("publish");
    // …and then the tip becomes somebody else's.
    world
        .rt
        .block_on(world.bus.publish(&triggered(
            OTHER_AGENT,
            "2f000000-0000-7000-8000-000000000002",
            50,
            BASE_MS + 1_000,
        )))
        .expect("publish other agent");

    let answered = world.rt.block_on(client.rpc.invoke(
        tarpc::context::current(),
        fq_edge::InvokeRequest {
            op: fq_ops::OpId::List(fq_ops::Domain::Turn),
            version: 1,
            input: serde_json::json!({"invocation_id": INV}),
            // Read-your-writes on the trigger, so the agent lookup
            // this list depends on is certain to resolve.
            min_seq: Some(mine),
        },
    ));
    let turns = answered
        .expect("turn.list must answer, not hang until the deadline")
        .expect("turn.list");
    // A trigger is not a turn, so the answer is empty — *answering* is
    // the assertion.
    assert_eq!(turns.output, serde_json::json!([]));
}

/// A tail that sees nothing for a while must still be a tail.
///
/// This caught a real bug, and how it catches it is the whole design
/// of the test. `fq events tail` asks the daemon to hold each poll for
/// 30s, but a tarpc call carries its own deadline and the default is a
/// flat **10s** — so a poll that waits out its window is abandoned by
/// the client that asked for it, and the verb exits with `edge rpc
/// failed: DeadlineExceeded`.
///
/// The obvious version of this test — tail everything, idle, publish —
/// did **not** catch it when the tail served heartbeats, and I wrote
/// that version first. An unfiltered tail saw the daemon's worker
/// heartbeat, which lands every 10s (`DEFAULT_INTERVAL_MS`) and ended
/// the poll in a photo finish with the 10s deadline: measured at
/// 9.908s, 9.997s, then 10.004s and death. That is a coin flip decided
/// by machine load, so as a test it failed intermittently and as a
/// *negative control* it passed with the bug reinstated — the worst of
/// both.
///
/// What removes the race is that **a heartbeat no longer ends any
/// poll**: it is transient, so `event.stream` does not serve it, and
/// an idle window is genuinely idle whatever the filter says (see
/// `fq_runtime::events::transient`). The scoping this test still asks
/// for is the realistic case — an operator watching one quiet agent —
/// rather than the mechanism. It used to be the mechanism: `--agent`
/// compiled to the consumer subject `fq.agent.<id>.>`, so a heartbeat
/// on `fq.worker.*.heartbeat` was never delivered to the poll. That
/// narrowing was also a lie about what `--agent` means (it is the
/// envelope's agent, and an agent's events are not all on its
/// subject), and it went with the population fix; the determinism it
/// used to buy now comes from the transient exclusion instead.
///
/// Nothing ends the poll early, the daemon holds it for its full 30s,
/// and a client with a 10s deadline dies **every time**. The idle
/// window then only has to outlast that deadline, which is why it is
/// 15 seconds and not 35.
///
/// This narrowing used to be spelled `--subject fq.agent.<id>.>`,
/// which reached the daemon by the same route (the client pushed the
/// pattern's agent into the same typed filter). The flag retired with
/// D8; the determinism it bought did not.
#[test]
fn an_idle_tail_survives_its_own_long_poll() {
    let world = World::start();
    // Agent-scoped deliberately: see above. This is also the realistic
    // case — an operator watching one quiet agent.
    let mut tail = world.tail(&["--json", "--agent", AGENT]);
    // Readiness first, and it is a rendered event rather than a sleep:
    // from here the idle window measures idleness and nothing else.
    tail.wait_until_listening(&world.bus, &world.rt);

    // Longer than tarpc's default deadline, with nothing on this
    // subject to end the poll early.
    std::thread::sleep(Duration::from_secs(15));

    // The bug lands HERE, not on the render below: with the default
    // deadline the process is already gone by this point.
    tail.assert_alive("during the idle window");

    // …and it is still a working tail, not merely a live process.
    world.publish(&fixture_events());
    assert_eq!(
        tail.fixture_lines(3).len(),
        3,
        "the tail still renders what is published to it"
    );
}

/// The client binary. `CARGO_BIN_EXE_*` only names binaries of the
/// package the test lives in, and `fq` is `fq-cli`'s — but both land in
/// the same target directory, so the daemon's own path names it.
#[allow(dead_code)]
fn fq_client_binary() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_fqd")).with_file_name("fq")
}
