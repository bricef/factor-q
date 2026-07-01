//! The named object store — composes a [`crate::BlockStore`] (immutable,
//! content-addressed blobs with block-level writes) with a [`NameIndex`] (the
//! mutable name index) into the user-facing API: store and read content by
//! hierarchical name, with version history and the reference counts that drive GC.

use std::collections::HashMap;

use crate::{BlockStore, Cid, NameIndex, Result, StoreError};

/// A named, versioned object store over a content store and a name index.
pub struct Repository<C, N> {
    content: C,
    index: N,
}

impl<C: BlockStore, N: NameIndex> Repository<C, N> {
    /// Compose a repository from a content store and a name index.
    pub fn new(content: C, index: N) -> Self {
        Self { content, index }
    }

    /// Store `content` and bind it to `name`, returning its CID. Ordering is the
    /// crash-safe one the protocol requires: each block is **materialized**
    /// (written + fsynced) first, then the **manifest**, then the **index** —
    /// so a crash in between leaves orphan blocks / an orphan manifest (GC
    /// reclaims them) rather than a name pointing at absent bytes. Each block is
    /// reserved on the index's available generation, or — if GC has claimed every
    /// generation — minted onto a fresh one (the verified generation-on-collision
    /// write path: reserve-before-rely).
    pub async fn put(&self, name: &str, content: &[u8]) -> Result<Cid> {
        let cid = Cid::of(content);
        let chunks = self.content.chunk(content);

        // Reserve or materialize each *unique* block, recording the generation it
        // landed on; duplicate chunks within the object reuse the first's.
        let mut generations: HashMap<Cid, u32> = HashMap::new();
        let mut reserved: Vec<(Cid, u32)> = Vec::new();
        for chunk in &chunks {
            if generations.contains_key(&chunk.hash) {
                continue;
            }
            let bytes = &content[chunk.offset..chunk.offset + chunk.len];
            let generation = self.reserve_or_materialize(&chunk.hash, bytes).await?;
            generations.insert(chunk.hash, generation);
            reserved.push((chunk.hash, generation));
        }

        // The manifest records each block's resolved generation, then the index
        // edges hand off the reservations.
        let blocks: Vec<(Cid, u32, u64)> = chunks
            .iter()
            .map(|c| (c.hash, generations[&c.hash], c.len as u64))
            .collect();
        self.content
            .write_object(&cid, content.len() as u64, &blocks)
            .await?;
        self.index.bind(name, &cid, &reserved).await?;
        Ok(cid)
    }

    /// Bind `name` to an already-stored `cid` (aliasing — many names, one
    /// object). [`StoreError::NotFound`] if the object is absent. It reserves the
    /// object's existing blocks; it cannot resurrect a block GC has already
    /// claimed (minting one needs the bytes, which only [`put`](Self::put) has),
    /// so aliasing an object mid-collection fails with [`StoreError::Conflict`] —
    /// retry, or re-`put` the content.
    pub async fn bind(&self, name: &str, cid: &Cid) -> Result<()> {
        let mut blocks = self.content.blocks(cid).await?;
        blocks.sort_by_key(|c| *c.as_bytes());
        blocks.dedup();
        let mut reserved = Vec::with_capacity(blocks.len());
        for hash in blocks {
            // A live object holds each block on its one available generation, so
            // reserving that generation yields exactly the one the manifest
            // references. `None` means GC claimed it mid-flight — a retryable
            // conflict, not corruption.
            let generation = self.index.reserve_block(&hash).await?.ok_or_else(|| {
                StoreError::Conflict(format!(
                    "cannot alias block {hash}: no available generation (collected concurrently)"
                ))
            })?;
            reserved.push((hash, generation));
        }
        self.index.bind(name, cid, &reserved).await
    }

    /// The current CID for `name`, or `None` if unbound.
    pub async fn resolve(&self, name: &str) -> Result<Option<Cid>> {
        self.index.resolve(name).await
    }

    /// Read the current content for `name`. [`StoreError::NameNotFound`] if
    /// the name is unbound.
    pub async fn get(&self, name: &str) -> Result<Vec<u8>> {
        let cid = self.require(name).await?;
        self.content.get(&cid).await
    }

    /// Read a byte range of the current content for `name`.
    pub async fn get_range(&self, name: &str, offset: u64, len: u64) -> Result<Vec<u8>> {
        let cid = self.require(name).await?;
        self.content.get_range(&cid, offset, len).await
    }

    /// Unbind `name` (current binding and history) — the inverse of
    /// [`bind`](Self::bind) / [`put`](Self::put). Idempotent.
    pub async fn unbind(&self, name: &str) -> Result<()> {
        self.index.unbind(name).await
    }

    /// Names within the namespace `prefix` (see [`NameIndex::list`]).
    pub async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        self.index.list(prefix).await
    }

    /// `name`'s version history, newest first.
    pub async fn history(&self, name: &str) -> Result<Vec<Cid>> {
        self.index.history(name).await
    }

    /// The underlying content store (for direct CID access, metrics, GC).
    pub fn content(&self) -> &C {
        &self.content
    }

    /// The underlying name index (for GC candidate enumeration).
    pub fn index(&self) -> &N {
        &self.index
    }

    async fn require(&self, name: &str) -> Result<Cid> {
        self.index
            .resolve(name)
            .await?
            .ok_or_else(|| StoreError::NameNotFound(name.to_string()))
    }

    /// Reserve block `hash` on its currently-available generation; or, if none is
    /// available — a genuinely new block, or GC has claimed every existing
    /// generation — materialize a fresh one from `bytes`. Returns the generation
    /// the block landed on.
    ///
    /// This is the verified write path. Reserve first (reserve-before-rely): if a
    /// generation is available, take it. Otherwise mint — write+fsync the file at
    /// the smallest free generation *before* its row exists (I2), then insert that
    /// row conditional on none being available, so concurrent minters and a racing
    /// GC converge: a refused mint means either a peer just minted an available
    /// generation (the next reserve takes it) or it grabbed this generation first
    /// (the next mint picks a higher one). The loop is wait-free — it never blocks
    /// on or fails because of GC.
    async fn reserve_or_materialize(&self, hash: &Cid, bytes: &[u8]) -> Result<u32> {
        loop {
            if let Some(generation) = self.index.reserve_block(hash).await? {
                return Ok(generation);
            }
            let generation = self.index.next_generation(hash).await?;
            self.content.write_block(hash, generation, bytes).await?;
            if self.index.mint_block(hash, generation).await? {
                return Ok(generation);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::FilesystemStore;
    use crate::{ContentStore, SqliteNameIndex, verify};
    use tempfile::TempDir;

    async fn repository() -> (TempDir, Repository<FilesystemStore, SqliteNameIndex>) {
        crate::test_support::repo().await
    }

    #[tokio::test]
    async fn put_get_roundtrip() {
        let (_d, repo) = repository().await;
        let cid = repo.put("a.b.doc", b"hello named world").await.unwrap();
        assert_eq!(repo.resolve("a.b.doc").await.unwrap(), Some(cid));
        assert_eq!(repo.get("a.b.doc").await.unwrap(), b"hello named world");
    }

    #[tokio::test]
    async fn missing_name_errors() {
        let (_d, repo) = repository().await;
        assert!(matches!(
            repo.get("nope").await,
            Err(StoreError::NameNotFound(_))
        ));
    }

    #[tokio::test]
    async fn overwrite_updates_current_and_keeps_history() {
        let (_d, repo) = repository().await;
        let c1 = repo.put("a", b"one").await.unwrap();
        let c2 = repo.put("a", b"two").await.unwrap();
        assert_eq!(repo.get("a").await.unwrap(), b"two");
        assert_eq!(repo.history("a").await.unwrap(), vec![c2, c1]);
    }

    #[tokio::test]
    async fn aliasing_shares_one_object() {
        let (_d, repo) = repository().await;
        let cid = repo.put("original", b"shared bytes").await.unwrap();
        repo.bind("alias", &cid).await.unwrap();
        assert_eq!(repo.get("alias").await.unwrap(), b"shared bytes");
        assert_eq!(repo.resolve("alias").await.unwrap(), Some(cid));
    }

    #[tokio::test]
    async fn delete_makes_object_a_gc_candidate() {
        let (_d, repo) = repository().await;
        let cid = repo.put("temp", b"disposable").await.unwrap();
        assert!(
            repo.index()
                .unreferenced_objects()
                .await
                .unwrap()
                .is_empty()
        );
        repo.unbind("temp").await.unwrap();
        assert_eq!(repo.resolve("temp").await.unwrap(), None);
        assert_eq!(
            repo.index().unreferenced_objects().await.unwrap(),
            vec![cid]
        );
    }

    #[tokio::test]
    async fn get_range_reads_a_slice() {
        let (_d, repo) = repository().await;
        repo.put("data", b"0123456789").await.unwrap();
        assert_eq!(repo.get_range("data", 3, 4).await.unwrap(), b"3456");
    }

    #[tokio::test]
    async fn put_mints_a_fresh_generation_when_gc_claimed_the_block() {
        let (_d, repo) = repository().await;
        // Store content (a single block), then delete the name — the block is now
        // unreferenced (refcount 0), but its row and file remain.
        let content = b"a small object that is exactly one block";
        let cid = repo.put("a", content).await.unwrap();
        repo.unbind("a").await.unwrap();

        // Simulate GC mid-reclaim: claim generation 0 (refcount 0 → unavailable)
        // but stop before unlinking. A writer re-putting the same content must not
        // touch the claimed generation.
        let block = repo.content().blocks(&cid).await.unwrap()[0];
        assert!(repo.index().claim_block(&block, 0).await.unwrap());

        // Re-put: reserve fails (generation 0 is claimed, none available), so the
        // writer mints a fresh generation rather than blocking or failing.
        let cid2 = repo.put("a2", content).await.unwrap();
        assert_eq!(cid2, cid); // same content → same object id
        assert_eq!(repo.get("a2").await.unwrap(), content);

        // Exactly one generation is available — the fresh one (1) — and the
        // claimed orphan (0) is left for the collector. Invariants hold.
        let snap = repo.index().snapshot().await.unwrap();
        let available: Vec<_> = snap
            .blocks
            .iter()
            .filter(|b| b.hash == block && b.available)
            .collect();
        assert_eq!(available.len(), 1, "one available generation: {snap:#?}");
        assert_eq!(available[0].generation, 1);
        verify::assert_clean(repo.index(), repo.content()).await;
    }
}
