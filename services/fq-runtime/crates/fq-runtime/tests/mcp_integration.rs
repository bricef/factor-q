//! Integration tests for MCP client support using the official
//! `@modelcontextprotocol/server-everything` test server.
//!
//! These tests require `npx` to be on the PATH and the package to be
//! installed (or auto-fetched). They exercise the full MCP lifecycle:
//! server startup, tool discovery, tool invocation, and shutdown.

use std::sync::Arc;

use fq_runtime::mcp::{
    FactorQClientHandler, McpClientManager, McpServerConfig, ServerNotification, ServerRequest,
};
use fq_tools::{Tool, ToolContext, ToolSandbox};
use rmcp::model::{CreateMessageResult, LoggingLevel, Root, SamplingMessage};

/// Skip the test if `npx` is not available.
fn require_npx() -> bool {
    match std::process::Command::new("npx")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(status) => status.success(),
        Err(_) => false,
    }
}

/// Pinned so the TDD oracle is stable and runs from the local npx
/// cache without hitting the npm registry. Bump deliberately.
const EVERYTHING_SERVER: &str = "@modelcontextprotocol/server-everything@2026.1.26";

fn everything_config() -> McpServerConfig {
    McpServerConfig {
        name: "everything".to_string(),
        command: "npx".to_string(),
        args: vec!["-y".to_string(), EVERYTHING_SERVER.to_string()],
        env: vec![],
    }
}

#[tokio::test]
async fn discovers_tools_from_everything_server() {
    if !require_npx() {
        eprintln!("skipping: npx not found");
        return;
    }

    let mut manager = McpClientManager::new();
    let tools = manager
        .start_server(everything_config())
        .await
        .expect("start server-everything");

    // The everything server exposes many tools; verify we got at least
    // the echo tool.
    assert!(!tools.is_empty(), "should discover at least one tool");

    let echo = tools.iter().find(|t| t.name() == "echo");
    assert!(echo.is_some(), "should discover the echo tool");

    let echo = echo.unwrap();
    assert!(
        !echo.description().is_empty(),
        "echo should have a description"
    );
    assert!(
        echo.parameters_schema().is_object(),
        "echo should have an object schema"
    );

    manager.shutdown().await;
}

#[tokio::test]
async fn calls_echo_tool_end_to_end() {
    if !require_npx() {
        eprintln!("skipping: npx not found");
        return;
    }

    let mut manager = McpClientManager::new();
    let tools = manager
        .start_server(everything_config())
        .await
        .expect("start server-everything");

    let echo: Arc<dyn Tool> = tools
        .into_iter()
        .find(|t| t.name() == "echo")
        .expect("echo tool should exist");

    let sandbox = ToolSandbox::new();
    let ctx = ToolContext::new(&sandbox);
    let result = echo
        .execute(&ctx, serde_json::json!({"message": "hello from factor-q"}))
        .await
        .expect("echo should succeed");

    assert!(!result.is_error, "echo should not report an error");
    assert!(
        result.output.contains("hello from factor-q"),
        "echo output should contain the input message, got: {}",
        result.output
    );

    manager.shutdown().await;
}

#[tokio::test]
async fn duplicate_server_is_deduplicated() {
    if !require_npx() {
        eprintln!("skipping: npx not found");
        return;
    }

    let mut manager = McpClientManager::new();

    let tools_first = manager
        .start_server(everything_config())
        .await
        .expect("first start");
    assert!(!tools_first.is_empty());

    // Starting the same server again should be a no-op.
    let tools_second = manager
        .start_server(everything_config())
        .await
        .expect("duplicate start");
    assert!(
        tools_second.is_empty(),
        "duplicate server should return empty tool list"
    );

    manager.shutdown().await;
}

#[tokio::test]
async fn bad_command_returns_error() {
    let mut manager = McpClientManager::new();
    let result = manager
        .start_server(McpServerConfig {
            name: "nonexistent".to_string(),
            command: "this-binary-does-not-exist-12345".to_string(),
            args: vec![],
            env: vec![],
        })
        .await;

    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("bad command should return an error"),
    };
    assert!(
        err.to_string().contains("this-binary-does-not-exist-12345"),
        "error should mention the command: {}",
        err
    );
}

/// Step 2: the client advertises its full capability set (roots,
/// sampling, elicitation) and negotiates the server's advertised
/// capabilities during the initialize handshake.
#[tokio::test]
async fn negotiates_full_capability_set() {
    if !require_npx() {
        eprintln!("skipping: npx not found");
        return;
    }

    let mut manager = McpClientManager::new();
    manager
        .start_server(everything_config())
        .await
        .expect("start server-everything");

    // The everything server advertises the full server-side surface.
    let server = manager
        .server_capabilities("everything")
        .expect("negotiated server capabilities");
    assert!(
        server.resources.is_some(),
        "server should advertise resources"
    );
    assert!(server.prompts.is_some(), "server should advertise prompts");
    assert!(server.tools.is_some(), "server should advertise tools");
    assert!(server.logging.is_some(), "server should advertise logging");
    assert!(
        server.completions.is_some(),
        "server should advertise completions"
    );

    // factor-q advertises the client-side capabilities it intends to honour.
    let client = FactorQClientHandler::advertised_capabilities();
    assert!(client.roots.is_some(), "client should advertise roots");
    assert!(
        client.sampling.is_some(),
        "client should advertise sampling"
    );
    assert!(
        client.elicitation.is_some(),
        "client should advertise elicitation"
    );

    manager.shutdown().await;
}

/// Step 3a: the manager lists and reads resources (and templates) from a
/// server over the MCP resource protocol.
#[tokio::test]
async fn lists_and_reads_resources_from_everything_server() {
    if !require_npx() {
        eprintln!("skipping: npx not found");
        return;
    }

    let mut manager = McpClientManager::new();
    manager
        .start_server(everything_config())
        .await
        .expect("start server-everything");

    let resources = manager
        .list_resources("everything")
        .await
        .expect("list resources");
    assert!(!resources.is_empty(), "everything server exposes resources");

    // Read the first resource by URI; it should carry contents.
    let uri = resources[0].raw.uri.clone();
    let result = manager
        .read_resource("everything", &uri)
        .await
        .expect("read resource");
    assert!(!result.contents.is_empty(), "resource should have contents");

    let templates = manager
        .list_resource_templates("everything")
        .await
        .expect("list resource templates");
    assert!(
        !templates.is_empty(),
        "everything server exposes resource templates"
    );

    manager.shutdown().await;
}

/// Step 3b: resources surface to the agent as host-fulfilled tools the
/// LLM can call (`<server>__list_resources` / `<server>__read_resource`).
#[tokio::test]
async fn resource_tools_let_the_agent_list_and_read() {
    if !require_npx() {
        eprintln!("skipping: npx not found");
        return;
    }

    let mut manager = McpClientManager::new();
    let tools = manager
        .start_server(everything_config())
        .await
        .expect("start server-everything");

    let list_tool = tools
        .iter()
        .find(|t| t.name() == "everything__list_resources")
        .expect("list_resources tool synthesized");
    let read_tool = tools
        .iter()
        .find(|t| t.name() == "everything__read_resource")
        .expect("read_resource tool synthesized");

    let sandbox = ToolSandbox::new();
    let ctx = ToolContext::new(&sandbox);

    let listed = list_tool
        .execute(&ctx, serde_json::json!({}))
        .await
        .expect("list tool runs");
    assert!(!listed.is_error);
    assert!(
        listed.output.contains("://"),
        "listing should contain resource URIs, got: {}",
        listed.output
    );

    // Read a real resource by URI (taken from the protocol-level list).
    let uri = manager
        .list_resources("everything")
        .await
        .expect("list resources")[0]
        .raw
        .uri
        .clone();
    let read = read_tool
        .execute(&ctx, serde_json::json!({ "uri": uri }))
        .await
        .expect("read tool runs");
    assert!(!read.is_error);
    assert!(
        !read.output.trim().is_empty(),
        "read tool should return resource content"
    );

    manager.shutdown().await;
}

/// Step 3c: subscribing to a resource and enabling the everything
/// server's simulated updates delivers `resources/updated`
/// notifications through the client handler's sink.
#[tokio::test]
async fn subscribe_delivers_resource_update_notifications() {
    if !require_npx() {
        eprintln!("skipping: npx not found");
        return;
    }

    let mut manager = McpClientManager::new();
    let tools = manager
        .start_server(everything_config())
        .await
        .expect("start server-everything");

    // Subscribe to the first resource.
    let uri = manager
        .list_resources("everything")
        .await
        .expect("list resources")[0]
        .raw
        .uri
        .clone();
    manager
        .subscribe("everything", &uri)
        .await
        .expect("subscribe");

    // Updates are opt-in: the everything server only emits them after the
    // subscriber-updates toggle tool is invoked (no args; ~5s pace).
    let toggle = tools
        .iter()
        .find(|t| t.name().contains("subscriber"))
        .expect("subscriber-updates toggle tool");
    let sandbox = ToolSandbox::new();
    let ctx = ToolContext::new(&sandbox);
    toggle
        .execute(&ctx, serde_json::json!({}))
        .await
        .expect("toggle updates on");

    // The unified notification channel also carries log records (the
    // server logs the subscribe request), so skip past anything that
    // isn't the resources/updated we're waiting for.
    let notification = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            match manager.recv_notification("everything").await {
                Some(n @ ServerNotification::ResourceUpdated { .. }) => break n,
                Some(_) => continue,
                None => panic!("notification channel closed"),
            }
        }
    })
    .await
    .expect("a resources/updated notification within 15s");
    assert!(
        matches!(notification, ServerNotification::ResourceUpdated { .. }),
        "expected a resources/updated notification, got {notification:?}"
    );

    manager.shutdown().await;
}

/// Step 3b (addendum): the model can discover resource templates via a
/// `<server>__list_resource_templates` tool, so templated resources like
/// `scheme://path/{param}` can be filled in and read.
#[tokio::test]
async fn resource_template_tool_lists_templates() {
    if !require_npx() {
        eprintln!("skipping: npx not found");
        return;
    }

    let mut manager = McpClientManager::new();
    let tools = manager
        .start_server(everything_config())
        .await
        .expect("start server-everything");

    let templates_tool = tools
        .iter()
        .find(|t| t.name() == "everything__list_resource_templates")
        .expect("list_resource_templates tool synthesized");

    let sandbox = ToolSandbox::new();
    let ctx = ToolContext::new(&sandbox);
    let listed = templates_tool
        .execute(&ctx, serde_json::json!({}))
        .await
        .expect("list templates");
    assert!(!listed.is_error);
    assert!(
        listed.output.contains("://"),
        "template listing should contain template URIs, got: {}",
        listed.output
    );

    manager.shutdown().await;
}

/// Step 3d (foundation): the manager hands out a cloneable read-only
/// resource handle that reads by (server, uri) — the handle
/// ReducerContext holds for static_resources injection.
#[tokio::test]
async fn resource_reader_reads_by_server_and_uri() {
    if !require_npx() {
        eprintln!("skipping: npx not found");
        return;
    }

    let mut manager = McpClientManager::new();
    manager
        .start_server(everything_config())
        .await
        .expect("start server-everything");

    let uri = manager
        .list_resources("everything")
        .await
        .expect("list resources")[0]
        .raw
        .uri
        .clone();

    let reader = manager.resource_reader();
    let result = reader
        .read_resource("everything", &uri)
        .await
        .expect("read via handle");
    assert!(!result.contents.is_empty(), "handle should read contents");

    manager.shutdown().await;
}

/// Step 3d (iii) — end-to-end: an agent that declares a concrete
/// `static_resources` pin sees that resource's content in its very
/// first model request. Exercises the full wiring — the manager's
/// resource handle on `ReducerContext`, the runner reading pins
/// before the step loop, and the harness injecting the rendered
/// content after the system prompt. Drives a real everything server
/// with a scripted (fixture) LLM so the assertion is on what the
/// model was actually sent.
///
/// Needs both `npx` (the server) and NATS (the runner publishes the
/// canonical event sequence); skips if either is absent.
#[tokio::test]
async fn static_resource_pin_appears_in_first_model_request() {
    use std::collections::HashMap;

    use fq_runtime::events::{StopReason, TokenUsage, TriggerSource};
    use fq_runtime::llm::fixture::FixtureClient;
    use fq_runtime::{
        Agent, ChatResponse, EventBus, Harness, ModelPricing, PricingTable, ReducerContext,
        ReducerRunner, RunnerConfig, ToolRegistry, WorkerStore,
    };

    if !require_npx() {
        eprintln!("skipping: npx not found");
        return;
    }
    let Ok(nats_url) = std::env::var("FQ_NATS_URL") else {
        eprintln!("skipping: FQ_NATS_URL not set");
        return;
    };

    // Start the everything server and pick a concrete resource to pin.
    let mut manager = McpClientManager::new();
    manager
        .start_server(everything_config())
        .await
        .expect("start server-everything");
    let uri = manager
        .list_resources("everything")
        .await
        .expect("list resources")[0]
        .raw
        .uri
        .clone();
    // The exact text the runner will inject for this pin — rendered
    // through the same shared helper the runner uses.
    let read = manager
        .read_resource("everything", &uri)
        .await
        .expect("read pinned resource");
    let expected_body = fq_runtime::mcp::render_resource_contents(&read);
    assert!(
        !expected_body.is_empty(),
        "pinned resource should render to non-empty content"
    );

    // An agent that statically pins that resource.
    let pin = fq_runtime::agent::StaticResourcePin {
        server: "everything".to_string(),
        uri: uri.clone(),
    };
    let agent = Agent::builder()
        .id("static-resource-e2e")
        .model("claude-haiku")
        .system_prompt("You are a test agent.")
        .budget(1.0)
        .static_resources(vec![pin])
        .build()
        .expect("build agent");

    // Scripted model: a single end-turn response ends the invocation
    // after exactly one model request — the one we inspect.
    let llm = FixtureClient::new();
    llm.push_response(ChatResponse {
        content: Some("done.".to_string()),
        tool_calls: vec![],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    });

    // Host machinery: NATS bus + a throwaway worker store.
    let bus = EventBus::connect(&nats_url).await.expect("connect to NATS");
    let store_dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(
        WorkerStore::open(&store_dir.path().join("events.db"))
            .await
            .expect("open worker store"),
    );
    let mut pricing = HashMap::new();
    pricing.insert(
        "claude-haiku".to_string(),
        ModelPricing {
            input_per_million: 1.0,
            output_per_million: 5.0,
            cache_read_per_million: None,
            cache_write_per_million: None,
        },
    );
    let worker_id =
        fq_runtime::worker::WorkerId::new(uuid::Uuid::now_v7().to_string()).expect("worker id");

    let runner = ReducerRunner::new(
        Arc::new(
            ReducerContext::builder()
                .tools(Arc::new(ToolRegistry::with_builtins()))
                .resources(manager.resource_reader())
                .build(),
        ),
        Arc::new(
            RunnerConfig::builder()
                .bus(bus)
                .pricing(Arc::new(PricingTable::from_map(pricing)))
                .store(store)
                .worker_id(worker_id)
                .build(),
        ),
        Harness::new(),
    );

    runner
        .run(
            &agent,
            &llm,
            TriggerSource::Manual,
            None,
            serde_json::json!("hello"),
        )
        .await
        .expect("invocation runs to completion");

    // The fixture client recorded every request it saw; the first
    // is step 0's model request — it must carry the pinned resource.
    let requests = llm.requests();
    assert_eq!(requests.len(), 1, "exactly one model request was made");
    let first = &requests[0];
    assert!(
        first.messages.iter().any(|m| m
            .content
            .as_deref()
            .is_some_and(|c| c.contains(&expected_body))),
        "the pinned resource's content must appear in the first model request; \
         messages were: {:?}",
        first.messages
    );

    manager.shutdown().await;
}

// ---------------------------------------------------------------------------
// Step 4 — Prompts + Completion (P2)
//
// The pinned everything server registers four prompts:
//   simple-prompt      — no args, one user/text message
//   args-prompt        — city (required) + state (optional), substituted
//   completable-prompt — department + name, both with completion handlers
//   resource-prompt    — resourceType ("Text"/"Blob") + resourceId, returns a
//                        user/text message followed by an embedded resource
// ---------------------------------------------------------------------------

/// Step 4: list a server's prompts over the MCP prompt protocol.
#[tokio::test]
async fn lists_prompts_from_everything_server() {
    if !require_npx() {
        eprintln!("skipping: npx not found");
        return;
    }

    let mut manager = McpClientManager::new();
    manager
        .start_server(everything_config())
        .await
        .expect("start server-everything");

    let prompts = manager
        .list_prompts("everything")
        .await
        .expect("list prompts");
    let names: Vec<&str> = prompts.iter().map(|p| p.name.as_str()).collect();
    for expected in [
        "simple-prompt",
        "args-prompt",
        "completable-prompt",
        "resource-prompt",
    ] {
        assert!(
            names.contains(&expected),
            "expected prompt {expected:?} in {names:?}"
        );
    }

    manager.shutdown().await;
}

/// Step 4: fetch a parameterised prompt as a reusable, owned seed value —
/// the server substitutes the bound arguments into the message sequence,
/// and the seed records its provenance (server, name, arguments).
#[tokio::test]
async fn gets_parameterised_prompt_as_seed() {
    use std::collections::BTreeMap;

    use fq_runtime::prompt::{PromptRole, PromptSeed};

    if !require_npx() {
        eprintln!("skipping: npx not found");
        return;
    }

    let mut manager = McpClientManager::new();
    manager
        .start_server(everything_config())
        .await
        .expect("start server-everything");

    let args = BTreeMap::from([
        ("city".to_string(), "London".to_string()),
        ("state".to_string(), "Ontario".to_string()),
    ]);
    let seed: PromptSeed = manager
        .get_prompt("everything", "args-prompt", args.clone())
        .await
        .expect("get args-prompt");

    // Provenance.
    assert_eq!(seed.server, "everything");
    assert_eq!(seed.name, "args-prompt");
    assert_eq!(seed.arguments, args);

    // Bound message sequence: one user message with both args substituted.
    assert_eq!(seed.messages.len(), 1, "args-prompt returns one message");
    assert_eq!(seed.messages[0].role, PromptRole::User);
    let text = seed.messages[0]
        .content
        .to_text()
        .expect("text content renders");
    assert!(
        text.contains("What's weather in London, Ontario?"),
        "expected substituted arguments, got {text:?}"
    );
}

/// Step 4: an embedded *text* resource in a prompt is captured losslessly
/// and renders into the seed transcript (a supported handling path).
#[tokio::test]
async fn embedded_text_resource_prompt_renders() {
    use std::collections::BTreeMap;

    use fq_runtime::prompt::{EmbeddedResource, PromptContent, PromptSeed};

    if !require_npx() {
        eprintln!("skipping: npx not found");
        return;
    }

    let mut manager = McpClientManager::new();
    manager
        .start_server(everything_config())
        .await
        .expect("start server-everything");

    let args = BTreeMap::from([
        ("resourceType".to_string(), "Text".to_string()),
        ("resourceId".to_string(), "1".to_string()),
    ]);
    let seed: PromptSeed = manager
        .get_prompt("everything", "resource-prompt", args)
        .await
        .expect("get resource-prompt (Text)");

    // user/text followed by user/resource (embedded).
    assert_eq!(seed.messages.len(), 2, "text + embedded resource");
    assert!(
        matches!(
            seed.messages[1].content,
            PromptContent::EmbeddedResource(EmbeddedResource::Text { .. })
        ),
        "second message should be an embedded text resource, got {:?}",
        seed.messages[1].content
    );

    // The whole sequence is supported, so it renders to a transcript.
    let transcript = seed.to_transcript().expect("text resource renders");
    assert_eq!(transcript.len(), 2);
    assert!(
        transcript[1]
            .content
            .as_deref()
            .is_some_and(|c| c.contains("This is a plaintext resource")),
        "embedded resource text should reach the transcript"
    );
}

/// Step 4: an embedded *blob* resource is captured losslessly but is not yet
/// handled — rendering fails loudly with NotImplemented rather than silently
/// dropping it. Server-driven coverage of the handler-stub path.
#[tokio::test]
async fn embedded_blob_resource_prompt_is_not_implemented() {
    use std::collections::BTreeMap;

    use fq_runtime::prompt::{EmbeddedResource, PromptContent, PromptError, PromptSeed};

    if !require_npx() {
        eprintln!("skipping: npx not found");
        return;
    }

    let mut manager = McpClientManager::new();
    manager
        .start_server(everything_config())
        .await
        .expect("start server-everything");

    let args = BTreeMap::from([
        ("resourceType".to_string(), "Blob".to_string()),
        ("resourceId".to_string(), "1".to_string()),
    ]);
    let seed: PromptSeed = manager
        .get_prompt("everything", "resource-prompt", args)
        .await
        .expect("get resource-prompt (Blob)");

    // Captured losslessly as a blob resource...
    assert!(
        matches!(
            seed.messages[1].content,
            PromptContent::EmbeddedResource(EmbeddedResource::Blob { .. })
        ),
        "second message should be an embedded blob resource, got {:?}",
        seed.messages[1].content
    );
    // ...but handling it is not implemented, and says so loudly.
    assert!(
        matches!(seed.to_transcript(), Err(PromptError::NotImplemented(_))),
        "blob resource handling should be NotImplemented"
    );
}

/// Step 4: request argument completion for a prompt argument and assert the
/// server's suggestions (per ADR-0017, prompts/completion are
/// model-controlled — there is no human menu).
#[tokio::test]
async fn completes_prompt_argument() {
    if !require_npx() {
        eprintln!("skipping: npx not found");
        return;
    }

    let mut manager = McpClientManager::new();
    manager
        .start_server(everything_config())
        .await
        .expect("start server-everything");

    // department completions starting with "S" → Sales, Support.
    let completion = manager
        .complete_prompt("everything", "completable-prompt", "department", "S", None)
        .await
        .expect("complete department argument");
    assert!(
        completion.values.contains(&"Sales".to_string())
            && completion.values.contains(&"Support".to_string()),
        "expected Sales + Support, got {:?}",
        completion.values
    );

    manager.shutdown().await;
}

/// Step 5b — the handler→runner sampling bridge.
///
/// The everything server registers `trigger-sampling-request` only
/// when the client advertises the sampling capability (we do); calling
/// it makes the server send `sampling/createMessage` *back* to us and
/// block on the answer. With a per-invocation server (the channel
/// wired), `create_message` forwards the request on the
/// `ServerRequest` channel and awaits the host's reply. Here the test
/// plays the runner: it drains one request and replies with a canned
/// result, and the tool call completes carrying that result.
#[tokio::test]
async fn sampling_request_bridges_to_the_host() {
    if !require_npx() {
        eprintln!("skipping: npx not found");
        return;
    }

    let mut manager = McpClientManager::new();
    let (tools, mut requests, _roots) = manager
        .start_server_with_requests(everything_config(), vec![])
        .await
        .expect("start server-everything (per-invocation)");

    let trigger: Arc<dyn Tool> = tools
        .into_iter()
        .find(|t| t.name() == "trigger-sampling-request")
        .expect("everything server exposes trigger-sampling-request when sampling is advertised");

    let sandbox = ToolSandbox::new();

    // The tool call blocks until the host answers the bridged sampling
    // request, so drive both sides concurrently on this task. (rmcp
    // services the inbound request on its own background task, so the
    // bridge makes progress independently of this `join!`.)
    let tool_call = async {
        let ctx = ToolContext::new(&sandbox);
        trigger
            .execute(&ctx, serde_json::json!({"prompt": "ping", "maxTokens": 16}))
            .await
    };

    let host = async {
        let request = requests
            .recv()
            .await
            .expect("host should receive the bridged sampling request");
        let ServerRequest::Sampling { params, reply } = request else {
            panic!("expected a sampling request, got an elicitation one");
        };
        // The server forwarded its prompt through to us.
        assert!(
            !params.messages.is_empty(),
            "sampling request should carry the server's messages"
        );
        assert_eq!(params.max_tokens, 16, "max_tokens should round-trip");
        // Reply with a canned result, as the runner would after
        // running the LLM call.
        let result = CreateMessageResult::new(
            SamplingMessage::assistant_text("pong from host"),
            "test-sampling-model".to_string(),
        )
        .with_stop_reason(CreateMessageResult::STOP_REASON_END_TURN);
        reply
            .send(Ok(result))
            .expect("bridge should still be awaiting the reply");
    };

    let (tool_result, ()) = tokio::join!(tool_call, host);

    let result = tool_result.expect("trigger-sampling-request should complete");
    assert!(!result.is_error, "sampling tool should not report an error");
    assert!(
        result.output.contains("test-sampling-model") && result.output.contains("pong from host"),
        "tool output should echo the host's sampling result, got: {}",
        result.output
    );

    manager.shutdown().await;
}

// ---------------------------------------------------------------------------
// Step 5c — Sampling policy, end to end through the runner.
//
// A scripted agent calls the everything server's
// `trigger-sampling-request` tool, which makes the server send
// `sampling/createMessage` back to the runner mid-tool-call. The
// runner gates the request against the agent's grant/budget, and (when
// permitted) answers it on the agent's model through the one
// budgeted/WAL'd/evented LLM path. These tests assert the policy:
// permitted → a sampling model call happens; ungranted / over-budget →
// it is declined with *no* model call.
// ---------------------------------------------------------------------------

/// The everything server's sampling tool sends this exact system
/// prompt; spotting it in a recorded model request proves the sampling
/// completion ran through the runner's LLM path.
const SAMPLING_SYSTEM_PROMPT: &str = "You are a helpful test server.";

fn saw_sampling_call(requests: &[fq_runtime::ChatRequest]) -> bool {
    requests.iter().any(|r| {
        r.messages.iter().any(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.contains(SAMPLING_SYSTEM_PROMPT))
        })
    })
}

/// Drive a scripted agent that calls `trigger-sampling-request` under
/// the given sampling `grant`, against a per-invocation everything
/// server whose request channel is wired into the runner. Returns the
/// invocation outcome and every `ChatRequest` the agent model saw (the
/// sampling completion, when permitted, is one of them). `None` if the
/// test environment (npx / NATS) is unavailable.
async fn run_sampling_scenario(
    grant: Option<fq_runtime::SamplingGrant>,
) -> Option<(fq_runtime::InvocationOutcome, Vec<fq_runtime::ChatRequest>)> {
    use std::collections::HashMap;

    use fq_runtime::events::{StopReason, TokenUsage, TriggerSource};
    use fq_runtime::llm::fixture::FixtureClient;
    use fq_runtime::{
        Agent, ChatResponse, EventBus, Harness, ModelPricing, PricingTable, ReducerContext,
        ReducerRunner, RunnerConfig, SamplingChannel, ToolRegistry, WorkerStore,
    };

    if !require_npx() {
        eprintln!("skipping: npx not found");
        return None;
    }
    let Ok(nats_url) = std::env::var("FQ_NATS_URL") else {
        eprintln!("skipping: FQ_NATS_URL not set");
        return None;
    };

    // Per-invocation everything server with its inbound request channel.
    let mut manager = McpClientManager::new();
    let (tools, rx, _roots) = manager
        .start_server_with_requests(everything_config(), vec![])
        .await
        .expect("start server-everything (per-invocation)");

    let mut registry = ToolRegistry::with_builtins();
    for tool in tools {
        registry.register(tool);
    }

    // Agent permitted to call the sampling tool, with the grant under test.
    let mut builder = Agent::builder()
        .id("sampling-policy-e2e")
        .model("claude-haiku")
        .system_prompt("You are a test agent.")
        .budget(1.0)
        .tools(["trigger-sampling-request".to_string()]);
    if let Some(grant) = grant {
        builder = builder.sampling_grant(grant);
    }
    let agent = builder.build().expect("build agent");

    // Scripted model: (1) call the sampling tool, (2) the sampling
    // completion itself, (3) end the turn. When sampling is declined,
    // no model call consumes (2), so the agent's step 1 does — the
    // invocation still completes, just with one fewer model call.
    let canned = |text: &str| ChatResponse {
        content: Some(text.to_string()),
        tool_calls: vec![],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 8,
            output_tokens: 4,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    };
    let llm = FixtureClient::new();
    llm.push_response(ChatResponse {
        content: None,
        tool_calls: vec![fq_runtime::events::MessageToolCall {
            tool_call_id: fq_runtime::events::ToolCallId::new("call-sampling").unwrap(),
            tool_name: "trigger-sampling-request".to_string(),
            parameters: serde_json::json!({"prompt": "hello from agent", "maxTokens": 16}),
        }],
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    });
    llm.push_response(canned("SAMPLED-ANSWER"));
    llm.push_response(canned("done."));

    // Host machinery (mirrors the static-resource e2e test).
    let bus = EventBus::connect(&nats_url).await.expect("connect to NATS");
    let store_dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(
        WorkerStore::open(&store_dir.path().join("events.db"))
            .await
            .expect("open worker store"),
    );
    let mut pricing = HashMap::new();
    pricing.insert(
        "claude-haiku".to_string(),
        ModelPricing {
            input_per_million: 1.0,
            output_per_million: 5.0,
            cache_read_per_million: None,
            cache_write_per_million: None,
        },
    );
    let worker_id =
        fq_runtime::worker::WorkerId::new(uuid::Uuid::now_v7().to_string()).expect("worker id");

    let runner = ReducerRunner::new(
        Arc::new(
            ReducerContext::builder()
                .tools(Arc::new(registry))
                .resources(manager.resource_reader())
                .build(),
        ),
        Arc::new(
            RunnerConfig::builder()
                .bus(bus)
                .pricing(Arc::new(PricingTable::from_map(pricing)))
                .store(store)
                .worker_id(worker_id)
                .build(),
        ),
        Harness::new(),
    );

    let outcome = runner
        .run_with_server_requests(
            &agent,
            &llm,
            TriggerSource::Manual,
            None,
            serde_json::json!("go"),
            Some(SamplingChannel::new("everything", rx)),
        )
        .await
        .expect("invocation runs to completion");

    let requests = llm.requests();
    manager.shutdown().await;
    Some((outcome, requests))
}

/// Permitted: the grant covers the requesting server, so the runner
/// answers the sampling request on the agent's model — a third model
/// call (the sampling completion) appears, carrying the server's
/// system prompt.
#[tokio::test]
async fn sampling_permitted_runs_on_the_agent_model() {
    let grant = fq_runtime::SamplingGrant {
        servers: vec!["everything".to_string()],
        max_cost: None,
    };
    let Some((outcome, requests)) = run_sampling_scenario(Some(grant)).await else {
        return;
    };

    assert!(
        matches!(outcome, fq_runtime::InvocationOutcome::Completed { .. }),
        "invocation should complete, got {outcome:?}"
    );
    assert!(
        saw_sampling_call(&requests),
        "a sampling completion should have run on the agent model"
    );
    assert_eq!(
        requests.len(),
        3,
        "agent turn 0 + sampling + agent turn 1 = 3 model calls, got {}",
        requests.len()
    );
}

/// Over-budget: granted, but the sampling sub-budget is exhausted
/// (`max_cost: 0`), so the request is declined *before* any model call.
#[tokio::test]
async fn sampling_over_subbudget_is_declined_without_a_model_call() {
    let grant = fq_runtime::SamplingGrant {
        servers: vec!["everything".to_string()],
        max_cost: Some(0.0),
    };
    let Some((outcome, requests)) = run_sampling_scenario(Some(grant)).await else {
        return;
    };

    assert!(
        matches!(outcome, fq_runtime::InvocationOutcome::Completed { .. }),
        "invocation should still complete, got {outcome:?}"
    );
    assert!(
        !saw_sampling_call(&requests),
        "an over-budget sampling request must not call the model"
    );
    assert_eq!(
        requests.len(),
        2,
        "only the two agent turns should call the model, got {}",
        requests.len()
    );
}

/// Ungranted: the agent has no sampling grant at all, so every request
/// is declined with no model call.
#[tokio::test]
async fn sampling_ungranted_is_declined_without_a_model_call() {
    let Some((outcome, requests)) = run_sampling_scenario(None).await else {
        return;
    };

    assert!(
        matches!(outcome, fq_runtime::InvocationOutcome::Completed { .. }),
        "invocation should still complete, got {outcome:?}"
    );
    assert!(
        !saw_sampling_call(&requests),
        "an ungranted sampling request must not call the model"
    );
    assert_eq!(
        requests.len(),
        2,
        "only the two agent turns should call the model, got {}",
        requests.len()
    );
}

// ---------------------------------------------------------------------------
// Step 6a — Roots (handler-only; no LLM, no budget).
//
// Roots are derived from the agent's sandbox fs grant (advertised ⊆
// sandbox boundary), advertised to granted servers on `roots/list`,
// and updated via `roots/list_changed`. The everything server's
// `get-roots-list` tool calls `roots/list` (and re-fetches on
// `list_changed`), so it doubles as the oracle for both.
// ---------------------------------------------------------------------------

/// Read the everything server's `get-roots-list` tool output, which
/// reflects the roots it currently knows for this client.
async fn read_roots_list(tool: &Arc<dyn Tool>) -> String {
    let sandbox = ToolSandbox::new();
    let ctx = ToolContext::new(&sandbox);
    tool.execute(&ctx, serde_json::json!({}))
        .await
        .expect("get-roots-list should succeed")
        .output
}

#[tokio::test]
async fn roots_derived_from_sandbox_are_advertised_and_updatable() {
    use std::time::Duration;

    use fq_runtime::Sandbox;
    use fq_runtime::validation::ValidatorChain;

    if !require_npx() {
        eprintln!("skipping: npx not found");
        return;
    }

    // An agent whose sandbox grants two filesystem paths, with roots
    // advertised to the everything server.
    let sandbox = Sandbox::new()
        .fs_read("/tmp/factorq-roots-read")
        .fs_write("/tmp/factorq-roots-write");
    let grant = fq_runtime::RootsGrant {
        servers: vec!["everything".to_string()],
    };
    let roots = fq_runtime::advertised_roots(
        &sandbox,
        Some(&grant),
        "everything",
        &ValidatorChain::<Vec<Root>>::new(),
    );
    assert_eq!(roots.len(), 2, "both sandbox paths become roots");

    let mut manager = McpClientManager::new();
    let (tools, _rx, roots_handle) = manager
        .start_server_with_requests(everything_config(), roots)
        .await
        .expect("start server-everything (per-invocation)");

    let get_roots: Arc<dyn Tool> = tools
        .into_iter()
        .find(|t| t.name() == "get-roots-list")
        .expect("everything server exposes get-roots-list when roots is advertised");

    // The advertised roots reach the server on roots/list.
    let listed = read_roots_list(&get_roots).await;
    assert!(
        listed.contains("file:///tmp/factorq-roots-read")
            && listed.contains("file:///tmp/factorq-roots-write"),
        "server should see both advertised roots, got: {listed}"
    );

    // roots/list_changed: update the advertised set and notify; the
    // server re-fetches and reflects the new root.
    roots_handle
        .set_roots(vec![
            Root::new("file:///tmp/factorq-roots-extra").with_name("/tmp/factorq-roots-extra"),
        ])
        .await
        .expect("set_roots + notify list_changed");

    // The re-fetch is async; poll briefly until the change lands.
    let mut updated = String::new();
    for _ in 0..20 {
        updated = read_roots_list(&get_roots).await;
        if updated.contains("file:///tmp/factorq-roots-extra") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        updated.contains("file:///tmp/factorq-roots-extra"),
        "after list_changed the server should see the updated root, got: {updated}"
    );

    manager.shutdown().await;
}

/// Ungranted: roots are nothing-by-default, so a server not in the
/// grant is advertised an empty set.
#[tokio::test]
async fn roots_not_advertised_without_a_grant() {
    use fq_runtime::Sandbox;
    use fq_runtime::validation::ValidatorChain;

    let sandbox = Sandbox::new().fs_read("/tmp/factorq-roots-read");

    // No grant at all → empty.
    let none = fq_runtime::advertised_roots(
        &sandbox,
        None,
        "everything",
        &ValidatorChain::<Vec<Root>>::new(),
    );
    assert!(none.is_empty(), "no grant advertises no roots");

    // Granted to a different server → empty for this one.
    let grant = fq_runtime::RootsGrant {
        servers: vec!["some-other-server".to_string()],
    };
    let other = fq_runtime::advertised_roots(
        &sandbox,
        Some(&grant),
        "everything",
        &ValidatorChain::<Vec<Root>>::new(),
    );
    assert!(
        other.is_empty(),
        "a grant that omits this server advertises no roots"
    );
}

// ---------------------------------------------------------------------------
// Step 6b — Elicitation policy, end to end through the runner.
//
// A scripted agent calls the everything server's
// `trigger-elicitation-request` tool, which sends `elicitation/create`
// with a schema (`required: ["name"]`). The runner gates the request,
// then answers it as a schema-constrained completion on the agent's
// model: parse JSON → validate against the schema → retry up to N → on
// success `accept` with the value (which the server echoes back into
// the tool result), else `decline`.
// ---------------------------------------------------------------------------

/// The runner prefixes every elicitation completion with this phrase;
/// its presence in a recorded model request proves the schema-constrained
/// completion ran.
const ELICITATION_MARKER: &str = "completing a structured form";

fn saw_elicitation_call(requests: &[fq_runtime::ChatRequest]) -> bool {
    requests.iter().any(|r| {
        r.messages.iter().any(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.contains(ELICITATION_MARKER))
        })
    })
}

/// Drive a scripted agent that calls `trigger-elicitation-request`
/// under the given `grant`. `elicitation_answers` are the JSON strings
/// the model returns for successive elicitation attempts (each a
/// budget-counted model call); they are consumed only when the request
/// is permitted. Returns the outcome and every recorded ChatRequest.
async fn run_elicitation_scenario(
    grant: Option<fq_runtime::ElicitationGrant>,
    elicitation_answers: Vec<String>,
) -> Option<(fq_runtime::InvocationOutcome, Vec<fq_runtime::ChatRequest>)> {
    use std::collections::HashMap;

    use fq_runtime::events::{StopReason, TokenUsage, TriggerSource};
    use fq_runtime::llm::fixture::FixtureClient;
    use fq_runtime::{
        Agent, ChatResponse, EventBus, Harness, ModelPricing, PricingTable, ReducerContext,
        ReducerRunner, RunnerConfig, SamplingChannel, ToolRegistry, WorkerStore,
    };

    if !require_npx() {
        eprintln!("skipping: npx not found");
        return None;
    }
    let Ok(nats_url) = std::env::var("FQ_NATS_URL") else {
        eprintln!("skipping: FQ_NATS_URL not set");
        return None;
    };

    let mut manager = McpClientManager::new();
    let (tools, rx, _roots) = manager
        .start_server_with_requests(everything_config(), vec![])
        .await
        .expect("start server-everything (per-invocation)");

    let mut registry = ToolRegistry::with_builtins();
    for tool in tools {
        registry.register(tool);
    }

    let mut builder = Agent::builder()
        .id("elicitation-policy-e2e")
        .model("claude-haiku")
        .system_prompt("You are a test agent.")
        .budget(1.0)
        .tools(["trigger-elicitation-request".to_string()]);
    if let Some(grant) = grant {
        builder = builder.elicitation_grant(grant);
    }
    let agent = builder.build().expect("build agent");

    let canned = |text: &str, stop: StopReason| ChatResponse {
        content: Some(text.to_string()),
        tool_calls: vec![],
        stop_reason: stop,
        usage: TokenUsage {
            input_tokens: 8,
            output_tokens: 4,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    };

    let llm = FixtureClient::new();
    // (1) call the elicitation tool.
    llm.push_response(ChatResponse {
        content: None,
        tool_calls: vec![fq_runtime::events::MessageToolCall {
            tool_call_id: fq_runtime::events::ToolCallId::new("call-elicit").unwrap(),
            tool_name: "trigger-elicitation-request".to_string(),
            parameters: serde_json::json!({}),
        }],
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    });
    // (2) the elicitation completion attempts.
    for answer in &elicitation_answers {
        llm.push_response(canned(answer, StopReason::EndTurn));
    }
    // (3) end the agent turn.
    llm.push_response(canned("done.", StopReason::EndTurn));

    let bus = EventBus::connect(&nats_url).await.expect("connect to NATS");
    let store_dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(
        WorkerStore::open(&store_dir.path().join("events.db"))
            .await
            .expect("open worker store"),
    );
    let mut pricing = HashMap::new();
    pricing.insert(
        "claude-haiku".to_string(),
        ModelPricing {
            input_per_million: 1.0,
            output_per_million: 5.0,
            cache_read_per_million: None,
            cache_write_per_million: None,
        },
    );
    let worker_id =
        fq_runtime::worker::WorkerId::new(uuid::Uuid::now_v7().to_string()).expect("worker id");

    let runner = ReducerRunner::new(
        Arc::new(
            ReducerContext::builder()
                .tools(Arc::new(registry))
                .resources(manager.resource_reader())
                .build(),
        ),
        Arc::new(
            RunnerConfig::builder()
                .bus(bus)
                .pricing(Arc::new(PricingTable::from_map(pricing)))
                .store(store)
                .worker_id(worker_id)
                .build(),
        ),
        Harness::new(),
    );

    let outcome = runner
        .run_with_server_requests(
            &agent,
            &llm,
            TriggerSource::Manual,
            None,
            serde_json::json!("go"),
            Some(SamplingChannel::new("everything", rx)),
        )
        .await
        .expect("invocation runs to completion");

    let requests = llm.requests();
    manager.shutdown().await;
    Some((outcome, requests))
}

/// Permitted: the runner answers the elicitation on the agent model
/// with a schema-valid value; the server `accept`s it and echoes the
/// value back into the tool result (which reaches the agent transcript).
#[tokio::test]
async fn elicitation_permitted_accepts_schema_valid_value() {
    let grant = fq_runtime::ElicitationGrant {
        servers: vec!["everything".to_string()],
        max_cost: None,
    };
    let Some((outcome, requests)) =
        run_elicitation_scenario(Some(grant), vec![r#"{"name": "Ada Lovelace"}"#.to_string()])
            .await
    else {
        return;
    };

    assert!(
        matches!(outcome, fq_runtime::InvocationOutcome::Completed { .. }),
        "invocation should complete, got {outcome:?}"
    );
    assert!(
        saw_elicitation_call(&requests),
        "the schema-constrained completion should have run on the agent model"
    );
    assert_eq!(
        requests.len(),
        3,
        "agent turn 0 + elicitation + agent turn 1 = 3 model calls, got {}",
        requests.len()
    );
    // The accepted value round-trips: the server echoes it into the
    // tool result, which lands in the agent's next model request.
    assert!(
        requests.iter().any(|r| r.messages.iter().any(|m| m
            .content
            .as_deref()
            .is_some_and(|c| c.contains("Ada Lovelace")))),
        "the accepted value should round-trip into the agent transcript"
    );
}

/// Ungranted: no grant, so the request is declined with no model call.
#[tokio::test]
async fn elicitation_ungranted_is_declined_without_a_model_call() {
    let Some((outcome, requests)) =
        run_elicitation_scenario(None, vec![r#"{"name": "Ada Lovelace"}"#.to_string()]).await
    else {
        return;
    };

    assert!(
        matches!(outcome, fq_runtime::InvocationOutcome::Completed { .. }),
        "invocation should still complete, got {outcome:?}"
    );
    assert!(
        !saw_elicitation_call(&requests),
        "an ungranted elicitation request must not call the model"
    );
    assert_eq!(requests.len(), 2, "got {}", requests.len());
}

/// Over-budget: granted but the elicitation sub-budget is exhausted
/// (`max_cost: 0`), so it is declined before any model call.
#[tokio::test]
async fn elicitation_over_subbudget_is_declined_without_a_model_call() {
    let grant = fq_runtime::ElicitationGrant {
        servers: vec!["everything".to_string()],
        max_cost: Some(0.0),
    };
    let Some((outcome, requests)) =
        run_elicitation_scenario(Some(grant), vec![r#"{"name": "Ada Lovelace"}"#.to_string()])
            .await
    else {
        return;
    };

    assert!(
        matches!(outcome, fq_runtime::InvocationOutcome::Completed { .. }),
        "invocation should still complete, got {outcome:?}"
    );
    assert!(
        !saw_elicitation_call(&requests),
        "an over-budget elicitation request must not call the model"
    );
    assert_eq!(requests.len(), 2, "got {}", requests.len());
}

/// Retries exhausted: granted, but the model never produces a
/// schema-valid value (missing the required `name`, then unparseable),
/// so after the bounded retries the request is declined.
#[tokio::test]
async fn elicitation_retries_exhausted_declines() {
    let grant = fq_runtime::ElicitationGrant {
        servers: vec!["everything".to_string()],
        max_cost: None,
    };
    // Two invalid attempts: missing required field, then not JSON.
    let Some((outcome, requests)) = run_elicitation_scenario(
        Some(grant),
        vec![
            r#"{"unrelated": 1}"#.to_string(),
            "not json at all".to_string(),
        ],
    )
    .await
    else {
        return;
    };

    assert!(
        matches!(outcome, fq_runtime::InvocationOutcome::Completed { .. }),
        "invocation should complete, got {outcome:?}"
    );
    assert!(
        saw_elicitation_call(&requests),
        "the model should have been asked (and retried)"
    );
    assert_eq!(
        requests.len(),
        4,
        "agent turn 0 + two elicitation attempts + agent turn 1 = 4 model calls, got {}",
        requests.len()
    );
    // The server reports the decline in the tool result.
    assert!(
        requests.iter().any(|r| r
            .messages
            .iter()
            .any(|m| m.content.as_deref().is_some_and(|c| c.contains("declined")))),
        "the decline should round-trip into the agent transcript"
    );
}

// ---------------------------------------------------------------------------
// Step 7 — Logging (notifications/message).
//
// `logging/setLevel` chooses the minimum level; the everything
// server's `toggle-simulated-logging` tool emits one log immediately
// on enable. The handler folds logs into tracing and forwards them on
// the notification sink.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn server_log_messages_are_forwarded_after_set_level() {
    if !require_npx() {
        eprintln!("skipping: npx not found");
        return;
    }

    let mut manager = McpClientManager::new();
    let tools = manager
        .start_server(everything_config())
        .await
        .expect("start server-everything");

    // Ask for everything (debug+) so the immediate message — whatever
    // random level it is — is not filtered server-side.
    manager
        .set_logging_level("everything", LoggingLevel::Debug)
        .await
        .expect("logging/setLevel should be accepted");

    // Enabling simulated logging sends one message immediately.
    let toggle: &Arc<dyn Tool> = tools
        .iter()
        .find(|t| t.name() == "toggle-simulated-logging")
        .expect("everything server exposes toggle-simulated-logging");
    let sandbox = ToolSandbox::new();
    let ctx = ToolContext::new(&sandbox);
    toggle
        .execute(&ctx, serde_json::json!({}))
        .await
        .expect("toggle simulated logging on");

    let notification = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        manager.recv_notification("everything"),
    )
    .await
    .expect("a log notification within 10s")
    .expect("notification channel open");
    assert!(
        matches!(notification, ServerNotification::Log { .. }),
        "expected a notifications/message log, got {notification:?}"
    );

    manager.shutdown().await;
}

// ---------------------------------------------------------------------------
// Step 7 — Progress (notifications/progress).
//
// Every tool call carries a progress token, so the everything server's
// `trigger-long-running-operation` reports one progress notification
// per step against it.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn progress_notifications_track_a_long_running_operation() {
    if !require_npx() {
        eprintln!("skipping: npx not found");
        return;
    }

    let mut manager = McpClientManager::new();
    let tools = manager
        .start_server(everything_config())
        .await
        .expect("start server-everything");

    let op: &Arc<dyn Tool> = tools
        .iter()
        .find(|t| t.name() == "trigger-long-running-operation")
        .expect("everything server exposes trigger-long-running-operation");

    // 3 steps over ~1s → 3 progress notifications (progress 1..3, total 3).
    let sandbox = ToolSandbox::new();
    let ctx = ToolContext::new(&sandbox);
    let result = op
        .execute(&ctx, serde_json::json!({"duration": 1, "steps": 3}))
        .await
        .expect("long-running operation completes");
    assert!(!result.is_error, "operation should not error");

    // Progress notifications are buffered on the channel during the
    // call; drain three of them.
    let mut count = 0;
    while count < 3 {
        let n = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            manager.recv_notification("everything"),
        )
        .await
        .expect("a progress notification within 5s")
        .expect("notification channel open");
        if let ServerNotification::Progress { total, .. } = n {
            assert_eq!(total, Some(3.0), "total should be the step count");
            count += 1;
        }
    }
    assert_eq!(count, 3, "expected one progress notification per step");

    manager.shutdown().await;
}
