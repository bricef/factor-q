//! Fault injection against the MCP boot path, with real child
//! processes (#548, review finding B3).
//!
//! The unit tests in `mcp::lifecycle` and `mcp::discovery` inject the
//! faults the *protocol* can carry — a peer that never answers, a
//! cursor that never ends, a tool list that never stops. What needs a
//! child process is the other two: a server that appears on disk only
//! after the daemon has given up on it, and one that writes a line
//! longer than the transport will read.
//!
//! The fixture is a fifteen-line POSIX shell script that speaks just
//! enough MCP to complete `initialize` and answer one `tools/list`.
//! That is deliberate: `@modelcontextprotocol/server-everything`
//! answers both, but it arrives through `npx`, takes seconds to start,
//! and its startup is the known flake (#115) — a test about *when a
//! server appears* cannot also be a test about how long npm takes.

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use fq_runtime::mcp::{McpClientManager, McpLimits, McpServerConfig, McpServerState};

/// A stdio MCP server in POSIX shell: answer `initialize`, answer
/// `tools/list` with one tool, ignore everything else.
///
/// The request id is read back out of the line, because a JSON-RPC
/// response has to carry the id it answers. There is exactly one
/// `"id":<number>` in either request this server sees, so a `sed`
/// extraction is sound here in a way it would not be in general.
const STUB_SERVER: &str = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"stub","version":"0.1.0"}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"ping","description":"a stub tool","inputSchema":{"type":"object"}}]}}\n' "$id"
      ;;
  esac
done
"#;

/// A server that writes far more than any sane `max_line_bytes` with
/// no newline in it, before the host has said anything. The wedge the
/// bound exists for: `read_until(b'\n')` on this grows the daemon's
/// memory for as long as the server keeps writing.
const FIREHOSE_SERVER: &str = r#"#!/bin/sh
head -c 4000000 /dev/zero | tr '\0' x
sleep 60
"#;

fn write_script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).expect("write stub server");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("make the stub executable");
}

fn config(name: &str, program: &Path) -> McpServerConfig {
    McpServerConfig {
        name: name.to_string(),
        command: program.display().to_string(),
        args: Vec::new(),
        env: Vec::new(),
        url: None,
    }
}

fn manager(root: &Path, limits: McpLimits) -> Arc<tokio::sync::Mutex<McpClientManager>> {
    Arc::new(tokio::sync::Mutex::new(
        McpClientManager::with_server_root(root.to_path_buf()).with_limits(limits),
    ))
}

/// The fixture is a real MCP server, so the tests that depend on it
/// are meaningful: it completes the handshake and its tool is
/// discovered and namespaced like any other server's.
#[tokio::test]
async fn the_stub_server_is_a_real_mcp_server() {
    let dir = tempfile::tempdir().expect("tempdir");
    let program = dir.path().join("stub-server");
    write_script(&program, STUB_SERVER);
    let manager = manager(&dir.path().join("root"), McpLimits::default());

    let outcomes = {
        let mut guard = manager.lock().await;
        guard
            .start_shared_servers(vec![config("stub", &program)])
            .await
    };
    match &outcomes[0].outcome {
        Ok(tools) => assert_eq!(tools.len(), 1, "the stub advertises exactly one tool"),
        Err(err) => panic!("the fixture must speak MCP: {err}"),
    }
    assert_eq!(
        manager.lock().await.states().state("stub"),
        Some(McpServerState::Ready { tools: 1 })
    );
    manager.lock().await.shutdown().await;
}

/// A server that is not on disk at boot — the package still
/// installing, the mount not yet there — is unavailable, and the retry
/// loop picks it up the moment it appears. No `fq reload`, and its
/// tools reach the shared registry through the same rebuild a
/// `tools/list_changed` uses.
#[tokio::test]
async fn a_server_that_appears_later_is_picked_up_by_the_retry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let program = dir.path().join("late-server");
    let limits = McpLimits {
        startup_timeout: Duration::from_secs(5),
        retry_initial: Duration::from_millis(150),
        retry_max: Duration::from_millis(300),
        ..McpLimits::default()
    };
    let manager = manager(&dir.path().join("root"), limits);
    let declared = config("late", &program);

    // Boot: the command does not exist, so the server is unavailable
    // and boot carries on.
    let outcomes = {
        let mut guard = manager.lock().await;
        guard.start_shared_servers(vec![declared.clone()]).await
    };
    assert!(outcomes[0].outcome.is_err(), "the command is not on disk");
    assert!(matches!(
        manager.lock().await.states().state("late"),
        Some(McpServerState::Unavailable { .. })
    ));

    let (came_up_tx, mut came_up_rx) = tokio::sync::mpsc::unbounded_channel();
    let stop = Arc::new(tokio::sync::Notify::new());
    let retry = tokio::spawn(fq_runtime::mcp::retry_unavailable(
        Arc::clone(&manager),
        vec![declared],
        came_up_tx,
        Arc::clone(&stop),
    ));

    // The server arrives.
    write_script(&program, STUB_SERVER);

    let (name, _notifications) = tokio::time::timeout(Duration::from_secs(30), came_up_rx.recv())
        .await
        .expect("the retry loop must pick the server up")
        .expect("the retry loop announces it");
    assert_eq!(name, "late");
    assert_eq!(
        manager.lock().await.states().state("late"),
        Some(McpServerState::Ready { tools: 1 }),
        "the manager's own state is the answer every surface reads"
    );

    // And the tools are dispatchable: the rebuild the drain performs on
    // a late arrival is what puts them in front of the next invocation.
    let registry = manager
        .lock()
        .await
        .tool_refresher(fq_tools::builtin::ExecConfig::default())
        .rebuild_registry()
        .await;
    assert!(
        registry.get("late__ping").is_some(),
        "a server that came up on retry must be in the rebuilt registry"
    );

    stop.notify_one();
    tokio::time::timeout(Duration::from_secs(10), retry)
        .await
        .expect("the retry loop stops when told")
        .expect("retry task");
    manager.lock().await.shutdown().await;
}

/// A stdio line past `[mcp] max_line_bytes` is refused rather than
/// buffered: the read fails, the transport ends, and the server is
/// unavailable. Four megabytes with no newline against a cap of four
/// kilobytes — before the bound, that was four megabytes of daemon
/// memory per such server, and nothing in the protocol says stop.
///
/// What the bound *is* — where it counts from, that it resets on a
/// newline, that it is terminal — is asserted in `mcp::stdio::child`'s
/// unit tests; this one is about a real child on a real pipe.
#[tokio::test]
async fn a_stdio_line_past_the_cap_is_refused_not_buffered() {
    let dir = tempfile::tempdir().expect("tempdir");
    let program = dir.path().join("firehose-server");
    write_script(&program, FIREHOSE_SERVER);
    let limits = McpLimits {
        // Long enough that the deadline is not what ends this: the
        // refusal has to be the reader's.
        startup_timeout: Duration::from_secs(20),
        max_line_bytes: 4096,
        ..McpLimits::default()
    };
    let manager = manager(&dir.path().join("root"), limits);

    let started = std::time::Instant::now();
    let outcomes = {
        let mut guard = manager.lock().await;
        guard
            .start_shared_servers(vec![config("firehose", &program)])
            .await
    };
    let elapsed = started.elapsed();

    assert!(outcomes[0].outcome.is_err(), "the connection must be refused");
    assert!(
        elapsed < Duration::from_secs(15),
        "the refusal must come from the reader, not the deadline: {elapsed:?}"
    );
    assert!(matches!(
        manager.lock().await.states().state("firehose"),
        Some(McpServerState::Unavailable { .. })
    ));
    manager.lock().await.shutdown().await;
}
