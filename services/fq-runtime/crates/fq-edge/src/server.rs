//! The edge server: TLS accept → token preamble → tarpc. Born
//! authenticated — there is no unauthenticated mode, and that is what
//! buys the bind address back: there is no loopback-only refusal here
//! because every connection proves who it is, where an unauthenticated
//! surface can only be made safe by being unreachable.
//!
//! The connection preamble (beneath the RPC contract, per ADR-0031):
//! after the TLS handshake the client writes its token
//! (u32-length-prefixed base64 bytes); the server verifies signature
//! and principal, answers one status byte (0 = accepted), and only
//! then speaks tarpc. Per request, the resolved operation's required
//! authority is subset-checked against the connection token's grants.
//!
//! **Binding and serving are two steps on purpose.** [`EdgeListener`]
//! holds a bound TCP socket and nothing else — no identity, no
//! registry, no task. The daemon takes that socket before it registers
//! a worker or publishes anything, so the bind doubles as its
//! single-instance lock: a second daemon on the same address loses at
//! `bind(2)`, having caused no side effect. [`EdgeListener::serve`]
//! then attaches the identity and the registry once the runtime behind
//! them is assembled.

use std::net::SocketAddr;
use std::sync::Arc;

use fq_ops::{Domain, OpCategory, OpId};
use futures::StreamExt;
use futures::future::BoxFuture;
use tarpc::server::{BaseChannel, Channel};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
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

/// What the edge will spend on connections it has not authenticated,
/// and on requests from ones it has.
///
/// Every field is a bound on a resource an unauthenticated peer can
/// make the daemon allocate: an fd and a task per connection, a rustls
/// session per handshake, an in-flight request per call. Without them
/// the accept loop is an amplifier — which is what it was, and why
/// this struct exists.
#[derive(Debug, Clone, Copy)]
pub struct EdgeLimits {
    /// How long a peer gets to finish the TLS handshake and the token
    /// preamble, and to be given a handshake slot in the first place.
    pub preamble_timeout: Duration,
    /// Ceiling on connections the edge holds at once, authenticated or
    /// not. Reached, the loop stops accepting: further peers queue in
    /// the kernel backlog and are refused past that, which is
    /// backpressure the operator can see rather than an fd table the
    /// daemon runs out of.
    pub max_connections: usize,
    /// Ceiling on connections in the *pre-auth* phase — the TLS
    /// handshake and token read. Tighter than `max_connections`
    /// because a handshake costs CPU and a rustls session where an
    /// established connection costs an fd, and because everything in
    /// this phase is by definition anonymous.
    pub max_pre_auth_connections: usize,
    /// Ceiling on in-flight requests per authenticated channel, so one
    /// client cannot queue unbounded work on the daemon.
    pub max_concurrent_requests: usize,
    /// How long the accept loop pauses after an `accept` error.
    ///
    /// Not a nicety: tokio does not clear a listener's readiness on
    /// `EMFILE`, so a `continue` on error turns fd exhaustion into a
    /// spinning worker thread that never lets the tasks holding those
    /// fds finish and give them back. Sleeping yields the thread and
    /// makes the condition survivable.
    pub accept_error_backoff: Duration,
}

impl Default for EdgeLimits {
    fn default() -> Self {
        Self {
            preamble_timeout: DEFAULT_PREAMBLE_TIMEOUT,
            max_connections: 256,
            max_pre_auth_connections: 64,
            max_concurrent_requests: 32,
            accept_error_backoff: Duration::from_millis(100),
        }
    }
}

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
        // The min_seq gate is generic-surface semantics, enforced
        // centrally: reads wait (bounded) until the fold includes the
        // caller's watermark; on anything that isn't a read the field
        // is refused — commands return receipts, they don't gate.
        if let Some(min_seq) = request.min_seq {
            let is_read = matches!(resolved.category, OpCategory::Get | OpCategory::List);
            if !is_read {
                return Err(WireError::InvalidInput {
                    op: request.op.to_string(),
                    message: "min_seq gates reads; commands and reports answer at                               their own watermark"
                        .into(),
                });
            }
            let Some(gate) = self.registry.read_gate() else {
                return Err(WireError::InvalidInput {
                    op: request.op.to_string(),
                    message: "this daemon serves no watermark gate; retry without                               min_seq"
                        .into(),
                });
            };
            if let Err(applied) = gate(min_seq).await {
                return Err(WireError::Lagging {
                    op: request.op.to_string(),
                    wanted: min_seq,
                    applied,
                });
            }
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
        let Some(handler) = self.registry.stream_handler(&request.op.to_string()) else {
            return Err(WireError::NotRegistered {
                op: request.op.to_string(),
            });
        };
        handler(request.filter, request.from_seq, request.max_wait_ms).await
    }
}

/// Where connections come from.
///
/// A trait rather than a bare [`TcpListener`] so the accept loop's
/// error branch is reachable from a test: `EMFILE` is a process-wide
/// condition that cannot be injected in-process without breaking the
/// test harness holding the fds, and the branch it exercises — sleep,
/// do not spin — is the one that decides whether fd exhaustion is
/// survivable.
pub(crate) trait AcceptSource: Send + Sync + 'static {
    fn accept(&self) -> BoxFuture<'_, std::io::Result<(TcpStream, SocketAddr)>>;
}

impl AcceptSource for TcpListener {
    fn accept(&self) -> BoxFuture<'_, std::io::Result<(TcpStream, SocketAddr)>> {
        Box::pin(TcpListener::accept(self))
    }
}

/// A bound edge socket that is not yet serving anything.
///
/// The daemon takes one of these before it registers its worker or
/// publishes a lifecycle event, so losing the race for the address is
/// a clean, side-effect-free exit rather than an orphaned worker row.
pub struct EdgeListener {
    listener: TcpListener,
    local_addr: SocketAddr,
}

impl EdgeListener {
    /// Bind `addr`, and nothing else. The error names the address:
    /// "already in use" on a port a draining predecessor still holds
    /// is the common case, and an operator needs to know which one.
    pub async fn bind(addr: &str) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|err| anyhow::anyhow!("failed to bind the edge on {addr}: {err}"))?;
        let local_addr = listener.local_addr()?;
        Ok(Self {
            listener,
            local_addr,
        })
    }

    /// The address actually bound — the requested one, or what the
    /// kernel chose when the request named port 0.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Attach the identity and the registry and start answering.
    /// Returns the bound address and the serving future.
    pub fn serve(
        self,
        identity: &EdgeIdentity,
        registry: Arc<EdgeRegistry>,
        limits: EdgeLimits,
    ) -> anyhow::Result<(SocketAddr, BoxFuture<'static, ()>)> {
        let local_addr = self.local_addr;
        let ctx = ConnectionContext::new(identity, registry)?;
        let source: Arc<dyn AcceptSource> = Arc::new(self.listener);
        Ok((
            local_addr,
            Box::pin(accept_loop(source, Arc::new(ctx), limits)),
        ))
    }
}

/// Everything a connection needs that does not vary between them.
struct ConnectionContext {
    acceptor: TlsAcceptor,
    registry: Arc<EdgeRegistry>,
    root_public: biscuit_auth::PublicKey,
}

impl ConnectionContext {
    fn new(identity: &EdgeIdentity, registry: Arc<EdgeRegistry>) -> anyhow::Result<Self> {
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
        Ok(Self {
            acceptor: TlsAcceptor::from(Arc::new(tls_config)),
            registry,
            root_public: identity.public_key(),
        })
    }
}

/// Accept until the process stops, within the limits.
///
/// The total-connection permit is taken *before* `accept`, so a
/// saturated edge simply stops taking sockets off the backlog rather
/// than accepting them in order to refuse them: the peer sees a
/// connection that does not progress and then a refusal from the
/// kernel, and the daemon spends nothing on it.
async fn accept_loop(
    source: Arc<dyn AcceptSource>,
    ctx: Arc<ConnectionContext>,
    limits: EdgeLimits,
) {
    let connections = Arc::new(Semaphore::new(limits.max_connections));
    let pre_auth = Arc::new(Semaphore::new(limits.max_pre_auth_connections));
    loop {
        let Ok(permit) = connections.clone().acquire_owned().await else {
            return;
        };
        match source.accept().await {
            Ok((tcp, _peer)) => {
                let ctx = ctx.clone();
                let pre_auth = pre_auth.clone();
                tokio::spawn(async move {
                    serve_connection(tcp, ctx, pre_auth, limits).await;
                    drop(permit);
                });
            }
            Err(error) => {
                // Never `continue` straight back into `accept`: on
                // `EMFILE` the listener stays readable and the loop
                // becomes a busy wait that starves the very tasks
                // whose fds it is waiting for.
                tracing::warn!(%error, "edge: accept failed; pausing before the next accept");
                drop(permit);
                tokio::time::sleep(limits.accept_error_backoff).await;
            }
        }
    }
}

/// One connection: the pre-auth preamble under its own semaphore, then
/// tarpc for as long as the peer stays.
async fn serve_connection(
    tcp: TcpStream,
    ctx: Arc<ConnectionContext>,
    pre_auth: Arc<Semaphore>,
    limits: EdgeLimits,
) {
    // A handshake slot, bounded by the same clock as the handshake
    // itself: a peer that cannot be given one within the preamble
    // timeout is dropped rather than queued behind an unbounded line.
    let Ok(Ok(handshake_slot)) = timeout(limits.preamble_timeout, pre_auth.acquire_owned()).await
    else {
        return;
    };
    // Every pre-auth await is time-bounded: a client that
    // stalls the handshake or dribbles the token is dropped
    // rather than pinning a task + fd + rustls session.
    let Ok(Ok(mut tls)) = timeout(limits.preamble_timeout, ctx.acceptor.accept(tcp)).await else {
        return;
    };
    // Token preamble: length-prefixed base64 token bytes.
    let Ok(Ok(len)) = timeout(limits.preamble_timeout, tls.read_u32()).await else {
        return;
    };
    if len > MAX_TOKEN_BYTES {
        let _ = tls.write_u8(1).await;
        return;
    }
    let mut buf = vec![0u8; len as usize];
    let Ok(Ok(_)) = timeout(limits.preamble_timeout, tls.read_exact(&mut buf)).await else {
        return;
    };
    let presented = String::from_utf8_lossy(&buf).into_owned();
    let token = match verify_token(&presented, ctx.root_public) {
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
    // Authenticated: give the handshake slot back before the session,
    // which may last hours. The connection still counts against
    // `max_connections` until it closes.
    drop::<OwnedSemaphorePermit>(handshake_slot);

    let framed = tokio_util::codec::Framed::new(tls, LengthDelimitedCodec::new());
    let transport =
        tarpc::serde_transport::new(framed, tarpc::tokio_serde::formats::Json::default());
    let server = EdgeServer {
        registry: ctx.registry.clone(),
        token: Arc::new(token),
    };
    BaseChannel::with_defaults(transport)
        .max_concurrent_requests(limits.max_concurrent_requests)
        .execute(server.serve())
        .for_each(|response| async move {
            tokio::spawn(response);
        })
        .await;
}

/// Bind the edge on `addr` and serve it at once, at the default
/// limits. Every connection must present a token minted under
/// `identity`'s root key.
///
/// The daemon takes the two steps separately — [`EdgeListener::bind`]
/// early, [`EdgeListener::serve`] once its runtime is assembled — so
/// this is for callers with nothing to order the bind against: tests,
/// and the dashboard's in-process fixture.
pub async fn bind(
    addr: &str,
    identity: &EdgeIdentity,
    registry: Arc<EdgeRegistry>,
) -> anyhow::Result<(SocketAddr, BoxFuture<'static, ()>)> {
    bind_with_limits(addr, identity, registry, EdgeLimits::default()).await
}

/// [`bind`], with an explicit bound on the per-connection preamble.
pub async fn bind_with_timeout(
    addr: &str,
    identity: &EdgeIdentity,
    registry: Arc<EdgeRegistry>,
    preamble_timeout: Duration,
) -> anyhow::Result<(SocketAddr, BoxFuture<'static, ()>)> {
    bind_with_limits(
        addr,
        identity,
        registry,
        EdgeLimits {
            preamble_timeout,
            ..EdgeLimits::default()
        },
    )
    .await
}

/// [`bind`], with every limit chosen.
pub async fn bind_with_limits(
    addr: &str,
    identity: &EdgeIdentity,
    registry: Arc<EdgeRegistry>,
    limits: EdgeLimits,
) -> anyhow::Result<(SocketAddr, BoxFuture<'static, ()>)> {
    EdgeListener::bind(addr)
        .await?
        .serve(identity, registry, limits)
}

#[cfg(test)]
mod tests;
