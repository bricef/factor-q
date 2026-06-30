//! The CAS network service — a `tarpc` server ([`serve`]) over TCP, and a
//! [`RemoteStore`] client that *is* a [`ContentStore`].
//!
//! This proves ADR-0023's core claim — the same `ContentStore` contract runs
//! in-process and distributed — on the M1a CAS. Because `RemoteStore`
//! implements `ContentStore`, the conformance suite runs against it over the
//! wire.
//!
//! The server is **unauthenticated** (capability tokens are M2). Default the
//! bind address to localhost; do not expose it on a public interface until
//! M2 lands.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::{StreamExt, future};
use serde::{Deserialize, Serialize};
use tarpc::server::{BaseChannel, Channel};
use tarpc::tokio_serde::formats::Bincode;
use tarpc::{client, context};

use crate::{Cid, ContentStore, Result, Stats, StoreError};

/// A serializable error carried over the wire (since [`StoreError`] holds a
/// non-serializable `io::Error`).
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
pub enum WireError {
    /// The requested content id is not present (carries the id as hex).
    #[error("content not found: {0}")]
    NotFound(String),
    /// Any other store error, flattened to a message.
    #[error("{0}")]
    Message(String),
}

impl From<StoreError> for WireError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound(cid) => WireError::NotFound(cid.to_hex()),
            other => WireError::Message(other.to_string()),
        }
    }
}

impl From<WireError> for StoreError {
    fn from(e: WireError) -> Self {
        match e {
            WireError::NotFound(hex) => Cid::from_hex(&hex)
                .map(StoreError::NotFound)
                .unwrap_or_else(|_| StoreError::Corrupt(format!("not found: {hex}"))),
            WireError::Message(msg) => StoreError::Corrupt(msg),
        }
    }
}

/// The RPC surface, mirroring [`ContentStore`].
#[tarpc::service]
pub trait CasService {
    async fn put(content: Vec<u8>) -> std::result::Result<Cid, WireError>;
    async fn get(cid: Cid) -> std::result::Result<Vec<u8>, WireError>;
    async fn get_range(cid: Cid, offset: u64, len: u64) -> std::result::Result<Vec<u8>, WireError>;
    async fn has(cid: Cid) -> std::result::Result<bool, WireError>;
    async fn size(cid: Cid) -> std::result::Result<u64, WireError>;
    async fn stats() -> std::result::Result<Stats, WireError>;
    async fn remove(cid: Cid) -> std::result::Result<(), WireError>;
    async fn has_block(block: Cid, generation: u32) -> std::result::Result<bool, WireError>;
    async fn remove_block(block: Cid, generation: u32) -> std::result::Result<(), WireError>;
}

/// Server handler: forwards each RPC to a backing [`ContentStore`].
#[derive(Clone)]
struct CasServer {
    store: Arc<dyn ContentStore>,
}

impl CasService for CasServer {
    async fn put(
        self,
        _: context::Context,
        content: Vec<u8>,
    ) -> std::result::Result<Cid, WireError> {
        self.store.put(&content).await.map_err(WireError::from)
    }

    async fn get(self, _: context::Context, cid: Cid) -> std::result::Result<Vec<u8>, WireError> {
        self.store.get(&cid).await.map_err(WireError::from)
    }

    async fn get_range(
        self,
        _: context::Context,
        cid: Cid,
        offset: u64,
        len: u64,
    ) -> std::result::Result<Vec<u8>, WireError> {
        self.store
            .get_range(&cid, offset, len)
            .await
            .map_err(WireError::from)
    }

    async fn has(self, _: context::Context, cid: Cid) -> std::result::Result<bool, WireError> {
        self.store.has(&cid).await.map_err(WireError::from)
    }

    async fn size(self, _: context::Context, cid: Cid) -> std::result::Result<u64, WireError> {
        self.store.size(&cid).await.map_err(WireError::from)
    }

    async fn stats(self, _: context::Context) -> std::result::Result<Stats, WireError> {
        self.store.stats().await.map_err(WireError::from)
    }

    async fn remove(self, _: context::Context, cid: Cid) -> std::result::Result<(), WireError> {
        self.store.remove(&cid).await.map_err(WireError::from)
    }

    async fn has_block(
        self,
        _: context::Context,
        block: Cid,
        generation: u32,
    ) -> std::result::Result<bool, WireError> {
        self.store
            .has_block(&block, generation)
            .await
            .map_err(WireError::from)
    }

    async fn remove_block(
        self,
        _: context::Context,
        block: Cid,
        generation: u32,
    ) -> std::result::Result<(), WireError> {
        self.store
            .remove_block(&block, generation)
            .await
            .map_err(WireError::from)
    }
}

/// Bind a TCP listener and return its address plus a future that serves
/// requests until dropped. Splitting bind from serve lets callers (tests)
/// learn the ephemeral address before the server starts.
pub async fn bind(
    addr: &str,
    store: Arc<dyn ContentStore>,
) -> std::io::Result<(SocketAddr, BoxFuture<'static, ()>)> {
    let mut listener = tarpc::serde_transport::tcp::listen(addr, Bincode::default).await?;
    listener.config_mut().max_frame_length(usize::MAX);
    let local_addr = listener.local_addr();
    let serving: BoxFuture<'static, ()> = Box::pin(async move {
        listener
            .filter_map(|r| future::ready(r.ok()))
            .map(BaseChannel::with_defaults)
            .for_each_concurrent(None, move |channel| {
                let server = CasServer {
                    store: store.clone(),
                };
                channel
                    .execute(server.serve())
                    .for_each(|response| async move {
                        tokio::spawn(response);
                    })
            })
            .await;
    });
    Ok((local_addr, serving))
}

/// Serve the CAS over TCP at `addr` until the task is dropped.
pub async fn serve(addr: &str, store: Arc<dyn ContentStore>) -> std::io::Result<()> {
    let (_addr, serving) = bind(addr, store).await?;
    serving.await;
    Ok(())
}

/// A [`ContentStore`] that forwards every call to a remote CAS server over
/// `tarpc` — the same contract, over the wire.
pub struct RemoteStore {
    client: CasServiceClient,
}

impl RemoteStore {
    /// Connect to a CAS server at `addr` (e.g. "127.0.0.1:9000").
    pub async fn connect(addr: &str) -> std::io::Result<Self> {
        let transport = tarpc::serde_transport::tcp::connect(addr, Bincode::default).await?;
        let client = CasServiceClient::new(client::Config::default(), transport).spawn();
        Ok(Self { client })
    }
}

#[async_trait]
impl ContentStore for RemoteStore {
    async fn put(&self, content: &[u8]) -> Result<Cid> {
        self.client
            .put(context::current(), content.to_vec())
            .await
            .map_err(rpc_err)?
            .map_err(StoreError::from)
    }

    async fn get(&self, cid: &Cid) -> Result<Vec<u8>> {
        self.client
            .get(context::current(), *cid)
            .await
            .map_err(rpc_err)?
            .map_err(StoreError::from)
    }

    async fn get_range(&self, cid: &Cid, offset: u64, len: u64) -> Result<Vec<u8>> {
        self.client
            .get_range(context::current(), *cid, offset, len)
            .await
            .map_err(rpc_err)?
            .map_err(StoreError::from)
    }

    async fn has(&self, cid: &Cid) -> Result<bool> {
        self.client
            .has(context::current(), *cid)
            .await
            .map_err(rpc_err)?
            .map_err(StoreError::from)
    }

    async fn size(&self, cid: &Cid) -> Result<u64> {
        self.client
            .size(context::current(), *cid)
            .await
            .map_err(rpc_err)?
            .map_err(StoreError::from)
    }

    async fn stats(&self) -> Result<Stats> {
        self.client
            .stats(context::current())
            .await
            .map_err(rpc_err)?
            .map_err(StoreError::from)
    }

    async fn remove(&self, cid: &Cid) -> Result<()> {
        self.client
            .remove(context::current(), *cid)
            .await
            .map_err(rpc_err)?
            .map_err(StoreError::from)
    }

    async fn has_block(&self, block: &Cid, generation: u32) -> Result<bool> {
        self.client
            .has_block(context::current(), *block, generation)
            .await
            .map_err(rpc_err)?
            .map_err(StoreError::from)
    }

    async fn remove_block(&self, block: &Cid, generation: u32) -> Result<()> {
        self.client
            .remove_block(context::current(), *block, generation)
            .await
            .map_err(rpc_err)?
            .map_err(StoreError::from)
    }
}

fn rpc_err(e: tarpc::client::RpcError) -> StoreError {
    StoreError::Corrupt(format!("rpc error: {e}"))
}
