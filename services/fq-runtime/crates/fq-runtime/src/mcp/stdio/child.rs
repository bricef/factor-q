//! The stdio child's transport, with a bound on how long one line may
//! be (#548).
//!
//! **Why this is hand-rolled rather than rmcp's `TokioChildProcess`.**
//! A JSON-RPC message on a stdio transport is one line, and rmcp's
//! reader is a `read_until(b'\n')` into a `Vec` with no ceiling: a
//! server that streams bytes and never sends a newline grows the
//! daemon's memory for as long as it keeps writing, and nothing in the
//! protocol says stop. rmcp's own `JsonRpcMessageCodec` carries a
//! `max_length`, but it is used only on the *write* side, so there is
//! no setting to reach for — the bound has to be interposed in front of
//! the reader. Everything else about the child is unchanged, including
//! the ordering issue #25 is about: `close` drops the write half so the
//! server sees EOF on stdin, waits for it to exit, and only then kills.
//!
//! The bound is on the *stream*, not on the parser. A line past the cap
//! fails the read, which ends the transport and takes the connection
//! with it — at boot that is a start that failed, and after boot the
//! notification drain sees the server's stream end and hands it to the
//! supervisor, so either way it is marked unavailable and dialled
//! again. Refusing the connection is the point: a host that skipped the
//! oversized line and carried on would be resynchronising with a peer
//! that had already broken the frame contract.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use rmcp::RoleClient;
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use rmcp::transport::async_rw::AsyncRwTransport;
use tokio::io::{AsyncRead, ReadBuf};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tracing::{debug, warn};

/// How long [`ChildTransport::close`] waits for the child to exit after
/// it has been sent EOF on stdin, before killing it. Matches the wait
/// rmcp's own child transport used, so the graceful path (EOF → the
/// server tears its stdio down and exits) still wins the race against
/// an abrupt kill mid-write — the cause of the flaky `EPIPE` on Node
/// stdio servers (issue #25).
const EXIT_GRACE: Duration = Duration::from_secs(3);

/// An [`AsyncRead`] that fails once it has passed `cap` bytes without a
/// newline.
///
/// It counts what it hands *on*, so the bound is on the line the reader
/// above will assemble rather than on any one syscall. The failure is
/// terminal: once tripped, every later read fails the same way, because
/// the stream's framing is already lost and the bytes that would
/// resynchronise it are exactly the bytes being refused.
pub(crate) struct LineCapped<R> {
    inner: R,
    cap: usize,
    /// Bytes passed on since the last newline.
    since_newline: usize,
    tripped: bool,
}

impl<R> LineCapped<R> {
    pub(crate) fn new(inner: R, cap: usize) -> Self {
        Self {
            inner,
            cap,
            since_newline: 0,
            tripped: false,
        }
    }

    fn refusal(&self) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "MCP server wrote more than {} bytes with no newline: one JSON-RPC message is \
                 one line, so the connection is refused rather than buffered ([mcp] \
                 max_line_bytes)",
                self.cap
            ),
        )
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for LineCapped<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        if me.tripped {
            return Poll::Ready(Err(me.refusal()));
        }
        let before = buf.filled().len();
        match Pin::new(&mut me.inner).poll_read(cx, buf) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
            Poll::Ready(Ok(())) => {}
        }
        // Every line *inside* the chunk is measured, not just the one
        // it ends on: a single read can carry a whole oversized line
        // between two newlines, and looking only at the tail would let
        // it through.
        let mut rest = &buf.filled()[before..];
        loop {
            match rest.iter().position(|&b| b == b'\n') {
                Some(at) => {
                    me.since_newline += at;
                    if me.since_newline > me.cap {
                        break;
                    }
                    me.since_newline = 0;
                    rest = &rest[at + 1..];
                }
                None => {
                    me.since_newline += rest.len();
                    break;
                }
            }
        }
        if me.since_newline > me.cap {
            me.tripped = true;
            return Poll::Ready(Err(me.refusal()));
        }
        Poll::Ready(Ok(()))
    }
}

/// A stdio MCP server: the child process, and the JSON-RPC transport
/// over its pipes with [`LineCapped`] in front of stdout.
///
/// Owning the [`Child`] is what makes the bound safe to add: dropping
/// the transport drops the child, which is spawned with
/// `kill_on_drop`, so a server whose connection is refused cannot
/// outlive it.
pub(crate) struct ChildTransport {
    /// `None` once [`close`](Transport::close) has taken it to wait for
    /// the exit; the drop guard then has nothing left to kill.
    child: Option<Child>,
    transport: AsyncRwTransport<RoleClient, LineCapped<ChildStdout>, ChildStdin>,
}

impl ChildTransport {
    pub(crate) fn new(
        child: Child,
        stdout: ChildStdout,
        stdin: ChildStdin,
        max_line_bytes: usize,
    ) -> Self {
        Self {
            child: Some(child),
            transport: AsyncRwTransport::new_client(LineCapped::new(stdout, max_line_bytes), stdin),
        }
    }
}

impl Transport<RoleClient> for ChildTransport {
    type Error = io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.transport.send(item)
    }

    fn receive(
        &mut self,
    ) -> impl std::future::Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        self.transport.receive()
    }

    /// Close the write half — which sends the child EOF on stdin — then
    /// give it [`EXIT_GRACE`] to exit on its own before killing it.
    /// The order is the whole point (issue #25): a server killed
    /// mid-write takes `EPIPE`, and the `@modelcontextprotocol/sdk`
    /// stdio transport installs no socket `error` handler, so Node
    /// throws and the process exits non-zero after every request has
    /// already succeeded.
    async fn close(&mut self) -> Result<(), Self::Error> {
        self.transport.close().await?;
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        match tokio::time::timeout(EXIT_GRACE, child.wait()).await {
            Ok(Ok(status)) => debug!(%status, "MCP stdio server exited on EOF"),
            Ok(Err(err)) => warn!(error = %err, "waiting for MCP stdio server failed"),
            Err(_) => {
                debug!("MCP stdio server did not exit on EOF within the grace; killing it");
                if let Err(err) = child.kill().await {
                    warn!(error = %err, "killing MCP stdio server failed");
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
