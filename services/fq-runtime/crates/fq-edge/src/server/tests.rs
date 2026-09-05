//! The accept loop's error branch, which no integration test can
//! reach.
//!
//! `EMFILE` is the condition that matters — tokio does not clear a
//! listener's readiness on it, so the old `continue` turned fd
//! exhaustion into a busy loop — and it cannot be injected in-process:
//! the rlimit is per-process, so lowering it starves the test harness
//! and every other socket in the same binary before it reaches the
//! listener. A fake [`AcceptSource`] injects the error directly
//! instead, and the assertion is the one that matters: the loop yields
//! between attempts rather than spinning.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

/// An accept source that only ever fails, counting how often it was
/// asked. `EMFILE` by name because it is the error the branch exists
/// for.
struct AlwaysEmfile {
    calls: Arc<AtomicUsize>,
}

impl AcceptSource for AlwaysEmfile {
    fn accept(&self) -> BoxFuture<'_, std::io::Result<(TcpStream, SocketAddr)>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Box::pin(async {
            Err(std::io::Error::from_raw_os_error(
                libc_emfile_errno_for_tests(),
            ))
        })
    }
}

/// `EMFILE` — spelled out rather than pulled from `libc`, which this
/// crate does not otherwise depend on. The loop treats every accept
/// error the same way; the number is here so the test says which
/// condition it is standing in for.
fn libc_emfile_errno_for_tests() -> i32 {
    24
}

fn context_for_tests() -> Arc<ConnectionContext> {
    let identity = EdgeIdentity::provision().expect("provision identity");
    let registry = Arc::new(EdgeRegistry::new());
    Arc::new(ConnectionContext::new(&identity, registry).expect("connection context"))
}

/// A listener that only errors must not become a busy loop.
///
/// Real time, not tokio's paused clock: a paused clock auto-advances
/// through the sleep, which is exactly the property under test. With a
/// 100 ms backoff over a 500 ms window the loop can make at most a
/// handful of attempts; without the sleep it makes tens of thousands.
#[tokio::test]
async fn an_accept_error_pauses_the_loop_instead_of_spinning() {
    let calls = Arc::new(AtomicUsize::new(0));
    let source: Arc<dyn AcceptSource> = Arc::new(AlwaysEmfile {
        calls: calls.clone(),
    });
    let limits = EdgeLimits {
        accept_error_backoff: std::time::Duration::from_millis(100),
        ..EdgeLimits::default()
    };

    // The loop never returns; bound it by wall clock and read the
    // counter.
    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        accept_loop(source, context_for_tests(), limits),
    )
    .await;

    let attempts = calls.load(Ordering::Relaxed);
    assert!(
        attempts > 0,
        "the loop never called accept — the test proves nothing"
    );
    assert!(
        attempts <= 12,
        "the accept loop spun on error: {attempts} attempts in 500ms at a 100ms \
         backoff (a spinning loop makes thousands)"
    );
}

/// A failed accept must not leak its connection permit: after N
/// errors the loop still has its full budget, so an edge that
/// survives an fd squeeze can still serve.
#[tokio::test]
async fn accept_errors_do_not_consume_the_connection_budget() {
    let calls = Arc::new(AtomicUsize::new(0));
    let source: Arc<dyn AcceptSource> = Arc::new(AlwaysEmfile {
        calls: calls.clone(),
    });
    // One connection's worth of budget: if the error branch kept its
    // permit, the second attempt could never happen.
    let limits = EdgeLimits {
        max_connections: 1,
        accept_error_backoff: std::time::Duration::from_millis(20),
        ..EdgeLimits::default()
    };
    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        accept_loop(source, context_for_tests(), limits),
    )
    .await;
    assert!(
        calls.load(Ordering::Relaxed) > 1,
        "the loop stopped after one error — the connection permit was leaked"
    );
}
