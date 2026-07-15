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
/// object *at the instant the collector is about to unlink its manifest*, which
/// pre-fix left a live name resolving to a missing manifest — the object
/// forbidden state (S1-obj).
///
/// The interleaving is forced through the collector's `before_unlink` seam: at
/// the seam the object has just been **claimed** and its manifest is still
/// present, and the callback performs the racing `bind` right there. Under
/// back-off (ADR-0030) the bind meets a claimed object and is **refused**
/// (`Conflict`), so no name is resurrected and the store stays clean. On the
/// pre-fix collector (no claim, no `available` bit) the bind resurrected the
/// object and the collector then unlinked a live object's manifest — the
/// assertion below caught that as `LostLiveBlock`. Doing the bind inside the
/// callback makes the race deterministic without threads.
///
/// So this test is **green with back-off and red without it** — the regression
/// that pins the fix.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bind_alias_racing_collect_is_refused_no_s1_obj() {
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

    // At the seam — object just claimed, manifest still present — race a `bind`
    // that would resurrect it under a new name. Under back-off this is refused
    // (Conflict); pre-fix it succeeded and caused S1-obj. Either outcome is
    // tolerated here (the store-clean assertion below is what discriminates), so
    // the bind result is deliberately ignored. Run inline via a nested block_on
    // (valid on a multi-thread runtime inside block_in_place).
    {
        let repo = repo.clone();
        fail::cfg_callback("fq_store::gc::obj::before_unlink", move || {
            let repo = repo.clone();
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    let _ = repo.bind("b", &cid).await;
                });
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
        "back-off must refuse the racing bind — a violation means S1-obj was reached (#173 regression): {violations:#?}"
    );
}
