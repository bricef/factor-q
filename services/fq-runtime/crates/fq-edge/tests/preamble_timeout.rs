//! The connection preamble is time-bounded: a client that opens a
//! TCP connection and then stalls — never beginning the TLS handshake
//! — is dropped by the server once the preamble timeout elapses,
//! rather than pinning a task + fd + rustls session pre-auth
//! (slowloris-style resource exhaustion).

use fq_edge::EdgeIdentity;
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn stalled_connection_is_dropped_after_the_preamble_timeout() {
    let identity = EdgeIdentity::provision().unwrap();
    let registry = std::sync::Arc::new(fq_edge::EdgeRegistry::new());
    let (addr, serving) = fq_edge::server::bind_with_timeout(
        "127.0.0.1:0",
        &identity,
        registry,
        std::time::Duration::from_millis(200),
    )
    .await
    .unwrap();
    tokio::spawn(serving);

    // Connect but send NOTHING — never begin the TLS handshake, so the
    // server's `acceptor.accept` stalls waiting for a ClientHello.
    let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Without the timeout the server blocks forever, this read never
    // returns, the outer 5s guard elapses and `.expect` panics.
    let mut buf = [0u8; 1];
    let n = tokio::time::timeout(std::time::Duration::from_secs(5), tcp.read(&mut buf))
        .await
        .expect(
            "server must close the stalled connection well within 5s; it did not, \
             so there is no preamble timeout",
        )
        .unwrap();
    assert_eq!(
        n, 0,
        "server should close the connection (EOF) after the preamble timeout"
    );
}
