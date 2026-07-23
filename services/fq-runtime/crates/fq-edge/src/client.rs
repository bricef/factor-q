//! The edge client: TLS with **fingerprint pinning** (the daemon's
//! certificate is self-signed, so chain validation is replaced by
//! exact identity matching — SSH's known_hosts model), then the token
//! preamble, then tarpc. TOFU is a policy at the config layer: obtain
//! the fingerprint out-of-band (the daemon prints it at first run) or
//! pin whatever the first connection presents; this client always
//! requires *a* fingerprint.

use std::sync::Arc;

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
}
