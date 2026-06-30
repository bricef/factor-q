//! The storage index — the mutable name layer over the immutable CAS
//! (ADR-0023 layer 1; the authoritative SQLite DB of ADR-0024).
//!
//! Names are hierarchical dotted-path strings (`research.papers.doc1`);
//! namespaces are any prefix, not first-class objects. A name resolves to its
//! current [`Cid`], with retained version history (keep-all by default).
//!
//! The index maintains **two-level reference counts** — objects (how many
//! name-versions point at an object) and blocks (how many live objects
//! reference a block) — transactionally, so GC (M1c) can reclaim whatever
//! falls to zero. `NameIndex` only maintains the counts; it never deletes
//! from the CAS.

use std::collections::HashSet;
use std::path::Path;

use async_trait::async_trait;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};

use crate::{Cid, Result, StoreError};

/// The mutable name index over a content store. See the module docs.
#[async_trait]
pub trait NameIndex: Send + Sync {
    /// Bind `name` to `cid`. `blocks` are the object's dedup units (from
    /// [`ContentStore::blocks`](crate::ContentStore::blocks)), used to
    /// maintain block refcounts. Re-binding a name to the CID it already
    /// holds is a no-op; otherwise a new current version is recorded and the
    /// previous one retained as history.
    async fn bind(&self, name: &str, cid: &Cid, blocks: &[Cid]) -> Result<()>;

    /// The current CID for `name`, or `None` if there is no such name.
    async fn resolve(&self, name: &str) -> Result<Option<Cid>>;

    /// Names within the namespace `prefix` (segment-aware: `research.papers`
    /// matches `research.papers` and `research.papers.*`, not
    /// `research.papersX`), sorted. An empty prefix lists all names.
    async fn list(&self, prefix: &str) -> Result<Vec<String>>;

    /// Remove `name` entirely (current binding and history), dropping the
    /// references its versions held. Removing an absent name is a no-op.
    async fn unbind(&self, name: &str) -> Result<()>;

    /// `name`'s version history, newest first (empty if absent).
    async fn history(&self, name: &str) -> Result<Vec<Cid>>;

    /// Objects no longer referenced by any name-version — GC candidates.
    async fn unreferenced_objects(&self) -> Result<Vec<Cid>>;

    /// Blocks no longer referenced by any live object — GC candidates.
    async fn unreferenced_blocks(&self) -> Result<Vec<Cid>>;

    /// A consistent read of the two-level reference-counting state, for the
    /// invariant oracle ([`crate::verify`]) and the reachability audit (M1c).
    /// Suitable for tests and small stores; the production audit may stream.
    async fn snapshot(&self) -> Result<IndexSnapshot>;
}

/// A point-in-time read of the index's reference-counting state — objects,
/// blocks, the object→block edges, the per-object name-version counts, and the
/// current name bindings. Consumed by [`crate::verify`] to check the invariants.
#[derive(Debug, Clone, Default)]
pub struct IndexSnapshot {
    /// Object CID → stored refcount.
    pub objects: Vec<(Cid, i64)>,
    /// Block CID → stored refcount.
    pub blocks: Vec<(Cid, i64)>,
    /// `(object CID, block CID)` edges, one per object→block reference.
    pub object_blocks: Vec<(Cid, Cid)>,
    /// Object CID → number of name-version rows referencing it (its true refcount).
    pub name_refs: Vec<(Cid, i64)>,
    /// Current name → bound object CID (the newest version of each name).
    pub current_names: Vec<(String, Cid)>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS name_versions (
    name TEXT NOT NULL,
    seq  INTEGER NOT NULL,
    cid  TEXT NOT NULL,
    PRIMARY KEY (name, seq)
);
CREATE INDEX IF NOT EXISTS idx_name_versions_cid ON name_versions(cid);
CREATE TABLE IF NOT EXISTS objects (
    cid      TEXT PRIMARY KEY,
    refcount INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS object_blocks (
    object_cid TEXT NOT NULL,
    block_cid  TEXT NOT NULL,
    PRIMARY KEY (object_cid, block_cid)
);
CREATE INDEX IF NOT EXISTS idx_object_blocks_block ON object_blocks(block_cid);
CREATE TABLE IF NOT EXISTS blocks (
    cid      TEXT PRIMARY KEY,
    refcount INTEGER NOT NULL
);
";

fn index_err(e: sqlx::Error) -> StoreError {
    StoreError::Index(e.to_string())
}

fn hexes_to_cids(hexes: Vec<String>) -> Result<Vec<Cid>> {
    hexes.iter().map(|h| Cid::from_hex(h)).collect()
}

fn cid_counts(rows: Vec<(String, i64)>) -> Result<Vec<(Cid, i64)>> {
    rows.into_iter()
        .map(|(h, n)| Ok((Cid::from_hex(&h)?, n)))
        .collect()
}

/// SQLite-backed [`NameIndex`] — the reference implementation (ADR-0024 DB #1).
pub struct SqliteNameIndex {
    pool: SqlitePool,
}

impl SqliteNameIndex {
    /// Open (creating if needed) the index database at `path`.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePool::connect_with(opts).await.map_err(index_err)?;
        sqlx::raw_sql(SCHEMA)
            .execute(&pool)
            .await
            .map_err(index_err)?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl NameIndex for SqliteNameIndex {
    #[tracing::instrument(level = "debug", skip_all, fields(name, cid = %cid))]
    async fn bind(&self, name: &str, cid: &Cid, blocks: &[Cid]) -> Result<()> {
        let cid_hex = cid.to_hex();
        let mut tx = self.pool.begin().await.map_err(index_err)?;

        // No-op if this is already the current binding.
        let current: Option<String> = sqlx::query_scalar(
            "SELECT cid FROM name_versions WHERE name = ? ORDER BY seq DESC LIMIT 1",
        )
        .bind(name)
        .fetch_optional(&mut *tx)
        .await
        .map_err(index_err)?;
        if current.as_deref() == Some(cid_hex.as_str()) {
            tx.commit().await.map_err(index_err)?;
            return Ok(());
        }

        // Append a new current version.
        let next_seq: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM name_versions WHERE name = ?",
        )
        .bind(name)
        .fetch_one(&mut *tx)
        .await
        .map_err(index_err)?;
        sqlx::query("INSERT INTO name_versions (name, seq, cid) VALUES (?, ?, ?)")
            .bind(name)
            .bind(next_seq)
            .bind(&cid_hex)
            .execute(&mut *tx)
            .await
            .map_err(index_err)?;

        // Bump the object refcount; set up block edges the first time it goes live.
        let prev_rc: Option<i64> = sqlx::query_scalar("SELECT refcount FROM objects WHERE cid = ?")
            .bind(&cid_hex)
            .fetch_optional(&mut *tx)
            .await
            .map_err(index_err)?;
        sqlx::query(
            "INSERT INTO objects (cid, refcount) VALUES (?, 1)
             ON CONFLICT(cid) DO UPDATE SET refcount = refcount + 1",
        )
        .bind(&cid_hex)
        .execute(&mut *tx)
        .await
        .map_err(index_err)?;

        if prev_rc.unwrap_or(0) == 0 {
            let mut seen = HashSet::new();
            for block in blocks {
                let b = block.to_hex();
                if !seen.insert(b.clone()) {
                    continue; // an object referencing the same block twice = one edge
                }
                sqlx::query(
                    "INSERT INTO object_blocks (object_cid, block_cid) VALUES (?, ?)
                     ON CONFLICT DO NOTHING",
                )
                .bind(&cid_hex)
                .bind(&b)
                .execute(&mut *tx)
                .await
                .map_err(index_err)?;
                sqlx::query(
                    "INSERT INTO blocks (cid, refcount) VALUES (?, 1)
                     ON CONFLICT(cid) DO UPDATE SET refcount = refcount + 1",
                )
                .bind(&b)
                .execute(&mut *tx)
                .await
                .map_err(index_err)?;
            }
        }

        tx.commit().await.map_err(index_err)?;
        Ok(())
    }

    async fn resolve(&self, name: &str) -> Result<Option<Cid>> {
        let hex: Option<String> = sqlx::query_scalar(
            "SELECT cid FROM name_versions WHERE name = ? ORDER BY seq DESC LIMIT 1",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(index_err)?;
        hex.map(|h| Cid::from_hex(&h)).transpose()
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let names: Vec<String> = if prefix.is_empty() {
            sqlx::query_scalar("SELECT DISTINCT name FROM name_versions ORDER BY name")
                .fetch_all(&self.pool)
                .await
        } else {
            sqlx::query_scalar(
                "SELECT DISTINCT name FROM name_versions
                 WHERE name = ? OR (name >= ? AND name < ?)
                 ORDER BY name",
            )
            .bind(prefix)
            .bind(format!("{prefix}."))
            .bind(format!("{prefix}/"))
            .fetch_all(&self.pool)
            .await
        }
        .map_err(index_err)?;
        Ok(names)
    }

    #[tracing::instrument(level = "debug", skip_all, fields(name))]
    async fn unbind(&self, name: &str) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(index_err)?;
        // This name's references per object (with multiplicity).
        let refs: Vec<(String, i64)> =
            sqlx::query_as("SELECT cid, COUNT(*) FROM name_versions WHERE name = ? GROUP BY cid")
                .bind(name)
                .fetch_all(&mut *tx)
                .await
                .map_err(index_err)?;
        sqlx::query("DELETE FROM name_versions WHERE name = ?")
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(index_err)?;

        for (cid_hex, m) in refs {
            let new_rc: i64 = sqlx::query_scalar(
                "UPDATE objects SET refcount = refcount - ? WHERE cid = ? RETURNING refcount",
            )
            .bind(m)
            .bind(&cid_hex)
            .fetch_one(&mut *tx)
            .await
            .map_err(index_err)?;
            if new_rc == 0 {
                let block_hexes: Vec<String> =
                    sqlx::query_scalar("SELECT block_cid FROM object_blocks WHERE object_cid = ?")
                        .bind(&cid_hex)
                        .fetch_all(&mut *tx)
                        .await
                        .map_err(index_err)?;
                for b in block_hexes {
                    sqlx::query("UPDATE blocks SET refcount = refcount - 1 WHERE cid = ?")
                        .bind(&b)
                        .execute(&mut *tx)
                        .await
                        .map_err(index_err)?;
                }
            }
        }
        tx.commit().await.map_err(index_err)?;
        Ok(())
    }

    async fn history(&self, name: &str) -> Result<Vec<Cid>> {
        let hexes: Vec<String> =
            sqlx::query_scalar("SELECT cid FROM name_versions WHERE name = ? ORDER BY seq DESC")
                .bind(name)
                .fetch_all(&self.pool)
                .await
                .map_err(index_err)?;
        hexes_to_cids(hexes)
    }

    async fn unreferenced_objects(&self) -> Result<Vec<Cid>> {
        let hexes: Vec<String> = sqlx::query_scalar("SELECT cid FROM objects WHERE refcount = 0")
            .fetch_all(&self.pool)
            .await
            .map_err(index_err)?;
        hexes_to_cids(hexes)
    }

    async fn unreferenced_blocks(&self) -> Result<Vec<Cid>> {
        let hexes: Vec<String> = sqlx::query_scalar("SELECT cid FROM blocks WHERE refcount = 0")
            .fetch_all(&self.pool)
            .await
            .map_err(index_err)?;
        hexes_to_cids(hexes)
    }

    async fn snapshot(&self) -> Result<IndexSnapshot> {
        let objects: Vec<(String, i64)> = sqlx::query_as("SELECT cid, refcount FROM objects")
            .fetch_all(&self.pool)
            .await
            .map_err(index_err)?;
        let blocks: Vec<(String, i64)> = sqlx::query_as("SELECT cid, refcount FROM blocks")
            .fetch_all(&self.pool)
            .await
            .map_err(index_err)?;
        let edges: Vec<(String, String)> =
            sqlx::query_as("SELECT object_cid, block_cid FROM object_blocks")
                .fetch_all(&self.pool)
                .await
                .map_err(index_err)?;
        let name_refs: Vec<(String, i64)> =
            sqlx::query_as("SELECT cid, COUNT(*) FROM name_versions GROUP BY cid")
                .fetch_all(&self.pool)
                .await
                .map_err(index_err)?;
        let current: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, cid FROM name_versions AS nv
             WHERE seq = (SELECT MAX(seq) FROM name_versions WHERE name = nv.name)",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(index_err)?;

        Ok(IndexSnapshot {
            objects: cid_counts(objects)?,
            blocks: cid_counts(blocks)?,
            object_blocks: edges
                .into_iter()
                .map(|(o, b)| Ok((Cid::from_hex(&o)?, Cid::from_hex(&b)?)))
                .collect::<Result<_>>()?,
            name_refs: cid_counts(name_refs)?,
            current_names: current
                .into_iter()
                .map(|(n, c)| Ok((n, Cid::from_hex(&c)?)))
                .collect::<Result<_>>()?,
        })
    }
}

#[cfg(test)]
mod tests {
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

    #[tokio::test]
    async fn bind_and_resolve() {
        let (_d, s) = store().await;
        let c = cid("doc");
        s.bind("research.papers.doc1", &c, &[c]).await.unwrap();
        assert_eq!(s.resolve("research.papers.doc1").await.unwrap(), Some(c));
        assert_eq!(s.resolve("research.papers.nope").await.unwrap(), None);
    }

    #[tokio::test]
    async fn rebind_keeps_history_newest_first() {
        let (_d, s) = store().await;
        let (v1, v2, v3) = (cid("v1"), cid("v2"), cid("v3"));
        s.bind("a.b", &v1, &[v1]).await.unwrap();
        s.bind("a.b", &v2, &[v2]).await.unwrap();
        s.bind("a.b", &v2, &[v2]).await.unwrap(); // no-op (same cid)
        s.bind("a.b", &v3, &[v3]).await.unwrap();
        assert_eq!(s.resolve("a.b").await.unwrap(), Some(v3));
        assert_eq!(s.history("a.b").await.unwrap(), vec![v3, v2, v1]);
    }

    #[tokio::test]
    async fn list_is_segment_aware() {
        let (_d, s) = store().await;
        for name in ["a.b.c", "a.b.d", "a.x", "ab.c", "z"] {
            let c = cid(name);
            s.bind(name, &c, &[c]).await.unwrap();
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
        s.bind("a", &obj, &[b1, b2]).await.unwrap();
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
        s.bind("name.one", &obj, &[b1, b2]).await.unwrap();
        s.bind("name.two", &obj, &[b1, b2]).await.unwrap(); // alias: refcount 2

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
        s.bind("x", &x, &[b1, b2]).await.unwrap();
        s.bind("y", &y, &[b2, b3]).await.unwrap(); // b2 shared by x and y

        s.unbind("x").await.unwrap();
        // x dead -> b1 reclaimable; b2 still held by y; b3 still held by y.
        assert_eq!(s.unreferenced_objects().await.unwrap(), vec![x]);
        assert_eq!(s.unreferenced_blocks().await.unwrap(), vec![b1]);
    }
}
