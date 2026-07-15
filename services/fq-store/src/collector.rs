//! The reference garbage collector (M1c) — the online reclaim worker.
//!
//! It reclaims what the index reports unreferenced, in the order the protocol
//! requires (`docs/design/committed/storage-garbage-collection.md`): an object's manifest
//! then its row; a block's `claim → unlink → delete`. Claiming is the GC
//! compare-and-swap — a writer that reserves first wins and the block is left
//! alone. This is the *online* collector; the reachability audit (still to come)
//! is the systematic backstop the model proves is also required for reclamation
//! liveness.

use async_trait::async_trait;

use crate::{BlockStore, NameIndex, Repository, Result};

/// What a collection pass reclaimed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reclaimed {
    /// Objects (manifests) removed.
    pub objects: usize,
    /// Block generations unlinked.
    pub blocks: usize,
}

/// A garbage collector over a [`Repository`]. Pluggable so retention policies
/// (online batches, full sweep, …) can vary.
#[async_trait]
pub trait Collector<C: BlockStore, N: NameIndex>: Send + Sync {
    /// Run one reclamation pass, returning what was freed.
    async fn collect(&self, repo: &Repository<C, N>) -> Result<Reclaimed>;
}

/// The reference collector: reclaim every currently-unreferenced object and
/// block in a single pass.
pub struct ReferenceCollector;

#[async_trait]
impl<C: BlockStore, N: NameIndex> Collector<C, N> for ReferenceCollector {
    async fn collect(&self, repo: &Repository<C, N>) -> Result<Reclaimed> {
        let content = repo.content();
        let index = repo.index();
        let mut reclaimed = Reclaimed::default();

        // Unreferenced objects: remove the manifest, then the row + its edges.
        for cid in index.unreferenced_objects().await? {
            // Seam: the object has been selected as unreferenced but its manifest
            // is still present. Pausing here lets a test interleave a concurrent
            // `bind`-alias that resurrects the object over the manifest we are
            // about to unlink — the #173 race. Zero-cost unless `failpoints` is on.
            fail::fail_point!("fq_store::gc::obj::before_unlink");
            content.remove(&cid).await?;
            fail::fail_point!("fq_store::gc::obj::before_delete");
            index.delete_object(&cid).await?;
            reclaimed.objects += 1;
        }

        // Unreferenced blocks: claim → unlink → delete. A writer that reserves
        // first makes the claim fail (refcount > 0) and the block is left alone.
        for (hash, generation, available) in index.claimable_blocks().await? {
            let owned = if available {
                index.claim_block(&hash, generation).await?
            } else {
                // An orphaned claim (a crash mid-reclaim): adopt and finish it.
                true
            };
            if owned {
                content.remove_block(&hash, generation).await?;
                index.delete_block(&hash, generation).await?;
                reclaimed.blocks += 1;
            }
        }

        Ok(reclaimed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::FilesystemStore;
    use crate::{SqliteNameIndex, verify};

    async fn repository() -> (
        tempfile::TempDir,
        Repository<FilesystemStore, SqliteNameIndex>,
    ) {
        crate::test_support::repo().await
    }

    #[tokio::test]
    async fn reclaims_a_deleted_object_and_its_blocks() {
        let (_d, repo) = repository().await;
        repo.put("a", &vec![1u8; 20_000]).await.unwrap();
        repo.put("b", &vec![2u8; 20_000]).await.unwrap();
        repo.unbind("a").await.unwrap();

        let reclaimed = ReferenceCollector.collect(&repo).await.unwrap();
        assert!(
            reclaimed.objects >= 1 && reclaimed.blocks >= 1,
            "{reclaimed:?}"
        );

        assert!(repo.resolve("a").await.unwrap().is_none());
        assert_eq!(repo.get("b").await.unwrap(), vec![2u8; 20_000]); // b survives
        assert!(
            repo.index()
                .unreferenced_objects()
                .await
                .unwrap()
                .is_empty()
        );
        assert!(repo.index().unreferenced_blocks().await.unwrap().is_empty());
        verify::assert_clean(repo.index(), repo.content()).await;
    }

    #[tokio::test]
    async fn keeps_blocks_shared_with_a_live_object() {
        let (_d, repo) = repository().await;
        // Two objects sharing a prefix → shared blocks.
        let mut a = vec![0u8; 40_000];
        for (i, byte) in a.iter_mut().enumerate() {
            *byte = (i % 251) as u8;
        }
        let mut b = a.clone();
        b.extend_from_slice(b" a distinct suffix");
        repo.put("a", &a).await.unwrap();
        repo.put("b", &b).await.unwrap(); // shares a's blocks

        repo.unbind("a").await.unwrap();
        ReferenceCollector.collect(&repo).await.unwrap();

        // b is intact — its blocks (shared with the late a) were not reclaimed.
        assert_eq!(repo.get("b").await.unwrap(), b);
        verify::assert_clean(repo.index(), repo.content()).await;
    }

    #[tokio::test]
    async fn collect_is_idempotent_and_invisible() {
        let (_d, repo) = repository().await;
        repo.put("x", b"content x").await.unwrap();
        let before = repo.get("x").await.unwrap();
        // A pass with nothing to reclaim changes nothing.
        assert_eq!(
            ReferenceCollector.collect(&repo).await.unwrap(),
            Reclaimed::default()
        );
        assert_eq!(repo.get("x").await.unwrap(), before);
    }
}
