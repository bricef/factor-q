//! The named object store — composes a [`ContentStore`] (immutable,
//! content-addressed blobs) with a [`NameStore`] (the mutable name index)
//! into the user-facing API: store and read content by hierarchical name,
//! with version history and the reference counts that drive GC.

use crate::{Cid, ContentStore, NameStore, Result, StoreError};

/// A named, versioned object store over a content store and a name index.
pub struct Catalog<C, N> {
    content: C,
    names: N,
}

impl<C: ContentStore, N: NameStore> Catalog<C, N> {
    /// Compose a catalog from a content store and a name index.
    pub fn new(content: C, names: N) -> Self {
        Self { content, names }
    }

    /// Store `content` and bind it to `name`, returning its CID. The blob is
    /// written **first**, then the index updated — so a crash in between
    /// leaves an orphan blob (GC reclaims it) rather than a dangling name.
    pub async fn put(&self, name: &str, content: &[u8]) -> Result<Cid> {
        let cid = self.content.put(content).await?;
        let blocks = self.content.blocks(&cid).await?;
        self.names.bind(name, &cid, &blocks).await?;
        Ok(cid)
    }

    /// Bind `name` to an already-stored `cid` (aliasing — many names, one
    /// object). [`StoreError::NotFound`] if the object is absent.
    pub async fn bind(&self, name: &str, cid: &Cid) -> Result<()> {
        let blocks = self.content.blocks(cid).await?;
        self.names.bind(name, cid, &blocks).await
    }

    /// The current CID for `name`, or `None` if unbound.
    pub async fn resolve(&self, name: &str) -> Result<Option<Cid>> {
        self.names.resolve(name).await
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
        self.names.unbind(name).await
    }

    /// Names within the namespace `prefix` (see [`NameStore::list`]).
    pub async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        self.names.list(prefix).await
    }

    /// `name`'s version history, newest first.
    pub async fn history(&self, name: &str) -> Result<Vec<Cid>> {
        self.names.history(name).await
    }

    /// The underlying content store (for direct CID access, metrics, GC).
    pub fn content(&self) -> &C {
        &self.content
    }

    /// The underlying name index (for GC candidate enumeration).
    pub fn names(&self) -> &N {
        &self.names
    }

    async fn require(&self, name: &str) -> Result<Cid> {
        self.names
            .resolve(name)
            .await?
            .ok_or_else(|| StoreError::NameNotFound(name.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqliteNameStore;
    use crate::fs::FilesystemStore;
    use tempfile::TempDir;

    async fn catalog() -> (TempDir, Catalog<FilesystemStore, SqliteNameStore>) {
        let dir = tempfile::tempdir().unwrap();
        let content = FilesystemStore::new(dir.path().join("cas"));
        let names = SqliteNameStore::open(dir.path().join("index.db"))
            .await
            .unwrap();
        (dir, Catalog::new(content, names))
    }

    #[tokio::test]
    async fn put_get_roundtrip() {
        let (_d, cat) = catalog().await;
        let cid = cat.put("a.b.doc", b"hello named world").await.unwrap();
        assert_eq!(cat.resolve("a.b.doc").await.unwrap(), Some(cid));
        assert_eq!(cat.get("a.b.doc").await.unwrap(), b"hello named world");
    }

    #[tokio::test]
    async fn missing_name_errors() {
        let (_d, cat) = catalog().await;
        assert!(matches!(
            cat.get("nope").await,
            Err(StoreError::NameNotFound(_))
        ));
    }

    #[tokio::test]
    async fn overwrite_updates_current_and_keeps_history() {
        let (_d, cat) = catalog().await;
        let c1 = cat.put("a", b"one").await.unwrap();
        let c2 = cat.put("a", b"two").await.unwrap();
        assert_eq!(cat.get("a").await.unwrap(), b"two");
        assert_eq!(cat.history("a").await.unwrap(), vec![c2, c1]);
    }

    #[tokio::test]
    async fn aliasing_shares_one_object() {
        let (_d, cat) = catalog().await;
        let cid = cat.put("original", b"shared bytes").await.unwrap();
        cat.bind("alias", &cid).await.unwrap();
        assert_eq!(cat.get("alias").await.unwrap(), b"shared bytes");
        assert_eq!(cat.resolve("alias").await.unwrap(), Some(cid));
    }

    #[tokio::test]
    async fn delete_makes_object_a_gc_candidate() {
        let (_d, cat) = catalog().await;
        let cid = cat.put("temp", b"disposable").await.unwrap();
        assert!(cat.names().unreferenced_objects().await.unwrap().is_empty());
        cat.delete("temp").await.unwrap();
        assert_eq!(cat.resolve("temp").await.unwrap(), None);
        assert_eq!(cat.names().unreferenced_objects().await.unwrap(), vec![cid]);
    }

    #[tokio::test]
    async fn get_range_reads_a_slice() {
        let (_d, cat) = catalog().await;
        cat.put("data", b"0123456789").await.unwrap();
        assert_eq!(cat.get_range("data", 3, 4).await.unwrap(), b"3456");
    }
}
