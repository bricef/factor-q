//! Integration tests for MCP client support using the official
//! `@modelcontextprotocol/server-everything` test server.
//!
//! These tests require `npx` to be on the PATH and the package to be
//! installed (or auto-fetched). They exercise the full MCP lifecycle:
//! server startup, tool discovery, tool invocation, and shutdown.

use std::sync::Arc;

use fq_runtime::mcp::{McpClientManager, McpServerConfig};
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

fn everything_config() -> McpServerConfig {
    McpServerConfig {
        name: "everything".to_string(),
        command: "npx".to_string(),
        args: vec!["@modelcontextprotocol/server-everything".to_string()],
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
    assert!(!echo.description().is_empty(), "echo should have a description");
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
