//! Unit tests for [`super`]. Extracted from the parent module so the
//! file that ships is the file you read (#390); `super::*` keeps the
//! same access it had inline.

use super::*;
use tempfile::TempDir;

async fn store() -> (TempDir, SqliteNameIndex) {
    let dir = tempfile::tempdir().unwrap();
    let s = SqliteNameIndex::open(dir.path().join("index.db"))
        .await
        .unwrap();
    (dir, s)
}

/// A distinct test CID derived from a label.
fn cid(label: &str) -> Cid {
    Cid::of(label.as_bytes())
}

fn sorted(mut v: Vec<Cid>) -> Vec<Cid> {
    v.sort_by_key(|c| c.to_hex());
    v
}

/// Mimic a put at the index layer: reserve (or mint) each unique block,
/// reserve the object (reserve-before-rely), then hand the reservations to
/// `bind`. `Conflict` (a claimed object) propagates as an error.
async fn reserve_and_bind(
    s: &SqliteNameIndex,
    name: &str,
    obj: &Cid,
    blocks: &[Cid],
) -> Result<()> {
    let mut reserved = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for b in blocks {
        if !seen.insert(*b) {
            continue;
        }
        let generation = match s.reserve_block(b).await? {
            Some(g) => g,
            None => {
                assert!(s.mint_block(b, 0).await?, "fresh block minted");
                0
            }
        };
        reserved.push((*b, generation));
    }
    let prev_rc = s
        .reserve_object(obj, true)
        .await?
        .expect("object not claimed in this test");
    s.bind(name, obj, &reserved, prev_rc).await
}

#[tokio::test]
async fn bind_and_resolve() {
    let (_d, s) = store().await;
    let c = cid("doc");
    reserve_and_bind(&s, "research.papers.doc1", &c, &[c])
        .await
        .unwrap();
    assert_eq!(s.resolve("research.papers.doc1").await.unwrap(), Some(c));
    assert_eq!(s.resolve("research.papers.nope").await.unwrap(), None);
}

#[tokio::test]
async fn rebind_keeps_history_newest_first() {
    let (_d, s) = store().await;
    let (v1, v2, v3) = (cid("v1"), cid("v2"), cid("v3"));
    reserve_and_bind(&s, "a.b", &v1, &[v1]).await.unwrap();
    reserve_and_bind(&s, "a.b", &v2, &[v2]).await.unwrap();
    reserve_and_bind(&s, "a.b", &v2, &[v2]).await.unwrap(); // no-op (same cid)
    reserve_and_bind(&s, "a.b", &v3, &[v3]).await.unwrap();
    assert_eq!(s.resolve("a.b").await.unwrap(), Some(v3));
    assert_eq!(s.history("a.b").await.unwrap(), vec![v3, v2, v1]);
}

#[tokio::test]
async fn list_is_segment_aware() {
    let (_d, s) = store().await;
    for name in ["a.b.c", "a.b.d", "a.x", "ab.c", "z"] {
        let c = cid(name);
        reserve_and_bind(&s, name, &c, &[c]).await.unwrap();
    }
    assert_eq!(s.list("a.b").await.unwrap(), vec!["a.b.c", "a.b.d"]);
    assert_eq!(s.list("a").await.unwrap(), vec!["a.b.c", "a.b.d", "a.x"]);
    assert_eq!(
        s.list("").await.unwrap(),
        vec!["a.b.c", "a.b.d", "a.x", "ab.c", "z"]
    );
}

#[tokio::test]
async fn unbind_unreferences_object_and_blocks() {
    let (_d, s) = store().await;
    let (obj, b1, b2) = (cid("obj"), cid("b1"), cid("b2"));
    reserve_and_bind(&s, "a", &obj, &[b1, b2]).await.unwrap();
    assert!(s.unreferenced_objects().await.unwrap().is_empty());

    s.unbind("a").await.unwrap();
    assert_eq!(s.resolve("a").await.unwrap(), None);
    assert_eq!(s.unreferenced_objects().await.unwrap(), vec![obj]);
    assert_eq!(
        sorted(s.unreferenced_blocks().await.unwrap()),
        sorted(vec![b1, b2])
    );
}

#[tokio::test]
async fn aliasing_holds_a_shared_object_live() {
    let (_d, s) = store().await;
    let (obj, b1, b2) = (cid("obj"), cid("b1"), cid("b2"));
    reserve_and_bind(&s, "name.one", &obj, &[b1, b2])
        .await
        .unwrap();
    reserve_and_bind(&s, "name.two", &obj, &[b1, b2])
        .await
        .unwrap(); // alias: refcount 2

    s.unbind("name.one").await.unwrap();
    // Still referenced by name.two — not a GC candidate.
    assert!(s.unreferenced_objects().await.unwrap().is_empty());
    assert!(s.unreferenced_blocks().await.unwrap().is_empty());

    s.unbind("name.two").await.unwrap();
    assert_eq!(s.unreferenced_objects().await.unwrap(), vec![obj]);
}

#[tokio::test]
async fn shared_blocks_stay_live_until_last_object_dies() {
    let (_d, s) = store().await;
    let (x, y, b1, b2, b3) = (cid("x"), cid("y"), cid("b1"), cid("b2"), cid("b3"));
    reserve_and_bind(&s, "x", &x, &[b1, b2]).await.unwrap();
    reserve_and_bind(&s, "y", &y, &[b2, b3]).await.unwrap(); // b2 shared by x and y

    s.unbind("x").await.unwrap();
    // x dead -> b1 reclaimable; b2 still held by y; b3 still held by y.
    assert_eq!(s.unreferenced_objects().await.unwrap(), vec![x]);
    assert_eq!(s.unreferenced_blocks().await.unwrap(), vec![b1]);
}

#[tokio::test]
async fn reserve_and_claim_linearise() {
    let (_d, s) = store().await;
    let h = cid("block");
    // Mint a fresh available generation (refcount 1 — the minter's hold).
    assert!(s.mint_block(&h, 0).await.unwrap(), "first mint inserts");
    // A second mint is refused while a generation is available (dedup).
    assert!(
        !s.mint_block(&h, 1).await.unwrap(),
        "mint refused while available"
    );

    // A reserve bumps the available generation; refcount is now 2.
    assert_eq!(s.reserve_block(&h).await.unwrap(), Some(0));
    // GC's claim loses against the live refcount.
    assert!(
        !s.claim_block(&h, 0).await.unwrap(),
        "claim loses to a reservation"
    );

    // Release both holds; the block is now dead (refcount 0, still available).
    s.release_block(&h, 0).await.unwrap();
    s.release_block(&h, 0).await.unwrap();
    // Now the claim wins, is idempotent, and the block is no longer reservable.
    assert!(
        s.claim_block(&h, 0).await.unwrap(),
        "claim wins on a dead block"
    );
    assert!(!s.claim_block(&h, 0).await.unwrap(), "already claimed");
    assert_eq!(
        s.reserve_block(&h).await.unwrap(),
        None,
        "reserve loses to a claim"
    );
}

#[tokio::test]
async fn mint_recovers_after_a_claim() {
    let (_d, s) = store().await;
    assert_eq!(s.reserve_block(&cid("absent")).await.unwrap(), None);

    let h = cid("claimed");
    assert!(s.mint_block(&h, 0).await.unwrap());
    s.release_block(&h, 0).await.unwrap(); // refcount 0
    assert!(s.claim_block(&h, 0).await.unwrap()); // generation 0 claimed
    assert_eq!(s.reserve_block(&h).await.unwrap(), None);
    // With the old generation claimed, a writer mints a fresh one (collision
    // recovery) — I1 still holds: exactly one available generation.
    assert!(s.mint_block(&h, 1).await.unwrap(), "mint a new generation");
    assert_eq!(s.reserve_block(&h).await.unwrap(), Some(1));
}

#[tokio::test]
async fn migrates_a_pre_m1c_blocks_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.db");
    let (a, b) = (cid("block-a").to_hex(), cid("block-b").to_hex());

    // Hand-build a pre-M1c database: the old `blocks (cid, refcount)` table,
    // no recorded schema version.
    {
        let opts = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let pool = SqlitePool::connect_with(opts).await.unwrap();
        sqlx::raw_sql("CREATE TABLE blocks (cid TEXT PRIMARY KEY, refcount INTEGER NOT NULL);")
            .execute(&pool)
            .await
            .unwrap();
        for (c, rc) in [(a.as_str(), 3), (b.as_str(), 0)] {
            sqlx::query("INSERT INTO blocks (cid, refcount) VALUES (?, ?)")
                .bind(c)
                .bind(rc)
                .execute(&pool)
                .await
                .unwrap();
        }
        pool.close().await;
    }

    // Opening it migrates blocks to (hash, generation, refcount, available).
    let s = SqliteNameIndex::open(&path).await.unwrap();
    let mut blocks = s.snapshot().await.unwrap().blocks;
    blocks.sort_by_key(|row| row.hash.to_hex());
    assert_eq!(blocks.len(), 2);
    for row in &blocks {
        assert_eq!(
            row.generation, 0,
            "migrated rows are the canonical generation"
        );
        assert!(row.available, "migrated rows are available");
    }
    let by_hash: std::collections::HashMap<_, _> = blocks
        .iter()
        .map(|row| (row.hash.to_hex(), row.refcount))
        .collect();
    assert_eq!(by_hash[&a], 3, "refcount preserved");
    assert_eq!(by_hash[&b], 0, "refcount preserved");

    // Re-opening is idempotent (the migration does not run a second time).
    drop(s);
    let s2 = SqliteNameIndex::open(&path).await.unwrap();
    assert_eq!(s2.snapshot().await.unwrap().blocks.len(), 2);
}
