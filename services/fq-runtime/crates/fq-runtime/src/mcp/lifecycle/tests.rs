//! Fault injection against the boot path (#548).
//!
//! The stub is `sleep`: a process that accepts its stdin and never
//! writes a byte, which is exactly the shape of the remote server that
//! froze `fqd` at startup — the connection is fine, the peer simply
//! never answers `initialize`. It needs no fixture and no network, and
//! it is the same stdio shape #597's tests used.

use std::time::{Duration, Instant};

use super::*;
use crate::mcp::McpClientManager;

/// A server that will never answer: `sleep` holds its stdin open for
/// far longer than any deadline under test.
fn hung(name: &str) -> McpServerConfig {
    McpServerConfig {
        name: name.to_string(),
        command: "sleep".to_string(),
        args: vec!["120".to_string()],
        env: Vec::new(),
        url: None,
    }
}

fn quick_startup(startup: Duration) -> McpLimits {
    McpLimits {
        startup_timeout: startup,
        // Retrying is a separate test; a boot test that also retried
        // would be measuring two things at once.
        retry_initial: Duration::ZERO,
        ..McpLimits::default()
    }
}

fn manager(limits: McpLimits) -> (McpClientManager, tempfile::TempDir) {
    let root = tempfile::tempdir().expect("tempdir");
    (
        McpClientManager::with_server_root(root.path().to_path_buf()).with_limits(limits),
        root,
    )
}

/// The finding, directly: a server that accepts and never answers
/// `initialize` leaves the daemon *booted*, with that server
/// unavailable and its reason recorded, within one start-up deadline.
#[tokio::test]
async fn a_server_that_never_answers_initialize_is_unavailable_not_fatal() {
    let (mut manager, _root) = manager(quick_startup(Duration::from_millis(300)));
    let started = Instant::now();
    let outcomes = manager
        .start_shared_servers(vec![hung("wedged")])
        .await;
    let elapsed = started.elapsed();

    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].outcome.is_err(), "the start must not succeed");
    assert!(
        elapsed < Duration::from_secs(5),
        "boot must end within one deadline, took {elapsed:?}"
    );
    match manager.states().state("wedged") {
        Some(McpServerState::Unavailable { reason, .. }) => {
            assert!(reason.contains("startup_timeout_secs"), "{reason}");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
    manager.shutdown().await;
}

/// Boot is bounded by the slowest server, not by the sum of them: two
/// stubs that each cost one deadline finish in about one deadline
/// between them. This is the property the sequential loop did not have,
/// and the one that keeps a directory of agents from multiplying a
/// single hang by the number of servers declared.
#[tokio::test]
async fn two_hung_servers_cost_one_deadline_between_them() {
    let deadline = Duration::from_millis(700);
    let (mut manager, _root) = manager(quick_startup(deadline));
    let started = Instant::now();
    let outcomes = manager
        .start_shared_servers(vec![hung("first"), hung("second")])
        .await;
    let elapsed = started.elapsed();

    assert_eq!(outcomes.len(), 2);
    assert!(outcomes.iter().all(|o| o.outcome.is_err()));
    assert!(
        elapsed < deadline * 2,
        "the two starts must overlap: {elapsed:?} is not under {:?}",
        deadline * 2
    );
    assert!(
        elapsed >= deadline,
        "sanity: both must actually have waited on the deadline, {elapsed:?}"
    );
    manager.shutdown().await;
}

/// A server declaring no transport at all cannot be started and cannot
/// be deduplicated either; it is refused by name rather than silently
/// sharing a bucket with every other such config.
#[tokio::test]
async fn a_server_with_no_transport_is_refused_by_name() {
    let (mut manager, _root) = manager(quick_startup(Duration::from_millis(200)));
    let outcomes = manager
        .start_shared_servers(vec![McpServerConfig {
            name: "nowhere".to_string(),
            command: String::new(),
            args: Vec::new(),
            env: Vec::new(),
            url: None,
        }])
        .await;
    assert_eq!(outcomes[0].server, "nowhere");
    assert!(matches!(
        outcomes[0].outcome,
        Err(McpError::UndeclaredTransport { .. })
    ));
}

/// Two agents naming one transport still dial it once, and the
/// concurrent start cannot race them into two connections: the
/// deduplication happens before any handshake begins.
#[tokio::test]
async fn one_transport_declared_twice_is_dialled_once() {
    let (mut manager, _root) = manager(quick_startup(Duration::from_millis(300)));
    let outcomes = manager
        .start_shared_servers(vec![hung("first"), hung("second")])
        .await;
    // `hung` builds the same command and args for both names, so the
    // second is the duplicate: an empty `Ok`, and never dialled.
    let empty = outcomes
        .iter()
        .filter(|o| matches!(&o.outcome, Ok(tools) if tools.is_empty()))
        .count();
    assert_eq!(empty, 1, "exactly one declaration must be the duplicate");
    assert!(
        manager.states().state("second").is_none()
            || manager.states().state("first").is_none(),
        "the deduplicated declaration is never dialled, so it has no state"
    );
    manager.shutdown().await;
}

/// The backoff doubles from the configured start and saturates at the
/// configured ceiling; a zero start disables retrying, which is the one
/// way "unavailable until restart" can be asked for.
#[test]
fn the_retry_backoff_doubles_and_saturates() {
    let limits = McpLimits {
        retry_initial: Duration::from_secs(30),
        retry_max: Duration::from_secs(600),
        ..McpLimits::default()
    };
    assert_eq!(limits.retry_backoff(1), Some(Duration::from_secs(30)));
    assert_eq!(limits.retry_backoff(2), Some(Duration::from_secs(60)));
    assert_eq!(limits.retry_backoff(5), Some(Duration::from_secs(480)));
    assert_eq!(limits.retry_backoff(6), Some(Duration::from_secs(600)));
    assert_eq!(
        limits.retry_backoff(4000),
        Some(Duration::from_secs(600)),
        "a long outage must not overflow the doubling"
    );
    assert_eq!(
        McpLimits {
            retry_initial: Duration::ZERO,
            ..limits
        }
        .retry_backoff(1),
        None
    );
}

/// The state table is what every surface reads, so its transitions are
/// asserted directly: a name this daemon never declared has no state at
/// all, which is how "no shared servers" is told apart from "all fine".
#[test]
fn the_state_table_records_what_a_health_surface_needs() {
    let states = McpServerStates::default();
    assert!(states.is_empty());
    assert_eq!(states.state("absent"), None);

    states.starting("one");
    assert_eq!(states.state("one"), Some(McpServerState::Starting));
    states.ready("one", 7);
    assert_eq!(states.state("one"), Some(McpServerState::Ready { tools: 7 }));

    states.unavailable("two", "no initialize response".to_string(), 3, Some(42));
    assert_eq!(
        states.state("two"),
        Some(McpServerState::Unavailable {
            reason: "no initialize response".to_string(),
            attempts: 3,
            next_retry_at_ms: Some(42),
        })
    );
    assert_eq!(
        states.snapshot().iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        vec!["one", "two"],
        "the snapshot is ordered, so a report reads the same way twice"
    );
}
