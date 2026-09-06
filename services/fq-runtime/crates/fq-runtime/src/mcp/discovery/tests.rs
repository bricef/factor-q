//! Fault injection against discovery (#548): the two caps and the
//! deadline, each against a server that actually misbehaves.
//!
//! The in-process mock is the right fixture here rather than a child
//! process — `@modelcontextprotocol/server-everything` neither
//! paginates nor spins, and the faults being injected are about what
//! the *server* chooses to answer.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use super::*;
use crate::mcp::mock::{mock_tool, serve_mock, serve_mock_endless_cursor};

fn tools(n: usize) -> Arc<Mutex<Vec<rmcp::model::Tool>>> {
    Arc::new(Mutex::new(
        (0..n).map(|i| mock_tool(&format!("t{i}"))).collect(),
    ))
}

/// Discovery that must fail, rendered. `Vec<Arc<dyn Tool>>` is not
/// `Debug`, so `expect_err` cannot be used on the result directly.
async fn discovery_failure(client: &Arc<McpClient>, server: &str, limits: &McpLimits) -> String {
    match discover_tools(client, server, &ProgressRegistry::default(), limits).await {
        Err(err) => err.to_string(),
        Ok((tools, _)) => panic!(
            "{server}: discovery must have been refused, got {} tools",
            tools.len()
        ),
    }
}

/// A server that answers `tools/list` with a `next_cursor` for ever is
/// cut off at the page cap, and the error names the cap and the key
/// that sets it. Before the cap this loop ran until the process died,
/// with the tool vector growing every page — and `tools/list_changed`
/// could restart it at will (finding B3).
#[tokio::test]
async fn an_endless_cursor_is_cut_off_at_the_page_cap() {
    let client = serve_mock_endless_cursor(tools(4), 2).await;
    let limits = McpLimits {
        max_discovery_pages: 5,
        ..McpLimits::default()
    };
    let message = discovery_failure(&client, "spinner", &limits).await;
    assert!(message.contains("5 pages"), "{message}");
    assert!(message.contains("max_discovery_pages"), "{message}");
}

/// A server declaring more tools than the cap is refused rather than
/// registered: every tool it lists becomes a schema in every prompt the
/// agents that use it send, so the cost of a runaway list is not just
/// memory.
#[tokio::test]
async fn more_tools_than_the_cap_is_refused_by_name() {
    let client = serve_mock(tools(40), 8).await;
    let limits = McpLimits {
        max_tools: 10,
        ..McpLimits::default()
    };
    let message = discovery_failure(&client, "prolific", &limits).await;
    assert!(message.contains("max_tools"), "{message}");
}

/// The cap counts across pages, not per page: a server that stays under
/// it on every page but past it in total is still refused.
#[tokio::test]
async fn the_tool_cap_counts_across_pages() {
    let client = serve_mock(tools(12), 3).await;
    let limits = McpLimits {
        max_tools: 10,
        ..McpLimits::default()
    };
    let message = discovery_failure(&client, "prolific", &limits).await;
    assert!(message.contains("page 4"), "{message}");
}

/// A well-behaved paginating server is unaffected by any of it: the
/// caps are bounds on a fault, not a change to the protocol.
#[tokio::test]
async fn a_paginating_server_within_the_caps_discovers_everything() {
    let client = serve_mock(tools(25), 4).await;
    let (discovered, names) = discover_tools(
        &client,
        "polite",
        &ProgressRegistry::default(),
        &McpLimits::default(),
    )
    .await
    .expect("within every cap");
    assert_eq!(discovered.len(), 25);
    assert_eq!(names.len(), 25);
    assert_eq!(names[0], "polite__t0");
}

/// Exactly the cap is allowed, so the documented number is a tool count
/// a server may actually advertise.
#[tokio::test]
async fn exactly_the_tool_cap_is_allowed() {
    let client = serve_mock(tools(10), 10).await;
    let limits = McpLimits {
        max_tools: 10,
        ..McpLimits::default()
    };
    let (discovered, _) = discover_tools(&client, "polite", &ProgressRegistry::default(), &limits)
        .await
        .expect("at the cap");
    assert_eq!(discovered.len(), 10);
}

/// The deadline covers the whole walk, not one request: a server that
/// answers each page promptly but has unboundedly many of them still
/// ends. Injected here as a cursor that never ends with a page cap high
/// enough that the deadline is what stops it.
#[tokio::test]
async fn the_discovery_deadline_bounds_the_whole_walk() {
    let client = serve_mock_endless_cursor(tools(2), 1).await;
    let limits = McpLimits {
        discovery_timeout: Duration::from_millis(150),
        max_discovery_pages: u32::MAX,
        ..McpLimits::default()
    };
    let started = std::time::Instant::now();
    let message = discovery_failure(&client, "spinner", &limits).await;
    assert!(message.contains("discovery_timeout_secs"), "{message}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the deadline must end the walk promptly, took {:?}",
        started.elapsed()
    );
}
