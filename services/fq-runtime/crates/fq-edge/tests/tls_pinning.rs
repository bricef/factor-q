//! The pin proves *identity*, not merely *appearance*: an on-path
//! attacker can replay the daemon's self-signed certificate (it is
//! public — sent in every handshake), so the fingerprint match alone is
//! not enough. The handshake signature is the server's proof it holds
//! the pinned key. This test stands up a rogue server that presents the
//! pinned certificate but signs with a *different* key, and asserts the
//! client refuses it — the key-possession proof that closes the MITM.

use std::sync::Arc;

use fq_edge::{EdgeClient, EdgeIdentity};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::server::{ClientHello, ResolvesServerCert};
use tokio_rustls::rustls::sign::CertifiedKey;

/// Serve a fixed certificate + signing key with no consistency check.
/// `with_single_cert` cross-checks key against cert and would reject the
/// mismatch (rustls 0.23), so the MITM is assembled through a resolver,
/// which does not: the pinned certificate in front of a key the attacker
/// actually owns.
#[derive(Debug)]
struct MismatchedResolver(Arc<CertifiedKey>);

impl ResolvesServerCert for MismatchedResolver {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
}

#[tokio::test]
async fn rogue_key_behind_the_pinned_cert_is_refused() {
    // Two identities: `real` is the one the client pins; `rogue` owns
    // the key the attacker actually controls.
    let real = EdgeIdentity::provision().unwrap();
    let rogue = EdgeIdentity::provision().unwrap();

    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());

    // The MITM: present the pinned certificate, but hold the *rogue*
    // signing key. Assemble the CertifiedKey by hand — `with_single_cert`
    // refuses a key that does not match the cert, a resolver does not.
    let rogue_key = provider
        .key_provider
        .load_private_key(PrivateKeyDer::try_from(rogue.key_der.clone()).unwrap())
        .unwrap();
    let certified = Arc::new(CertifiedKey::new(
        vec![CertificateDer::from(real.cert_der.clone())],
        rogue_key,
    ));

    let server_config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(MismatchedResolver(certified)));
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Accept exactly one connection. If the handshake completes (which
    // it does *without* the fix), read the length-prefixed token and
    // answer the accepted status byte — demonstrably harvesting the
    // bearer token the client hands over.
    tokio::spawn(async move {
        let Ok((tcp, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut tls) = acceptor.accept(tcp).await else {
            return;
        };
        let Ok(len) = tls.read_u32().await else {
            return;
        };
        let mut buf = vec![0u8; len as usize];
        if tls.read_exact(&mut buf).await.is_ok() {
            let _ = tls.write_u8(0).await;
        }
    });

    let token = real.mint_admin_token().unwrap();
    let res = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        EdgeClient::connect(&addr.to_string(), real.fingerprint(), &token),
    )
    .await
    .expect("connect must not hang");

    // connect() maps any TLS handshake failure to FingerprintMismatch,
    // so the bad-signature handshake surfaces there. Without the fix the
    // handshake succeeds and `res` is Ok(_) — the token was harvested —
    // and this assertion fails, which is the proof.
    assert!(
        matches!(res, Err(fq_edge::client::ConnectError::FingerprintMismatch)),
        "a server signing with a key it does not own must be refused: {res:?}"
    );
}
