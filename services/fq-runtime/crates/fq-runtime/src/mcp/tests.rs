//! Unit tests for [`super`]. Extracted from the parent module so the
//! file that ships is the file you read (#390); `super::*` keeps the
//! same access it had inline.

use super::*;

#[test]
fn server_name_validation_enforces_charset_length_and_reservation() {
    for ok in ["everything", "a", "srv-2", &"x".repeat(48)] {
        assert!(validate_server_name(ok).is_ok(), "'{ok}' should be valid");
    }
    for (bad, why) in [
        ("", "empty"),
        ("Server", "uppercase"),
        ("my_server", "underscore breaks __ splitting"),
        ("srv.1", "dot violates provider tool-name rules"),
        (&"x".repeat(49), "over the 48-char bound"),
        ("builtin", "reserved runtime namespace"),
    ] {
        assert!(validate_server_name(bad).is_err(), "'{bad}' ({why})");
    }
    // The reservation gets its own message so the failure is
    // self-explaining, not a charset complaint.
    let err = validate_server_name("builtin").unwrap_err();
    assert!(format!("{err}").contains("reserved"), "{err}");
}

#[test]
fn namespaced_tool_names_are_bounded_to_provider_limits() {
    assert_eq!(
        namespaced_tool_name("everything", "echo").unwrap(),
        "everything__echo"
    );
    // 48 (max server) + 2 + 14 = 64: exactly at the bound is fine.
    let server = "x".repeat(48);
    assert!(namespaced_tool_name(&server, &"t".repeat(14)).is_ok());
    // One more character crosses the provider bound and must fail
    // loudly at discovery, not at the first LLM call.
    let err = namespaced_tool_name(&server, &"t".repeat(15)).unwrap_err();
    assert!(format!("{err}").contains("64"), "{err}");
    // A remote tool name containing `__` is legal — only the FIRST
    // `__` is the namespace split (server ids cannot contain `_`).
    assert_eq!(
        namespaced_tool_name("srv", "get__thing").unwrap(),
        "srv__get__thing"
    );
}

/// The shared-server dedup key must be the transport target. Before
/// this was fixed the key was `(command, args)`, and `command` is `""`
/// for every `url:` server — so all remote servers collided on one
/// bucket and only the first ever started.
#[test]
fn shared_server_key_is_the_transport_target_not_the_name() {
    let remote = |name: &str, url: &str| McpServerConfig {
        name: name.to_string(),
        command: String::new(),
        args: vec![],
        env: vec![],
        url: Some(url.to_string()),
    };
    let stdio = |name: &str, command: &str, args: &[&str]| McpServerConfig {
        name: name.to_string(),
        command: command.to_string(),
        args: args.iter().map(|a| a.to_string()).collect(),
        env: vec![],
        url: None,
    };
    let key = |config: &McpServerConfig| SharedServerKey::from_config(config).expect("keyable");

    // Two distinct endpoints are two servers, even sharing a name.
    assert_ne!(
        key(&remote("docs", "https://a.example/mcp")),
        key(&remote("docs", "https://b.example/mcp")),
    );
    // One endpoint is one server, however the agents name it — that is
    // the sharing the dedup exists to provide.
    assert_eq!(
        key(&remote("docs", "https://a.example/mcp")),
        key(&remote("reference", "https://a.example/mcp")),
    );

    // Same, for stdio: the spawned process is the identity.
    assert_ne!(
        key(&stdio("a", "npx", &["server-a"])),
        key(&stdio("a", "npx", &["server-b"])),
    );
    assert_eq!(
        key(&stdio("a", "npx", &["server-a"])),
        key(&stdio("b", "npx", &["server-a"])),
    );

    // A stdio and a remote server never collide, whatever the strings.
    assert_ne!(
        key(&stdio("x", "https://a.example/mcp", &[])),
        key(&remote("x", "https://a.example/mcp")),
    );

    // `url` wins when both are set, matching `start_inner`'s transport
    // selection — the key can never disagree with what gets started.
    let both = McpServerConfig {
        url: Some("https://a.example/mcp".to_string()),
        ..stdio("x", "npx", &["server-a"])
    };
    assert_eq!(key(&both), key(&remote("x", "https://a.example/mcp")));

    // Declaring neither is unstartable, so it is an error rather than a
    // bucket every such config silently joins.
    let err = SharedServerKey::from_config(&McpServerConfig {
        name: "nothing".to_string(),
        command: String::new(),
        args: vec![],
        env: vec![],
        url: None,
    })
    .expect_err("a config with no transport has no identity");
    assert!(matches!(err, McpError::UndeclaredTransport { .. }), "{err}");
}

#[test]
fn advertised_capabilities_reflect_the_grant() {
    let all = FactorQClientHandler::advertised_capabilities(AdvertisedCapabilities::all());
    assert!(all.roots.is_some() && all.sampling.is_some() && all.elicitation.is_some());

    let none = FactorQClientHandler::advertised_capabilities(AdvertisedCapabilities::none());
    assert!(
        none.roots.is_none() && none.sampling.is_none() && none.elicitation.is_none(),
        "nothing is advertised without a grant"
    );

    // Partial grant: only the granted capability is advertised.
    let sampling_only = FactorQClientHandler::advertised_capabilities(AdvertisedCapabilities {
        sampling: true,
        ..AdvertisedCapabilities::none()
    });
    assert!(sampling_only.sampling.is_some());
    assert!(sampling_only.roots.is_none() && sampling_only.elicitation.is_none());
}

#[test]
fn get_info_carries_granted_capabilities() {
    // Default handler (tool-only) advertises nothing inbound.
    let tool_only = FactorQClientHandler::default().get_info();
    assert!(tool_only.capabilities.sampling.is_none());
    assert!(tool_only.capabilities.roots.is_none());
    assert!(tool_only.capabilities.elicitation.is_none());

    // A fully-granted handler advertises all three.
    let granted = FactorQClientHandler::default()
        .with_capabilities(AdvertisedCapabilities::all())
        .get_info();
    assert!(granted.capabilities.sampling.is_some());
    assert!(granted.capabilities.roots.is_some());
    assert!(granted.capabilities.elicitation.is_some());
}

// --- D1: in-process mock MCP server (pagination + mutation) ---------
// The everything server neither paginates its tool list nor mutates
// it, so these tests serve a small in-process MCP server over a
// duplex to exercise cursor-following discovery and re-discovery
// after a tool-list change (the refresh path).
use rmcp::ServerHandler;
use rmcp::model::{ListToolsResult, PaginatedRequestParams, ServerInfo, Tool};

struct MockToolServer {
    tools: Arc<Mutex<Vec<Tool>>>,
    page_size: usize,
}

impl ServerHandler for MockToolServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let tools = self.tools.lock().await.clone();
        let start: usize = request
            .and_then(|r| r.cursor)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let end = (start + self.page_size).min(tools.len());
        let next_cursor = (end < tools.len()).then(|| end.to_string());
        Ok(ListToolsResult {
            tools: tools[start..end].to_vec(),
            next_cursor,
            ..Default::default()
        })
    }
}

fn mock_tool(name: &str) -> Tool {
    Tool::new(
        name.to_string(),
        "mock tool".to_string(),
        Arc::new(serde_json::Map::new()),
    )
}

async fn serve_mock(tools: Arc<Mutex<Vec<Tool>>>, page_size: usize) -> Arc<McpClient> {
    let (server_transport, client_transport) = tokio::io::duplex(4096);
    let server = MockToolServer { tools, page_size };
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_transport).await {
            let _ = running.waiting().await;
        }
    });
    let client = FactorQClientHandler::default()
        .with_capabilities(AdvertisedCapabilities::none())
        .serve(client_transport)
        .await
        .expect("client serves over the duplex");
    Arc::new(client)
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

/// B1 / ADR-0020: the drain consumes notifications and, on
/// `tools/list_changed`, hands a rebuilt registry (built-ins +
/// the server's *current* tools) to the install callback.
#[tokio::test]
async fn drain_rebuilds_the_registry_on_tool_list_changed() {
    let tools = Arc::new(Mutex::new(vec![mock_tool("alpha")]));
    let client = serve_mock(tools.clone(), 10).await;
    let refresher = McpToolRefresher {
        clients: vec![("mock".to_string(), client)],
        exec_config: fq_tools::builtin::ExecConfig::default(),
    };

    let (notif_tx, notif_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    let (log_tx, mut log_rx) = mpsc::unbounded_channel();
    let drain = tokio::spawn(drain_server_notifications(
        vec![("mock".to_string(), notif_rx)],
        refresher,
        move |registry| {
            let _ = out_tx.send(registry);
        },
        move |server, level, _logger, _data| {
            let _ = log_tx.send((server, level));
        },
    ));

    // Logs are consumed without rebuilding anything.
    notif_tx
        .send(ServerNotification::Log {
            level: "info".to_string(),
            logger: None,
            data: Value::String("hello".to_string()),
        })
        .expect("send log");

    // The server mutates its tool list and signals list_changed;
    // the drain rebuilds and the new tool is in the registry.
    tools.lock().await.push(mock_tool("beta"));
    notif_tx
        .send(ServerNotification::ToolListChanged)
        .expect("send list_changed");

    let rebuilt = tokio::time::timeout(std::time::Duration::from_secs(10), out_rx.recv())
        .await
        .expect("drain should rebuild before the timeout")
        .expect("registry");
    assert!(
        rebuilt.get("mock__alpha").is_some(),
        "existing tool present"
    );
    assert!(rebuilt.get("mock__beta").is_some(), "new tool present");
    assert!(
        rebuilt.get("builtin__file_read").is_some(),
        "built-ins present"
    );
    assert_eq!(
        out_rx.try_recv().ok().map(|_| ()),
        None,
        "the log record must not trigger a rebuild"
    );

    // The log record was forwarded to the bus bridge (B2).
    let (log_server, log_level) =
        tokio::time::timeout(std::time::Duration::from_secs(5), log_rx.recv())
            .await
            .expect("log forwarded before timeout")
            .expect("log record");
    assert_eq!(log_server, "mock");
    assert_eq!(log_level, "info");

    // Closing the last channel ends the drain.
    drop(notif_tx);
    tokio::time::timeout(std::time::Duration::from_secs(5), drain)
        .await
        .expect("drain exits when channels close")
        .expect("drain task");
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
