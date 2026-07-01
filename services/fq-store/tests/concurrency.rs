//! Concurrency stress for the online-reclaim protocol (M1c slice 5d).
//!
//! Writers and the collector pound *shared* blocks simultaneously: four writers
//! repeatedly put then delete the same content — so each shared block is
//! reserved, released, then falls to refcount 0 and becomes claimable in quick
//! succession — while a collector reclaims in a tight loop. Their reserve and
//! claim compare-and-swaps race on the index's single writer; whenever GC claims
//! a generation a writer is re-putting, the writer mints a fresh one
//! (generation-on-collision) instead of blocking or failing.
//!
//! The exactly-one-wins discipline is checked indirectly but decisively: no put
//! or collect may error, every name a writer leaves bound must read back its
//! content, and after a final settling pass the invariant oracle must be clean —
//! no lost live block, no refcount drift, at most one available generation per
//! hash.

use std::sync::Arc;

use fq_store::fs::{ChunkParams, FilesystemStore};
use fq_store::{Collector, ReferenceCollector, Repository, SqliteNameIndex, verify};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writers_and_collector_race_on_shared_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let cas = dir.path().join("cas");
    std::fs::create_dir_all(&cas).unwrap();
    let store = FilesystemStore::with_params(
        cas,
        ChunkParams {
            min: 256,
            avg: 1024,
            max: 4096,
        },
    );
    let index = SqliteNameIndex::open(dir.path().join("index.db"))
        .await
        .unwrap();
    let repo = Arc::new(Repository::new(store, index));

    // One shared value → shared blocks every writer contends on.
    let shared = vec![7u8; 6000];

    let mut tasks = Vec::new();
    for w in 0..4 {
        let repo = repo.clone();
        let content = shared.clone();
        tasks.push(tokio::spawn(async move {
            let name = format!("obj{w}");
            // Churn: bind then unbind repeatedly, so the shared blocks cycle
            // through reservable → released → claimable while GC races alongside.
            for _ in 0..15 {
                repo.put(&name, &content).await.unwrap();
                repo.delete(&name).await.unwrap();
            }
            repo.put(&name, &content).await.unwrap(); // leave it bound
        }));
    }
    // The collector, concurrent with the writers, claims whatever hits refcount 0.
    {
        let repo = repo.clone();
        tasks.push(tokio::spawn(async move {
            for _ in 0..60 {
                ReferenceCollector.collect(&repo).await.unwrap();
                tokio::task::yield_now().await;
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }

    // Every writer left its name bound — each must read back the shared content,
    // proving no live block was reclaimed out from under it.
    for w in 0..4 {
        assert_eq!(
            repo.get(&format!("obj{w}")).await.unwrap(),
            shared,
            "obj{w} lost its content"
        );
    }
    // A final settling pass reaps any generations orphaned by the storm, then the
    // oracle must find a wholly consistent index.
    ReferenceCollector.collect(&repo).await.unwrap();
    verify::assert_clean(repo.index(), repo.content()).await;
}
