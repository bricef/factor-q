//! Integration tests for MCP client support using the official
//! `@modelcontextprotocol/server-everything` test server.
//!
//! These tests require `npx` to be on the PATH and the package to be
//! installed (or auto-fetched). They exercise the full MCP lifecycle:
//! server startup, tool discovery, tool invocation, and shutdown.

use std::sync::Arc;

use fq_runtime::mcp::{
    FactorQClientHandler, McpClientManager, McpServerConfig, ResourceNotification,
};
use fq_tools::{Tool, ToolContext, ToolSandbox};

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

    let notification = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        manager.recv_resource_notification("everything"),
    )
    .await
    .expect("a resource notification within 15s")
    .expect("notification channel open");
    assert!(
        matches!(notification, ResourceNotification::Updated { .. }),
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
