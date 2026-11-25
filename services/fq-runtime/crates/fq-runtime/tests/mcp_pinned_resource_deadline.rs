//! A pinned resource whose server never answers (#1040).
//!
//! Before a run's first durable write, the runner reads its agent's
//! `static_resources` pins, one after another. A shared server that
//! accepted a `resources/read` and never answered held the run in that
//! setup, with its worker permit, for as long as the daemon lived: rmcp
//! sends the request with no timeout, and nothing above it set one.
//! These tests pin what such a read does now: it is ended by `[mcp]
//! resource_read_timeout_secs`, and is then handled like every other
//! failed read of a pin. The pin is left out of the context, a warn line
//! names the server, the resource and the deadline, and the run goes on.
//!
//! **The stand-in** is the POSIX shell server of `mcp_boot_faults.rs`,
//! grown by `resources/read`. It answers every read but one of a
//! `silent://` resource, which it holds and never answers, and it writes
//! down each read it held and each `notifications/cancelled` it was sent,
//! in a file the test reads. It runs as `sh -c <script>` rather than from
//! a file the test writes, so no executable is ever open for writing
//! while another test starts a process (issue 1136).
//!
//! **A binary of its own,** as `coordination_refused_write_log.rs` is:
//! the warn line is read through a captured subscriber, and that has to
//! be the process-global one, since tracing caches a callsite's interest
//! for the whole process. Each test reads only the lines of its own
//! agent.

#![cfg(unix)]

use std::collections::{BTreeMap, HashMap};
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use fq_runtime::agent::{Agent, AgentId, AgentRegistry, StaticResourcePin};
use fq_runtime::events::{EventPayload, StopReason, TokenUsage};
use fq_runtime::llm::LlmClient;
use fq_runtime::llm::fixture::FixtureClient;
use fq_runtime::mcp::{McpClientManager, McpLimits, McpServerConfig};
use fq_runtime::worker::{InvocationStateRow, WorkerId};
use fq_runtime::{
    ChatRequest, ChatResponse, EventBus, Harness, InvocationOutcome, McpResourceReader,
    ModelPricing, PricingTable, ReducerContext, ReducerRunner, RunnerConfig, ToolRegistry,
    TriggerDispatcher, WorkerStore, shared_registry,
};
use futures::StreamExt;
use serde_json::json;
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer as _;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Context, SubscriberExt};
use uuid::Uuid;

/// A stdio MCP server in POSIX shell. It answers `initialize`, an empty
/// `tools/list`, and every `resources/read` with a text naming the
/// resource, except a read of a `silent://` resource: that one is held
/// for good, and its request id is written to the file named by `$1`.
/// Every `notifications/cancelled` it is sent is written there too,
/// whole.
///
/// The request id and the uri are read back out of the line with `sed`,
/// as `mcp_boot_faults.rs` reads the id: each occurs once in the requests
/// this server answers.
const STUB_SERVER: &str = r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{},"resources":{}},"serverInfo":{"name":"stub","version":"0.1.0"}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[]}}\n' "$id"
      ;;
    *'"method":"resources/read"'*'"uri":"silent://'*)
      printf 'held %s\n' "$id" >> "$1"
      ;;
    *'"method":"resources/read"'*)
      uri=$(printf '%s' "$line" | sed -n 's/.*"uri":"\([^"]*\)".*/\1/p')
      printf '{"jsonrpc":"2.0","id":%s,"result":{"contents":[{"uri":"%s","mimeType":"text/plain","text":"the text of %s"}]}}\n' "$id" "$uri" "$uri"
      ;;
    *'"method":"notifications/cancelled"'*)
      printf '%s\n' "$line" >> "$1"
      ;;
  esac
done
"#;

/// What every agent here pins, in order: a resource its server never
/// answers, a second resource on that same server, and a resource on a
/// server that answers everything. The second pin is the next request
/// the silent server gets after the one it held.
const PINS: [&str; 3] = [
    "mcp://slow/silent://held",
    "mcp://slow/doc://after",
    "mcp://docs/doc://readme",
];

/// The start of the line the runner logs for a pin it could not read.
const OMITTED: &str = "failed to read static_resources pin";

/// Long enough for every run here to finish on the configured deadline,
/// short of the shipped 30-second default, so a run that waited on the
/// default instead fails rather than passes slowly.
const RUN_BOUND: Duration = Duration::from_secs(20);

/// The `[mcp]` table this file runs under: the shipped defaults, which
/// put no deadline on a read.
fn limits() -> McpLimits {
    McpLimits::default()
}

/// One of the two stand-ins, writing what it held and what it was told
/// to cancel into `record`. The record path is an argument, so the two
/// are two processes rather than one deduplicated server.
fn stub(name: &str, record: &Path) -> McpServerConfig {
    McpServerConfig {
        name: name.to_string(),
        command: "sh".to_string(),
        args: vec![
            "-c".to_string(),
            STUB_SERVER.to_string(),
            name.to_string(),
            record.display().to_string(),
        ],
        env: Vec::new(),
        url: None,
    }
}

/// The two shared servers, started as the daemon starts them, and the
/// file the silent one writes its record into.
async fn start_servers(dir: &Path) -> (McpClientManager, PathBuf) {
    let slow_record = dir.join("slow.record");
    let mut manager =
        McpClientManager::with_server_root(dir.join("mcp-root")).with_limits(limits());
    let outcomes = manager
        .start_shared_servers(vec![
            stub("slow", &slow_record),
            stub("docs", &dir.join("docs.record")),
        ])
        .await;
    for outcome in &outcomes {
        if let Err(err) = &outcome.outcome {
            panic!("the stand-in {} must speak MCP: {err}", outcome.server);
        }
    }
    (manager, slow_record)
}

fn pricing() -> Arc<PricingTable> {
    let mut entries = HashMap::new();
    entries.insert(
        "claude-haiku".to_string(),
        ModelPricing {
            input_per_million: 1.0,
            output_per_million: 5.0,
            cache_read_per_million: None,
            cache_write_per_million: None,
            cache_write_1h_per_million: None,
        },
    );
    Arc::new(PricingTable::from_map(entries))
}

/// A runner that reads pins through `reader`, as the daemon's does.
fn runner(bus: EventBus, store: Arc<WorkerStore>, reader: McpResourceReader) -> ReducerRunner {
    ReducerRunner::new(
        Arc::new(
            ReducerContext::builder()
                .tools(Arc::new(ToolRegistry::with_builtins()))
                .resources(reader)
                .build(),
        ),
        Arc::new(
            RunnerConfig::builder()
                .bus(bus)
                .pricing(pricing())
                .store(store)
                .worker_id(WorkerId::new(Uuid::now_v7().to_string()).expect("worker id"))
                .build(),
        ),
        Harness::new(),
    )
}

/// A model that ends the run on its first turn, once per run.
fn model(runs: usize) -> Arc<FixtureClient> {
    let llm = FixtureClient::new();
    for _ in 0..runs {
        llm.push_response(ChatResponse {
            parts: fq_runtime::events::assistant_parts(
                None,
                vec![fq_runtime::events::MessageToolCall {
                    tool_call_id: fq_runtime::events::ToolCallId::new("report-outcome").unwrap(),
                    tool_name: fq_runtime::tools::REPORT_OUTCOME_CANONICAL_NAME.to_string(),
                    parameters: json!({"status": "success", "summary": "done."}),
                }],
            ),
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cache_write_5m_tokens: None,
                cache_write_1h_tokens: None,
                reasoning_tokens: None,
            },
            reported_cost_usd: None,
        });
    }
    Arc::new(llm)
}

/// A run's first model request carries the two pins that answered and
/// not the one that did not.
fn assert_pins_in_context(request: &ChatRequest) {
    let context: Vec<String> = request.messages.iter().filter_map(|m| m.text()).collect();
    let context = context.join("\n");
    for answered in ["the text of doc://after", "the text of doc://readme"] {
        assert!(
            context.contains(answered),
            "a pin that answered must be in the opening context ({answered:?}): {context}"
        );
    }
    assert!(
        !context.contains("silent://held"),
        "the pin that never answered must be left out: {context}"
    );
}

/// The silent server held `reads` reads of the silent pin, and was told
/// to cancel each one: the request id it held is the one a
/// `notifications/cancelled` names. Polled, because the cancellation
/// travels on its own and may land just after the run that sent it ends.
async fn assert_each_held_read_was_cancelled(record: &Path, reads: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let text = std::fs::read_to_string(record).unwrap_or_default();
        let held: Vec<&str> = text
            .lines()
            .filter_map(|line| line.strip_prefix("held "))
            .collect();
        let cancelled = held
            .iter()
            .filter(|id| {
                text.lines().any(|line| {
                    line.contains("notifications/cancelled")
                        && line.contains(&format!("\"requestId\":{id}"))
                })
            })
            .count();
        if held.len() == reads && cancelled == reads {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the silent server held {} read(s) and was told to cancel {cancelled}; \
             expected {reads} of each. Its record:\n{text}",
            held.len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The warn lines this agent's runs wrote for the silent pin: one per
/// run, at warn, naming the server, the resource and the deadline.
fn assert_omitted_lines(log: &Log, agent: &str, runs: usize) {
    let lines = log.omitted(agent);
    assert_eq!(
        lines.len(),
        runs,
        "one line per run for the pin that never answered, and none for the \
         pins that did: {lines:#?}"
    );
    for line in &lines {
        assert_eq!(line.level, tracing::Level::WARN, "{line:#?}");
        assert_eq!(line.field("server"), Some("slow"), "{line:#?}");
        assert_eq!(line.field("uri"), Some("silent://held"), "{line:#?}");
        let error = line.field("error").unwrap_or_default();
        assert!(
            error.contains("within the 1s resource read deadline")
                && error.contains("[mcp] resource_read_timeout_secs"),
            "the line names the deadline and the key that sets it: {error}"
        );
    }
}

/// **The fresh path, through the dispatcher, on one worker permit.** Two
/// triggers for an agent whose first pin never answers: the first run's
/// setup ends on the deadline with that pin left out, the run completes
/// and gives its permit back, and the second trigger then runs on it the
/// same way. Before the deadline the first run held the only permit in
/// setup for good and the second never started.
///
/// Both runs read the silent server again after the read it held — the
/// second pin is on the same server — and both get an answer, so the
/// connection outlives a read that timed out, for the run that timed out
/// and for the next one.
#[tokio::test]
async fn a_pin_that_never_answers_is_left_out_and_the_permit_comes_back() {
    let log = Log::installed();
    let broker = fq_test_support::NatsServer::start();
    let bus = EventBus::connect(broker.url()).await.expect("connect NATS");
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut manager, slow_record) = start_servers(dir.path()).await;

    let agent = format!("pinned-silent-{}", Uuid::now_v7().simple());
    let definition = dir.path().join(format!("{agent}.md"));
    std::fs::write(
        &definition,
        format!(
            "---\nname: {agent}\nmodel: claude-haiku\nbudget: 1.0\nstatic_resources:\n{}---\n\n\
             You are a test agent.\n",
            PINS.iter()
                .map(|pin| format!("  - \"{pin}\"\n"))
                .collect::<String>()
        ),
    )
    .expect("write the agent");
    let mut registry = AgentRegistry::new();
    registry.load_file(&definition);
    assert!(registry.errors().is_empty(), "{:?}", registry.errors());

    let store = Arc::new(
        WorkerStore::open(&dir.path().join("worker.db"))
            .await
            .expect("worker store"),
    );
    let llm = model(2);
    let dispatcher = TriggerDispatcher::new(
        bus.clone(),
        shared_registry(registry),
        Arc::new(runner(bus.clone(), store, manager.resource_reader())),
        Arc::clone(&llm) as Arc<dyn LlmClient>,
        1,
    );
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let dispatching = tokio::spawn(dispatcher.run(stopped));
    let mut events = bus
        .subscribe(format!("fq.agent.{agent}.>"))
        .await
        .expect("subscribe");
    // Let the dispatcher's durable consumer attach before publishing.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let id = AgentId::new(&agent).expect("agent id");
    for n in 0..2 {
        bus.publish_trigger(&id, &json!({"input": n}))
            .await
            .expect("publish trigger");
    }
    let deadline = tokio::time::Instant::now() + RUN_BOUND;
    let mut completed = 0;
    while completed < 2 {
        match tokio::time::timeout_at(deadline, events.next()).await {
            Ok(Some(Ok(event))) => {
                if matches!(event.payload, EventPayload::Completed(_)) {
                    completed += 1;
                }
            }
            Ok(Some(Err(err))) => panic!("an event that does not read: {err}"),
            Ok(None) => panic!("the event subscription ended"),
            Err(_) => panic!(
                "{completed} of 2 runs completed within {RUN_BOUND:?}: a run whose pin never \
                 answers must not hold the only worker permit in its setup"
            ),
        }
    }

    let requests = llm.requests();
    assert_eq!(requests.len(), 2, "one model turn per run");
    for request in &requests {
        assert_pins_in_context(request);
    }
    assert_omitted_lines(log, &agent, 2);
    assert_each_held_read_was_cancelled(&slow_record, 2).await;

    let _ = stop.send(());
    let _ = tokio::time::timeout(Duration::from_secs(10), dispatching).await;
    manager.shutdown().await;
}

/// **The resume path is bounded the same way.** A resumed run re-reads
/// its pins to rebuild the same opening context, before it goes on. A
/// run seeded as stopped at its first step resumes, the silent pin is
/// left out on the deadline as on a fresh start, and the run completes.
#[tokio::test]
async fn a_resumed_run_leaves_the_silent_pin_out_on_the_same_deadline() {
    let log = Log::installed();
    let broker = fq_test_support::NatsServer::start();
    let bus = EventBus::connect(broker.url()).await.expect("connect NATS");
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut manager, slow_record) = start_servers(dir.path()).await;

    let agent_id = format!("resumed-silent-{}", Uuid::now_v7().simple());
    let agent = Agent::builder()
        .id(&agent_id)
        .model("claude-haiku")
        .system_prompt("You are a test agent.")
        .budget(1.0)
        .static_resources(
            PINS.iter()
                .map(|pin| StaticResourcePin::parse(pin).expect("a pin"))
                .collect(),
        )
        .build()
        .expect("build agent");
    let store = Arc::new(
        WorkerStore::open(&dir.path().join("worker.db"))
            .await
            .expect("worker store"),
    );
    // Stopped after its first durable write and before its first step
    // ran: nothing to replay, so the resume's own setup is all that
    // stands between it and its first model turn.
    let invocation_id = Uuid::now_v7();
    store
        .upsert_invocation_state(&InvocationStateRow {
            invocation_id: invocation_id.to_string(),
            agent_id: agent_id.clone(),
            schema_version: 1,
            phase: "awaiting_model".to_string(),
            state_blob: Vec::new(),
            step_index: 0,
            started_at: 1_000,
            updated_at: 1_000,
            terminal_at: None,
            workspace_ref: None,
            archive_status: None,
            archive_published_at: None,
            trigger_source: Some("manual".to_string()),
            trigger_subject: None,
            trigger_payload: Some("\"hello\"".to_string()),
            terminal_outcome: None,
            terminal_published_at: None,
        })
        .await
        .expect("seed the run");

    let llm = model(1);
    let runner = runner(bus, store, manager.resource_reader());
    let outcome = tokio::time::timeout(RUN_BOUND, runner.resume(&agent, &*llm, invocation_id))
        .await
        .unwrap_or_else(|_| {
            panic!(
                "the resume did not finish within {RUN_BOUND:?}: its setup must not wait on a \
                 pin that never answers"
            )
        });
    assert!(
        matches!(outcome, Ok(InvocationOutcome::Completed { .. })),
        "the resumed run goes on and completes: {outcome:?}"
    );

    let requests = llm.requests();
    assert_eq!(requests.len(), 1, "one model turn");
    assert_pins_in_context(&requests[0]);
    assert_omitted_lines(log, &agent_id, 1);
    assert_each_held_read_was_cancelled(&slow_record, 1).await;
    manager.shutdown().await;
}

/// One event logged at `info` or above: its level, message and fields.
#[derive(Debug, Clone)]
struct Line {
    level: tracing::Level,
    message: String,
    fields: BTreeMap<&'static str, String>,
}

impl Visit for Line {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        self.record(field, format!("{value:?}"));
    }
}

impl Line {
    fn record(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = value;
        } else {
            self.fields.insert(field.name(), value);
        }
    }

    fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }
}

/// Every line logged at `info` or above in this process.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<Line>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Log {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut line = Line {
            level: *event.metadata().level(),
            message: String::new(),
            fields: BTreeMap::new(),
        };
        event.record(&mut line);
        self.0.lock().unwrap().push(line);
    }
}

impl Log {
    /// The process's one subscriber, installed on the first call, before
    /// anything here can log.
    fn installed() -> &'static Log {
        static LOG: OnceLock<Log> = OnceLock::new();
        LOG.get_or_init(|| {
            let log = Log::default();
            tracing::subscriber::set_global_default(
                tracing_subscriber::registry().with(log.clone().with_filter(LevelFilter::INFO)),
            )
            .expect("install the process's one subscriber");
            log
        })
    }

    /// The lines for a pin left out of `agent`'s context.
    fn omitted(&self, agent: &str) -> Vec<Line> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|line| {
                line.message.starts_with(OMITTED) && line.field("agent_id") == Some(agent)
            })
            .cloned()
            .collect()
    }
}
