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
use std::sync::atomic::{AtomicBool, Ordering};

use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ListToolsResult, PaginatedRequestParams,
    ProgressNotificationParam, ProgressToken, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ServerHandler, ServiceExt};
use tokio::sync::Mutex;

use super::{AdvertisedCapabilities, FactorQClientHandler, McpClient};

/// How the mock answers `tools/call`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum CallBehaviour {
    /// Record the call and answer immediately.
    #[default]
    Answer,
    /// Emit one `notifications/progress` against the request's own
    /// token, then wait for the test to release the call before
    /// answering. The fixture for #605: the token the server reports
    /// under is the one rmcp minted, not one the host chose, and
    /// holding the call open is what lets a test observe the
    /// correlation *while it exists* rather than racing the guard that
    /// clears it.
    ReportProgressThenHold,
    /// Accept the call and never answer — the wedge the deadline
    /// exists for. Returns only when the host's
    /// `notifications/cancelled` cancels the request context, which is
    /// recorded in [`MockToolServer::cancelled`].
    HoldUntilCancelled,
}

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
    /// Answer every `tools/list` with a `next_cursor`, whether or not
    /// there are more tools — the discovery spin finding B3 describes
    /// (#548). A real server can produce it by accident (a cursor that
    /// never advances) as easily as on purpose.
    endless_cursor: bool,
    calls: CallLog,
    behaviour: CallBehaviour,
    control: MockControl,
}

/// The two flags a test uses to drive a mock's timing: one it reads,
/// one it sets.
#[derive(Clone, Default)]
pub(super) struct MockControl {
    /// Set by the server once it sees the host's
    /// `notifications/cancelled` for a held call — rmcp cancels the
    /// request context on receipt.
    pub(super) cancelled: Arc<AtomicBool>,
    /// Set by the *test* to let a
    /// [`ReportProgressThenHold`](CallBehaviour::ReportProgressThenHold)
    /// call answer.
    pub(super) release: Arc<AtomicBool>,
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
        let next_cursor = if self.endless_cursor {
            Some((start + self.page_size).to_string())
        } else {
            (end < tools.len()).then(|| end.to_string())
        };
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
        let progress_token = context.meta.get_progress_token();
        self.calls.lock().await.push(RecordedCall {
            name: request.name.to_string(),
            progress_token: progress_token.clone(),
        });
        match self.behaviour {
            CallBehaviour::Answer => {}
            CallBehaviour::ReportProgressThenHold => {
                if let Some(token) = progress_token {
                    let _ = context
                        .peer
                        .notify_progress(ProgressNotificationParam {
                            progress_token: token,
                            progress: 1.0,
                            total: Some(4.0),
                            message: Some("working".to_string()),
                        })
                        .await;
                }
                while !self.control.release.load(Ordering::SeqCst) {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            }
            CallBehaviour::HoldUntilCancelled => {
                context.ct.cancelled().await;
                self.control.cancelled.store(true, Ordering::SeqCst);
                // rmcp has already answered the caller's oneshot with
                // `Cancelled`; what this returns is never read.
                return Err(rmcp::ErrorData::internal_error("cancelled", None));
            }
        }
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

/// A server whose `tools/list` always answers with a `next_cursor`, so
/// following the chain never ends. The fixture for the discovery page
/// cap (#548).
pub(super) async fn serve_mock_endless_cursor(
    tools: Arc<Mutex<Vec<Tool>>>,
    page_size: usize,
) -> Arc<McpClient> {
    serve_mock_options(MockOptions {
        tools,
        page_size,
        endless_cursor: true,
        ..MockOptions::default()
    })
    .await
    .0
}

/// [`serve_mock`], plus the log of every `tools/call` the server
/// received — for asserting on what the host actually put on the wire.
pub(super) async fn serve_mock_recording(
    tools: Arc<Mutex<Vec<Tool>>>,
    page_size: usize,
) -> (Arc<McpClient>, CallLog) {
    let (client, calls, _control) =
        serve_mock_behaving(tools, page_size, CallBehaviour::Answer, None).await;
    (client, calls)
}

/// Everything a mock can be told to do, so a new dimension is a field
/// rather than another `serve_mock_*` arity.
pub(super) struct MockOptions {
    pub(super) tools: Arc<Mutex<Vec<Tool>>>,
    pub(super) page_size: usize,
    pub(super) endless_cursor: bool,
    pub(super) behaviour: CallBehaviour,
    pub(super) progress: Option<(String, super::progress::ProgressRegistry)>,
}

impl Default for MockOptions {
    fn default() -> Self {
        Self {
            tools: Arc::new(Mutex::new(Vec::new())),
            page_size: 10,
            endless_cursor: false,
            behaviour: CallBehaviour::Answer,
            progress: None,
        }
    }
}

/// The full seam: choose how the server answers `tools/call`, wire the
/// client's handler to a progress registry (so an inbound
/// `notifications/progress` is correlated), and get back the flags that
/// drive the call's timing.
pub(super) async fn serve_mock_behaving(
    tools: Arc<Mutex<Vec<Tool>>>,
    page_size: usize,
    behaviour: CallBehaviour,
    progress: Option<(String, super::progress::ProgressRegistry)>,
) -> (Arc<McpClient>, CallLog, MockControl) {
    serve_mock_options(MockOptions {
        tools,
        page_size,
        behaviour,
        progress,
        ..MockOptions::default()
    })
    .await
}

/// Serve a mock configured by [`MockOptions`].
pub(super) async fn serve_mock_options(
    options: MockOptions,
) -> (Arc<McpClient>, CallLog, MockControl) {
    let MockOptions {
        tools,
        page_size,
        endless_cursor,
        behaviour,
        progress,
    } = options;
    let (server_transport, client_transport) = tokio::io::duplex(4096);
    let calls: CallLog = Arc::new(Mutex::new(Vec::new()));
    let control = MockControl::default();
    let server = MockToolServer {
        tools,
        page_size,
        endless_cursor,
        calls: Arc::clone(&calls),
        behaviour,
        control: control.clone(),
    };
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_transport).await {
            let _ = running.waiting().await;
        }
    });
    let mut handler =
        FactorQClientHandler::default().with_capabilities(AdvertisedCapabilities::none());
    if let Some((server_name, registry)) = progress {
        handler = handler.with_progress(server_name, registry);
    }
    let client = handler
        .serve(client_transport)
        .await
        .expect("client serves over the duplex");
    (Arc::new(client), calls, control)
}
