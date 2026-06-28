//! A filesystem-backed [`ContentStore`] — the M1a reference backend.
//!
//! Layout under a root directory:
//!
//! ```text
//! <root>/blocks/<aa>/<block-hash>     content-defined blocks (deduplicated)
//! <root>/objects/<aa>/<object-cid>    JSON manifests: ordered (block, len)
//! ```
//!
//! Content is split into content-defined blocks (FastCDC); each block is
//! stored once, keyed by its BLAKE3 hash, so identical blocks across objects
//! share storage. An object's `Cid` is the BLAKE3 hash of its full content.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{Cid, ContentStore, Result, Stats, StoreError};

/// Content-defined chunking parameters (FastCDC target block sizes, bytes).
#[derive(Clone, Copy, Debug)]
pub struct ChunkParams {
    pub min: u32,
    pub avg: u32,
    pub max: u32,
}

impl Default for ChunkParams {
    fn default() -> Self {
        // Tuned for large media; small files become a single block.
        Self {
            min: 16 * 1024,
            avg: 64 * 1024,
            max: 256 * 1024,
        }
    }
}

/// A [`ContentStore`] backed by a directory tree of blocks and manifests.
pub struct FilesystemStore {
    root: PathBuf,
    params: ChunkParams,
}

#[derive(Serialize, Deserialize)]
struct BlockRef {
    hash: String,
    len: u64,
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    size: u64,
    blocks: Vec<BlockRef>,
}

impl FilesystemStore {
    /// Open (or create) a store rooted at `root` with default chunk params.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            params: ChunkParams::default(),
        }
    }

    /// Open a store with explicit chunk parameters (tests use small blocks to
    /// exercise multi-block objects).
    pub fn with_params(root: impl Into<PathBuf>, params: ChunkParams) -> Self {
        Self {
            root: root.into(),
            params,
        }
    }

    fn block_path(&self, hash: &str) -> PathBuf {
        self.root.join("blocks").join(&hash[0..2]).join(hash)
    }

    fn object_path(&self, cid: &Cid) -> PathBuf {
        let hex = cid.to_hex();
        self.root.join("objects").join(&hex[0..2]).join(hex)
    }

    async fn read_manifest(&self, cid: &Cid) -> Result<Manifest> {
        match tokio::fs::read(self.object_path(cid)).await {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| StoreError::Corrupt(format!("manifest {cid}: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(StoreError::NotFound(*cid)),
            Err(e) => Err(e.into()),
        }
    }
}

#[async_trait]
impl ContentStore for FilesystemStore {
    async fn put(&self, content: &[u8]) -> Result<Cid> {
        let cid = Cid::of(content);
        // Idempotent: an existing manifest means the content is already stored.
        if tokio::fs::try_exists(self.object_path(&cid)).await? {
            return Ok(cid);
        }

        let mut blocks = Vec::new();
        if !content.is_empty() {
            let chunker = fastcdc::v2020::FastCDC::new(
                content,
                self.params.min,
                self.params.avg,
                self.params.max,
            );
            for chunk in chunker {
                let block = &content[chunk.offset..chunk.offset + chunk.length];
                let hash = Cid::of(block).to_hex();
                let path = self.block_path(&hash);
                if !tokio::fs::try_exists(&path).await? {
                    write_atomic(&path, block).await?;
                }
                blocks.push(BlockRef {
                    hash,
                    len: chunk.length as u64,
                });
            }
        }

        let manifest = Manifest {
            size: content.len() as u64,
            blocks,
        };
        let encoded =
            serde_json::to_vec(&manifest).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        write_atomic(&self.object_path(&cid), &encoded).await?;
        Ok(cid)
    }

    async fn get(&self, cid: &Cid) -> Result<Vec<u8>> {
        let manifest = self.read_manifest(cid).await?;
        let mut out = Vec::with_capacity(manifest.size as usize);
        for block in &manifest.blocks {
            out.extend_from_slice(&self.read_block(cid, block).await?);
        }
        Ok(out)
    }

    async fn get_range(&self, cid: &Cid, offset: u64, len: u64) -> Result<Vec<u8>> {
        let manifest = self.read_manifest(cid).await?;
        let end = offset.saturating_add(len).min(manifest.size);
        if offset >= manifest.size || end <= offset {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity((end - offset) as usize);
        let mut pos = 0u64;
        for block in &manifest.blocks {
            let block_start = pos;
            let block_end = pos + block.len;
            pos = block_end;
            if block_end <= offset {
                continue; // entirely before the range
            }
            if block_start >= end {
                break; // entirely after the range
            }
            let data = self.read_block(cid, block).await?;
            let from = offset.saturating_sub(block_start) as usize;
            let to = (end - block_start).min(block.len) as usize;
            out.extend_from_slice(&data[from..to]);
        }
        Ok(out)
    }

    async fn has(&self, cid: &Cid) -> Result<bool> {
        Ok(tokio::fs::try_exists(self.object_path(cid)).await?)
    }

    async fn size(&self, cid: &Cid) -> Result<u64> {
        Ok(self.read_manifest(cid).await?.size)
    }

    async fn stats(&self) -> Result<Stats> {
        let mut stats = Stats::default();
        for path in list_files_two_level(&self.root.join("objects")).await? {
            let bytes = tokio::fs::read(&path).await?;
            let manifest: Manifest = serde_json::from_slice(&bytes)
                .map_err(|e| StoreError::Corrupt(format!("manifest {}: {e}", path.display())))?;
            stats.objects += 1;
            stats.logical_bytes += manifest.size;
            stats.block_refs += manifest.blocks.len() as u64;
        }
        for path in list_files_two_level(&self.root.join("blocks")).await? {
            stats.blocks += 1;
            stats.physical_bytes += tokio::fs::metadata(&path).await?.len();
        }
        Ok(stats)
    }
}

impl FilesystemStore {
    async fn read_block(&self, cid: &Cid, block: &BlockRef) -> Result<Vec<u8>> {
        tokio::fs::read(self.block_path(&block.hash))
            .await
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    StoreError::Corrupt(format!("object {cid}: missing block {}", block.hash))
                } else {
                    e.into()
                }
            })
    }
}

/// Write `bytes` to `path` via a uniquely-named temp file + atomic rename, so
/// a concurrent reader never observes a partial file. Content-addressed paths
/// make the final content identical regardless of which writer wins a race.
async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!(".tmp.{}.{n}", std::process::id()));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// List the files two levels deep under `dir` (the `<shard>/<file>` layout),
/// skipping transient temp files. A missing `dir` yields an empty list.
async fn list_files_two_level(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut shards = match tokio::fs::read_dir(dir).await {
        Ok(reader) => reader,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    while let Some(shard) = shards.next_entry().await? {
        if !shard.file_type().await?.is_dir() {
            continue;
        }
        let mut files = tokio::fs::read_dir(shard.path()).await?;
        while let Some(file) = files.next_entry().await? {
            if file.file_name().to_string_lossy().starts_with('.') {
                continue; // transient .tmp staging files
            }
            if file.file_type().await?.is_file() {
                out.push(file.path());
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, FilesystemStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = FilesystemStore::with_params(
            dir.path(),
            ChunkParams {
                min: 256,
                avg: 1024,
                max: 4096,
            },
        );
        (dir, store)
    }

    #[tokio::test]
    async fn stats_reflect_dedup() {
        let (_dir, store) = store();
        let mut a = vec![0u8; 40_000];
        for (i, byte) in a.iter_mut().enumerate() {
            *byte = (i % 251) as u8;
        }
        let mut b = a.clone();
        b.extend_from_slice(b" a distinct tail");
        store.put(&a).await.unwrap();
        store.put(&b).await.unwrap();
        store.put(&a).await.unwrap(); // duplicate -> idempotent

        let stats = store.stats().await.unwrap();
        assert_eq!(
            stats.objects, 2,
            "two distinct objects (the duplicate is idempotent)"
        );
        assert_eq!(stats.logical_bytes, a.len() as u64 + b.len() as u64);
        assert!(
            stats.physical_bytes < stats.logical_bytes,
            "a and b share blocks"
        );
        assert!(stats.dedup_ratio() > 1.0);
        assert!(stats.blocks <= stats.block_refs);
    }

    #[tokio::test]
    async fn stats_invariants_hold() {
        let (_dir, store) = store();
        store.put(b"alpha").await.unwrap();
        store.put(b"beta beta beta beta").await.unwrap();
        // Exercise the reusable conformance invariant against an isolated store.
        crate::conformance::stats_consistent(&store, b"gamma")
            .await
            .unwrap();
    }

    fn count_blocks(root: &Path) -> usize {
        let blocks = root.join("blocks");
        if !blocks.exists() {
            return 0;
        }
        walkdir_count(&blocks)
    }

    fn walkdir_count(dir: &Path) -> usize {
        let mut n = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                n += walkdir_count(&path);
            } else {
                n += 1;
            }
        }
        n
    }

    #[tokio::test]
    async fn identical_content_is_deduplicated_on_disk() {
        let (dir, store) = store();
        let content = vec![7u8; 50_000]; // many blocks
        let a = store.put(&content).await.unwrap();
        let blocks_after_first = count_blocks(dir.path());
        let b = store.put(&content).await.unwrap();
        assert_eq!(a, b);
        // Re-putting identical content writes no new blocks.
        assert_eq!(count_blocks(dir.path()), blocks_after_first);
        assert!(blocks_after_first >= 1);
    }

    #[tokio::test]
    async fn objects_sharing_a_prefix_share_blocks() {
        let (dir, store) = store();
        let mut a = vec![0u8; 40_000];
        for (i, b) in a.iter_mut().enumerate() {
            *b = (i % 251) as u8; // pseudo-structured so blocks form
        }
        let mut b = a.clone();
        b.extend_from_slice(b"a small distinct suffix"); // shares the prefix's blocks
        store.put(&a).await.unwrap();
        let after_a = count_blocks(dir.path());
        store.put(&b).await.unwrap();
        let after_b = count_blocks(dir.path());
        // b adds only a few blocks, not a whole second copy.
        assert!(
            after_b < after_a * 2,
            "expected shared blocks: {after_a} then {after_b}"
        );
    }

    #[tokio::test]
    async fn empty_content_roundtrips() {
        let (_dir, store) = store();
        let cid = store.put(b"").await.unwrap();
        assert_eq!(store.get(&cid).await.unwrap(), b"");
        assert_eq!(store.size(&cid).await.unwrap(), 0);
        assert!(store.has(&cid).await.unwrap());
    }

    #[tokio::test]
    async fn missing_object_is_not_found() {
        let (_dir, store) = store();
        let err = store.get(&Cid::of(b"never stored")).await.unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)));
    }
}
