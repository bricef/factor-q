//! The reachability audit (M1c slice 6) — the strong-fairness backstop.
//!
//! The online [`ReferenceCollector`] reclaims what the index reports
//! unreferenced, but only opportunistically — the TLA⁺ model shows weak fairness
//! alone can leave a crash-orphaned generation unreclaimed forever
//! (`docs/design/storage-gc-verification.md`). The audit is the systematic sweep
//! that closes that gap. It:
//!
//! 1. **reclaims completely** — runs the collector to guarantee every dead block
//!    and object (including orphaned claims a crash left mid-reclaim) is freed;
//! 2. **reaps orphan files** — block/object files present on disk with no index
//!    row, older than a reap **grace** (younger files may be an in-flight write
//!    still inside the fsync-before-insert window, so they are spared);
//! 3. **reconciles refcount drift** and **alarms on the forbidden state** — slice
//!    6c.
//!
//! It reads the real index + filesystem; the invariant oracle ([`crate::verify`])
//! is its correctness core.

use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use crate::verify::Violation;
use crate::{
    BlockStore, Cid, Collector, NameIndex, Reclaimed, ReferenceCollector, Repository, Result,
};

/// What one audit pass did. All-zero / empty is a clean, fully-reclaimed store.
#[derive(Debug, Clone, Default)]
pub struct AuditReport {
    /// Freed by the systematic reclaim pass (guaranteed, not best-effort).
    pub reclaimed: Reclaimed,
    /// Orphan block files (on disk, no index row, past grace) unlinked.
    pub orphan_blocks: usize,
    /// Orphan object manifests (on disk, no index row, past grace) unlinked.
    pub orphan_objects: usize,
    /// Refcounts corrected down to the recomputed truth (slice 6c).
    pub reconciled: usize,
    /// Invariant violations the audit will **not** auto-repair (slice 6c) — the
    /// forbidden state (a live object missing a block) or a refcount below truth.
    /// Empty means every invariant holds.
    pub alarms: Vec<Violation>,
}

/// The reachability auditor. `grace` is how long a file must have gone untouched
/// before it is eligible for reaping — it must exceed the write→fsync→insert
/// window so an in-flight write is never mistaken for an orphan.
pub struct ReachabilityAuditor;

impl ReachabilityAuditor {
    /// Run one audit pass over `repo`, reaping orphan files older than `grace`.
    pub async fn audit<C: BlockStore, N: NameIndex>(
        &self,
        repo: &Repository<C, N>,
        grace: Duration,
    ) -> Result<AuditReport> {
        let content = repo.content();
        let index = repo.index();

        // Phase 1 — systematic reclaim. Running the collector to completion is the
        // strong-fairness guarantee: every dead block/object and every orphaned
        // claim (a crash mid-reclaim) is freed, not merely eligible to be.
        let reclaimed = ReferenceCollector.collect(repo).await?;

        // Phase 2 — reap orphan files. A file whose identity has no index row is a
        // crash-orphaned write (fsync'd before its row committed). Only reap once
        // it is older than the grace, so a live in-flight write is left alone.
        let now = SystemTime::now();
        let snapshot = index.snapshot().await?;
        let rowed_blocks: HashSet<(Cid, u32)> = snapshot
            .blocks
            .iter()
            .map(|b| (b.hash, b.generation))
            .collect();
        let rowed_objects: HashSet<Cid> = snapshot.objects.iter().map(|(cid, _)| *cid).collect();

        let mut orphan_blocks = 0;
        for (hash, generation, mtime) in content.list_stored_blocks().await? {
            if !rowed_blocks.contains(&(hash, generation)) && aged(now, mtime, grace) {
                content.remove_block(&hash, generation).await?;
                orphan_blocks += 1;
            }
        }
        let mut orphan_objects = 0;
        for (cid, mtime) in content.list_stored_objects().await? {
            if !rowed_objects.contains(&cid) && aged(now, mtime, grace) {
                content.remove(&cid).await?;
                orphan_objects += 1;
            }
        }

        Ok(AuditReport {
            reclaimed,
            orphan_blocks,
            orphan_objects,
            reconciled: 0,
            alarms: Vec::new(),
        })
    }
}

/// Whether `mtime` is at least `grace` in the past relative to `now`. A file
/// dated in the future (clock skew) is treated as *not* aged — never reap on
/// uncertainty.
fn aged(now: SystemTime, mtime: SystemTime, grace: Duration) -> bool {
    now.duration_since(mtime)
        .map(|age| age >= grace)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentStore, verify};
    use std::time::Duration;

    const NO_GRACE: Duration = Duration::ZERO;
    const LONG_GRACE: Duration = Duration::from_secs(3600);

    #[tokio::test]
    async fn reaps_an_orphan_block_file_past_grace() {
        let (_d, repo) = crate::test_support::repo().await;
        // A block file written with no index row — a crash after write_block,
        // before mint_block.
        let bytes = b"an orphaned block, no row";
        let hash = Cid::of(bytes);
        repo.content().write_block(&hash, 0, bytes).await.unwrap();
        assert!(repo.content().has_block(&hash, 0).await.unwrap());

        let report = ReachabilityAuditor.audit(&repo, NO_GRACE).await.unwrap();
        assert_eq!(report.orphan_blocks, 1, "{report:?}");
        assert!(!repo.content().has_block(&hash, 0).await.unwrap());
        verify::assert_clean(repo.index(), repo.content()).await;
    }

    #[tokio::test]
    async fn spares_a_fresh_orphan_within_grace() {
        let (_d, repo) = crate::test_support::repo().await;
        let bytes = b"a just-written block still in flight";
        let hash = Cid::of(bytes);
        repo.content().write_block(&hash, 0, bytes).await.unwrap();

        // With a long grace the fresh file is an in-flight write, not an orphan.
        let report = ReachabilityAuditor.audit(&repo, LONG_GRACE).await.unwrap();
        assert_eq!(report.orphan_blocks, 0, "{report:?}");
        assert!(repo.content().has_block(&hash, 0).await.unwrap());
    }

    #[tokio::test]
    async fn completes_reclaim_and_leaves_live_data() {
        let (_d, repo) = crate::test_support::repo().await;
        repo.put("a", &vec![1u8; 20_000]).await.unwrap();
        repo.put("b", &vec![2u8; 20_000]).await.unwrap();
        repo.unbind("a").await.unwrap();

        // The audit alone reclaims a's dead object + blocks (no prior collect).
        let report = ReachabilityAuditor.audit(&repo, NO_GRACE).await.unwrap();
        assert!(
            report.reclaimed.objects >= 1 && report.reclaimed.blocks >= 1,
            "{report:?}"
        );
        assert!(repo.resolve("a").await.unwrap().is_none());
        assert_eq!(repo.get("b").await.unwrap(), vec![2u8; 20_000]);
        verify::assert_clean(repo.index(), repo.content()).await;
    }
}
