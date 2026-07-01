//! The invariant oracle — runnable checks of the storage-GC correctness claims
//! (`docs/design/storage-gc-verification.md`) against the *real* index + content
//! store. It is the runnable counterpart of the invariants the TLA⁺ model checks
//! abstractly — those expressible on the live index; durability (I2) is left to
//! the model and the fsync tests — and it doubles as the core of the
//! reachability audit (M1c).
//!
//! Today it covers the claims expressible on the current schema:
//!
//! - **S1 — no lost live block:** every current name's object is fully
//!   retrievable (no referenced block file is missing).
//! - **object refcount consistency:** an object's stored refcount equals the
//!   number of name-version rows that reference it.
//! - **I1 — one available generation:** at most one generation per hash is
//!   available to writers.
//! - **I3 — no unlink under reference:** a claimed (unavailable) generation has
//!   no live references.
//! - **I4 — counts dominate references:** a block's refcount is at least the
//!   number of live objects that reference it.
//! - **I5 — manifests resolve:** every block a live object references has a
//!   positive refcount (it is not a GC candidate).
//! - refcounts never go negative.

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
    ObjectRefcountDrift {
        object: Cid,
        stored: i64,
        name_refs: i64,
    },
    /// **I4.** A block's refcount is below the number of live objects that
    /// reference it — it could be reclaimed out from under a live object.
    BlockRefcountTooLow {
        block: Cid,
        stored: i64,
        live_refs: i64,
    },
    /// **I5.** A block referenced by a live object has a non-positive refcount,
    /// so it is (wrongly) a GC candidate.
    LiveBlockReclaimable {
        object: Cid,
        block: Cid,
        refcount: i64,
    },
    /// A refcount went negative.
    NegativeRefcount {
        kind: &'static str,
        cid: Cid,
        refcount: i64,
    },
    /// **I1.** More than one generation of a block hash is `available` — a
    /// writer could reserve a generation GC is about to reap.
    MultipleAvailableGenerations { hash: Cid, available: i64 },
    /// **I3.** A claimed (unavailable) generation still has live references, so
    /// GC would unlink a block under a reference.
    ClaimedBlockHasRefs {
        hash: Cid,
        generation: u32,
        refcount: i64,
    },
}

/// Check the invariants over an index `snapshot` and the live `store`. Returns
/// every violation found (empty ⇒ all invariants hold).
pub async fn check<C: ContentStore + ?Sized>(
    snapshot: &IndexSnapshot,
    store: &C,
) -> Result<Vec<Violation>> {
    let mut violations = Vec::new();

    let obj_rc: HashMap<Cid, i64> = snapshot.objects.iter().copied().collect();
    let name_refs: HashMap<Cid, i64> = snapshot.name_refs.iter().copied().collect();
    // (block hash, generation) -> refcount, for I4/I5. An object's edge records
    // the exact generation it references.
    let blk_rc: HashMap<(Cid, u32), i64> = snapshot
        .blocks
        .iter()
        .map(|b| ((b.hash, b.generation), b.refcount))
        .collect();

    // object CID -> the (block, generation)s it references.
    let mut edges: HashMap<Cid, Vec<(Cid, u32)>> = HashMap::new();
    for edge in &snapshot.object_blocks {
        edges
            .entry(edge.object)
            .or_default()
            .push((edge.block, edge.generation));
    }

    // Refcounts never go negative.
    for (cid, rc) in &snapshot.objects {
        if *rc < 0 {
            violations.push(Violation::NegativeRefcount {
                kind: "object",
                cid: *cid,
                refcount: *rc,
            });
        }
    }
    for b in &snapshot.blocks {
        if b.refcount < 0 {
            violations.push(Violation::NegativeRefcount {
                kind: "block",
                cid: b.hash,
                refcount: b.refcount,
            });
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

    // The number of live objects referencing each (block, generation).
    let mut live_refs: HashMap<(Cid, u32), i64> = HashMap::new();
    for (object, blocks) in &edges {
        if obj_rc.get(object).copied().unwrap_or(0) > 0 {
            for bg in blocks {
                *live_refs.entry(*bg).or_default() += 1;
            }
        }
    }

    // I4: block refcount dominates the live reference count.
    for (bg, live) in &live_refs {
        let stored = blk_rc.get(bg).copied().unwrap_or(0);
        if stored < *live {
            violations.push(Violation::BlockRefcountTooLow {
                block: bg.0,
                stored,
                live_refs: *live,
            });
        }
    }

    // I5: every block a live object references has a positive refcount.
    for (object, blocks) in &edges {
        if obj_rc.get(object).copied().unwrap_or(0) > 0 {
            for bg in blocks {
                let rc = blk_rc.get(bg).copied().unwrap_or(0);
                if rc < 1 {
                    violations.push(Violation::LiveBlockReclaimable {
                        object: *object,
                        block: bg.0,
                        refcount: rc,
                    });
                }
            }
        }
    }

    // I1: at most one available generation per block hash.
    let mut available: HashMap<Cid, i64> = HashMap::new();
    for b in &snapshot.blocks {
        if b.available {
            *available.entry(b.hash).or_default() += 1;
        }
    }
    for (hash, count) in &available {
        if *count > 1 {
            violations.push(Violation::MultipleAvailableGenerations {
                hash: *hash,
                available: *count,
            });
        }
    }

    // I3: a claimed (unavailable) generation has no live references.
    for b in &snapshot.blocks {
        if !b.available && b.refcount != 0 {
            violations.push(Violation::ClaimedBlockHasRefs {
                hash: b.hash,
                generation: b.generation,
                refcount: b.refcount,
            });
        }
    }

    // S1: every current name's object is retrievable — its manifest is present
    // and every block it references has a file. Existence checks (not a full
    // read) keep this cheap, and a transient I/O error is skipped rather than
    // misread as a lost block (the audit re-runs).
    for (name, object) in &snapshot.current_names {
        let mut lost = matches!(store.has(object).await, Ok(false)); // manifest gone
        if !lost {
            for &(block, generation) in edges.get(object).into_iter().flatten() {
                if let Ok(false) = store.has_block(&block, generation).await {
                    lost = true; // a referenced block file is gone
                    break;
                }
            }
        }
        if lost {
            violations.push(Violation::LostLiveBlock {
                name: name.clone(),
                object: *object,
            });
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
    assert!(
        violations.is_empty(),
        "invariant violations: {violations:#?}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::FilesystemStore;
    use crate::{Repository, SqliteNameIndex};
    use std::path::{Path, PathBuf};

    async fn repo() -> (
        tempfile::TempDir,
        Repository<FilesystemStore, SqliteNameIndex>,
    ) {
        crate::test_support::repo().await
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
        assert!(
            check_index(r.index(), r.content())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn detects_a_lost_live_block() {
        let (d, r) = repo().await;
        r.put("doc", &vec![7u8; 8192]).await.unwrap();
        assert!(
            check_index(r.index(), r.content())
                .await
                .unwrap()
                .is_empty()
        );

        // Induce the forbidden state: delete a block file under a live name.
        let block = first_file(&d.path().join("cas").join("blocks")).expect("a block file");
        std::fs::remove_file(block).unwrap();

        let v = check_index(r.index(), r.content()).await.unwrap();
        assert!(
            v.iter()
                .any(|x| matches!(x, Violation::LostLiveBlock { .. })),
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
        assert!(
            v.iter()
                .any(|x| matches!(x, Violation::ObjectRefcountDrift { .. })),
            "{v:#?}"
        );
    }

    #[tokio::test]
    async fn detects_live_block_that_is_reclaimable() {
        let (_d, r) = repo().await;
        r.put("a", &vec![3u8; 4096]).await.unwrap();
        let mut snap = r.index().snapshot().await.unwrap();
        for b in &mut snap.blocks {
            b.refcount = 0; // a live object's blocks dropped to GC-candidate
        }
        let v = check(&snap, r.content()).await.unwrap();
        assert!(
            v.iter()
                .any(|x| matches!(x, Violation::LiveBlockReclaimable { .. })),
            "{v:#?}"
        );
        assert!(
            v.iter()
                .any(|x| matches!(x, Violation::BlockRefcountTooLow { .. })),
            "{v:#?}"
        );
    }

    #[tokio::test]
    async fn detects_multiple_available_generations() {
        let (_d, r) = repo().await;
        r.put("a", &vec![1u8; 4096]).await.unwrap();
        let mut snap = r.index().snapshot().await.unwrap();
        let mut extra = snap.blocks[0]; // a second available generation of the hash
        extra.generation = 1;
        snap.blocks.push(extra);
        let v = check(&snap, r.content()).await.unwrap();
        assert!(
            v.iter()
                .any(|x| matches!(x, Violation::MultipleAvailableGenerations { .. })),
            "{v:#?}"
        );
    }

    #[tokio::test]
    async fn detects_claimed_block_with_references() {
        let (_d, r) = repo().await;
        r.put("a", &vec![1u8; 4096]).await.unwrap();
        let mut snap = r.index().snapshot().await.unwrap();
        snap.blocks[0].available = false; // claimed while still referenced
        let v = check(&snap, r.content()).await.unwrap();
        assert!(
            v.iter()
                .any(|x| matches!(x, Violation::ClaimedBlockHasRefs { .. })),
            "{v:#?}"
        );
    }
}
