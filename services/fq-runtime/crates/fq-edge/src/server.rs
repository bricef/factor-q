//! The edge server: TLS accept → token preamble → tarpc. Born
//! authenticated — there is no unauthenticated mode, so unlike the
//! legacy `ReadService` there is no loopback-only refusal: the bind
//! address is the operator's choice because every connection
//! authenticates.
//!
//! The connection preamble (beneath the RPC contract, per ADR-0031):
//! after the TLS handshake the client writes its token
//! (u32-length-prefixed base64 bytes); the server verifies signature
//! and principal, answers one status byte (0 = accepted), and only
//! then speaks tarpc. Per request, the resolved operation's required
//! authority is subset-checked against the connection token's grants.

use std::net::SocketAddr;
use std::sync::Arc;

use fq_ops::{Domain, OpCategory, OpId};
use futures::StreamExt;
use futures::future::BoxFuture;
use tarpc::server::{BaseChannel, Channel};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{Duration, timeout};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_util::codec::LengthDelimitedCodec;

use crate::auth::{EdgeIdentity, VerifiedToken, verify_token};
use crate::registry::EdgeRegistry;
use crate::service::Edge;
use crate::wire::{InvokeRequest, InvokeResponse, NextBatchRequest, StreamBatch, WireError};

/// Tokens are small; anything larger than this in the preamble is not
/// a token.
const MAX_TOKEN_BYTES: u32 = 64 * 1024;

/// A pre-auth client gets this long to complete the whole preamble —
/// TLS handshake, length prefix, token bytes. A few seconds is plenty
/// for a <=64 KiB token over local TLS; 10s is generous. Bounding it
/// stops a stalled anonymous connection from pinning a task + fd +
/// rustls session indefinitely (slowloris-style resource exhaustion):
/// `MAX_TOKEN_BYTES` caps space, this caps time.
pub const DEFAULT_PREAMBLE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
struct EdgeServer {
    registry: Arc<EdgeRegistry>,
    token: Arc<VerifiedToken>,
}

impl EdgeServer {
    fn authorize(&self, op: &OpId) -> Result<(), WireError> {
        let resolved = self
            .registry
            .registry()
            .resolve(op)
            .ok_or_else(|| WireError::NotRegistered { op: op.to_string() })?;
        if !self.token.allows(&resolved.authority) {
            return Err(WireError::Denied {
                op: op.to_string(),
                message: format!(
                    "token for `{}` lacks the required authority",
                    self.token.principal
                ),
            });
        }
        Ok(())
    }
}

impl Edge for EdgeServer {
    async fn invoke(
        self,
        _ctx: tarpc::context::Context,
        request: InvokeRequest,
    ) -> Result<InvokeResponse, WireError> {
        // The surface describing itself: List(Operation) is served
        // from the registry directly — the model's one
        // self-referential op.
        if request.op == OpId::List(Domain::Operation) {
            // Describe is readable by any authenticated caller whose
            // token grants Read on the operation domain or anything
            // (`"*"`); resolve() has no entry for it, so authorize
            // against its own derived shape.
            let required = [fq_ops::Authority {
                verb: fq_ops::Verb::Read,
                scope: Domain::Operation,
            }];
            if !self.token.allows(&required) {
                return Err(WireError::Denied {
                    op: request.op.to_string(),
                    message: format!(
                        "token for `{}` lacks the required authority",
                        self.token.principal
                    ),
                });
            }
            return Ok(InvokeResponse {
                output: self.registry.describe_value()?,
            });
        }

        self.authorize(&request.op)?;
        let resolved = self
            .registry
            .registry()
            .resolve(&request.op)
            .expect("authorized implies resolved");
        if resolved.category == OpCategory::Stream {
            return Err(WireError::InvalidInput {
                op: request.op.to_string(),
                message: "stream operations ride next_batch, not invoke".into(),
            });
        }
        let handler = self
            .registry
            .handler(&request.op.to_string())
            .ok_or_else(|| WireError::NotRegistered {
                op: request.op.to_string(),
            })?;
        let output = handler(request.input).await?;
        Ok(InvokeResponse { output })
    }

    async fn next_batch(
        self,
        _ctx: tarpc::context::Context,
        request: NextBatchRequest,
    ) -> Result<StreamBatch, WireError> {
        self.authorize(&request.op)?;
        let resolved = self
            .registry
            .registry()
            .resolve(&request.op)
            .expect("authorized implies resolved");
        if resolved.category != OpCategory::Stream {
            return Err(WireError::InvalidInput {
                op: request.op.to_string(),
                message: "next_batch carries stream operations only".into(),
            });
        }
        // Stream handlers arrive with the Phase-3 Turn exemplar.
        Err(WireError::NotRegistered {
            op: request.op.to_string(),
        })
    }
}

/// Bind the edge on `addr`, returning the bound address and the
/// serving future. Every connection must present a token minted under
/// `identity`'s root key. The preamble is bounded by
/// [`DEFAULT_PREAMBLE_TIMEOUT`]; use [`bind_with_timeout`] to choose.
pub async fn bind(
    addr: &str,
    identity: &EdgeIdentity,
    registry: Arc<EdgeRegistry>,
) -> anyhow::Result<(SocketAddr, BoxFuture<'static, ()>)> {
    bind_with_timeout(addr, identity, registry, DEFAULT_PREAMBLE_TIMEOUT).await
}

/// [`bind`], but with an explicit bound on the per-connection preamble
/// — the TLS handshake and token exchange each get `preamble_timeout`;
/// a client that stalls in any of them is dropped rather than left
/// pinning resources pre-auth.
pub async fn bind_with_timeout(
    addr: &str,
    identity: &EdgeIdentity,
    registry: Arc<EdgeRegistry>,
    preamble_timeout: Duration,
) -> anyhow::Result<(SocketAddr, BoxFuture<'static, ()>)> {
    let cert = CertificateDer::from(identity.cert_der.clone());
    let key = PrivateKeyDer::try_from(identity.key_der.clone())
        .map_err(|e| anyhow::anyhow!("edge key: {e}"))?;
    // Explicit provider: the workspace unions rustls features across
    // crates (reqwest pulls `ring`), and relying on the process
    // default panics the moment two providers are enabled.
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let tls_config = tokio_rustls::rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;
    let acceptor = TlsAcceptor::from(Arc::new(tls_config));
    let root_public = identity.public_key();

    let listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;

    let serving: BoxFuture<'static, ()> = Box::pin(async move {
        loop {
            let Ok((tcp, _peer)) = listener.accept().await else {
                continue;
            };
            let acceptor = acceptor.clone();
            let registry = registry.clone();
            let root_public = root_public;
            let preamble_timeout = preamble_timeout;
            tokio::spawn(async move {
                // Every pre-auth await is time-bounded: a client that
                // stalls the handshake or dribbles the token is dropped
                // rather than pinning a task + fd + rustls session.
                let Ok(Ok(mut tls)) = timeout(preamble_timeout, acceptor.accept(tcp)).await else {
                    return;
                };
                // Token preamble: length-prefixed base64 token bytes.
                let Ok(Ok(len)) = timeout(preamble_timeout, tls.read_u32()).await else {
                    return;
                };
                if len > MAX_TOKEN_BYTES {
                    let _ = tls.write_u8(1).await;
                    return;
                }
                let mut buf = vec![0u8; len as usize];
                let Ok(Ok(_)) = timeout(preamble_timeout, tls.read_exact(&mut buf)).await else {
                    return;
                };
                let presented = String::from_utf8_lossy(&buf).into_owned();
                let token = match verify_token(&presented, root_public) {
                    Ok(token) => token,
                    Err(_) => {
                        // Fail closed, but tell the client it was the
                        // token (they completed TLS, so they already
                        // know the server's identity).
                        let _ = tls.write_u8(1).await;
                        return;
                    }
                };
                if tls.write_u8(0).await.is_err() {
                    return;
                }

                let framed = tokio_util::codec::Framed::new(tls, LengthDelimitedCodec::new());
                let transport = tarpc::serde_transport::new(
                    framed,
                    tarpc::tokio_serde::formats::Json::default(),
                );
                let server = EdgeServer {
                    registry,
                    token: Arc::new(token),
                };
                BaseChannel::with_defaults(transport)
                    .execute(server.serve())
                    .for_each(|response| async move {
                        tokio::spawn(response);
                    })
                    .await;
            });
        }
    });
    Ok((local_addr, serving))
}
