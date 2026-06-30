//! The named object store — composes a [`ContentStore`] (immutable,
//! content-addressed blobs) with a [`NameIndex`] (the mutable name index)
//! into the user-facing API: store and read content by hierarchical name,
//! with version history and the reference counts that drive GC.

use crate::{Cid, ContentStore, NameIndex, Result, StoreError};

/// A named, versioned object store over a content store and a name index.
pub struct Repository<C, N> {
    content: C,
    index: N,
}

impl<C: ContentStore, N: NameIndex> Repository<C, N> {
    /// Compose a repository from a content store and a name index.
    pub fn new(content: C, index: N) -> Self {
        Self { content, index }
    }

    /// Store `content` and bind it to `name`, returning its CID. The blob is
    /// written **first**, then the index updated — so a crash in between
    /// leaves an orphan blob (GC reclaims it) rather than a dangling name.
    pub async fn put(&self, name: &str, content: &[u8]) -> Result<Cid> {
        let cid = self.content.put(content).await?;
        let reserved = self.reserve_blocks(&cid).await?;
        self.index.bind(name, &cid, &reserved).await?;
        Ok(cid)
    }

    /// Bind `name` to an already-stored `cid` (aliasing — many names, one
    /// object). [`StoreError::NotFound`] if the object is absent.
    pub async fn bind(&self, name: &str, cid: &Cid) -> Result<()> {
        let reserved = self.reserve_blocks(cid).await?;
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

    /// Remove `name` (current binding and history). Idempotent.
    pub async fn delete(&self, name: &str) -> Result<()> {
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

    /// Reserve every unique block of the already-stored object `cid` (the
    /// reserve-before-rely write path), returning the `(hash, generation)`
    /// reserved. `bind` then hands these off to edges (or releases them on an
    /// alias). A genuinely new block is minted at generation 0 — its file was
    /// written and fsynced by `content.put`; collision mints onto fresh
    /// generations arrive in slice 5c.
    async fn reserve_blocks(&self, cid: &Cid) -> Result<Vec<(Cid, u32)>> {
        let mut blocks = self.content.blocks(cid).await?;
        blocks.sort_by_key(|c| c.to_hex());
        blocks.dedup();
        let mut reserved = Vec::with_capacity(blocks.len());
        for hash in blocks {
            let generation = self.reserve_or_materialize(&hash).await?;
            reserved.push((hash, generation));
        }
        Ok(reserved)
    }

    async fn reserve_or_materialize(&self, hash: &Cid) -> Result<u32> {
        if let Some(generation) = self.index.reserve_block(hash).await? {
            return Ok(generation);
        }
        // No available generation: content.put wrote the canonical (generation 0)
        // file, so mint its row.
        if self.index.mint_block(hash, 0).await? {
            return Ok(0);
        }
        // The mint was refused: either a concurrent writer just minted an
        // available generation (reserve it), or generation 0 is claimed by GC.
        if let Some(generation) = self.index.reserve_block(hash).await? {
            return Ok(generation);
        }
        // Generation 0 is claimed and none is available — a write/GC collision.
        // The generation-on-collision mint (writing a fresh generation) lands in
        // slice 5d; until then a collision surfaces as a rare, safe error.
        Err(StoreError::Corrupt(format!(
            "block {hash} collided with GC; generation-on-collision is slice 5d"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqliteNameIndex;
    use crate::fs::FilesystemStore;
    use tempfile::TempDir;

    async fn repository() -> (TempDir, Repository<FilesystemStore, SqliteNameIndex>) {
        let dir = tempfile::tempdir().unwrap();
        let content = FilesystemStore::new(dir.path().join("cas"));
        let index = SqliteNameIndex::open(dir.path().join("index.db"))
            .await
            .unwrap();
        (dir, Repository::new(content, index))
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
        repo.delete("temp").await.unwrap();
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
}
