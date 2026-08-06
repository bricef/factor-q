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

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use fq_runtime::events::{
    CostMetadata, Event, EventPayload, LlmCallOrigin, LlmResponsePayload, StopReason, TokenUsage,
    ToolCallId, ToolResultPayload, TriggerSource, TriggeredPayload,
};
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
                round: 1,
                call_id: uuid_at(20),
                content: Some("Reading the fixture file first.".into()),
                tool_calls: Vec::new(),
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
            .env("FQ_CONFIG", dir.join("fqd.toml"))
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
        let connect = Command::new(env!("CARGO_BIN_EXE_fq"))
            .args(["connect", &addr, "--token", &token])
            .env("FQ_CONFIG", dir.join("fq.toml"))
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
            .expect("read op")
            .output
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
        let mut child = Command::new(env!("CARGO_BIN_EXE_fq"))
            .arg("events")
            .arg("tail")
            .args(args)
            .env("FQ_CONFIG", self.dir.join("fq.toml"))
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

    /// Publish, returning the last event's log sequence — the
    /// watermark a following projection read gates on.
    fn publish(&self, events: &[Event]) -> u64 {
        let mut last = 0;
        for event in events {
            last = self.rt.block_on(self.bus.publish(event)).expect("publish");
        }
        last
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
/// answer is the `seq` on every row, and
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
    for field in ["event_id", "seq", "timestamp", "agent_id", "event_type"] {
        assert!(row.get(field).is_some(), "index row lacks `{field}`: {row}");
    }

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
/// row is listed, its `seq` is handed straight back to `event.get`,
/// and the event that comes out has to be the one the row described.
/// A projection that stopped recording log positions would still list
/// perfectly well and would fail exactly here.
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
    let seq = row["seq"]
        .as_u64()
        .unwrap_or_else(|| panic!("a listed row must carry its `event.get` key: {row}"));

    // The walk itself — nothing constructed, the row's own number.
    let whole = world.invoke(
        &client,
        fq_ops::OpId::Get(fq_ops::Domain::Event),
        serde_json::json!({ "seq": seq }),
    );
    assert_eq!(
        whole["event"]["envelope"]["event_id"].as_str(),
        row["event_id"].as_str(),
        "the walk must land on the event the row described"
    );
    // …and what the walk buys is the payload the row does not carry.
    assert_eq!(
        whole["event"]["payload"]["payload"]["content"].as_str(),
        Some("Reading the fixture file first."),
        "event.get answers with the whole event: {whole}"
    );
}

/// `event.get` addresses one event by log sequence — the number a
/// command receipt's `AtomRef` carries, resolved back to the fact.
#[test]
fn get_answers_by_log_sequence() {
    let world = World::start();
    let client = world.edge_client();
    let get = fq_ops::OpId::Get(fq_ops::Domain::Event);

    let seq = world
        .rt
        .block_on(world.bus.publish(&triggered(AGENT, INV, 1, BASE_MS)))
        .expect("publish");

    let got = world.invoke(&client, get.clone(), serde_json::json!({"seq": seq}));
    assert_eq!(got["seq"].as_u64(), Some(seq));
    assert_eq!(
        got["event"]["envelope"]["event_id"].as_str(),
        Some(uuid_at(1).to_string().as_str())
    );

    // A sequence past the tip is a miss, not the next event along.
    let missing = world.rt.block_on(client.rpc.invoke(
        tarpc::context::current(),
        fq_edge::InvokeRequest {
            op: get,
            version: 1,
            input: serde_json::json!({"seq": seq + 10_000}),
            min_seq: None,
        },
    ));
    assert!(
        matches!(
            missing.expect("rpc"),
            Err(fq_edge::wire::WireError::NotFound { .. })
        ),
        "a sequence the log has never reached is NotFound"
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
/// does **not** catch it, and I wrote that version first. An
/// unfiltered tail sees the daemon's worker heartbeat, which lands
/// every 10s (`DEFAULT_INTERVAL_MS`) and ends the poll in a photo
/// finish with the 10s deadline: measured at 9.908s, 9.997s, then
/// 10.004s and death. That is a coin flip decided by machine load, so
/// as a test it fails intermittently and as a *negative control* it
/// passes with the bug reinstated — the worst of both.
///
/// Scoping the tail to one agent removes the race entirely, and the
/// scoping has to reach the *daemon* to do it: `--agent` compiles to
/// the consumer subject `fq.agent.<id>.>` (`EventSelection::compile`),
/// so a heartbeat on `fq.worker.*.heartbeat` is never delivered to the
/// poll at all. Nothing ends it early, the daemon holds it for its
/// full 30s, and a client with a 10s deadline dies **every time**. The
/// idle window then only has to outlast that deadline, which is why it
/// is 15 seconds and not 35.
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
