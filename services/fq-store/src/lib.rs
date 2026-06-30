//! `fq-store` — factor-q's content-addressed storage + semantic index
//! service (Phase 2 pillar #2). See `docs/adrs/accepted/0023-*` and
//! `0024-*` for the design.
//!
//! This is **M1a**: the content-addressed store (CAS) — the bedrock layer.
//! It stores arbitrary bytes, deduplicated at the block level, addressed by
//! the BLAKE3 hash of their content.
//!
//! Backends implement [`ContentStore`] and prove correctness against the
//! shared, backend-agnostic conformance suite ([`content_store_conformance!`]).
//! See `docs/guide/implementing-a-storage-backend.md`.

mod cid;
mod error;

pub mod conformance;
pub mod fs;
pub mod index;
pub mod repository;
pub mod stats;
pub mod verify;

#[cfg(feature = "cli")]
pub mod cli;

#[cfg(feature = "service")]
pub mod service;

pub use cid::Cid;
pub use error::{Result, StoreError};
pub use index::{IndexSnapshot, NameIndex, SqliteNameIndex};
pub use repository::Repository;
pub use stats::Stats;

use async_trait::async_trait;

/// A content-addressed blob store: write bytes and get back their [`Cid`];
/// read by `Cid`, in full or by range. Identical content is deduplicated and
/// always maps to the same `Cid`.
///
/// This is the storage substrate (ADR-0023 layer 1). Every backend must
/// satisfy the conformance suite — see the crate docs.
#[async_trait]
pub trait ContentStore: Send + Sync {
    /// Store `content`; return its content id. Storing identical content
    /// again is idempotent and returns the same [`Cid`] without re-writing.
    async fn put(&self, content: &[u8]) -> Result<Cid>;

    /// Read the full content for `cid`. [`StoreError::NotFound`] if absent.
    async fn get(&self, cid: &Cid) -> Result<Vec<u8>>;

    /// Read up to `len` bytes starting at `offset`. A range extending past
    /// the end is truncated to the available bytes; an `offset` at or beyond
    /// the end yields an empty result.
    async fn get_range(&self, cid: &Cid, offset: u64, len: u64) -> Result<Vec<u8>>;

    /// Whether content for `cid` is present.
    async fn has(&self, cid: &Cid) -> Result<bool>;

    /// Total size in bytes of the content for `cid`.
    async fn size(&self, cid: &Cid) -> Result<u64>;

    /// Aggregate statistics over all stored content — the basis for metrics
    /// such as the deduplication ratio. May scan the store.
    async fn stats(&self) -> Result<Stats>;

    /// The object's dedup units, as block content-ids in order. The default
    /// treats the whole object as one block (`[cid]`); backends that sub-chunk
    /// (e.g. the filesystem backend) override this so the storage index can
    /// reference-count shared blocks. [`StoreError::NotFound`] if absent.
    async fn blocks(&self, cid: &Cid) -> Result<Vec<Cid>> {
        if self.has(cid).await? {
            Ok(vec![*cid])
        } else {
            Err(StoreError::NotFound(*cid))
        }
    }

    /// Remove the object `cid` (its manifest / representation). Removing an
    /// absent object is a no-op. This does **not** remove the object's blocks —
    /// those are reference-counted and reclaimed separately via
    /// [`remove_block`](Self::remove_block). The garbage collector calls this
    /// for objects the index reports unreferenced.
    async fn remove(&self, cid: &Cid) -> Result<()>;

    /// Whether the block file for `(block, generation)` is present. Generation
    /// 0 is the canonical block; a non-zero generation is a collision-minted
    /// copy (M1c). Backends that do not sub-chunk treat a block as the object
    /// and ignore the generation. A cheap existence check for GC and the audit.
    async fn has_block(&self, block: &Cid, generation: u32) -> Result<bool> {
        let _ = generation;
        self.has(block).await
    }

    /// Remove the block file for `(block, generation)`. Removing an absent block
    /// is a no-op. Backends that do not sub-chunk remove the object instead.
    async fn remove_block(&self, block: &Cid, generation: u32) -> Result<()> {
        let _ = generation;
        self.remove(block).await
    }
}
