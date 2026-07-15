#![cfg(feature = "failpoints")]
//! Adversarial interleaving tests for GC, driven through the protocol-step
//! seams (`fail_point!` in the collector and repository).
//!
//! Requires `--features failpoints` — the seams compile to nothing otherwise, so
//! this file is empty without it. The store CI job enables the feature (see
//! `docs/design/committed/storage-concurrency-verification.md`). Phase 1 of that
//! plan: a *red* regression that reaches the object forbidden state (S1-obj) on
//! today's code; back-off (ADR-0030) turns it green.

use std::path::Path;
use std::sync::Arc;

use fq_store::fs::{ChunkParams, FilesystemStore};
use fq_store::{
    Collector, ContentStore, NameIndex, ReferenceCollector, Repository, SqliteNameIndex, verify,
};

async fn open_repo(dir: &Path) -> Repository<FilesystemStore, SqliteNameIndex> {
    let cas = dir.join("cas");
    std::fs::create_dir_all(&cas).unwrap();
    let store = FilesystemStore::with_params(cas, ChunkParams::small());
    let index = SqliteNameIndex::open(dir.join("index.db")).await.unwrap();
    Repository::new(store, index)
}

/// #173, deterministically. A `bind`-alias resurrects a dead-but-uncollected
/// object *at the instant the collector is about to unlink its manifest*, leaving
/// a live name that resolves to a missing manifest — the object forbidden state
/// (S1-obj).
///
/// The interleaving is forced through the collector's `before_unlink` seam: the
/// object has been selected as unreferenced and its manifest is still present,
/// and the seam callback performs the resurrecting `bind` right there — so when
/// the collector resumes it unlinks the manifest of a now-live object, and its
/// `delete_object` (guarded on refcount 0) then no-ops, leaving the row. Doing
/// the bind inside the callback makes the race deterministic without threads.
///
/// The final assertion — the store is clean — **fails on today's code** (the
/// oracle reports the lost manifest) and must pass once back-off lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bind_alias_racing_collect_reaches_s1_obj() {
    let scenario = fail::FailScenario::setup();

    let dir = tempfile::tempdir().unwrap();
    let repo = Arc::new(open_repo(dir.path()).await);

    // A dead-but-uncollected object: put under "a", then unbind — object refcount
    // drops to 0 (a GC candidate) while its manifest stays on disk.
    let cid = repo.put("a", &vec![7u8; 20_000]).await.unwrap();
    repo.unbind("a").await.unwrap();
    assert_eq!(
        repo.index().unreferenced_objects().await.unwrap(),
        vec![cid],
        "precondition: the object is a GC candidate"
    );
    assert!(
        repo.content().has(&cid).await.unwrap(),
        "precondition: its manifest is present"
    );

    // At the seam — object selected, manifest still present — resurrect it under a
    // new name. The bind is async; run it inline via a nested block_on (valid on a
    // multi-thread runtime inside block_in_place).
    {
        let repo = repo.clone();
        fail::cfg_callback("fq_store::gc::obj::before_unlink", move || {
            let repo = repo.clone();
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(async { repo.bind("b", &cid).await.unwrap() });
            });
        })
        .unwrap();
    }

    ReferenceCollector
        .collect(&*repo)
        .await
        .expect("collect() should not error");

    let violations = verify::check_index(repo.index(), repo.content())
        .await
        .unwrap();
    scenario.teardown();

    assert!(
        violations.is_empty(),
        "S1-obj reached — the live name `b` resolves to a missing manifest (#173): {violations:#?}"
    );
}
