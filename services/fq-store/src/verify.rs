//! The invariant oracle — runnable checks of the storage-GC correctness claims
//! (`docs/design/storage-gc-verification.md`) against the *real* index + content
//! store. It is the executable form of the invariants the TLA⁺ model checks
//! abstractly, and it doubles as the core of the reachability audit (M1c slice 6).
//!
//! Today it covers the claims expressible on the current schema:
//!
//! - **S1 — no lost live block:** every current name's object is fully
//!   retrievable (no referenced block file is missing).
//! - **object refcount consistency:** an object's stored refcount equals the
//!   number of name-version rows that reference it.
//! - **I4 — counts dominate references:** a block's refcount is at least the
//!   number of live objects that reference it.
//! - **I5 — manifests resolve:** every block a live object references has a
//!   positive refcount (it is not a GC candidate).
//! - refcounts never go negative.
//!
//! I1 (one available generation) and I3 (no unlink under reference) arrive with
//! the `available`/`gen` columns (slice 3).

use std::collections::HashMap;

use crate::{Cid, ContentStore, IndexSnapshot, NameIndex, Result};

/// A violated invariant. The oracle reports every violation it finds rather than
/// stopping at the first, so a single check pinpoints all the damage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// **S1.** A live name's content is not fully retrievable — a referenced
    /// block file is missing. This is the forbidden state; it must never occur.
    LostLiveBlock { name: String, object: Cid },
    /// An object's stored refcount disagrees with the number of name-version
    /// rows referencing it.
    ObjectRefcountDrift { object: Cid, stored: i64, name_refs: i64 },
    /// **I4.** A block's refcount is below the number of live objects that
    /// reference it — it could be reclaimed out from under a live object.
    BlockRefcountTooLow { block: Cid, stored: i64, live_refs: i64 },
    /// **I5.** A block referenced by a live object has a non-positive refcount,
    /// so it is (wrongly) a GC candidate.
    LiveBlockReclaimable { object: Cid, block: Cid, refcount: i64 },
    /// A refcount went negative.
    NegativeRefcount { kind: &'static str, cid: Cid, refcount: i64 },
}

/// Check the invariants over an index `snapshot` and the live `store`. Returns
/// every violation found (empty ⇒ all invariants hold).
pub async fn check<C: ContentStore + ?Sized>(
    snapshot: &IndexSnapshot,
    store: &C,
) -> Result<Vec<Violation>> {
    let mut violations = Vec::new();

    let obj_rc: HashMap<Cid, i64> = snapshot.objects.iter().copied().collect();
    let blk_rc: HashMap<Cid, i64> = snapshot.blocks.iter().copied().collect();
    let name_refs: HashMap<Cid, i64> = snapshot.name_refs.iter().copied().collect();

    // object CID -> the blocks it references.
    let mut edges: HashMap<Cid, Vec<Cid>> = HashMap::new();
    for (object, block) in &snapshot.object_blocks {
        edges.entry(*object).or_default().push(*block);
    }

    // Refcounts never go negative.
    for (cid, rc) in &snapshot.objects {
        if *rc < 0 {
            violations.push(Violation::NegativeRefcount { kind: "object", cid: *cid, refcount: *rc });
        }
    }
    for (cid, rc) in &snapshot.blocks {
        if *rc < 0 {
            violations.push(Violation::NegativeRefcount { kind: "block", cid: *cid, refcount: *rc });
        }
    }

    // Object refcount consistency: stored == name-version reference count.
    for (object, rc) in &snapshot.objects {
        let nr = name_refs.get(object).copied().unwrap_or(0);
        if *rc != nr {
            violations.push(Violation::ObjectRefcountDrift {
                object: *object,
                stored: *rc,
                name_refs: nr,
            });
        }
    }

    // The number of live objects referencing each block.
    let mut live_refs: HashMap<Cid, i64> = HashMap::new();
    for (object, blocks) in &edges {
        if obj_rc.get(object).copied().unwrap_or(0) > 0 {
            for block in blocks {
                *live_refs.entry(*block).or_default() += 1;
            }
        }
    }

    // I4: block refcount dominates the live reference count.
    for (block, live) in &live_refs {
        let stored = blk_rc.get(block).copied().unwrap_or(0);
        if stored < *live {
            violations.push(Violation::BlockRefcountTooLow { block: *block, stored, live_refs: *live });
        }
    }

    // I5: every block a live object references has a positive refcount.
    for (object, blocks) in &edges {
        if obj_rc.get(object).copied().unwrap_or(0) > 0 {
            for block in blocks {
                let rc = blk_rc.get(block).copied().unwrap_or(0);
                if rc < 1 {
                    violations.push(Violation::LiveBlockReclaimable {
                        object: *object,
                        block: *block,
                        refcount: rc,
                    });
                }
            }
        }
    }

    // S1: every current name's object is fully retrievable.
    for (name, object) in &snapshot.current_names {
        if store.get(object).await.is_err() {
            violations.push(Violation::LostLiveBlock { name: name.clone(), object: *object });
        }
    }

    Ok(violations)
}

/// Snapshot `index` and check the invariants against `store`.
pub async fn check_index<N, C>(index: &N, store: &C) -> Result<Vec<Violation>>
where
    N: NameIndex + ?Sized,
    C: ContentStore + ?Sized,
{
    let snapshot = index.snapshot().await?;
    check(&snapshot, store).await
}

/// Test helper: panic with the violations if any invariant fails.
pub async fn assert_clean<N, C>(index: &N, store: &C)
where
    N: NameIndex + ?Sized,
    C: ContentStore + ?Sized,
{
    let violations = check_index(index, store).await.expect("snapshot the index");
    assert!(violations.is_empty(), "invariant violations: {violations:#?}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{ChunkParams, FilesystemStore};
    use crate::{Repository, SqliteNameIndex};
    use std::path::{Path, PathBuf};

    async fn repo() -> (tempfile::TempDir, Repository<FilesystemStore, SqliteNameIndex>) {
        let dir = tempfile::tempdir().unwrap();
        let cas = dir.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let store = FilesystemStore::with_params(cas, ChunkParams { min: 256, avg: 1024, max: 4096 });
        let index = SqliteNameIndex::open(dir.path().join("index.db")).await.unwrap();
        (dir, Repository::new(store, index))
    }

    /// First regular file found under `dir` (recursive), if any.
    fn first_file(dir: &Path) -> Option<PathBuf> {
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let entries = std::fs::read_dir(&d).ok()?;
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    return Some(path);
                }
            }
        }
        None
    }

    #[tokio::test]
    async fn clean_store_has_no_violations() {
        let (_d, r) = repo().await;
        r.put("a.b", b"hello world, this is content").await.unwrap();
        r.put("a.c", b"hello world, this is content").await.unwrap(); // shares the object
        r.put("d", &vec![9u8; 6000]).await.unwrap();
        assert!(check_index(r.index(), r.content()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn detects_a_lost_live_block() {
        let (d, r) = repo().await;
        r.put("doc", &vec![7u8; 8192]).await.unwrap();
        assert!(check_index(r.index(), r.content()).await.unwrap().is_empty());

        // Induce the forbidden state: delete a block file under a live name.
        let block = first_file(&d.path().join("cas").join("blocks")).expect("a block file");
        std::fs::remove_file(block).unwrap();

        let v = check_index(r.index(), r.content()).await.unwrap();
        assert!(
            v.iter().any(|x| matches!(x, Violation::LostLiveBlock { .. })),
            "expected a LostLiveBlock, got {v:#?}"
        );
    }

    #[tokio::test]
    async fn detects_object_refcount_drift() {
        let (_d, r) = repo().await;
        r.put("a", b"some content for an object").await.unwrap();
        let mut snap = r.index().snapshot().await.unwrap();
        snap.objects[0].1 += 5; // inflate an object refcount
        let v = check(&snap, r.content()).await.unwrap();
        assert!(v.iter().any(|x| matches!(x, Violation::ObjectRefcountDrift { .. })), "{v:#?}");
    }

    #[tokio::test]
    async fn detects_live_block_that_is_reclaimable() {
        let (_d, r) = repo().await;
        r.put("a", &vec![3u8; 4096]).await.unwrap();
        let mut snap = r.index().snapshot().await.unwrap();
        for b in &mut snap.blocks {
            b.1 = 0; // a live object's blocks dropped to GC-candidate
        }
        let v = check(&snap, r.content()).await.unwrap();
        assert!(v.iter().any(|x| matches!(x, Violation::LiveBlockReclaimable { .. })), "{v:#?}");
        assert!(v.iter().any(|x| matches!(x, Violation::BlockRefcountTooLow { .. })), "{v:#?}");
    }
}
