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

pub use cid::Cid;
pub use error::{Result, StoreError};

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
}
