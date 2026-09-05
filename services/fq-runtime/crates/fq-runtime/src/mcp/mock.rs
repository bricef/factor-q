//! An in-process mock MCP server, shared by this module's unit tests.
//!
//! The `@modelcontextprotocol/server-everything` fixture the
//! integration tests use neither paginates its tool list, mutates it,
//! nor lets a test read back the `_meta` a request carried. This server
//! runs over a `tokio::io::duplex` in the same process, so the unit
//! tests can exercise cursor-following discovery, re-discovery after a
//! tool-list change, concurrent multiplexing over one client, and what
//! actually reached the server on the wire — with no child process and
//! no npx.
//!
//! It deliberately implements only `list_tools` and `call_tool`. Every
//! other request falls through to rmcp's default `ServerHandler`, which
//! answers `method_not_found` — which is itself the fixture for the
//! host's error mapping (see the `logging/setLevel` test).

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ListToolsResult, PaginatedRequestParams,
    ProgressToken, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ServerHandler, ServiceExt};
use tokio::sync::Mutex;

use super::{AdvertisedCapabilities, FactorQClientHandler, McpClient};

/// What one `tools/call` carried: the tool name and the progress token
/// the host attached, as the *server* saw them.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct RecordedCall {
    pub(super) name: String,
    pub(super) progress_token: Option<ProgressToken>,
}

/// Every `tools/call` a [`MockToolServer`] received, in order.
pub(super) type CallLog = Arc<Mutex<Vec<RecordedCall>>>;

pub(super) struct MockToolServer {
    tools: Arc<Mutex<Vec<Tool>>>,
    page_size: usize,
    calls: CallLog,
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

    /// Record what arrived and answer with a trivial success. The
    /// progress token is read from the request context's merged `_meta`
    /// — the same place a real server would look before deciding
    /// whether it may emit `notifications/progress`.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        self.calls.lock().await.push(RecordedCall {
            name: request.name.to_string(),
            progress_token: context.meta.get_progress_token(),
        });
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }
}

pub(super) fn mock_tool(name: &str) -> Tool {
    Tool::new(
        name.to_string(),
        "mock tool".to_string(),
        Arc::new(serde_json::Map::new()),
    )
}

/// Serve a mock over a duplex and return the connected client.
pub(super) async fn serve_mock(tools: Arc<Mutex<Vec<Tool>>>, page_size: usize) -> Arc<McpClient> {
    serve_mock_recording(tools, page_size).await.0
}

/// [`serve_mock`], plus the log of every `tools/call` the server
/// received — for asserting on what the host actually put on the wire.
pub(super) async fn serve_mock_recording(
    tools: Arc<Mutex<Vec<Tool>>>,
    page_size: usize,
) -> (Arc<McpClient>, CallLog) {
    let (server_transport, client_transport) = tokio::io::duplex(4096);
    let calls: CallLog = Arc::new(Mutex::new(Vec::new()));
    let server = MockToolServer {
        tools,
        page_size,
        calls: Arc::clone(&calls),
    };
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
    (Arc::new(client), calls)
}
