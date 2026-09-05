//! Unit tests for [`super`]. Extracted from the parent module so the
//! file that ships is the file you read (#390); `super::*` keeps the
//! same access it had inline.
//!
//! The server side is the in-process mock ([`crate::mcp::mock`]) served
//! over a duplex: the everything server neither paginates its tool list
//! nor mutates it, and cannot show a test what `_meta` arrived.

use super::*;

use crate::mcp::mock::{mock_tool, serve_mock, serve_mock_recording};

/// A manager holding `client` as a running server named `name`, for the
/// methods that need a *registered* server rather than a bare client.
/// Starting one the ordinary way would need a child process; the
/// manager's own state is reachable from here, so it is registered
/// directly.
fn manager_holding(name: &str, client: Arc<McpClient>) -> McpClientManager {
    let (_tx, notifications) = mpsc::unbounded_channel();
    let mut manager = McpClientManager::with_server_root(std::env::temp_dir().join("fq-mcp-unit"));
    manager.servers.push(RunningServer {
        name: name.to_string(),
        client,
        tool_names: Vec::new(),
        notifications: Mutex::new(notifications),
    });
    manager
}

async fn manager_with_mock(name: &str) -> McpClientManager {
    let client = serve_mock(Arc::new(Mutex::new(vec![mock_tool("only")])), 10).await;
    manager_holding(name, client)
}

#[tokio::test]
async fn discover_follows_the_pagination_cursor() {
    // 5 tools, 2 per page → 3 pages; discovery must follow the cursor.
    let tools = Arc::new(Mutex::new(
        (0..5)
            .map(|i| mock_tool(&format!("t{i}")))
            .collect::<Vec<_>>(),
    ));
    let client = serve_mock(tools, 2).await;
    let (_, names) = McpClientManager::discover_tools(&client, "mock")
        .await
        .expect("discover");
    assert_eq!(names.len(), 5, "all pages should be walked");
}

/// Parallel-workers Phase 2 (audit H3): concurrent invocations
/// share the base MCP connections, a load pattern serial dispatch
/// never produced. N concurrent callers over ONE shared client each
/// walk a full multi-page discovery — if rmcp's multiplexing
/// cross-routed or wedged concurrent requests on the one
/// connection, an interleaved cursor walk would come back short,
/// wrong, or not at all.
#[tokio::test]
async fn concurrent_callers_multiplex_over_one_shared_client() {
    let tools = Arc::new(Mutex::new(
        (0..7)
            .map(|i| mock_tool(&format!("t{i}")))
            .collect::<Vec<_>>(),
    ));
    // Page size 2 → four pages per discovery, so concurrent walks
    // genuinely interleave requests on the shared connection.
    let client = serve_mock(tools, 2).await;

    let mut set = tokio::task::JoinSet::new();
    for caller in 0..4 {
        let client = Arc::clone(&client);
        set.spawn(async move {
            let (_, names) = McpClientManager::discover_tools(&client, "mock")
                .await
                .expect("concurrent discover");
            (caller, names)
        });
    }
    while let Some(joined) = set.join_next().await {
        let (caller, names) = joined.expect("caller task");
        assert_eq!(
            names.len(),
            7,
            "caller {caller} must see the complete tool set despite \
             interleaving with its siblings"
        );
    }
}

#[tokio::test]
async fn rediscovery_reflects_a_mutated_tool_list() {
    let tools = Arc::new(Mutex::new(vec![mock_tool("a"), mock_tool("b")]));
    let client = serve_mock(tools.clone(), 10).await;
    let (_, before) = McpClientManager::discover_tools(&client, "mock")
        .await
        .expect("discover");
    assert_eq!(before.len(), 2);

    // The server mutates its tool list (what tools/list_changed
    // signals); a re-discovery (the refresh path) must reflect it.
    tools.lock().await.push(mock_tool("c"));
    let (_, after) = McpClientManager::discover_tools(&client, "mock")
        .await
        .expect("re-discover");
    assert_eq!(
        after.len(),
        3,
        "refresh re-discovery should see the new tool"
    );
}

/// Regression guard for issue #25 (teardown seam, deterministic
/// variant): after cancelling a service whose client `Arc` is still
/// held elsewhere (the condition under which `shutdown` can't
/// `close().await`), `await_graceful_close` observes the background
/// task finish its graceful transport close and returns *well within*
/// the grace window — it does not block for the full timeout and does
/// not require a force-kill. The stdio (child-process) EPIPE crash the
/// issue describes is exercised end to end by the `require_npx`-gated
/// `stdio_shutdown_with_outstanding_tool_arc_is_graceful` integration
/// test; this one pins the wait logic without needing a child process.
#[tokio::test]
async fn await_graceful_close_returns_once_the_service_tears_down() {
    let tools = Arc::new(Mutex::new(vec![mock_tool("only")]));
    let client = serve_mock(tools, 10).await;

    // A second Arc, standing in for a tool wrapper still holding the
    // client — exactly the case where `Arc::get_mut` fails and
    // `shutdown` must fall back to cancel + await.
    let held = Arc::clone(&client);

    client.cancellation_token().cancel();
    // Generous grace; the mock tears down in milliseconds, so this
    // must return well before the deadline (proving it waited for the
    // teardown rather than timing out or force-killing).
    let start = std::time::Instant::now();
    McpClientManager::await_graceful_close(&client, std::time::Duration::from_secs(5)).await;
    assert!(
        client.is_transport_closed(),
        "the service transport should be closed after cancellation"
    );
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "await_graceful_close should return on teardown, not time out (took {:?})",
        start.elapsed()
    );
    drop(held);
}

/// #191: a `logging/setLevel` failure used to be reported as
/// `McpError::ResourceOp`, so the operator was told "resource operation
/// on 'x' failed" for something that never touched a resource. The mock
/// implements no `set_level`, so rmcp's default handler answers
/// `method_not_found` — which is exactly the commonest real cause, a
/// server that never implemented the (SEP-2577 deprecated) request.
#[tokio::test]
async fn a_logging_level_failure_is_reported_as_a_logging_failure() {
    let manager = manager_with_mock("mock").await;

    let err = manager
        .set_logging_level("mock", LoggingLevel::Debug)
        .await
        .expect_err("the mock server implements no logging/setLevel");
    assert!(matches!(err, McpError::LoggingOp { .. }), "{err:?}");

    let rendered = err.to_string();
    assert!(
        rendered.starts_with("logging operation on 'mock' failed: "),
        "{rendered}"
    );
    assert!(
        !rendered.contains("resource operation"),
        "a logging failure must not name the resource half of the \
         protocol: {rendered}"
    );

    // An unknown server is still the unknown-server error, not a
    // logging one — the variant tracks what failed, not where it was
    // called from.
    let unknown = manager
        .set_logging_level("absent", LoggingLevel::Debug)
        .await
        .expect_err("no server named 'absent' is running");
    assert!(
        matches!(unknown, McpError::UnknownServer { .. }),
        "{unknown:?}"
    );
}

/// #191: `call_tool_cancellable` sent no progress token, while
/// `McpTool::execute` has always attached one — so the very calls that
/// would benefit from progress, the long-running ones a host wants to
/// be able to abort, were the ones that could not receive it. #547
/// wires every tool call through this method, so the two paths have to
/// agree before that lands.
///
/// The mock records what the *server* saw, which is the only place the
/// answer is honest: `_meta` is merged from two sources on the way out
/// (see the note in `call_tool_cancellable`).
#[tokio::test]
async fn a_cancellable_call_carries_a_progress_token() {
    let (client, calls) =
        serve_mock_recording(Arc::new(Mutex::new(vec![mock_tool("echo")])), 10).await;
    let manager = manager_holding("mock", client);

    let result = manager
        .call_tool_cancellable(
            "mock",
            // The canonical, provider-visible name; the server must be
            // asked for the remote name behind it.
            "mock__echo",
            serde_json::Map::new(),
            std::future::pending::<()>(),
        )
        .await
        .expect("the mock answers the call");
    assert!(
        result.is_some(),
        "the call completed rather than cancelling"
    );

    let recorded = calls.lock().await.clone();
    assert_eq!(recorded.len(), 1, "one call reached the server");
    assert_eq!(recorded[0].name, "echo", "the `mock__` prefix is stripped");
    assert!(
        recorded[0].progress_token.is_some(),
        "the server must be given a progress token, or it may not report \
         progress at all"
    );
}
