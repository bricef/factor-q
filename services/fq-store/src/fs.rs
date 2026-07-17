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

use crate::{BlockStore, Chunk, Cid, ContentStore, Result, Stats, StoreError};

/// Content-defined chunking parameters (FastCDC target block sizes, bytes).
#[derive(Clone, Copy, Debug)]
pub struct ChunkParams {
    /// Minimum block size in bytes; content shorter than this stays one block.
    pub min: u32,
    /// Target (average) block size in bytes — the FastCDC cut-point setpoint.
    pub avg: u32,
    /// Maximum block size in bytes; a block is cut here even without a match.
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

impl ChunkParams {
    /// Small target sizes (256 B / 1 KiB / 4 KiB) so tests exercise the
    /// multi-block and shared-block paths on modest inputs.
    pub fn small() -> Self {
        Self {
            min: 256,
            avg: 1024,
            max: 4096,
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
    /// The block generation this object references — 0 (canonical) for blocks
    /// written without a GC collision. Recording it in the manifest lets reads
    /// resolve the block file without an index lookup. `serde(default)` keeps
    /// pre-M1c manifests (no generation field) readable.
    #[serde(default)]
    generation: u32,
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

    fn block_path(&self, hash: &str, generation: u32) -> PathBuf {
        let dir = self.root.join("blocks").join(&hash[0..2]);
        if generation == 0 {
            dir.join(hash)
        } else {
            // Collision-minted generations carry a suffix; the canonical block
            // (generation 0) does not, so reads need no index lookup (M1c).
            dir.join(format!("{hash}.{generation}"))
        }
    }

    fn object_path(&self, cid: &Cid) -> PathBuf {
        let hex = cid.to_hex();
        self.root.join("objects").join(&hex[0..2]).join(hex)
    }

    async fn read_manifest(&self, cid: &Cid) -> Result<Manifest> {
        let manifest: Manifest = match tokio::fs::read(self.object_path(cid)).await {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| StoreError::Corrupt(format!("manifest {cid}: {e}")))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound(*cid));
            }
            Err(e) => return Err(e.into()),
        };
        // Every block hash becomes a filesystem path component (see `block_path`),
        // so validate each is a well-formed CID before use. A manifest's hash
        // strings are untrusted the moment any manifest-ingest path exists; this
        // bounds them to 64 hex chars — no traversal, no short-slice panic.
        for block in &manifest.blocks {
            Cid::from_hex(&block.hash).map_err(|_| {
                StoreError::Corrupt(format!(
                    "manifest {cid}: invalid block hash {:?}",
                    block.hash
                ))
            })?;
        }
        Ok(manifest)
    }
}

#[async_trait]
impl ContentStore for FilesystemStore {
    #[tracing::instrument(level = "debug", skip_all, fields(bytes = content.len()))]
    async fn put(&self, content: &[u8]) -> Result<Cid> {
        let cid = Cid::of(content);
        // Idempotent: an existing manifest means the content is already stored.
        if tokio::fs::try_exists(self.object_path(&cid)).await? {
            tracing::debug!(%cid, "deduplicated (already stored)");
            return Ok(cid);
        }

        // One chunker: reuse the shared content-defined split (see `chunk`), so
        // the raw-CAS path and the reserve-before-rely write path can never
        // diverge on where block boundaries fall.
        let mut blocks = Vec::new();
        for chunk in self.chunk(content) {
            let block = &content[chunk.offset..chunk.offset + chunk.len];
            let hash = chunk.hash.to_hex();
            let path = self.block_path(&hash, 0);
            if !tokio::fs::try_exists(&path).await? {
                write_atomic(&path, block).await?;
            }
            blocks.push(BlockRef {
                hash,
                len: chunk.len as u64,
                generation: 0,
            });
        }

        let manifest = Manifest {
            size: content.len() as u64,
            blocks,
        };
        let encoded =
            serde_json::to_vec(&manifest).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        write_atomic(&self.object_path(&cid), &encoded).await?;
        tracing::debug!(%cid, blocks = manifest.blocks.len(), "stored");
        Ok(cid)
    }

    #[tracing::instrument(level = "debug", skip_all, fields(cid = %cid))]
    async fn get(&self, cid: &Cid) -> Result<Vec<u8>> {
        let manifest = self.read_manifest(cid).await?;
        // Cap the up-front reservation so a corrupt manifest `size` can't drive
        // an unbounded allocation; the buffer still grows as blocks append.
        let mut out = Vec::with_capacity((manifest.size as usize).min(64 << 20));
        for block in &manifest.blocks {
            out.extend_from_slice(&self.read_block(cid, block).await?);
        }
        Ok(out)
    }

    #[tracing::instrument(level = "debug", skip_all, fields(cid = %cid, offset, len))]
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
            let block_end = pos.saturating_add(block.len);
            pos = block_end;
            if block_end <= offset {
                continue; // entirely before the range
            }
            if block_start >= end {
                break; // entirely after the range
            }
            let data = self.read_block(cid, block).await?;
            // Clamp to the block file's actual length: a manifest whose recorded
            // `len` exceeds the stored block must not drive an out-of-bounds slice.
            let from = (offset.saturating_sub(block_start) as usize).min(data.len());
            let to = ((end - block_start).min(block.len) as usize).min(data.len());
            out.extend_from_slice(&data[from..to.max(from)]);
        }
        Ok(out)
    }

    async fn has(&self, cid: &Cid) -> Result<bool> {
        Ok(tokio::fs::try_exists(self.object_path(cid)).await?)
    }

    async fn size(&self, cid: &Cid) -> Result<u64> {
        Ok(self.read_manifest(cid).await?.size)
    }

    #[tracing::instrument(level = "debug", skip_all)]
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

    #[tracing::instrument(level = "debug", skip_all, fields(cid = %cid))]
    async fn blocks(&self, cid: &Cid) -> Result<Vec<Cid>> {
        let manifest = self.read_manifest(cid).await?;
        manifest
            .blocks
            .iter()
            .map(|b| Cid::from_hex(&b.hash))
            .collect()
    }

    async fn remove(&self, cid: &Cid) -> Result<()> {
        remove_file_idempotent(&self.object_path(cid)).await
    }

    async fn has_block(&self, block: &Cid, generation: u32) -> Result<bool> {
        Ok(tokio::fs::try_exists(self.block_path(&block.to_hex(), generation)).await?)
    }

    async fn remove_block(&self, block: &Cid, generation: u32) -> Result<()> {
        remove_file_idempotent(&self.block_path(&block.to_hex(), generation)).await
    }
}

#[async_trait]
impl BlockStore for FilesystemStore {
    fn chunk(&self, content: &[u8]) -> Vec<Chunk> {
        if content.is_empty() {
            return Vec::new();
        }
        fastcdc::v2020::FastCDC::new(content, self.params.min, self.params.avg, self.params.max)
            .map(|c| Chunk {
                hash: Cid::of(&content[c.offset..c.offset + c.length]),
                offset: c.offset,
                len: c.length,
            })
            .collect()
    }

    async fn write_block(&self, block: &Cid, generation: u32, bytes: &[u8]) -> Result<()> {
        let path = self.block_path(&block.to_hex(), generation);
        // Content-addressed and idempotent: an extant file is already these bytes.
        if !tokio::fs::try_exists(&path).await? {
            write_atomic(&path, bytes).await?;
        }
        Ok(())
    }

    async fn write_object(&self, cid: &Cid, size: u64, blocks: &[(Cid, u32, u64)]) -> Result<()> {
        let manifest = Manifest {
            size,
            blocks: blocks
                .iter()
                .map(|(hash, generation, len)| BlockRef {
                    hash: hash.to_hex(),
                    len: *len,
                    generation: *generation,
                })
                .collect(),
        };
        let encoded =
            serde_json::to_vec(&manifest).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        write_atomic(&self.object_path(cid), &encoded).await?;
        Ok(())
    }

    async fn list_stored_blocks(&self) -> Result<Vec<(Cid, u32, std::time::SystemTime)>> {
        let mut out = Vec::new();
        for path in list_files_two_level(&self.root.join("blocks")).await? {
            let mtime = tokio::fs::metadata(&path).await?.modified()?;
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            // `<hash>` (canonical, generation 0) or `<hash>.<generation>`.
            let (hash_hex, generation) = match name.split_once('.') {
                Some((h, g)) => {
                    let Ok(generation) = g.parse::<u32>() else {
                        continue;
                    };
                    (h, generation)
                }
                None => (name, 0),
            };
            let Ok(hash) = Cid::from_hex(hash_hex) else {
                continue;
            };
            out.push((hash, generation, mtime));
        }
        Ok(out)
    }

    async fn list_stored_objects(&self) -> Result<Vec<(Cid, std::time::SystemTime)>> {
        let mut out = Vec::new();
        for path in list_files_two_level(&self.root.join("objects")).await? {
            let mtime = tokio::fs::metadata(&path).await?.modified()?;
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let Ok(cid) = Cid::from_hex(name) else {
                continue;
            };
            out.push((cid, mtime));
        }
        Ok(out)
    }
}

impl FilesystemStore {
    async fn read_block(&self, cid: &Cid, block: &BlockRef) -> Result<Vec<u8>> {
        tokio::fs::read(self.block_path(&block.hash, block.generation))
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
    use tokio::io::AsyncWriteExt;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!(".tmp.{}.{n}", std::process::id()));
    let mut file = tokio::fs::File::create(&tmp).await?;
    file.write_all(bytes).await?;
    // Fsync the data before publishing the file: bytes and the subsequent
    // rename must be durable before an index row that references the block
    // commits (I2 — see storage-gc-verification.md).
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&tmp, path).await?;
    if let Some(parent) = path.parent() {
        fsync_dir(parent).await?;
        // A newly-created shard directory also needs its entry in blocks/ or
        // objects/ to be durable. This is harmless for established shards.
        if let Some(grandparent) = parent.parent() {
            fsync_dir(grandparent).await?;
        }
    }
    Ok(())
}

/// Fsync a directory so entries created, renamed, or removed within it survive
/// a crash.
async fn fsync_dir(path: &Path) -> Result<()> {
    tokio::fs::File::open(path).await?.sync_all().await?;
    Ok(())
}

/// Remove `path`, treating an already-absent file as success — block and object
/// deletion are idempotent (a retried GC pass must not fail on a file a prior
/// pass already unlinked).
async fn remove_file_idempotent(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
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
        let store = FilesystemStore::with_params(dir.path(), ChunkParams::small());
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

    #[test]
    fn manifest_without_generation_field_reads_as_zero() {
        // A pre-M1c manifest had no `generation` on its block refs.
        let json = r#"{"size":3,"blocks":[{"hash":"deadbeef","len":3}]}"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.blocks[0].generation, 0);
    }

    #[tokio::test]
    async fn remove_deletes_the_object_but_keeps_blocks() {
        let (_dir, store) = store();
        let content = vec![3u8; 30_000];
        let cid = store.put(&content).await.unwrap();
        let blocks = store.blocks(&cid).await.unwrap();
        assert!(!blocks.is_empty());

        store.remove(&cid).await.unwrap();
        assert!(!store.has(&cid).await.unwrap());
        assert!(matches!(
            store.get(&cid).await.unwrap_err(),
            StoreError::NotFound(_)
        ));
        // Blocks are reference-counted; remove() leaves them for the collector.
        for b in &blocks {
            assert!(
                store.has_block(b, 0).await.unwrap(),
                "block {b} should remain"
            );
        }
    }

    #[tokio::test]
    async fn deletion_conformance() {
        let (_dir, store) = store();
        let big = vec![5u8; 40_000];
        for content in [&b""[..], &b"a small object"[..], big.as_slice()] {
            crate::conformance::removal(&store, content).await.unwrap();
        }
        crate::conformance::block_removal(&store, &big)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn canonical_and_minted_generations_use_distinct_paths() {
        let (_dir, store) = store();
        let hash = Cid::of(b"a block").to_hex();
        assert_eq!(
            store.block_path(&hash, 0).file_name().unwrap(),
            hash.as_str()
        );
        assert_ne!(store.block_path(&hash, 0), store.block_path(&hash, 1));
    }

    #[tokio::test]
    async fn enumerates_stored_blocks_and_objects() {
        use std::collections::HashSet;
        let (_dir, store) = store();
        let content = vec![4u8; 30_000]; // multi-block
        let cid = store.put(&content).await.unwrap();
        let manifest_blocks = store.blocks(&cid).await.unwrap();

        // The enumerated block set matches the manifest exactly (no orphans, none
        // missing), all at generation 0 for a fresh put, each with an mtime.
        let listed = store.list_stored_blocks().await.unwrap();
        assert!(!listed.is_empty());
        for (_hash, generation, _mtime) in &listed {
            assert_eq!(*generation, 0, "a fresh put is generation 0");
        }
        let listed_hashes: HashSet<_> = listed.iter().map(|(h, _, _)| *h).collect();
        let expected: HashSet<_> = manifest_blocks.into_iter().collect();
        assert_eq!(listed_hashes, expected);

        // The one object manifest is enumerated.
        let objects = store.list_stored_objects().await.unwrap();
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].0, cid);
    }
}
