//! The edge's connection budget is finite and enforced.
//!
//! An unauthenticated peer can make the daemon allocate an fd, a task
//! and a rustls session simply by connecting. Without a cap, opening
//! sockets is a denial of service that costs the attacker nothing, and
//! the failure lands as `EMFILE` in the accept loop rather than as a
//! refusal to the peer that caused it. These tests hold the budget
//! open with stalled connections and check two things: that the cap
//! actually stops the edge taking more, and that it *recovers* —
//! saturation must be a delay, never a wedge.

use std::time::{Duration, Instant};

use fq_edge::{EdgeIdentity, EdgeLimits};
use tokio::io::AsyncReadExt;

/// The preamble timeout doubles as the eviction clock for a stalled
/// pre-auth connection, so it also bounds how long saturation lasts.
const PREAMBLE: Duration = Duration::from_millis(400);

async fn spawn_capped_edge(limits: EdgeLimits) -> std::net::SocketAddr {
    let identity = EdgeIdentity::provision().unwrap();
    let registry = std::sync::Arc::new(fq_edge::EdgeRegistry::new());
    let (addr, serving) = fq_edge::bind_with_limits("127.0.0.1:0", &identity, registry, limits)
        .await
        .unwrap();
    tokio::spawn(serving);
    addr
}

/// Saturate the connection budget with peers that connect and say
/// nothing, then prove a further peer is not served until a slot frees.
///
/// The assertion is a *lower* bound on how long the extra peer waits,
/// which is what makes it robust: it can only fail if the cap is not
/// enforced. The upper bound is generous and only guards against the
/// edge wedging.
#[tokio::test]
async fn connections_past_the_cap_wait_for_a_slot_rather_than_being_served() {
    let addr = spawn_capped_edge(EdgeLimits {
        preamble_timeout: PREAMBLE,
        max_connections: 2,
        ..EdgeLimits::default()
    })
    .await;

    // Two stalled peers: accepted, holding both permits, evicted only
    // when the preamble timeout fires.
    let mut stalled = Vec::new();
    for _ in 0..2 {
        stalled.push(tokio::net::TcpStream::connect(addr).await.unwrap());
    }
    // Give the accept loop a moment to take both off the backlog.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // A third peer. Its TCP connect completes into the kernel backlog,
    // but the server cannot accept it until a permit frees, so its
    // first byte (the server's EOF when its own preamble expires)
    // cannot arrive before the first stalled peer is evicted.
    let started = Instant::now();
    let mut third = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(10), third.read(&mut buf))
        .await
        .expect("the edge wedged: a queued connection was never served or closed");
    let waited = started.elapsed();
    assert_eq!(read.unwrap(), 0, "expected the server to close the peer");
    assert!(
        waited >= PREAMBLE / 2,
        "the third connection was handled in {waited:?}, before either of the two \
         permitted ones could have been evicted — max_connections is not enforced"
    );

    drop(stalled);
}

/// The cap must not be a one-way door: once the stalled peers are
/// evicted, an ordinary client authenticates and gets an answer.
#[tokio::test]
async fn the_edge_still_serves_after_its_budget_has_been_saturated() {
    let identity = EdgeIdentity::provision().unwrap();
    let admin = identity.mint_admin_token().unwrap();
    let fingerprint = identity.fingerprint();
    let registry = std::sync::Arc::new(fq_edge::testing::MockDomain::seeded().registry());
    let (addr, serving) = fq_edge::bind_with_limits(
        "127.0.0.1:0",
        &identity,
        registry,
        EdgeLimits {
            preamble_timeout: PREAMBLE,
            max_connections: 2,
            max_pre_auth_connections: 1,
            ..EdgeLimits::default()
        },
    )
    .await
    .unwrap();
    tokio::spawn(serving);

    // Saturate, then let the eviction clock run.
    let stalled: Vec<_> = futures::future::join_all(
        (0..2).map(|_| async move { tokio::net::TcpStream::connect(addr).await.unwrap() }),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(stalled);
    tokio::time::sleep(PREAMBLE + Duration::from_millis(200)).await;

    let client = tokio::time::timeout(
        Duration::from_secs(10),
        fq_edge::EdgeClient::connect(&addr.to_string(), fingerprint, &admin),
    )
    .await
    .expect("connecting after saturation timed out — the edge did not recover")
    .expect("connect after saturation");
    let described = client
        .invoke(
            fq_ops::OpId::List(fq_ops::Domain::Operation),
            serde_json::json!({}),
        )
        .await
        .expect("describe after saturation")
        .expect("describe is allowed for the admin token");
    assert!(
        described.is_object() || described.is_array(),
        "the edge answered, but not with a description: {described}"
    );
}
