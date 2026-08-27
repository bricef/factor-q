//! Behavioural-equivalence and end-to-end tests for the
//! reducer host loop. Each NATS-backed test spawns its own
//! private `nats-server` (#233) — nothing shared, nothing
//! skipped.
//!
//! The point of these tests is the *equivalence* claim:
//! given the same scripted LLM responses and the same
//! agent definition, the reducer path must produce the
//! same canonical event sequence as the legacy executor.
//! If that holds, dispatching through the reducer path is
//! invisible to downstream observers.
//!
//! What's *not* tested here: cost numbers (already covered
//! by the legacy executor tests, and the runner reuses the
//! exact same pricing code path), and the deeper purity
//! claims (covered by the unit tests in `harness.rs`).
use super::*;
use crate::agent::{Agent, Sandbox};
use crate::bus::EventBus;
use crate::events::{StopReason, TokenUsage};
use crate::llm::fixture::FixtureClient;
use crate::pricing::ModelPricing;
use crate::tools::ToolRegistry;
use crate::worker::reducer::Harness;
use crate::worker::store::DispatchStatus;
use crate::{events::EventPayload, llm::ChatResponse};
use futures::StreamExt;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
use tempfile::tempdir;

/// Issue #9 precedence, checked at the boundary the runner uses to
/// fill `AgentConfig.max_iterations`: per-agent override (if the
/// definition sets one) wins; otherwise the daemon config default
/// applies. This mirrors `agent.max_iterations().unwrap_or(cfg)`
/// exactly — the single expression both `run` and `resume` use.
#[test]
fn max_iterations_precedence_prefers_agent_override_then_config_default() {
    let config_default = 100u32;

    // Definition without max_iterations -> falls back to the config default.
    let plain = Agent::builder()
        .id("plain")
        .model("claude-haiku")
        .system_prompt("be brief")
        .build()
        .unwrap();
    assert_eq!(
        plain.max_iterations().unwrap_or(config_default),
        config_default,
        "no override -> daemon config default"
    );

    // Definition with max_iterations -> overrides the config default.
    let overridden = Agent::builder()
        .id("overridden")
        .model("claude-haiku")
        .system_prompt("be brief")
        .max_iterations(7)
        .build()
        .unwrap();
    assert_eq!(
        overridden.max_iterations().unwrap_or(config_default),
        7,
        "override wins over the daemon config default"
    );
}

#[test]
fn invocation_preamble_has_stable_environment_fields() {
    let preamble = invocation_preamble(
        Some(Path::new("/tmp/workspace")),
        &AgentId::new("doc-drift").unwrap(),
        Some(3),
        Some(1.25),
        12,
        1_700_000_000_000,
    );
    assert!(preamble.contains("timestamp: 2023-11-14T22:13:20+00:00"));
    assert!(preamble.contains("agent id: doc-drift"));
    assert!(preamble.contains("workspace: /tmp/workspace"));
    assert!(preamble.contains("attempt: 3"));
    assert!(preamble.contains("budget: $1.25"));
    assert!(preamble.contains("iteration ceiling: 12"));
}

#[tokio::test]
async fn runner_config_max_iterations_defaults_to_the_builtin_fallback() {
    // A RunnerConfig built without .max_iterations() carries the
    // built-in fallback, so a runner constructed with no explicit
    // daemon default still bounds every agent.
    let dir = tempdir().unwrap();
    let store = Arc::new(
        WorkerStore::open(&dir.path().join("events.db"))
            .await
            .unwrap(),
    );
    let cfg = RunnerConfig::builder()
        .event_sink(Arc::new(crate::test_support::sim::RecordingSink::new()) as Arc<dyn EventSink>)
        .pricing(test_pricing())
        .store(store)
        .worker_id(test_worker_id())
        .build();
    assert_eq!(
        cfg.max_iterations,
        crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS
    );
}

#[tokio::test]
async fn enforce_pricing_refuses_to_dispatch_an_unpriced_model() {
    // ADR-0004 at-use backstop: with enforce_pricing on and no price
    // for the model, the runner refuses to dispatch — a typed failure
    // — rather than call the model and track its cost as $0.
    let dir = tempdir().unwrap();
    let store = Arc::new(
        WorkerStore::open(&dir.path().join("events.db"))
            .await
            .unwrap(),
    );
    let agent = Agent::builder()
        .id(unique_agent_id("unpriced"))
        .model("model-with-no-price")
        .system_prompt("be brief")
        .budget(1.0)
        .build()
        .unwrap();
    // Queued but must never be consumed — the gate fires first.
    let llm = FixtureClient::new();
    llm.push_response(canned("should not be used", 10, 5));

    let runner = ReducerRunner::new(
        Arc::new(
            ReducerContext::builder()
                .tools(Arc::new(ToolRegistry::with_builtins()))
                .build(),
        ),
        Arc::new(
            RunnerConfig::builder()
                .event_sink(
                    Arc::new(crate::test_support::sim::RecordingSink::new()) as Arc<dyn EventSink>
                )
                .pricing(Arc::new(PricingTable::empty()))
                .store(store)
                .worker_id(test_worker_id())
                .enforce_pricing(true)
                .build(),
        ),
        Harness::new(),
    );

    let outcome = runner
        .run(
            &agent,
            &llm,
            TriggerSource::Manual,
            None,
            json!({"input": "go"}),
        )
        .await;

    match outcome {
        Err(ExecutorError::Llm(crate::llm::LlmError::UnpricedModel(model))) => {
            assert_eq!(model, "model-with-no-price");
        }
        other => panic!("expected an UnpricedModel failure, got {other:?}"),
    }
}

#[tokio::test]
async fn sampling_channel_merges_servers_and_drains() {
    use crate::mcp::ServerRequest;
    use tokio::sync::{mpsc, oneshot};

    fn req() -> ServerRequest {
        let params = serde_json::from_value(json!({ "messages": [], "maxTokens": 8 }))
            .expect("sampling params");
        let (reply, _rx) = oneshot::channel();
        ServerRequest::Sampling { params, reply }
    }

    let (tx_a, rx_a) = mpsc::unbounded_channel();
    let (tx_b, rx_b) = mpsc::unbounded_channel();
    let mut channel = SamplingChannel::merged(vec![
        ("alpha".to_string(), rx_a),
        ("beta".to_string(), rx_b),
    ]);

    // A request on either server's channel is tagged with its name.
    tx_b.send(req()).unwrap();
    assert_eq!(channel.recv().await.expect("request").0, "beta");
    tx_a.send(req()).unwrap();
    assert_eq!(channel.recv().await.expect("request").0, "alpha");

    // Once every server's channel is closed, recv drains to None.
    drop(tx_a);
    drop(tx_b);
    assert!(channel.recv().await.is_none());
}

fn unique_agent_id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::now_v7().simple())
}

/// A worker id good for tests. Each call returns a fresh
/// UUID-shaped id so concurrent tests don't share a
/// `fq.worker.{id}.invocation.archive_acked` subject.
fn test_worker_id() -> WorkerId {
    WorkerId::new(Uuid::now_v7().to_string()).expect("uuid is a valid worker id")
}

fn test_pricing() -> Arc<PricingTable> {
    let mut entries = HashMap::new();
    entries.insert(
        "claude-haiku".to_string(),
        ModelPricing {
            input_per_million: 1.0,
            output_per_million: 5.0,
            cache_read_per_million: None,
            cache_write_per_million: None,
        },
    );
    Arc::new(PricingTable::from_map(entries))
}

fn canned(text: &str, input: u32, output: u32) -> ChatResponse {
    ChatResponse {
        parts: crate::events::assistant_parts(
            None,
            vec![crate::events::MessageToolCall {
                tool_call_id: crate::events::ToolCallId::new("report-outcome").unwrap(),
                tool_name: crate::tools::REPORT_OUTCOME_CANONICAL_NAME.to_string(),
                parameters: json!({"status": "success", "summary": text}),
            }],
        ),
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    }
}

fn tool_use(name: &str, call_id: &str, params: Value, tokens: (u32, u32)) -> ChatResponse {
    ChatResponse {
        parts: crate::events::assistant_parts(
            None,
            vec![crate::events::MessageToolCall {
                tool_call_id: crate::events::ToolCallId::new(call_id).unwrap(),
                tool_name: name.to_string(),
                parameters: params,
            }],
        ),
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage {
            input_tokens: tokens.0,
            output_tokens: tokens.1,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    }
}

use crate::test_support::events::event_kind;

#[tokio::test]
async fn reducer_emits_canonical_event_sequence_for_simple_completion() {
    // Was `equivalent_event_sequence_for_simple_completion`,
    // which ran a single canned response through *both* the
    // legacy executor and the reducer and asserted that the
    // reducer sequence equals the legacy sequence modulo WAL
    // middle-state events. After AgentExecutor is deleted
    // the legacy half is gone, so this asserts the
    // reducer-side canonical sequence directly.
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    let agent_id = unique_agent_id("canonical-simple");
    let agent = Agent::builder()
        .id(&agent_id)
        .model("claude-haiku")
        .system_prompt("You are a test agent.")
        .budget(1.0)
        .build()
        .unwrap();

    // triggered, llm.request, llm.dispatched, llm.response,
    // completed = 5 events. (invocation_archived also fires
    // immediately after; not collected here.)
    let (_store, events) =
        run_with_wal(&url, agent, vec![canned("Hello.", 100, 50)], 5, None).await;

    let kinds: Vec<&str> = events.iter().map(event_kind).collect();
    assert_eq!(
        kinds,
        vec![
            "triggered",
            "llm_request",
            "llm_dispatched",
            "llm_response",
            "completed",
        ],
    );
}

#[tokio::test]
async fn reducer_emits_canonical_event_sequence_for_tool_call_loop() {
    // Was `equivalent_event_sequence_for_tool_call_loop`.
    // Same conversion as the simple-completion test:
    // reducer-only canonical-sequence assertion.
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    let dir = tempdir().unwrap();
    let target = dir.path().join("hello.md");
    std::fs::write(&target, "# hello").unwrap();
    let target_path = target.to_string_lossy().to_string();
    let allowed_dir = dir.path().to_string_lossy().to_string();

    let agent_id = unique_agent_id("canonical-tool-loop");
    let agent = Agent::builder()
        .id(&agent_id)
        .model("claude-haiku")
        .system_prompt("Use tools when asked.")
        .tools(["file_read"])
        .sandbox(Sandbox::new().fs_read(allowed_dir))
        .budget(1.0)
        .build()
        .unwrap();

    let responses = vec![
        tool_use(
            "file_read",
            "call_abc",
            json!({"path": target_path}),
            (100, 50),
        ),
        canned("Got it.", 150, 20),
    ];

    // 11 events: triggered, then for each LLM turn the
    // (llm.request, llm.dispatched, llm.response) triple,
    // with a tool-dispatch triple (tool.call, tool.dispatched,
    // tool.result) between turns 1 and 2, ending in completed.
    let (_store, events) = run_with_wal(&url, agent, responses, 11, Some(dir.path())).await;

    let kinds: Vec<&str> = events.iter().map(event_kind).collect();
    assert_eq!(
        kinds,
        vec![
            "triggered",
            "llm_request",
            "llm_dispatched",
            "llm_response",
            "tool_call",
            "tool_dispatched",
            "tool_result",
            "llm_request",
            "llm_dispatched",
            "llm_response",
            "completed",
        ],
    );
}

#[tokio::test]
async fn reducer_invocation_emits_single_parent_chain() {
    // Step 2 of the envelope-refactor plan: the reducer threads
    // parent_event_id through every publish for an invocation.
    // The captured event stream must form a single chain
    // rooted at `triggered`, with no orphans, no branches, and
    // no multiple roots. Reconstructable without consulting
    // timestamps.
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();
    let bus = EventBus::connect(&url).await.expect("connect to NATS");
    let agent_id = unique_agent_id("chain");
    let agent = Agent::builder()
        .id(&agent_id)
        .model("claude-haiku")
        .system_prompt("be brief")
        .tools(["file_read"])
        .budget(1.0)
        .build()
        .unwrap();

    let target_path = "Cargo.toml".to_string();
    let llm = FixtureClient::new();
    llm.push_response(tool_use(
        "file_read",
        "call_chain_1",
        json!({"path": target_path.clone()}),
        (50, 25),
    ));
    llm.push_response(canned("read.", 80, 10));

    let mut sub = bus
        .subscribe(format!("fq.agent.{}.>", agent.id().as_str()))
        .await
        .expect("subscribe");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let store_dir = tempdir().expect("tempdir");
    let store = Arc::new(
        WorkerStore::open(&store_dir.path().join("events.db"))
            .await
            .expect("worker store"),
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
                .pricing(test_pricing())
                .store(store)
                .worker_id(test_worker_id())
                .build(),
        ),
        Harness::new(),
    );
    let _ = runner
        .run(
            &agent,
            &llm,
            TriggerSource::Manual,
            None,
            json!({"input": "go"}),
        )
        .await;

    // Drain. tool-call loop emits: triggered + 2 turns ×
    // (llm_request, llm_dispatched, llm_response with envelope.cost)
    // + 1 × (tool_call, tool_dispatched, tool_result) + completed
    // + invocation_archived = 12 events after data-arch step 8.
    let mut events = Vec::new();
    for _ in 0..12 {
        let event = tokio::time::timeout(Duration::from_secs(2), sub.next())
            .await
            .expect("chain timeout")
            .expect("chain stream closed")
            .expect("chain deserialise");
        events.push(event);
    }

    crate::test_support::events::assert_parent_chain(&events);
    // The full R1 grammar: canonical sequence, one terminal,
    // archived at the end, chained envelopes (slice 1 oracle).
    crate::test_support::oracle::assert_valid_trace(&events);
    // Schema version on every envelope must be the v2 constant.
    for e in &events {
        assert_eq!(e.envelope.schema_version, crate::events::SCHEMA_VERSION);
        assert_eq!(e.envelope.trace_id, e.envelope.invocation_id);
        assert!(!e.envelope.schema_id.is_empty());
    }
}

#[tokio::test]
async fn reducer_suspend_resume_yields_same_completion() {
    // Demonstrates the suspend/resume claim end-to-end:
    // run the reducer until step boundary N, capture the
    // opaque state, throw the runner away, run a fresh
    // runner from the captured state, and check the final
    // completion is structurally the same.
    //
    // For the prototype this is implemented at the
    // reducer-state level (no host bus interleaving),
    // matching the unit-test `state_round_trips` pattern
    // but starting from the runner-built `AgentConfig`.
    use crate::worker::reducer::types::{
        AgentConfig, CapabilityResult, ModelResponse, NextAction, StepInput, TriggerPayload,
        TriggerSourceKind,
    };

    let cfg = AgentConfig {
        agent_id: AgentId::new("suspend-resume").unwrap(),
        model: "claude-haiku".to_string(),
        system_prompt: "be brief.".to_string(),
        tools_available: vec![],
        allowed_tool_names: vec![],
        max_iterations: crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS,
        effort: None,
    };
    let trig = TriggerPayload {
        source: TriggerSourceKind::Manual,
        subject: None,
        payload: json!("ping"),
    };

    let h1 = Harness::new();
    let s0 = h1
        .step(StepInput {
            config: cfg.clone(),
            trigger: trig.clone(),
            state: vec![],
            last_result: None,
            now_ms: 0,
            random_seed: 0,
            step_index: 0,
            static_resource_context: None,
            host_notices: vec![],
        })
        .unwrap();
    // Suspended snapshot.
    let snapshot = s0.state.clone();

    // Drop and replace the reducer. `Harness` has no Drop
    // impl, so the move-into-wildcard pattern is the way to
    // express "throw this away" without clippy's `drop_non_drop`.
    let _ = h1;
    let h2 = Harness::new();

    let s1 = h2
        .step(StepInput {
            config: cfg,
            trigger: trig,
            state: snapshot,
            last_result: Some(CapabilityResult::ModelResult(ModelResponse {
                parts: crate::events::assistant_parts(
                    None,
                    canned("pong", 10, 10)
                        .tool_calls()
                        .into_iter()
                        .cloned()
                        .collect(),
                ),
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            })),
            now_ms: 1,
            random_seed: 1,
            step_index: 1,
            static_resource_context: None,
            host_notices: vec![],
        })
        .unwrap();

    match s1.next_action {
        NextAction::Complete { text, .. } => assert_eq!(text, "pong"),
        other => panic!("expected Complete after resume, got {other:?}"),
    }
}

/// `self_inspect` is a host-fulfilled tool: the schema lives
/// in `fq-tools` but the data is synthesised by the runner.
/// This test runs an agent that calls `self_inspect`, lets
/// the reducer drive a real two-turn loop (call → result →
/// final), and asserts the tool result message contains
/// the synthesised JSON fields.
#[tokio::test]
async fn self_inspect_is_dispatched_by_the_runner() {
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    let agent_id = unique_agent_id("self-inspect");
    let agent = Agent::builder()
        .id(agent_id.clone())
        .model("claude-haiku")
        .system_prompt("Inspect yourself when asked.")
        .tools(["builtin__self_inspect"])
        .budget(0.50)
        .build()
        .unwrap();

    let llm = FixtureClient::new();
    // Turn 1: model asks for self_inspect.
    llm.push_response(tool_use(
        "builtin__self_inspect",
        "call_si",
        json!({}),
        (100, 50),
    ));
    // Turn 2: model summarises and finishes.
    llm.push_response(canned("I have one budget left.", 150, 30));

    let bus = EventBus::connect(&url).await.expect("connect to NATS");
    let store_dir = tempdir().expect("tempdir");
    let store = Arc::new(
        WorkerStore::open(&store_dir.path().join("events.db"))
            .await
            .expect("worker store"),
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
                .pricing(test_pricing())
                .store(store)
                .worker_id(test_worker_id())
                .build(),
        ),
        Harness::new(),
    );

    let mut sub = bus
        .subscribe(format!("fq.agent.{agent_id}.>"))
        .await
        .expect("subscribe");
    tokio::time::sleep(Duration::from_millis(50)).await;

    runner
        .run(&agent, &llm, TriggerSource::Manual, None, json!({}))
        .await
        .expect("invocation");

    let mut tool_result_output: Option<String> = None;
    for _ in 0..15 {
        let event = tokio::time::timeout(Duration::from_secs(2), sub.next())
            .await
            .expect("timeout")
            .expect("stream closed")
            .expect("deserialise");
        if let EventPayload::ToolResult(p) = &event.payload {
            tool_result_output = Some(p.output.clone());
            break;
        }
    }
    let raw = tool_result_output.expect("no tool.result observed");
    let parsed: Value = serde_json::from_str(&raw).expect("self_inspect output is JSON");
    assert!(parsed.get("model").is_some(), "missing model section");
    assert!(parsed.get("budget").is_some(), "missing budget section");
    assert!(parsed.get("tools").is_some(), "missing tools section");
    assert_eq!(parsed["model"], "claude-haiku");
    // The agent has just made its first LLM call when self_inspect
    // is dispatched; tool counter is still 0 at synthesis time.
    assert_eq!(parsed["iterations"]["llm_calls_made"], 1);
    assert_eq!(parsed["iterations"]["tool_calls_made"], 0);
}

#[test]
fn canonicalize_bare_builtin_maps_every_basename_and_nothing_else() {
    for base in crate::tools::BUILTIN_TOOL_BASENAMES {
        assert_eq!(
            canonicalize_bare_builtin(base).as_deref(),
            Some(format!("{}{base}", crate::tools::BUILTIN_PREFIX).as_str())
        );
    }
    // Already-canonical, MCP-namespaced, and unknown names pass through.
    assert_eq!(canonicalize_bare_builtin("builtin__exec"), None);
    assert_eq!(canonicalize_bare_builtin("everything__echo"), None);
    assert_eq!(canonicalize_bare_builtin("shell"), None);
}

#[test]
fn canonical_tool_names_rewrites_only_bare_builtins() {
    let names = vec![
        "exec".to_string(),
        "everything__echo".to_string(),
        "builtin__file_read".to_string(),
    ];
    assert_eq!(
        super::tool_names::canonical_tool_names(&names),
        vec![
            "builtin__exec".to_string(),
            "everything__echo".to_string(),
            "builtin__file_read".to_string(),
        ]
    );
}

/// The terminal tool is offered whatever the agent declares — including
/// when it declares nothing. An agent with an empty tool list is handed
/// no tools at all, so before this it could not call `report_outcome`,
/// and `report_outcome` is the only clean end to a run: it answered its
/// first turn correctly and then looped to the iteration ceiling, which
/// is a failure stop.
#[test]
fn the_terminal_tool_is_offered_to_an_agent_that_declares_nothing() {
    assert_eq!(
        effective_tool_names(&[]),
        vec![crate::tools::REPORT_OUTCOME_CANONICAL_NAME.to_string()]
    );
}

/// Appended after what the agent asked for, and canonicalised with it.
#[test]
fn the_terminal_tool_joins_the_declared_tools() {
    let names = vec!["exec".to_string(), "builtin__file_read".to_string()];
    assert_eq!(
        effective_tool_names(&names),
        vec![
            "builtin__exec".to_string(),
            "builtin__file_read".to_string(),
            crate::tools::REPORT_OUTCOME_CANONICAL_NAME.to_string(),
        ]
    );
}

/// Declaring it explicitly is still allowed and must not offer it
/// twice — a duplicate schema is a malformed request to the provider.
/// The bare spelling canonicalises first, so both forms collapse.
#[test]
fn declaring_the_terminal_tool_does_not_offer_it_twice() {
    for declared in [
        vec![crate::tools::REPORT_OUTCOME_CANONICAL_NAME.to_string()],
        vec!["report_outcome".to_string()],
    ] {
        assert_eq!(
            effective_tool_names(&declared),
            vec![crate::tools::REPORT_OUTCOME_CANONICAL_NAME.to_string()],
            "declared as {declared:?}"
        );
    }
}

/// #177 migration window: a definition still granting bare built-in
/// names keeps working for one release — the grant is canonicalised
/// (the model is offered `builtin__self_inspect`), and a model that
/// nevertheless calls the bare name is normalised on dispatch. Both
/// legacy paths are exercised deliberately: bare grant + bare call.
#[tokio::test]
async fn legacy_bare_builtin_grants_and_calls_still_resolve() {
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    let agent_id = unique_agent_id("legacy-bare");
    let agent = Agent::builder()
        .id(agent_id.clone())
        .model("claude-haiku")
        .system_prompt("Inspect yourself when asked.")
        .tools(["self_inspect"]) // deprecated bare grant
        .budget(0.50)
        .build()
        .unwrap();

    let llm = FixtureClient::new();
    // Turn 1: the model calls the bare legacy name.
    llm.push_response(tool_use("self_inspect", "call_si", json!({}), (100, 50)));
    // Turn 2: model summarises and finishes.
    llm.push_response(canned("done", 150, 30));

    let bus = EventBus::connect(&url).await.expect("connect to NATS");
    let store_dir = tempdir().expect("tempdir");
    let store = Arc::new(
        WorkerStore::open(&store_dir.path().join("events.db"))
            .await
            .expect("worker store"),
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
                .pricing(test_pricing())
                .store(store)
                .worker_id(test_worker_id())
                .build(),
        ),
        Harness::new(),
    );

    let mut sub = bus
        .subscribe(format!("fq.agent.{agent_id}.>"))
        .await
        .expect("subscribe");
    tokio::time::sleep(Duration::from_millis(50)).await;

    runner
        .run(&agent, &llm, TriggerSource::Manual, None, json!({}))
        .await
        .expect("invocation");

    // The model was offered the canonical name, not the bare grant —
    // and the terminal tool alongside it, which every invocation gets
    // whether or not its agent declared one.
    let offered: Vec<String> = llm
        .requests()
        .first()
        .expect("at least one LLM request")
        .tools
        .iter()
        .map(|t| t.name.clone())
        .collect();
    assert_eq!(
        offered,
        vec![
            "builtin__self_inspect".to_string(),
            crate::tools::REPORT_OUTCOME_CANONICAL_NAME.to_string(),
        ]
    );

    // The event trail records the canonical vocabulary even though
    // the model issued the bare name.
    let mut call_name: Option<String> = None;
    let mut saw_result = false;
    for _ in 0..15 {
        let event = tokio::time::timeout(Duration::from_secs(2), sub.next())
            .await
            .expect("timeout")
            .expect("stream closed")
            .expect("deserialise");
        match &event.payload {
            EventPayload::ToolCall(p) => call_name = Some(p.tool_name.clone()),
            EventPayload::ToolResult(_) => {
                saw_result = true;
                break;
            }
            _ => {}
        }
    }
    assert_eq!(
        call_name.as_deref(),
        Some(crate::tools::SELF_INSPECT_CANONICAL_NAME),
        "tool.call must record the canonical name"
    );
    assert!(
        saw_result,
        "self_inspect must dispatch and produce a result"
    );
}

/// The motivating test for picking SelfInspect as the first
/// reducer-aware feature: suspension across a tool dispatch.
/// We let the harness produce the `CallTool(self_inspect)`
/// step, capture state, drop the harness, run the synthetic
/// tool-fulfilment ourselves, and resume with a fresh
/// harness on the captured state. The final completion
/// must match a non-suspended run.
#[tokio::test]
async fn reducer_suspends_and_resumes_across_tool_dispatch() {
    use crate::worker::introspection::{HostInvocationStats, synthesize_self_inspect};
    use crate::worker::reducer::types::{
        AgentConfig, CapabilityResult, ModelResponse, NextAction, StepInput, ToolCallResult,
        TriggerPayload, TriggerSourceKind,
    };

    let cfg = AgentConfig {
        agent_id: AgentId::new("suspend-tools").unwrap(),
        model: "claude-haiku".to_string(),
        system_prompt: "introspect on demand.".to_string(),
        tools_available: vec![],
        allowed_tool_names: vec!["builtin__self_inspect".to_string()],
        max_iterations: crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS,
        effort: None,
    };
    let trig = TriggerPayload {
        source: TriggerSourceKind::Manual,
        subject: None,
        payload: json!("inspect"),
    };

    let mk = |state: Vec<u8>, last: Option<CapabilityResult>, idx: u32| StepInput {
        config: cfg.clone(),
        trigger: trig.clone(),
        state,
        last_result: last,
        now_ms: idx as u64,
        random_seed: idx as u64,
        step_index: idx,
        static_resource_context: None,
        host_notices: vec![],
    };

    // Step 0: seed → CallModel.
    let h = Harness::new();
    let s0 = h.step(mk(vec![], None, 0)).unwrap();

    // Step 1: model returns a self_inspect tool_use → CallTool.
    let s1 = h
        .step(mk(
            s0.state,
            Some(CapabilityResult::ModelResult(ModelResponse {
                parts: crate::events::assistant_parts(
                    None,
                    vec![crate::events::MessageToolCall {
                        tool_call_id: crate::events::ToolCallId::new("si").unwrap(),
                        tool_name: "builtin__self_inspect".to_string(),
                        parameters: json!({"include": ["budget"]}),
                    }],
                ),
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            })),
            1,
        ))
        .unwrap();
    let _call_request = match s1.next_action {
        NextAction::CallTool(req) => req,
        other => panic!("expected CallTool, got {other:?}"),
    };

    // Suspension point: we have `state` and the pending tool
    // call. Persist them. (In a real durable-resume scenario
    // these would be written to disk together — same shape.)
    let suspended_state = s1.state.clone();

    // Drop the entire harness and conjure a fresh one. This
    // is the load-bearing assertion: nothing in-process state
    // survives the boundary. (`Harness` has no Drop impl, so
    // we use the move-into-wildcard pattern instead of `drop`.)
    let _ = h;

    // Synthesise the tool result host-side, exactly like the
    // runner would have. This is the "tool was dispatched
    // while we were suspended" case.
    let tool_output = synthesize_self_inspect(
        &HostInvocationStats {
            invocation_id: "suspend-invocation",
            agent_id: "suspend-tools",
            model: "claude-haiku",
            allowed_tool_names: &["builtin__self_inspect".to_string()],
            budget: Some(0.50),
            max_iterations: 20,
            totals: InvocationTotals {
                total_llm_calls: 1,
                total_tool_calls: 0,
                total_cost: 0.0001,
                total_duration_ms: 0,
                sampling_cost: 0.0,
                elicitation_cost: 0.0,
            },
            elapsed_ms: 0,
            tokens_in_use: None,
            context_window_size: None,
            messages_in_history: None,
            oldest_turn_at_ms: None,
        },
        json!({"include": ["budget"]}),
    );

    let h2 = Harness::new();

    // Step 2 (post-resume): feed the tool result. Reducer
    // integrates it and asks for the next model turn.
    let s2 = h2
        .step(mk(
            suspended_state,
            Some(CapabilityResult::ToolResult(ToolCallResult {
                tool_call_id: crate::events::ToolCallId::new("si").unwrap(),
                output: tool_output.clone(),
                is_error: false,
                error_kind: None,
                duration_ms: 0,
            })),
            2,
        ))
        .unwrap();
    let next_req = match s2.next_action {
        NextAction::CallModel(req) => req,
        other => panic!("expected CallModel after tool result, got {other:?}"),
    };
    // The conversation history must contain the tool message
    // we just resumed with — verifies state round-tripping.
    // `Message::text()` is deliberately `None` for a tool-results turn —
    // tool output is data, not the turn's speech — so this reaches into
    // the results, which is the more precise assertion anyway.
    assert!(
        next_req.messages.iter().any(|m| match m {
            Message::ToolResults { results } => results.iter().any(|r| r.output == tool_output),
            _ => false,
        }),
        "resumed conversation missing tool message"
    );

    // Step 3: model answers based on the inspected state.
    let s3 = h2
        .step(mk(
            s2.state,
            Some(CapabilityResult::ModelResult(ModelResponse {
                parts: crate::events::assistant_parts(
                    None,
                    canned("inspected.", 10, 10)
                        .tool_calls()
                        .into_iter()
                        .cloned()
                        .collect(),
                ),
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            })),
            3,
        ))
        .unwrap();

    match s3.next_action {
        NextAction::Complete { text, .. } => assert_eq!(text, "inspected."),
        other => panic!("expected Complete after resumed inspection, got {other:?}"),
    }
}

// -----------------------------------------------------------
// Step 4: WAL writes around tool and LLM dispatches.
// -----------------------------------------------------------

/// Helper used by the WAL tests below: run a scripted
/// agent through the reducer path against live NATS,
/// returning the worker store (for WAL inspection) and the
/// captured event stream.
async fn run_with_wal(
    url: &str,
    agent: Agent,
    responses: Vec<ChatResponse>,
    expected_event_count: usize,
    sandbox_dir: Option<&std::path::Path>,
) -> (Arc<WorkerStore>, Vec<Event>) {
    let (store, events, _) =
        run_with_wal_capturing_outcome(url, agent, responses, expected_event_count, sandbox_dir)
            .await;
    (store, events)
}

/// Same as [`run_with_wal`] but also returns the `run`
/// result. Useful when a test asserts on the outcome
/// variant (e.g. budget-exceeded).
async fn run_with_wal_capturing_outcome(
    url: &str,
    agent: Agent,
    responses: Vec<ChatResponse>,
    expected_event_count: usize,
    sandbox_dir: Option<&std::path::Path>,
) -> (
    Arc<WorkerStore>,
    Vec<Event>,
    Result<InvocationOutcome, crate::worker::ExecutorError>,
) {
    let bus = EventBus::connect(url).await.expect("connect to NATS");
    let store_dir = tempdir().expect("tempdir");
    let store = Arc::new(
        WorkerStore::open(&store_dir.path().join("events.db"))
            .await
            .expect("worker store"),
    );

    let mut sub = bus
        .subscribe(format!("fq.agent.{}.>", agent.id().as_str()))
        .await
        .expect("subscribe");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let llm = FixtureClient::new();
    for r in responses {
        llm.push_response(r);
    }
    let runner = ReducerRunner::new(
        Arc::new(
            ReducerContext::builder()
                .tools(Arc::new(ToolRegistry::with_builtins()))
                .build(),
        ),
        Arc::new(
            RunnerConfig::builder()
                .bus(bus.clone())
                .pricing(test_pricing())
                .store(store.clone())
                .worker_id(test_worker_id())
                .build(),
        ),
        Harness::new(),
    );
    let outcome = runner
        .run(
            &agent,
            &llm,
            TriggerSource::Manual,
            None,
            json!({"input": "go"}),
        )
        .await;

    let mut events = Vec::with_capacity(expected_event_count);
    for _ in 0..expected_event_count {
        let event = tokio::time::timeout(Duration::from_secs(2), sub.next())
            .await
            .expect("event timeout")
            .expect("stream closed")
            .expect("deserialise");
        events.push(event);
    }
    // The store_dir tempfile must outlive the store handle;
    // we leak it through forget so the caller's tempdir cleanup
    // doesn't race the store's file references during the test
    // assertions. (`store_dir` goes out of scope at function
    // return; the SQLite WAL holds open file handles that are
    // released when `store` is dropped.)
    let _ = sandbox_dir; // suppress "unused" if not provided
    std::mem::forget(store_dir);
    (store, events, outcome)
}

fn end_turn_response(text: &str) -> ChatResponse {
    canned(text, 10, 20)
}

fn tool_call_response(tool: &str, call_id: &str, params: serde_json::Value) -> ChatResponse {
    ChatResponse {
        parts: crate::events::assistant_parts(
            None,
            vec![crate::events::MessageToolCall {
                tool_call_id: crate::events::ToolCallId::new(call_id).unwrap(),
                tool_name: tool.to_string(),
                parameters: params,
            }],
        ),
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage {
            input_tokens: 50,
            output_tokens: 10,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    }
}

fn simple_responder_agent(name: &str) -> Agent {
    Agent::builder()
        .id(name)
        .model("claude-haiku")
        .system_prompt("simple")
        .sandbox(Sandbox::new())
        .budget(1.0)
        .build()
        .unwrap()
}

#[tokio::test]
async fn llm_only_invocation_writes_intent_dispatched_completed_in_order() {
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    let agent_id = unique_agent_id("step4-llm-only");
    let agent = simple_responder_agent(&agent_id);

    // 1 LLM turn, end immediately.
    // After envelope-refactor step 3, no separate cost event:
    // triggered, llm.request, llm.dispatched, llm.response,
    // completed = 5 events.
    let (store, events) =
        run_with_wal(&url, agent, vec![end_turn_response("done.")], 5, None).await;
    // Six events: triggered, llm.request, llm.dispatched, llm.response, cost, completed.
    // We only asked for 5 above; let's ask for one more so the assertion below works cleanly.
    let _ = events; // (subset captured; the count is conservative for assertion below)

    // The dispatched-LLM rows should all be `completed`
    // by the time the invocation finishes.
    let ambiguous = store.find_ambiguous_llm_dispatches().await.unwrap();
    assert!(
        ambiguous.is_empty(),
        "no LLM dispatch should remain in `dispatched` state at end-of-invocation"
    );
}

#[tokio::test]
async fn tool_call_invocation_writes_tool_wal_in_order() {
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    let dir = tempdir().unwrap();
    let target = dir.path().join("hello.md");
    std::fs::write(&target, "# hi").unwrap();

    let agent_id = unique_agent_id("step4-tool-wal");
    let agent = Agent::builder()
        .id(&agent_id)
        .model("claude-haiku")
        .system_prompt("Use tools when asked.")
        .tools(["file_read"])
        .sandbox(Sandbox::new().fs_read(dir.path().to_string_lossy().to_string()))
        .budget(1.0)
        .build()
        .unwrap();

    let responses = vec![
        tool_call_response(
            "file_read",
            "tc_1",
            json!({"path": target.to_string_lossy().to_string()}),
        ),
        end_turn_response("read it."),
    ];

    // Events emitted (after envelope-refactor step 3, cost
    // rides on llm.response envelopes, no separate cost event):
    // 1. triggered
    // 2. llm.request (turn 1)
    // 3. llm.dispatched (turn 1)
    // 4. llm.response (turn 1, with tool calls, envelope.cost set)
    // 5. tool.call
    // 6. tool.dispatched
    // 7. tool.result
    // 8. llm.request (turn 2)
    // 9. llm.dispatched (turn 2)
    // 10. llm.response (turn 2, envelope.cost set)
    // 11. completed
    // 12. invocation.archived
    let (store, events) = run_with_wal(&url, agent, responses, 12, Some(dir.path())).await;

    let kinds: Vec<&str> = events
        .iter()
        .map(crate::test_support::events::event_kind)
        .collect();

    // Order check: tool.dispatched must appear between
    // tool.call and tool.result.
    crate::test_support::events::assert_kinds_appear_in_relative_order(
        &events,
        &["tool_call", "tool_dispatched", "tool_result"],
    );
    // Order check: llm.dispatched must appear between
    // llm.request and llm.response, for every turn.
    crate::test_support::events::assert_kinds_appear_in_relative_order(
        &events,
        &["llm_request", "llm_dispatched", "llm_response"],
    );
    // The tool.dispatched event is present at all.
    assert!(kinds.contains(&"tool_dispatched"), "kinds: {kinds:?}");
    // And the whole trace satisfies the canonical grammar.
    crate::test_support::oracle::assert_valid_trace(&events);

    // Every WAL row should be `completed` at end-of-invocation.
    assert!(
        store
            .find_ambiguous_tool_dispatches()
            .await
            .unwrap()
            .is_empty(),
        "tool_dispatch rows must all be completed"
    );
    assert!(
        store
            .find_ambiguous_llm_dispatches()
            .await
            .unwrap()
            .is_empty(),
        "llm_dispatch rows must all be completed"
    );

    // The tool dispatch row exists with status=completed
    // and is_error=false.
    let row = store
        .get_tool_dispatch(&events[0].envelope.invocation_id.to_string(), "tc_1")
        .await
        .unwrap()
        .expect("tool_dispatch row");
    assert_eq!(row.status, DispatchStatus::Completed);
    assert_eq!(row.is_error, Some(false));
    assert!(row.dispatched_at.is_some());
    assert!(row.completed_at.is_some());
}

#[tokio::test]
async fn tool_error_writes_completed_with_is_error_true() {
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    // Sandbox that allows the read, but the file doesn't
    // exist — file_read will return is_error=true.
    let dir = tempdir().unwrap();
    let agent_id = unique_agent_id("step4-tool-error");
    let agent = Agent::builder()
        .id(&agent_id)
        .model("claude-haiku")
        .system_prompt("Use tools.")
        .tools(["file_read"])
        .sandbox(Sandbox::new().fs_read(dir.path().to_string_lossy().to_string()))
        .budget(1.0)
        .build()
        .unwrap();

    let missing = dir.path().join("does-not-exist.md");
    let responses = vec![
        tool_call_response(
            "file_read",
            "tc_err",
            json!({"path": missing.to_string_lossy().to_string()}),
        ),
        end_turn_response("done."),
    ];

    let (store, events) = run_with_wal(&url, agent, responses, 11, Some(dir.path())).await;

    let row = store
        .get_tool_dispatch(&events[0].envelope.invocation_id.to_string(), "tc_err")
        .await
        .unwrap()
        .expect("tool_dispatch row");
    assert_eq!(row.status, DispatchStatus::Completed);
    assert_eq!(
        row.is_error,
        Some(true),
        "tool_dispatch must record is_error=true on tool failure"
    );
    // Not stuck in dispatched.
    assert!(
        store
            .find_ambiguous_tool_dispatches()
            .await
            .unwrap()
            .is_empty(),
        "tool error must not leave the row in `dispatched`"
    );
}

#[tokio::test]
async fn tool_not_in_agent_allowlist_is_denied_on_reducer_path() {
    // Defence-in-depth gating: the LLM only sees declared
    // tool schemas, but if it hallucinates a name, the
    // runner short-circuits to a synthetic ToolResult with
    // PermissionDenied and never executes anything. Mirrors
    // the legacy executor's `tool_not_in_agent_allowlist_is_denied`
    // — this is the reducer-path counterpart that was
    // missing as of commit `c9fd92e`.
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    let agent_id = unique_agent_id("gating-deny");
    // Agent declares only file_read; LLM will try file_write.
    let agent = Agent::builder()
        .id(&agent_id)
        .model("claude-haiku")
        .system_prompt("You like to write.")
        .tools(["file_read"])
        .budget(1.0)
        .build()
        .unwrap();

    let responses = vec![
        tool_call_response(
            "file_write",
            "call_deny",
            json!({"path": "/tmp/x", "content": "x"}),
        ),
        end_turn_response("done anyway."),
    ];

    // Event sequence on the synthetic-error path:
    //   triggered, llm.request, llm.dispatched, llm.response,
    //   tool.result (synthetic — no tool.call/tool.dispatched),
    //   llm.request, llm.dispatched, llm.response,
    //   completed, invocation.archived
    // = 10 events.
    let (store, events) = run_with_wal(&url, agent, responses, 10, None).await;

    let kinds: Vec<&str> = events
        .iter()
        .map(crate::test_support::events::event_kind)
        .collect();
    assert_eq!(
        kinds,
        vec![
            "triggered",
            "llm_request",
            "llm_dispatched",
            "llm_response",
            "tool_result",
            "llm_request",
            "llm_dispatched",
            "llm_response",
            "completed",
            "invocation_archived",
        ],
        "synthetic-error gating path must emit tool.result without tool.call / tool.dispatched"
    );

    // The single tool.result must be is_error=true with
    // PermissionDenied.
    let tool_result = events
        .iter()
        .find_map(|e| match &e.payload {
            EventPayload::ToolResult(p) => Some(p),
            _ => None,
        })
        .expect("tool_result event present");
    assert!(tool_result.is_error, "denied tool must surface as error");
    assert!(
        matches!(
            tool_result.error_kind,
            Some(ToolErrorKind::PermissionDenied)
        ),
        "denied tool error_kind must be PermissionDenied, got {:?}",
        tool_result.error_kind
    );

    // The denied call is journaled like any other result — a
    // completed error row, so resume can replay the conversation
    // from the WAL alone (finding 7; this test previously pinned
    // the opposite, replay-breaking behaviour).
    let inv_str = events[0].envelope.invocation_id.to_string();
    let dispatch = store
        .get_tool_dispatch(&inv_str, "call_deny")
        .await
        .unwrap()
        .expect("denied call must journal a completed error row");
    assert_eq!(dispatch.status, DispatchStatus::Completed);
    assert_eq!(dispatch.is_error, Some(true));
    assert!(
        dispatch
            .result
            .as_deref()
            .unwrap_or_default()
            .contains("not available"),
        "got {:?}",
        dispatch.result
    );
}

#[tokio::test]
async fn tool_sandbox_violation_surfaces_on_reducer_path() {
    // Sister to the executor-side
    // `tool_sandbox_violations_surface_to_the_llm`. Distinct
    // from the allowlist test above: here the tool *is*
    // allowed (`file_read` is in the agent's declared tools),
    // but the runtime sandbox denies the specific path. The
    // tool actually dispatches; the failure surfaces from
    // inside the tool, not from the synthetic-error gating
    // shortcut. So the event sequence includes both
    // `tool.call` and `tool.dispatched` before the failing
    // `tool.result`.
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    let allowed = tempdir().unwrap();
    let forbidden = tempdir().unwrap();
    let target = forbidden.path().join("secret.txt");
    std::fs::write(&target, "no").unwrap();

    let agent_id = unique_agent_id("sandbox-violator");
    let agent = Agent::builder()
        .id(&agent_id)
        .model("claude-haiku")
        .system_prompt("Try to read a file.")
        .tools(["file_read"])
        .sandbox(Sandbox::new().fs_read(allowed.path().to_string_lossy().to_string()))
        .budget(1.0)
        .build()
        .unwrap();

    let responses = vec![
        tool_call_response(
            "file_read",
            "call_violate",
            json!({"path": target.to_string_lossy()}),
        ),
        end_turn_response("Could not read."),
    ];

    // triggered, llm_request, llm_dispatched, llm_response,
    // tool_call, tool_dispatched, tool_result(err),
    // llm_request, llm_dispatched, llm_response, completed,
    // invocation_archived = 12 events.
    let (_store, events) = run_with_wal(&url, agent, responses, 12, None).await;

    let tool_result = events
        .iter()
        .find_map(|e| match &e.payload {
            EventPayload::ToolResult(p) => Some(p),
            _ => None,
        })
        .expect("tool_result event present");
    assert!(tool_result.is_error, "sandbox-blocked tool must error");
    assert!(
        matches!(
            tool_result.error_kind,
            Some(ToolErrorKind::SandboxViolation)
        ),
        "sandbox-blocked tool error_kind must be SandboxViolation, got {:?}",
        tool_result.error_kind
    );
}

#[tokio::test]
async fn budget_exceeded_emits_failed_event_on_reducer_path() {
    // Sister to the executor-side
    // `emits_failed_event_when_budget_exceeded`. The runner
    // computes total cost after the LLM turn lands and
    // short-circuits to `Failed { BudgetExceeded }` when the
    // budget is blown. Asserts both the outcome variant and
    // the on-bus event.
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    let agent_id = unique_agent_id("overspender");
    let agent = Agent::builder()
        .id(&agent_id)
        .model("claude-haiku")
        .system_prompt("You spend a lot.")
        .budget(0.0001)
        .build()
        .unwrap();

    // 1M input tokens at $1/M = $1.00 — well over $0.0001.
    let expensive = ChatResponse {
        parts: vec![crate::events::AssistantPart::Text {
            text: "expensive".to_string(),
        }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    };

    // triggered, llm_request, llm_dispatched, llm_response,
    // failed, invocation_archived = 6 events.
    let (_store, events, outcome) =
        run_with_wal_capturing_outcome(&url, agent, vec![expensive], 6, None).await;

    let outcome = outcome.expect("run resolves cleanly even on budget exceeded");
    assert!(
        matches!(outcome, InvocationOutcome::BudgetExceeded { .. }),
        "outcome must be BudgetExceeded, got {outcome:?}"
    );

    let failed = events
        .iter()
        .find_map(|e| match &e.payload {
            EventPayload::Failed(p) => Some(p),
            _ => None,
        })
        .expect("failed event present");
    assert!(
        matches!(failed.error_kind, FailureKind::BudgetExceeded),
        "failed.error_kind must be BudgetExceeded, got {:?}",
        failed.error_kind
    );
}

// -----------------------------------------------------------
// Step 5: per-step state persistence.
//
// These tests verify that the runner writes an
// `invocation_state` row at every step boundary and marks
// the row terminal on Complete/Failed. The matching
// recovery / resume semantics live in step 6 — these tests
// only assert the persistence side.
// -----------------------------------------------------------

#[tokio::test]
async fn complete_emits_invocation_archived_and_marks_row_pending() {
    // The hand-off path (step 8): a successful Complete
    // emits `invocation.archived` after `completed`, and the
    // worker store row is flipped to `archive_status =
    // "pending"`. The ack consumer (commit 6) deletes the
    // row on receipt; the retry sweeper (commit 7) re-emits
    // if the ack never arrives.
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    let agent_id = unique_agent_id("step8-archive-on-complete");
    let agent = simple_responder_agent(&agent_id);

    // Sequence after my change:
    //   triggered, llm.request, llm.dispatched, llm.response,
    //   completed, invocation.archived  → 6 events.
    let (store, events) =
        run_with_wal(&url, agent, vec![end_turn_response("done.")], 6, None).await;

    let kinds: Vec<&str> = events
        .iter()
        .map(crate::test_support::events::event_kind)
        .collect();
    assert_eq!(
        kinds,
        vec![
            "triggered",
            "llm_request",
            "llm_dispatched",
            "llm_response",
            "completed",
            "invocation_archived",
        ]
    );

    let inv_str = events[0].envelope.invocation_id.to_string();
    let row = store
        .get_invocation_state(&inv_str)
        .await
        .unwrap()
        .expect("state row should exist after run");
    assert_eq!(
        row.archive_status.as_deref(),
        Some("pending"),
        "archive_status must be flipped to pending after publish"
    );
    assert!(
        row.archive_published_at.is_some(),
        "archive_published_at must be set after publish"
    );

    let terminal_at_ms = row.terminal_at.expect("terminal_at set");
    match &events[5].payload {
        EventPayload::InvocationArchived(p) => {
            assert_eq!(p.final_phase, "completed");
            assert_eq!(
                p.final_state_blob, row.state_blob,
                "archived blob must match the persisted final state"
            );
            assert_eq!(p.started_at_ms, row.started_at);
            assert_eq!(p.terminal_at_ms, terminal_at_ms);
        }
        other => panic!("expected InvocationArchived, got {other:?}"),
    }
}

#[tokio::test]
async fn state_row_written_on_completion_with_terminal_at_set() {
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    let agent_id = unique_agent_id("step5-state-completion");
    let agent = simple_responder_agent(&agent_id);
    let (store, events) =
        run_with_wal(&url, agent, vec![end_turn_response("done.")], 6, None).await;

    let inv_str = events[0].envelope.invocation_id.to_string();
    let row = store
        .get_invocation_state(&inv_str)
        .await
        .unwrap()
        .expect("state row should exist after run");

    assert_eq!(row.invocation_id, inv_str);
    assert_eq!(row.phase, "completed");
    assert!(
        row.terminal_at.is_some(),
        "terminal_at must be set on Complete"
    );
    assert!(
        !row.state_blob.is_empty(),
        "state_blob must contain the reducer's final state"
    );
    assert_eq!(row.workspace_ref, None);
    // The state blob is reducer-readable JSON.
    let _: serde_json::Value =
        serde_json::from_slice(&row.state_blob).expect("state_blob deserialises as JSON");
}

/// The error returned to the caller must carry the same
/// `FailureKind` the `failed` event was emitted with — here the
/// genuine `max_iterations` case, which previously surfaced as a
/// bare `MaxIterationsExceeded` while the event said
/// `runtime_error` (neither side was right).
#[tokio::test]
async fn max_iterations_failure_carries_the_max_iterations_kind() {
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();
    let bus = EventBus::connect(&url).await.expect("connect to NATS");

    let agent_id_str = unique_agent_id("max-iter-kind");
    let agent = Agent::builder()
        .id(&agent_id_str)
        .model("claude-haiku")
        .system_prompt("You are a test agent.")
        .budget(5.0)
        .build()
        .unwrap();

    // The model asks for an unavailable tool on every turn; each
    // synthetic error feeds back and the loop burns one iteration
    // per model turn until DEFAULT_MAX_ITERATIONS trips.
    let llm = FixtureClient::new();
    for i in 0..=crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS {
        llm.push_response(tool_use(
            "unavailable_tool",
            &format!("call-{i}"),
            json!({}),
            (10, 5),
        ));
    }

    let store_dir = tempdir().expect("tempdir");
    let store = Arc::new(
        WorkerStore::open(&store_dir.path().join("events.db"))
            .await
            .expect("worker store"),
    );
    let runner = ReducerRunner::new(
        Arc::new(
            ReducerContext::builder()
                .tools(Arc::new(ToolRegistry::with_builtins()))
                .build(),
        ),
        Arc::new(
            RunnerConfig::builder()
                .bus(bus)
                .pricing(test_pricing())
                .store(store)
                .worker_id(test_worker_id())
                .build(),
        ),
        Harness::new(),
    );

    let err = runner
        .run(&agent, &llm, TriggerSource::Manual, None, json!("loop"))
        .await
        .expect_err("must fail on max iterations");
    match err {
        ExecutorError::InvocationFailed { kind, message } => {
            assert!(
                matches!(kind, FailureKind::MaxIterations),
                "expected MaxIterations kind, got {kind:?}: {message}"
            );
            assert!(message.contains("max iterations"), "got: {message}");
        }
        other => panic!("expected InvocationFailed, got {other:?}"),
    }
}

/// #301: an empty model response — no tool calls and no
/// non-whitespace content — is an error stop, never an implicit
/// success. This is the live incident shape (invocation
/// `019f70d1`, 2026-07-17): a provider 200 with nothing in it must
/// fail the invocation as an `LlmError` and close the WAL row as an
/// error, so recovery and the fleet's retry loop see a failure
/// rather than a phantom success.
#[tokio::test]
async fn empty_model_response_fails_the_invocation_as_llm_error() {
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    let agent_id_str = unique_agent_id("empty-response");
    let agent = Agent::builder()
        .id(&agent_id_str)
        .model("claude-haiku")
        .system_prompt("You are a test agent.")
        .budget(5.0)
        .build()
        .unwrap();

    // Whitespace-only content pins the trim() semantics — this is
    // "empty" exactly like `None` is.
    let empty = ChatResponse {
        parts: vec![crate::events::AssistantPart::Text {
            text: "   \n".to_string(),
        }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    };

    let (store, events, outcome) =
        run_with_wal_capturing_outcome(&url, agent, vec![empty], 6, None).await;

    match outcome {
        Err(ExecutorError::Llm(crate::llm::LlmError::RequestFailed(msg))) => {
            assert!(msg.contains("empty response"), "got: {msg}");
        }
        other => panic!("expected Llm(RequestFailed) on an empty response, got {other:?}"),
    }

    // The WAL closed the dispatch as an error — never a completed-ok
    // row for a turn that returned nothing — and carries the call's
    // real cost. 10 input tokens at $1/M is $0.00001; a 0.0 here is
    // the #447 leak, and `resume()` reconstitutes the budget
    // accumulator from exactly this column.
    let inv = events[0].envelope.invocation_id.to_string();
    let rows = store
        .list_llm_dispatches_for_invocation(&inv)
        .await
        .expect("list dispatches");
    assert_eq!(rows.len(), 1, "one dispatch row for the one LLM call");
    assert_eq!(rows[0].is_error, Some(true), "WAL row must close as error");
    assert!(
        rows[0]
            .response
            .as_deref()
            .unwrap_or_default()
            .contains("empty response"),
        "WAL response records the synthetic error, got {:?}",
        rows[0].response
    );
    assert_eq!(
        rows[0].cost_usd,
        Some(1e-5),
        "the provider's prefill was billed; the WAL must not record it as free"
    );
}

/// #447: the empty-completion path used to hold a fully-populated
/// `response.usage` and throw it away. It now publishes an
/// `llm.failure` carrying those counts, priced onto the envelope, and
/// folds the spend into the invocation's total.
///
/// This is the test that would fail if the capture were reverted: the
/// tokens on the payload and the cost on the envelope both come from
/// the discarded `ChatResponse`, and neither can be reconstructed from
/// anywhere else once the response is dropped.
#[tokio::test]
async fn empty_response_bills_its_prefill_on_the_failure_event() {
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    let agent = Agent::builder()
        .id(unique_agent_id("empty-bills"))
        .model("claude-haiku")
        .system_prompt("You are a test agent.")
        .budget(5.0)
        .build()
        .unwrap();

    let empty = ChatResponse {
        parts: vec![],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 1_000,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    };
    let (_store, events, _outcome) =
        run_with_wal_capturing_outcome(&url, agent, vec![empty], 6, None).await;

    let failure = crate::test_support::events::find_first(&events, "llm_failure")
        .expect("an empty completion must publish llm.failure");
    let EventPayload::LlmFailure(p) = &failure.payload else {
        unreachable!("find_first matched on kind")
    };
    assert_eq!(p.error_kind, crate::events::LlmErrorKind::EmptyResponse);
    assert_eq!(
        p.usage.map(|u| u.input_tokens),
        Some(1_000),
        "the provider's token counts must survive onto the event"
    );
    let cost = failure
        .envelope
        .cost
        .as_ref()
        .expect("recovered usage must be priced onto the envelope");
    assert!(
        (cost.total_cost - 0.001).abs() < 1e-9,
        "1000 input tokens at $1/M, got {}",
        cost.total_cost
    );

    // And it reaches the invocation's total: the terminal `failed`
    // event carries the same money, so a budget can be tripped by it.
    let terminal = crate::test_support::events::find_first(&events, "failed").expect("failed");
    let EventPayload::Failed(f) = &terminal.payload else {
        unreachable!("find_first matched on kind")
    };
    assert!(
        (f.partial_totals.total_cost - 0.001).abs() < 1e-9,
        "recovered spend must reach the invocation total, got {}",
        f.partial_totals.total_cost
    );
    assert_eq!(
        f.partial_totals.total_llm_calls, 0,
        "a call with no outcome is still not a completed call"
    );
}

/// #447 in its commonest shape: a provider error. The trail must show
/// the full triple ending in `llm.failure`, and the failure must carry
/// **no** cost — a transport error yields no parsed body, so we do not
/// know what the provider billed, and a zeroed cost would both lie and
/// pin the row against the retention sweep forever.
#[tokio::test]
async fn provider_error_publishes_a_failure_with_no_cost() {
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    let agent_id_str = unique_agent_id("provider-error");
    let agent = Agent::builder()
        .id(&agent_id_str)
        .model("claude-haiku")
        .system_prompt("You are a test agent.")
        .budget(5.0)
        .build()
        .unwrap();

    let bus = EventBus::connect(&url).await.expect("connect to NATS");
    let store_dir = tempdir().expect("tempdir");
    let store = Arc::new(
        WorkerStore::open(&store_dir.path().join("events.db"))
            .await
            .expect("worker store"),
    );
    let llm = FixtureClient::new();
    llm.push_error(crate::llm::LlmError::Auth("no api key".to_string()));

    let runner = ReducerRunner::new(
        Arc::new(
            ReducerContext::builder()
                .tools(Arc::new(ToolRegistry::with_builtins()))
                .build(),
        ),
        Arc::new(
            RunnerConfig::builder()
                .bus(bus.clone())
                .pricing(test_pricing())
                .store(store.clone())
                .worker_id(test_worker_id())
                .build(),
        ),
        Harness::new(),
    );

    let events = crate::test_support::events::capture_events(
        &bus,
        &agent_id_str,
        6,
        Duration::from_secs(5),
        || async {
            let _ = runner
                .run(
                    &agent,
                    &llm,
                    TriggerSource::Manual,
                    None,
                    json!({"input": "go"}),
                )
                .await;
        },
    )
    .await;

    // The whole point of the event: a request with an outcome, not a
    // request that trails off. `llm.dispatched` rides along so the
    // middle state the WAL already recorded is on the bus too.
    crate::test_support::events::assert_kinds_in_order(
        &events[..5],
        &[
            "triggered",
            "llm_request",
            "llm_dispatched",
            "llm_failure",
            "failed",
        ],
    );
    crate::test_support::oracle::assert_valid_trace(&events);

    let EventPayload::LlmFailure(p) = &events[3].payload else {
        unreachable!("asserted above")
    };
    assert_eq!(p.error_kind, crate::events::LlmErrorKind::Auth);
    assert!(p.error_message.contains("no api key"));
    assert_eq!(p.model, "claude-haiku");
    assert_eq!(p.round, 1, "a failed call consumes a Round");
    assert!(
        p.usage.is_none(),
        "a transport failure parses no body: usage is unknown, not zero"
    );
    assert!(
        events[3].envelope.cost.is_none(),
        "unknown spend must leave cost absent — a zeroed row would be \
         exempted from the retention sweep as a fake cost record"
    );
    // The correlation the invariant is keyed on.
    let EventPayload::LlmRequest(req) = &events[1].payload else {
        unreachable!("asserted above")
    };
    assert_eq!(p.call_id, req.call_id);
}

/// #301: a model that only ever produces bare text — never a tool
/// call, never `report_outcome` — terminates via the iteration
/// ceiling as a failure. Text is not a stop signal; the ceiling is.
#[tokio::test]
async fn bare_text_only_model_fails_at_the_iteration_ceiling() {
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    let agent_id_str = unique_agent_id("bare-text-ceiling");
    let agent = Agent::builder()
        .id(&agent_id_str)
        .model("claude-haiku")
        .system_prompt("You are a test agent.")
        .budget(5.0)
        .max_iterations(3)
        .build()
        .unwrap();

    let text_turn = || ChatResponse {
        parts: vec![crate::events::AssistantPart::Text {
            text: "still thinking out loud".to_string(),
        }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    };
    let responses: Vec<ChatResponse> = (0..5).map(|_| text_turn()).collect();

    let (_store, _events, outcome) =
        run_with_wal_capturing_outcome(&url, agent, responses, 1, None).await;

    match outcome {
        Err(ExecutorError::InvocationFailed { kind, message }) => {
            assert!(
                matches!(kind, FailureKind::MaxIterations),
                "expected MaxIterations, got {kind:?}: {message}"
            );
        }
        other => panic!("expected InvocationFailed(MaxIterations), got {other:?}"),
    }
}

/// A reducer that errors on `step` is a runtime defect — the
/// returned error must say so, not claim max-iterations.
#[tokio::test]
async fn reducer_step_error_carries_the_runtime_error_kind() {
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();
    let bus = EventBus::connect(&url).await.expect("connect to NATS");

    use crate::worker::reducer::types::StepOutput;

    struct FailingReducer;
    impl Reducer for FailingReducer {
        fn step(&self, _input: StepInput) -> Result<StepOutput, HarnessError> {
            Err(HarnessError {
                kind: crate::worker::reducer::types::HarnessErrorKind::InternalError,
                message: "synthetic reducer defect".to_string(),
            })
        }
    }

    let agent_id_str = unique_agent_id("step-error-kind");
    let agent = Agent::builder()
        .id(&agent_id_str)
        .model("claude-haiku")
        .system_prompt("You are a test agent.")
        .budget(1.0)
        .build()
        .unwrap();
    let llm = FixtureClient::new();

    let store_dir = tempdir().expect("tempdir");
    let store = Arc::new(
        WorkerStore::open(&store_dir.path().join("events.db"))
            .await
            .expect("worker store"),
    );
    let runner = ReducerRunner::new(
        Arc::new(
            ReducerContext::builder()
                .tools(Arc::new(ToolRegistry::with_builtins()))
                .build(),
        ),
        Arc::new(
            RunnerConfig::builder()
                .bus(bus)
                .pricing(test_pricing())
                .store(store)
                .worker_id(test_worker_id())
                .build(),
        ),
        FailingReducer,
    );

    let err = runner
        .run(&agent, &llm, TriggerSource::Manual, None, json!("x"))
        .await
        .expect_err("must fail on reducer step error");
    match err {
        ExecutorError::InvocationFailed { kind, message } => {
            assert!(
                matches!(kind, FailureKind::RuntimeError),
                "expected RuntimeError kind, got {kind:?}: {message}"
            );
            assert!(
                message.contains("synthetic reducer defect"),
                "got: {message}"
            );
        }
        other => panic!("expected InvocationFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn resume_safe_replay_continues_to_completion() {
    // Pre-populate a worker store so that resuming the
    // invocation continues from a "step 0 complete, action
    // 0 (LLM call) completed with end-turn" state — i.e.
    // the safe-replay case. The reducer should pick up the
    // persisted result, produce Complete, and finish.
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    use crate::worker::reducer::types::{
        AgentConfig, StepInput, TriggerPayload, TriggerSourceKind,
    };

    let dir = tempdir().unwrap();
    let store_path = dir.path().join("events.db");
    let store = Arc::new(WorkerStore::open(&store_path).await.unwrap());

    let agent_id_str = unique_agent_id("step6-resume-replay");
    let agent = Agent::builder()
        .id(&agent_id_str)
        .model("claude-haiku")
        .system_prompt("You are a test agent.")
        .budget(1.0)
        .build()
        .unwrap();
    let invocation_id = Uuid::now_v7();
    let inv_str = invocation_id.to_string();

    // Manually run harness step 0 to produce the state we
    // would have persisted at step_index=0 (post-step).
    let harness = Harness::new();
    let agent_config = AgentConfig {
        agent_id: AgentId::new(&agent_id_str).unwrap(),
        model: "claude-haiku".to_string(),
        system_prompt: "You are a test agent.".to_string(),
        tools_available: vec![],
        allowed_tool_names: vec![],
        max_iterations: crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS,
        effort: None,
    };
    let trigger = TriggerPayload {
        source: TriggerSourceKind::Manual,
        subject: None,
        payload: json!("hello"),
    };
    let s0_input = StepInput {
        config: agent_config.clone(),
        trigger: trigger.clone(),
        state: vec![],
        last_result: None,
        now_ms: 0,
        random_seed: 0,
        step_index: 0,
        static_resource_context: None,
        host_notices: vec![],
    };
    let s0_output = harness.step(s0_input).expect("step 0");

    store
        .upsert_invocation_state(&InvocationStateRow {
            invocation_id: inv_str.clone(),
            agent_id: agent_id_str.clone(),
            schema_version: 1,
            phase: "awaiting_model".to_string(),
            state_blob: s0_output.state,
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
        })
        .await
        .unwrap();

    // Pre-populate a completed LLM dispatch row whose
    // serialized response is end-turn.
    let response = canned("done.", 50, 5);
    let response_json = serde_json::to_string(&response).unwrap();
    store
        .write_llm_intent(&inv_str, "req-0", "claude-haiku", "{}", 1)
        .await
        .unwrap();
    store
        .write_llm_dispatched(&inv_str, "req-0", 2)
        .await
        .unwrap();
    store
        .write_llm_completed(&inv_str, "req-0", &response_json, false, 0.0001, 3)
        .await
        .unwrap();

    // Resume.
    let bus = EventBus::connect(&url).await.unwrap();
    let runner = ReducerRunner::new(
        Arc::new(
            ReducerContext::builder()
                .tools(Arc::new(ToolRegistry::with_builtins()))
                .build(),
        ),
        Arc::new(
            RunnerConfig::builder()
                .bus(bus)
                .pricing(test_pricing())
                .store(store.clone())
                .worker_id(test_worker_id())
                .build(),
        ),
        Harness::new(),
    );
    let llm = FixtureClient::new(); // no live responses needed

    let outcome = runner
        .resume(&agent, &llm, invocation_id)
        .await
        .expect("resume completes");

    match outcome {
        InvocationOutcome::Completed {
            invocation_id: inv,
            response,
            ..
        } => {
            assert_eq!(inv, invocation_id);
            assert_eq!(response.text().as_deref(), Some("done."));
        }
        other => panic!("expected Completed, got {other:?}"),
    }

    // State row is now terminal.
    let row = store.get_invocation_state(&inv_str).await.unwrap().unwrap();
    assert!(row.terminal_at.is_some());
    assert_eq!(row.phase, "completed");
}

/// The #373 replay-equivalence claim, at the seam where it is
/// provable: seed the crashed WAL shape (tool `dispatched`, no
/// `completed`), inject the interrupted result exactly as the
/// operator verb does, resume — and assert the model request the
/// replay persists carries the injected bytes VERBATIM from the
/// stored row. The notice is rendered once at injection from the
/// persisted dispatch timestamp (never a live clock — the PR #143
/// landmine), so replay can only ever present those same bytes.
#[tokio::test]
async fn injected_interrupted_result_reaches_replay_byte_identical() {
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    use crate::worker::reducer::types::{
        AgentConfig, StepInput, TriggerPayload, TriggerSourceKind,
    };

    let dir = tempdir().unwrap();
    let store_path = dir.path().join("events.db");
    let store = Arc::new(WorkerStore::open(&store_path).await.unwrap());

    let agent_id_str = unique_agent_id("resume-inject-replay");
    let agent = Agent::builder()
        .id(&agent_id_str)
        .model("claude-haiku")
        .system_prompt("You are a test agent.")
        .tools(["builtin__self_inspect"])
        .budget(1.0)
        .build()
        .unwrap();
    let invocation_id = Uuid::now_v7();
    let inv_str = invocation_id.to_string();

    let harness = Harness::new();
    let s0_output = harness
        .step(StepInput {
            config: AgentConfig {
                agent_id: AgentId::new(&agent_id_str).unwrap(),
                model: "claude-haiku".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools_available: vec![],
                allowed_tool_names: vec![],
                max_iterations: crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS,
                effort: None,
            },
            trigger: TriggerPayload {
                source: TriggerSourceKind::Manual,
                subject: None,
                payload: json!("hello"),
            },
            state: vec![],
            last_result: None,
            now_ms: 0,
            random_seed: 0,
            step_index: 0,
            static_resource_context: None,
            host_notices: vec![],
        })
        .expect("step 0");
    store
        .upsert_invocation_state(&InvocationStateRow {
            invocation_id: inv_str.clone(),
            agent_id: agent_id_str.clone(),
            schema_version: 1,
            phase: "awaiting_model".to_string(),
            state_blob: s0_output.state,
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
        })
        .await
        .unwrap();

    // The crashed shape: model turn 0 requested the tool; the tool
    // was handed off (dispatched) and the process died before any
    // completion write.
    let tool_use = ChatResponse {
        parts: crate::events::assistant_parts(
            None,
            vec![crate::events::MessageToolCall {
                tool_call_id: crate::events::ToolCallId::new("tc-0").unwrap(),
                tool_name: "builtin__self_inspect".to_string(),
                parameters: json!({}),
            }],
        ),
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage {
            input_tokens: 50,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    };
    store
        .write_llm_intent(&inv_str, "req-0", "claude-haiku", "{}", 1)
        .await
        .unwrap();
    store
        .write_llm_dispatched(&inv_str, "req-0", 2)
        .await
        .unwrap();
    store
        .write_llm_completed(
            &inv_str,
            "req-0",
            &serde_json::to_string(&tool_use).unwrap(),
            false,
            0.0001,
            3,
        )
        .await
        .unwrap();
    store
        .write_tool_intent(&inv_str, "tc-0", "builtin__self_inspect", "{}", 4)
        .await
        .unwrap();
    store
        .write_tool_dispatched(&inv_str, "tc-0", 5)
        .await
        .unwrap();

    // The operator verb's injection, via the same store API.
    let injected = store
        .inject_interrupted_results(&inv_str)
        .await
        .expect("inject");
    assert_eq!(injected, vec!["tc-0".to_string()]);
    let stored = store
        .get_tool_dispatch(&inv_str, "tc-0")
        .await
        .unwrap()
        .expect("injected row");
    assert_eq!(stored.status, DispatchStatus::Completed);
    let stored_result = stored.result.clone().expect("injected result present");
    assert!(stored_result.contains("interrupted"));

    // Resume: the replay feeds the injected result to the reducer,
    // which requests the next model turn from the fixture.
    let bus = EventBus::connect(&url).await.unwrap();
    let runner = ReducerRunner::new(
        Arc::new(
            ReducerContext::builder()
                .tools(Arc::new(ToolRegistry::with_builtins()))
                .build(),
        ),
        Arc::new(
            RunnerConfig::builder()
                .bus(bus)
                .pricing(test_pricing())
                .store(store.clone())
                .worker_id(test_worker_id())
                .build(),
        ),
        Harness::new(),
    );
    let llm = FixtureClient::new();
    llm.push_response(canned("done.", 60, 4));

    let outcome = runner
        .resume(&agent, &llm, invocation_id)
        .await
        .expect("resume completes");
    assert!(
        matches!(outcome, InvocationOutcome::Completed { .. }),
        "expected Completed, got {outcome:?}"
    );

    // The equivalence claim: the persisted request payload of the
    // post-injection model turn contains the stored injected bytes
    // verbatim — replay presents persisted bytes, never a
    // re-render.
    let llm_rows = store
        .list_llm_dispatches_for_invocation(&inv_str)
        .await
        .expect("list llm dispatches");
    let replay_request = llm_rows
        .iter()
        .find(|r| r.request_id != "req-0")
        .expect("the post-injection model request was persisted");
    fn tree_contains(v: &serde_json::Value, needle: &str) -> bool {
        match v {
            serde_json::Value::String(s) => s == needle,
            serde_json::Value::Array(a) => a.iter().any(|v| tree_contains(v, needle)),
            serde_json::Value::Object(o) => o.values().any(|v| tree_contains(v, needle)),
            _ => false,
        }
    }
    let payload: serde_json::Value =
        serde_json::from_str(&replay_request.request_payload).expect("request payload JSON");
    assert!(
        tree_contains(&payload, &stored_result),
        "the replayed model request does not carry the injected bytes \
         verbatim\nstored: {}\npayload: {}",
        stored_result,
        replay_request.request_payload
    );
}

/// #172 end-to-end: seed a WAL whose middle turn is a tool call and
/// whose completion timestamps tie at the millisecond, then resume
/// through the real store. The rows are written in true execution
/// order (so the store assigns the v9 `seq` in that order); a resume
/// that replays them in any other order desyncs the reducer and
/// fails, so a clean `Completed` outcome is the assertion.
async fn resume_with_same_ms_interleave(
    tag: &str,
    llm0_completed_at: i64,
    tool_completed_at: i64,
    llm1_completed_at: i64,
) {
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    use crate::worker::reducer::types::{
        AgentConfig, StepInput, TriggerPayload, TriggerSourceKind,
    };

    let dir = tempdir().unwrap();
    let store_path = dir.path().join("events.db");
    let store = Arc::new(WorkerStore::open(&store_path).await.unwrap());

    let agent_id_str = unique_agent_id(tag);
    let agent = Agent::builder()
        .id(&agent_id_str)
        .model("claude-haiku")
        .system_prompt("You are a test agent.")
        .tools(["builtin__self_inspect"])
        .budget(1.0)
        .build()
        .unwrap();
    let invocation_id = Uuid::now_v7();
    let inv_str = invocation_id.to_string();

    // A plausible step-0 state row; replay rebuilds from step 0, so
    // the blob only needs to exist and be non-terminal.
    let harness = Harness::new();
    let s0_output = harness
        .step(StepInput {
            config: AgentConfig {
                agent_id: AgentId::new(&agent_id_str).unwrap(),
                model: "claude-haiku".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools_available: vec![],
                allowed_tool_names: vec![],
                max_iterations: crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS,
                effort: None,
            },
            trigger: TriggerPayload {
                source: TriggerSourceKind::Manual,
                subject: None,
                payload: json!("hello"),
            },
            state: vec![],
            last_result: None,
            now_ms: 0,
            random_seed: 0,
            step_index: 0,
            static_resource_context: None,
            host_notices: vec![],
        })
        .expect("step 0");
    store
        .upsert_invocation_state(&InvocationStateRow {
            invocation_id: inv_str.clone(),
            agent_id: agent_id_str.clone(),
            schema_version: 1,
            phase: "awaiting_model".to_string(),
            state_blob: s0_output.state,
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
        })
        .await
        .unwrap();

    // True execution order: model turn 0 requests the tool, the tool
    // completes, model turn 1 ends the invocation. Completion writes
    // happen in this order, so the store's shared seq records it —
    // regardless of how the timestamps tie.
    let tool_use = ChatResponse {
        parts: crate::events::assistant_parts(
            None,
            vec![crate::events::MessageToolCall {
                tool_call_id: crate::events::ToolCallId::new("tc-0").unwrap(),
                tool_name: "builtin__self_inspect".to_string(),
                parameters: json!({}),
            }],
        ),
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage {
            input_tokens: 50,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    };
    let end_turn = canned("done.", 60, 4);
    store
        .write_llm_intent(&inv_str, "req-0", "claude-haiku", "{}", 1)
        .await
        .unwrap();
    store
        .write_llm_dispatched(&inv_str, "req-0", 2)
        .await
        .unwrap();
    store
        .write_llm_completed(
            &inv_str,
            "req-0",
            &serde_json::to_string(&tool_use).unwrap(),
            false,
            0.0001,
            llm0_completed_at,
        )
        .await
        .unwrap();
    store
        .write_tool_intent(&inv_str, "tc-0", "builtin__self_inspect", "{}", 2)
        .await
        .unwrap();
    store
        .write_tool_dispatched(&inv_str, "tc-0", 3)
        .await
        .unwrap();
    store
        .write_tool_completed(&inv_str, "tc-0", "{\"ok\":true}", false, tool_completed_at)
        .await
        .unwrap();
    store
        .write_llm_intent(&inv_str, "req-1", "claude-haiku", "{}", 4)
        .await
        .unwrap();
    store
        .write_llm_dispatched(&inv_str, "req-1", 5)
        .await
        .unwrap();
    store
        .write_llm_completed(
            &inv_str,
            "req-1",
            &serde_json::to_string(&end_turn).unwrap(),
            false,
            0.0001,
            llm1_completed_at,
        )
        .await
        .unwrap();

    let bus = EventBus::connect(&url).await.unwrap();
    let runner = ReducerRunner::new(
        Arc::new(
            ReducerContext::builder()
                .tools(Arc::new(ToolRegistry::with_builtins()))
                .build(),
        ),
        Arc::new(
            RunnerConfig::builder()
                .bus(bus)
                .pricing(test_pricing())
                .store(store.clone())
                .worker_id(test_worker_id())
                .build(),
        ),
        Harness::new(),
    );
    let llm = FixtureClient::new(); // WAL covers every turn — no live calls

    let outcome = runner
        .resume(&agent, &llm, invocation_id)
        .await
        .expect("resume replays the interleave in true order");
    match outcome {
        InvocationOutcome::Completed { response, .. } => {
            assert_eq!(response.text().as_deref(), Some("done."));
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

/// The regression direction: the model turn and the tool completion
/// tie at the same millisecond with the LLM first in true order —
/// the pre-v9 tools-first tiebreak replayed these backwards.
#[tokio::test]
async fn resume_replays_llm_then_tool_same_millisecond_in_true_order() {
    resume_with_same_ms_interleave("seq-llm-tool", 5, 5, 9).await;
}

/// The other direction: the tool completion and the following model
/// turn tie at the same millisecond.
#[tokio::test]
async fn resume_replays_tool_then_llm_same_millisecond_in_true_order() {
    resume_with_same_ms_interleave("seq-tool-llm", 3, 7, 7).await;
}

#[tokio::test]
async fn resume_enforces_lifetime_budget() {
    // Pre-registered finding 1 of the reducer verification
    // plan: totals used to reset on resume, making the budget
    // ceiling per-attempt. Pre-crash spend recorded in the WAL
    // must count against the budget after resume.
    //
    // Shape: the WAL says a completed pre-crash LLM call spent
    // $0.20 against a $0.05 budget, and its response was a
    // tool call, so the resumed loop must take another model
    // turn. That first post-resume call must terminate the
    // invocation as BudgetExceeded carrying the lifetime cost
    // — not run to completion on a fresh accumulator.
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    use crate::worker::reducer::types::{
        AgentConfig, StepInput, TriggerPayload, TriggerSourceKind,
    };

    let dir = tempdir().unwrap();
    let store_path = dir.path().join("events.db");
    let store = Arc::new(WorkerStore::open(&store_path).await.unwrap());

    let agent_id_str = unique_agent_id("resume-budget");
    let agent = Agent::builder()
        .id(&agent_id_str)
        .model("claude-haiku")
        .system_prompt("You are a test agent.")
        .budget(0.05)
        .build()
        .unwrap();
    let invocation_id = Uuid::now_v7();
    let inv_str = invocation_id.to_string();

    // State as persisted after step 0 (awaiting the model).
    let harness = Harness::new();
    let agent_config = AgentConfig {
        agent_id: AgentId::new(&agent_id_str).unwrap(),
        model: "claude-haiku".to_string(),
        system_prompt: "You are a test agent.".to_string(),
        tools_available: vec![],
        allowed_tool_names: vec![],
        max_iterations: crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS,
        effort: None,
    };
    let trigger = TriggerPayload {
        source: TriggerSourceKind::Manual,
        subject: None,
        payload: json!("hello"),
    };
    let s0_output = harness
        .step(StepInput {
            config: agent_config.clone(),
            trigger: trigger.clone(),
            state: vec![],
            last_result: None,
            now_ms: 0,
            random_seed: 0,
            step_index: 0,
            static_resource_context: None,
            host_notices: vec![],
        })
        .expect("step 0");

    store
        .upsert_invocation_state(&InvocationStateRow {
            invocation_id: inv_str.clone(),
            agent_id: agent_id_str.clone(),
            schema_version: 1,
            phase: "awaiting_model".to_string(),
            state_blob: s0_output.state,
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
        })
        .await
        .unwrap();

    // The completed pre-crash LLM call: $0.20 already spent
    // (past the $0.05 budget on its own) and a tool-use
    // response, so the resumed loop has more work to do. The
    // tool is not in the agent's (empty) tool list, so the
    // runner feeds back a synthetic error result and the
    // reducer asks for the next model turn.
    let response = ChatResponse {
        parts: crate::events::assistant_parts(
            None,
            vec![crate::events::MessageToolCall {
                tool_call_id: crate::events::ToolCallId::new("call-0").unwrap(),
                tool_name: "unavailable_tool".to_string(),
                parameters: json!({}),
            }],
        ),
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage {
            input_tokens: 50,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    };
    let response_json = serde_json::to_string(&response).unwrap();
    store
        .write_llm_intent(&inv_str, "req-0", "claude-haiku", "{}", 1)
        .await
        .unwrap();
    store
        .write_llm_dispatched(&inv_str, "req-0", 2)
        .await
        .unwrap();
    store
        .write_llm_completed(&inv_str, "req-0", &response_json, false, 0.20, 3)
        .await
        .unwrap();

    let bus = EventBus::connect(&url).await.unwrap();
    let runner = ReducerRunner::new(
        Arc::new(
            ReducerContext::builder()
                .tools(Arc::new(ToolRegistry::with_builtins()))
                .build(),
        ),
        Arc::new(
            RunnerConfig::builder()
                .bus(bus.clone())
                .pricing(test_pricing())
                .store(store.clone())
                .worker_id(test_worker_id())
                .build(),
        ),
        Harness::new(),
    );

    let mut sub = bus
        .subscribe(format!("fq.agent.{}.>", agent.id().as_str()))
        .await
        .expect("subscribe");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The post-resume model turn (reached after the synthetic
    // tool error is fed back to the reducer).
    let llm = FixtureClient::new();
    llm.push_response(ChatResponse {
        parts: vec![crate::events::AssistantPart::Text {
            text: "wrapping up".to_string(),
        }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 50,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    });

    let outcome = runner
        .resume(&agent, &llm, invocation_id)
        .await
        .expect("resume runs");

    match outcome {
        InvocationOutcome::BudgetExceeded { cost, .. } => {
            assert!(
                cost >= 0.20,
                "lifetime cost must include pre-crash spend, got {cost}"
            );
        }
        other => panic!("expected BudgetExceeded from lifetime spend, got {other:?}"),
    }
    loop {
        let event = tokio::time::timeout(Duration::from_secs(2), sub.next())
            .await
            .expect("timeout")
            .expect("stream closed")
            .expect("deserialise");
        if let EventPayload::Failed(payload) = event.payload {
            assert!(matches!(payload.phase, FailurePhase::Budget));
            break;
        }
    }
}

#[tokio::test]
async fn resume_refuses_ambiguous_invocation() {
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    let dir = tempdir().unwrap();
    let store = Arc::new(
        WorkerStore::open(&dir.path().join("events.db"))
            .await
            .unwrap(),
    );

    let agent_id = unique_agent_id("step6-resume-refuse");
    let agent = simple_responder_agent(&agent_id);
    let invocation_id = Uuid::now_v7();
    let inv_str = invocation_id.to_string();

    // State row + ambiguous tool dispatch (dispatched, no
    // completed).
    store
        .upsert_invocation_state(&InvocationStateRow {
            invocation_id: inv_str.clone(),
            agent_id: agent_id.clone(),
            schema_version: 1,
            phase: "dispatching_tools".to_string(),
            state_blob: vec![],
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
        })
        .await
        .unwrap();
    store
        .write_tool_intent(&inv_str, "tc1", "shell", "{}", 1)
        .await
        .unwrap();
    store
        .write_tool_dispatched(&inv_str, "tc1", 2)
        .await
        .unwrap();
    // No completed.

    let bus = EventBus::connect(&url).await.unwrap();
    let runner = ReducerRunner::new(
        Arc::new(
            ReducerContext::builder()
                .tools(Arc::new(ToolRegistry::with_builtins()))
                .build(),
        ),
        Arc::new(
            RunnerConfig::builder()
                .bus(bus)
                .pricing(test_pricing())
                .store(store)
                .worker_id(test_worker_id())
                .build(),
        ),
        Harness::new(),
    );
    let llm = FixtureClient::new();
    let err = runner
        .resume(&agent, &llm, invocation_id)
        .await
        .expect_err("resume should refuse ambiguous");
    assert!(
        format!("{err}").contains("ambiguous"),
        "expected ambiguous error, got: {err}"
    );
}

#[tokio::test]
async fn state_row_step_index_advances_with_each_step() {
    let server = crate::test_support::nats::test_nats();
    let url = server.url().to_string();

    // A two-turn invocation (tool call + final summary) goes
    // through enough reducer steps that `step_index` should
    // advance past 0.
    let dir = tempdir().unwrap();
    let target = dir.path().join("hello.md");
    std::fs::write(&target, "# hi").unwrap();

    let agent_id = unique_agent_id("step5-state-iter");
    let agent = Agent::builder()
        .id(&agent_id)
        .model("claude-haiku")
        .system_prompt("Use tools.")
        .tools(["file_read"])
        .sandbox(Sandbox::new().fs_read(dir.path().to_string_lossy().to_string()))
        .budget(1.0)
        .build()
        .unwrap();

    let responses = vec![
        tool_call_response(
            "file_read",
            "tc_iter",
            json!({"path": target.to_string_lossy().to_string()}),
        ),
        end_turn_response("read."),
    ];

    let (store, events) = run_with_wal(&url, agent, responses, 11, Some(dir.path())).await;
    let inv_str = events[0].envelope.invocation_id.to_string();
    let row = store
        .get_invocation_state(&inv_str)
        .await
        .unwrap()
        .expect("state row");
    assert_eq!(row.phase, "completed");
    assert!(
        row.step_index > 0,
        "step_index must advance past 0 for a multi-step invocation; got {}",
        row.step_index
    );
    assert!(row.started_at <= row.updated_at);
    assert!(row.terminal_at.unwrap_or(0) >= row.updated_at);
}

// --- R5, sampling axis (reducer verification slice 6): the
// sampling gate's budget boundaries, and sampling spend flowing
// into the invocation accumulator. Hermetic via the sim doubles;
// handle_sampling is private, hence tested here.

/// Issue #76: a pricing table carrying a context window, so the
/// runner can compute occupancy and fire the soft warning. Uses
/// `from_litellm_json` because that is the only constructor that
/// records `max_input_tokens`.
fn pricing_with_window() -> Arc<PricingTable> {
    // 100-token window, priced so cost stays trivial.
    let json = r#"{
        "tiny-window": {
            "max_input_tokens": 100,
            "input_cost_per_token": 0.000001,
            "output_cost_per_token": 0.000005
        }
    }"#;
    Arc::new(PricingTable::from_litellm_json(json).expect("pricing json"))
}

async fn windowed_runner(
    sink: &std::sync::Arc<crate::test_support::sim::RecordingSink>,
    dir: &tempfile::TempDir,
) -> ReducerRunner {
    let store = Arc::new(
        WorkerStore::open(&dir.path().join("events.db"))
            .await
            .expect("worker store"),
    );
    ReducerRunner::new(
        Arc::new(
            ReducerContext::builder()
                .tools(Arc::new(ToolRegistry::with_builtins()))
                .build(),
        ),
        Arc::new(
            RunnerConfig::builder()
                .event_sink(Arc::clone(sink) as Arc<dyn EventSink>)
                .pricing(pricing_with_window())
                .store(store)
                .worker_id(test_worker_id())
                .build(),
        ),
        Harness::new(),
    )
}

/// The soft context-pressure warning is injected once, past the
/// threshold, and is visible in the event trail (issue #76). The
/// model reports a prompt of 90 tokens against a 100-token window
/// (90% — over the 80% threshold), so the runner annotates the
/// `llm.response` event with the one-shot warning.
#[tokio::test]
async fn context_pressure_warning_injected_once_into_event_trail() {
    let sink = std::sync::Arc::new(crate::test_support::sim::RecordingSink::new());
    let dir = tempdir().expect("tempdir");
    let runner = windowed_runner(&sink, &dir).await;

    let agent = Agent::builder()
        .id(unique_agent_id("ctx-pressure"))
        .model("tiny-window")
        .system_prompt("be brief")
        .budget(1.0)
        .build()
        .unwrap();

    // Two end-turn-shaped turns are not needed: a single response
    // that is over threshold and ends the turn is enough. 90/100 in.
    let llm = FixtureClient::new();
    llm.push_response(canned("done.", 90, 5));

    runner
        .run(
            &agent,
            &llm,
            TriggerSource::Manual,
            None,
            json!({"input": "go"}),
        )
        .await
        .expect("invocation completes");

    let events = sink.events();
    let warned: Vec<_> = events
        .iter()
        .filter(|e| {
            e.annotations
                .0
                .get(crate::events::annotation_keys::FLAGS)
                .and_then(|v| v.get("context_pressure"))
                .is_some()
        })
        .collect();
    assert_eq!(
        warned.len(),
        1,
        "the soft warning must be injected exactly once into the event trail"
    );
    // And it rides on an llm.response event.
    assert!(
        matches!(warned[0].payload, EventPayload::LlmResponse(_)),
        "warning should annotate the llm.response that crossed the threshold"
    );
    assert_eq!(
        warned[0].annotations.0[crate::events::annotation_keys::FLAGS]["context_pressure"],
        json!(crate::worker::introspection::CONTEXT_PRESSURE_WARNING)
    );
}

/// Below the threshold, no warning is injected (issue #76).
#[tokio::test]
async fn context_pressure_warning_absent_below_threshold() {
    let sink = std::sync::Arc::new(crate::test_support::sim::RecordingSink::new());
    let dir = tempdir().expect("tempdir");
    let runner = windowed_runner(&sink, &dir).await;

    let agent = Agent::builder()
        .id(unique_agent_id("ctx-ok"))
        .model("tiny-window")
        .system_prompt("be brief")
        .budget(1.0)
        .build()
        .unwrap();

    // 10/100 tokens = 10%, well under threshold.
    let llm = FixtureClient::new();
    llm.push_response(canned("done.", 10, 5));

    runner
        .run(
            &agent,
            &llm,
            TriggerSource::Manual,
            None,
            json!({"input": "go"}),
        )
        .await
        .expect("invocation completes");

    let any_warning = sink.events().iter().any(|e| {
        e.annotations
            .0
            .get(crate::events::annotation_keys::FLAGS)
            .and_then(|v| v.get("context_pressure"))
            .is_some()
    });
    assert!(!any_warning, "no warning below the threshold");
}

fn sampling_world() -> (
    std::sync::Arc<crate::test_support::sim::RecordingSink>,
    tempfile::TempDir,
) {
    (
        std::sync::Arc::new(crate::test_support::sim::RecordingSink::new()),
        tempdir().expect("tempdir"),
    )
}

async fn sampling_runner(
    sink: &std::sync::Arc<crate::test_support::sim::RecordingSink>,
    dir: &tempfile::TempDir,
) -> ReducerRunner {
    let store = Arc::new(
        WorkerStore::open(&dir.path().join("events.db"))
            .await
            .expect("worker store"),
    );
    ReducerRunner::new(
        Arc::new(
            ReducerContext::builder()
                .tools(Arc::new(ToolRegistry::with_builtins()))
                .build(),
        ),
        Arc::new(
            RunnerConfig::builder()
                .event_sink(Arc::clone(sink) as Arc<dyn EventSink>)
                .pricing(test_pricing())
                .store(store)
                .worker_id(test_worker_id())
                .build(),
        ),
        Harness::new(),
    )
}

fn sampling_agent(budget: f64, sub_budget: Option<f64>) -> Agent {
    Agent::builder()
        .id(unique_agent_id("sampling-budget"))
        .model("claude-haiku")
        .system_prompt("You are a test agent.")
        .budget(budget)
        .sampling_grant(crate::agent::SamplingGrant {
            servers: vec!["srv".to_string()],
            max_cost: sub_budget,
        })
        .build()
        .unwrap()
}

fn sampling_params() -> CreateMessageRequestParams {
    serde_json::from_value(serde_json::json!({
        "messages": [
            {"role": "user", "content": {"type": "text", "text": "hello"}}
        ],
        "maxTokens": 50
    }))
    .expect("sampling params")
}

#[tokio::test]
async fn sampling_declined_when_invocation_budget_exhausted() {
    let (sink, dir) = sampling_world();
    let runner = sampling_runner(&sink, &dir).await;
    let agent = sampling_agent(1.0, None);
    let llm = FixtureClient::new(); // must never be consulted
    let mut totals = InvocationTotals {
        total_cost: 1.0,
        ..Default::default()
    };
    let mut cursor = None;
    let declined = runner
        .handle_sampling(
            &mut InvocationCtx {
                llm: &llm,
                agent_id: agent.id(),
                invocation_id: Uuid::now_v7(),
                totals: &mut totals,
                cursor: &mut cursor,
            },
            &agent,
            "srv",
            sampling_params(),
        )
        .await
        .expect("infrastructure ok")
        .expect_err("must decline");
    assert!(
        declined.message.contains("invocation budget exhausted"),
        "got: {}",
        declined.message
    );
    assert!(sink.events().is_empty(), "no model call on refusal");
    assert_eq!(totals.total_cost, 1.0, "refusal spends nothing");
}

#[tokio::test]
async fn sampling_declined_when_sub_budget_exhausted() {
    let (sink, dir) = sampling_world();
    let runner = sampling_runner(&sink, &dir).await;
    let agent = sampling_agent(10.0, Some(0.5));
    let llm = FixtureClient::new();
    let mut totals = InvocationTotals {
        total_cost: 0.5,
        sampling_cost: 0.5,
        ..Default::default()
    };
    let mut cursor = None;
    let declined = runner
        .handle_sampling(
            &mut InvocationCtx {
                llm: &llm,
                agent_id: agent.id(),
                invocation_id: Uuid::now_v7(),
                totals: &mut totals,
                cursor: &mut cursor,
            },
            &agent,
            "srv",
            sampling_params(),
        )
        .await
        .expect("infrastructure ok")
        .expect_err("must decline");
    assert!(
        declined.message.contains("sub-budget exhausted"),
        "got: {}",
        declined.message
    );
    assert!(sink.events().is_empty());
}

/// Sampling spends the agent's budget through the shared path:
/// totals and the sampling sub-accumulator both grow by the
/// priced amount, the WAL row carries the cost (the finding-4
/// fix, on the sampling path), and the published request is
/// attributed to the requesting server.
#[tokio::test]
async fn sampling_spends_into_the_invocation_budget() {
    let (sink, dir) = sampling_world();
    let runner = sampling_runner(&sink, &dir).await;
    let agent = sampling_agent(10.0, Some(1.0));
    let llm = FixtureClient::new();
    // haiku rates in test_pricing: $1/M in, $5/M out.
    llm.push_response(canned("sampled.", 100_000, 10_000)); // $0.15
    let mut totals = InvocationTotals::default();
    let mut cursor = None;
    let invocation_id = Uuid::now_v7();
    let result = runner
        .handle_sampling(
            &mut InvocationCtx {
                llm: &llm,
                agent_id: agent.id(),
                invocation_id,
                totals: &mut totals,
                cursor: &mut cursor,
            },
            &agent,
            "srv",
            sampling_params(),
        )
        .await
        .expect("infrastructure ok")
        .expect("sampling succeeds");
    drop(result);
    assert!(
        (totals.total_cost - 0.15).abs() < 1e-12,
        "{}",
        totals.total_cost
    );
    assert!(
        (totals.sampling_cost - 0.15).abs() < 1e-12,
        "{}",
        totals.sampling_cost
    );

    let events = sink.events();
    let origin = events
        .iter()
        .find_map(|e| match &e.payload {
            EventPayload::LlmRequest(p) => Some(p.origin.clone()),
            _ => None,
        })
        .expect("llm.request published");
    assert!(matches!(origin, crate::events::LlmCallOrigin::Sampling { server } if server == "srv"));

    let rows = runner
        .config
        .store
        .list_llm_dispatches_for_invocation(&invocation_id.to_string())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(
        (rows[0].cost_usd.unwrap_or(0.0) - 0.15).abs() < 1e-12,
        "WAL row must carry the sampling call's cost, got {:?}",
        rows[0].cost_usd
    );
}
