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

/// #191: a chatty server must not starve a quiet one. The hand-rolled
/// poll-merge this replaced walked the channel list from index 0 and
/// returned the first ready one, so with `chatty` first and a backlog
/// queued, `quiet`'s single record could not be delivered until the
/// backlog had drained — position `CHATTY` in the output, not position
/// ~1. `StreamMap` polls from a random starting index, so each turn is
/// an even coin flip between the two ready streams and the quiet
/// server lands near the front.
///
/// The bound is generous on purpose: the assertion is "not starved",
/// not "exactly fair". Reaching it by chance needs the coin to come up
/// chatty `BOUND` times running (2^-64 here), while the old
/// implementation missed it by an order of magnitude every run.
#[tokio::test]
async fn a_chatty_server_cannot_starve_a_quiet_one() {
    const CHATTY: usize = 500;
    const BOUND: usize = 64;

    let log = |level: &str| ServerNotification::Log {
        level: level.to_string(),
        logger: None,
        data: Value::Null,
    };

    let (chatty_tx, chatty_rx) = mpsc::unbounded_channel();
    let (quiet_tx, quiet_rx) = mpsc::unbounded_channel();

    // Queue everything *before* the drain starts, so both streams are
    // ready on the very first poll and the ordering is entirely the
    // merge's choice rather than an artefact of arrival times.
    for _ in 0..CHATTY {
        chatty_tx.send(log("info")).expect("queue chatty record");
    }
    quiet_tx.send(log("warning")).expect("queue quiet record");
    drop(chatty_tx);
    drop(quiet_tx);

    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let drain = tokio::spawn(drain_server_notifications(
        // `chatty` first: the position the old merge always favoured.
        vec![
            ("chatty".to_string(), chatty_rx),
            ("quiet".to_string(), quiet_rx),
        ],
        McpToolRefresher::default(),
        |_registry| unreachable!("no tools/list_changed is sent"),
        move |server, _level, _logger, _data| {
            let _ = seen_tx.send(server);
        },
    ));

    let mut position = None;
    for index in 0..=CHATTY {
        let server = tokio::time::timeout(std::time::Duration::from_secs(5), seen_rx.recv())
            .await
            .expect("the drain forwards every queued record")
            .expect("sender lives until the drain ends");
        if server == "quiet" {
            position = Some(index);
            break;
        }
    }
    let position = position.expect("the quiet server's record is delivered");
    assert!(
        position < BOUND,
        "the quiet server waited behind {position} of the chatty server's \
         {CHATTY} records (bound {BOUND}) — the merge is starving it"
    );

    tokio::time::timeout(std::time::Duration::from_secs(5), drain)
        .await
        .expect("drain exits once both channels close")
        .expect("drain task");
}
