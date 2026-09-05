//! Unit tests for [`super`]. Extracted from the parent module so the
//! file that ships is the file you read (#390); `super::*` keeps the
//! same access it had inline.

use std::sync::Arc;

use tokio::sync::Mutex;

use super::*;
use crate::mcp::mock::{mock_tool, serve_mock};

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
