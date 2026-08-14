//! The edge client: TLS with **fingerprint pinning** (the daemon's
//! certificate is self-signed, so chain validation is replaced by
//! exact identity matching — SSH's known_hosts model), then the token
//! preamble, then tarpc. TOFU is a policy at the config layer: obtain
//! the fingerprint out-of-band (the daemon prints it at first run) or
//! pin whatever the first connection presents; this client always
//! requires *a* fingerprint.
//!
//! Above the transport sit the two calls the edge carries —
//! [`EdgeClient::invoke`] and [`EdgeClient::next_batch`] — so every
//! consumer of the edge shares one implementation of the envelope,
//! the watermark argument, and the long-poll deadline. They live here
//! rather than in a caller because there is more than one caller: the
//! `fq` CLI and the operator dashboard both speak this surface, and a
//! deadline bug fixed in one of them would otherwise stay live in the
//! other.
//!
//! Credentials are **not** here. Which pairing a client presents is a
//! policy of the program holding it — the CLI reads the operator's
//! `connections.toml`, a service takes its token from its
//! environment — and the two must not share a store, since the whole
//! point of a service's token is that it is narrower than the
//! operator's.

use std::sync::Arc;

use anyhow::Context as _;
use fq_ops::OpId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{self, DigitallySignedStruct, SignatureScheme};
use tokio_util::codec::LengthDelimitedCodec;

use crate::auth::fingerprint;
use crate::service::EdgeClient as TarpcEdgeClient;
use crate::wire::{InvokeRequest, NextBatchRequest, StreamBatch, WireError};

/// Why a connection attempt failed — each case distinct and tested.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("connect: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "server certificate fingerprint mismatch — possible interception, or the daemon's \
         certificate changed and must be re-pinned"
    )]
    FingerprintMismatch,
    #[error("token rejected by the daemon")]
    TokenRejected,
}

/// Pin verifier: accept exactly the certificate whose SHA-256
/// matches — and still verify the TLS handshake signatures against
/// that certificate's key, because the pin only proves the peer
/// *presented* our certificate; the signature check proves it *holds
/// the private key* (without it, a replayed certificate would
/// suffice to impersonate the daemon).
#[derive(Debug)]
struct PinnedCert {
    expected: [u8; 32],
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if fingerprint(end_entity.as_ref()) == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("pinned fingerprint mismatch".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Capture verifier for the TOFU probe: accept whatever certificate
/// the server presents, record its fingerprint. Handshake signatures
/// are still verified against the presented certificate, so the probe
/// proves the peer *holds the private key* for the fingerprint it
/// reports — a replayed certificate can't answer the probe.
#[derive(Debug)]
struct CaptureCert {
    seen: Arc<std::sync::Mutex<Option<[u8; 32]>>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for CaptureCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        *self.seen.lock().expect("fingerprint capture lock") =
            Some(fingerprint(end_entity.as_ref()));
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// The TOFU primitive: fetch the certificate fingerprint the server
/// at `addr` presents, without requiring a pin. The handshake
/// completes (proving the peer holds the matching private key) and
/// the connection is dropped before the token preamble — nothing
/// secret is sent. The caller shows the fingerprint to the operator
/// (or pins it non-interactively) and then connects properly via
/// [`EdgeClient::connect`], which always requires the pin.
pub async fn probe_fingerprint(addr: &str) -> Result<[u8; 32], ConnectError> {
    let tcp = TcpStream::connect(addr).await?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let seen = Arc::new(std::sync::Mutex::new(None));
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| ConnectError::Io(std::io::Error::other(e)))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(CaptureCert {
            seen: seen.clone(),
            provider,
        }))
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));
    let server_name = ServerName::try_from("fqd".to_string())
        .map_err(|e| ConnectError::Io(std::io::Error::other(e)))?;
    let tls = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| ConnectError::Io(std::io::Error::other(e)))?;
    drop(tls);
    let captured = seen.lock().expect("fingerprint capture lock").take();
    captured.ok_or_else(|| {
        ConnectError::Io(std::io::Error::other(
            "handshake completed without presenting a certificate",
        ))
    })
}

/// A connected, authenticated edge client.
#[derive(Debug)]
pub struct EdgeClient {
    pub rpc: TarpcEdgeClient,
}

impl EdgeClient {
    /// Connect to the edge at `addr`, requiring the server certificate
    /// to match `pinned_fingerprint` and presenting `token` in the
    /// connection preamble.
    pub async fn connect(
        addr: &str,
        pinned_fingerprint: [u8; 32],
        token: &str,
    ) -> Result<Self, ConnectError> {
        let tcp = TcpStream::connect(addr).await?;

        // Explicit provider — see the server-side note: process-default
        // resolution breaks under workspace feature unions.
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|_| ConnectError::FingerprintMismatch)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedCert {
                expected: pinned_fingerprint,
                provider,
            }))
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(config));
        let server_name = ServerName::try_from("fqd".to_string())
            .map_err(|_| ConnectError::FingerprintMismatch)?;
        let mut tls = connector
            .connect(server_name, tcp)
            .await
            .map_err(|_| ConnectError::FingerprintMismatch)?;

        // Token preamble.
        let bytes = token.as_bytes();
        tls.write_u32(bytes.len() as u32).await?;
        tls.write_all(bytes).await?;
        tls.flush().await?;
        let status = tls
            .read_u8()
            .await
            .map_err(|_| ConnectError::TokenRejected)?;
        if status != 0 {
            return Err(ConnectError::TokenRejected);
        }

        let framed = tokio_util::codec::Framed::new(tls, LengthDelimitedCodec::new());
        let transport =
            tarpc::serde_transport::new(framed, tarpc::tokio_serde::formats::Json::default());
        let rpc = TarpcEdgeClient::new(tarpc::client::Config::default(), transport).spawn();
        Ok(EdgeClient { rpc })
    }

    /// One authenticated call: the outer error is transport, the inner
    /// is the operation's own verdict — callers that care (a show's
    /// not-found path) match it, everyone else surfaces it.
    ///
    /// Deliberately at the **envelope level**: `(OpId, Value)` in,
    /// `Value` out. Dispatch is generic and the surface describes
    /// itself, so this signature does not change when an operation is
    /// added — which is the registry design paying off. A client with
    /// a method per operation would track the surface forever, and
    /// every field addition would become an edit in two places.
    /// Callers deserialise into the contract types they need.
    pub async fn invoke(
        &self,
        op: OpId,
        input: serde_json::Value,
    ) -> anyhow::Result<Result<serde_json::Value, WireError>> {
        self.invoke_gated(op, input, None).await
    }

    /// [`invoke`](Self::invoke), watermarked: `min_seq` holds the
    /// answer until this daemon's fold has applied at least that
    /// sequence. It is the read half of read-your-writes — the number
    /// comes from a command's receipt (D4) — and it is a read-only
    /// argument: the edge refuses a command that carries one.
    pub async fn invoke_gated(
        &self,
        op: OpId,
        input: serde_json::Value,
        min_seq: Option<u64>,
    ) -> anyhow::Result<Result<serde_json::Value, WireError>> {
        let response = self
            .rpc
            .invoke(
                tarpc::context::current(),
                InvokeRequest {
                    op,
                    version: 1,
                    input,
                    min_seq,
                },
            )
            .await
            .context("edge rpc failed")?;
        Ok(response.map(|r| r.output))
    }

    /// One long-poll batch from a Stream operation. `from_seq =
    /// u64::MAX` seeks the tail without consuming anything — the seam
    /// a tail starts from, and the same cursor it resumes at, so a
    /// reconnecting tail picks up exactly where it stopped rather than
    /// wherever the broker happens to be.
    pub async fn next_batch(
        &self,
        op: OpId,
        filter: serde_json::Value,
        from_seq: u64,
        max_wait_ms: u64,
    ) -> anyhow::Result<Result<StreamBatch, WireError>> {
        self.rpc
            .next_batch(
                long_poll_context(max_wait_ms),
                NextBatchRequest {
                    op,
                    version: 1,
                    filter,
                    from_seq,
                    max_wait_ms,
                },
            )
            .await
            .context("edge rpc failed")
    }
}

/// Slack on a long poll's deadline: how much longer the caller waits
/// than the window it asked the daemon to hold. Covers the round trip
/// and the daemon's own scheduling under load — generous, because the
/// deadline is a backstop against a hung daemon, not the thing that
/// ends a poll.
const LONG_POLL_DEADLINE_SLACK: std::time::Duration = std::time::Duration::from_secs(10);

/// The RPC context for a long poll: patient enough for the wait it is
/// asking for.
///
/// tarpc's default deadline is a flat ten seconds, which is **shorter
/// than the window these calls ask the daemon to hold** (30s). A poll
/// that legitimately waits out its window is then abandoned by the
/// very client that asked for it, and the call dies with `edge rpc
/// failed: DeadlineExceeded`.
///
/// That this was not obvious is worth recording: `event.stream` reads
/// the whole log, and the daemon heartbeats every ten seconds — so an
/// idle tail's poll was ended by a heartbeat in a photo finish with
/// the deadline, and lost the race only under load. `turn.stream` has
/// no such cover: it is filtered to one agent's subject, so following
/// a quiet invocation loses every time.
fn long_poll_context(max_wait_ms: u64) -> tarpc::context::Context {
    let mut ctx = tarpc::context::current();
    ctx.deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(max_wait_ms)
        + LONG_POLL_DEADLINE_SLACK;
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invariant every long-polling caller depends on, pinned
    /// where it cannot cost wall-clock time to check: a caller must be
    /// more patient than the wait it asks for. tarpc's default is a
    /// flat ten seconds, so this is the one thing that stops a
    /// 30-second poll from being abandoned at ten.
    #[test]
    fn a_long_poll_outlasts_the_wait_it_asks_for() {
        for max_wait_ms in [0, 30_000, 60_000] {
            let ctx = long_poll_context(max_wait_ms);
            let asked = std::time::Instant::now() + std::time::Duration::from_millis(max_wait_ms);
            assert!(
                ctx.deadline > asked,
                "a {max_wait_ms}ms poll must not be abandoned before it is answered"
            );
        }
        // And the default it replaces would not have been: this is
        // the regression, stated.
        assert!(
            tarpc::context::current().deadline
                < std::time::Instant::now() + std::time::Duration::from_millis(30_000),
            "tarpc's default deadline is shorter than a 30s poll — the bug this guards"
        );
    }
}
