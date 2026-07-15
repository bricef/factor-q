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

use std::path::Path;

use async_trait::async_trait;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

use crate::{Cid, Result, StoreError};

/// The mutable name index over a content store. See the module docs.
#[async_trait]
pub trait NameIndex: Send + Sync {
    /// Bind `name` to `cid`, handing off the writer's block reservations.
    /// `reserved` are the object's unique blocks as `(hash, generation)` pairs,
    /// each already reserved (its refcount bumped) by the caller. If the object
    /// becomes live the reservations become its object→block edges; if it was
    /// already live (an alias) or this re-binds the current CID, they are
    /// released. A new current version is recorded; the previous is retained.
    ///
    /// This is the object-side reserve-before-rely: it bumps the object refcount
    /// under the claim CAS, so a `bind` that would resurrect an object the
    /// collector has **claimed** (`available = 0`) is refused with
    /// [`StoreError::Conflict`] (ADR-0030) — the caller retries once the
    /// collector has finished. `reserved` is left untouched on that refusal, for
    /// the caller to release or reuse.
    async fn bind(&self, name: &str, cid: &Cid, reserved: &[(Cid, u32)]) -> Result<()>;

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

    /// Atomically reserve the currently-available generation of block `hash`
    /// (the writer's compare-and-swap): increment `refcount` on the row that is
    /// still `available`, returning its generation. `None` if no generation is
    /// available (none exists, or every one is claimed) — the writer then mints.
    async fn reserve_block(&self, hash: &Cid) -> Result<Option<u32>>;

    /// Atomically mint `generation` of `hash` as a fresh available block
    /// (refcount 1), **conditional on no generation currently being available**,
    /// so concurrent minters converge to one. `Ok(true)` if it inserted;
    /// `Ok(false)` if an available generation already exists, or `(hash,
    /// generation)` is already present (the caller picks another generation). The
    /// block file must already be written and fsynced (I2).
    async fn mint_block(&self, hash: &Cid, generation: u32) -> Result<bool>;

    /// The smallest generation of `hash` not currently present in the index — the
    /// generation a writer mints when reserving fails (a fresh block, or a GC
    /// collision where every existing generation is claimed). 0 for a hash with
    /// no rows.
    async fn next_generation(&self, hash: &Cid) -> Result<u32>;

    /// Atomically claim `(hash, generation)` for collection (the GC
    /// compare-and-swap): flip `available` to false, **conditional on `refcount
    /// = 0` and still available**. `Ok(true)` if claimed; `Ok(false)` if a writer
    /// reserved first (refcount > 0) or it was already claimed.
    async fn claim_block(&self, hash: &Cid, generation: u32) -> Result<bool>;

    /// Release a reservation on `(hash, generation)` — `refcount -= 1` — for a
    /// failed put, or the bind hand-off's alias case.
    async fn release_block(&self, hash: &Cid, generation: u32) -> Result<()>;

    /// Reduce block `(hash, generation)`'s `refcount` to `to_refcount` — the
    /// audit's leaked-reservation reconcile. Conditional and atomic: it fires
    /// only if `refcount > to_refcount` (there is drift to shed) **and** the row
    /// was last touched at or before `touched_before` (at rest past the grace,
    /// so no live in-flight reservation is being reduced). `Ok(true)` if it
    /// reconciled. Never increases a refcount.
    async fn reconcile_block(
        &self,
        hash: &Cid,
        generation: u32,
        to_refcount: i64,
        touched_before: i64,
    ) -> Result<bool>;

    /// Blocks eligible for collection — `(hash, generation, available)` for every
    /// row at refcount 0. An `available` row is claimed first; an unavailable one
    /// is an orphaned claim (a crash mid-reclaim) the collector adopts directly.
    async fn claimable_blocks(&self) -> Result<Vec<(Cid, u32, bool)>>;

    /// Delete a claimed, dead block row (`refcount = 0` and unavailable), after
    /// its file is unlinked. Idempotent.
    async fn delete_block(&self, hash: &Cid, generation: u32) -> Result<()>;

    /// Atomically claim an object for collection (the object-side GC
    /// compare-and-swap, ADR-0030): flip `available` to false, **conditional on
    /// `refcount = 0` and still available**. `Ok(true)` if claimed; `Ok(false)`
    /// if a writer reserved it first (`refcount > 0`) or it was already claimed.
    /// A claimed object cannot be resurrected by [`bind`](Self::bind), so its
    /// manifest is safe to unlink.
    async fn claim_object(&self, cid: &Cid) -> Result<bool>;

    /// Objects eligible for collection — `(cid, available)` for every object at
    /// `refcount = 0`. An `available` object is claimed first; an unavailable one
    /// is an orphaned claim (a crash mid-reclaim) the collector adopts directly.
    async fn claimable_objects(&self) -> Result<Vec<(Cid, bool)>>;

    /// Delete a claimed, dead object (`refcount = 0` **and** unavailable) — its
    /// row and its object→block edges — after its manifest is unlinked.
    /// Conditional on the claim so a writer that reserved it (making it available
    /// again is impossible, but `refcount > 0` is) is never deleted. Idempotent.
    async fn delete_object(&self, cid: &Cid) -> Result<()>;
}

/// One row of the `blocks` table: a block generation and its claim state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRow {
    /// The block's BLAKE3 hash.
    pub hash: Cid,
    /// The generation — 0 is canonical, higher generations are collision mints.
    pub generation: u32,
    /// Live references: bound object edges plus in-flight reservations.
    pub refcount: i64,
    /// Whether a writer may reserve this generation (false ⇒ claimed by GC).
    pub available: bool,
    /// When a writer last reserved or minted this generation, as Unix
    /// milliseconds (0 for rows predating the audit). The audit uses it to tell
    /// a leaked reservation from a live in-flight one.
    pub touched_at: i64,
}

/// One `object_blocks` edge: a live object references a specific block
/// generation. `block` is the block's hash — the `object_blocks.block_cid`
/// column holds that same value (it joins `blocks.hash`) under the join table's
/// own column name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edge {
    /// The referencing object's CID.
    pub object: Cid,
    /// The referenced block's hash (= `blocks.hash`).
    pub block: Cid,
    /// The block generation this edge pins.
    pub generation: u32,
}

/// A point-in-time read of the index's reference-counting state — objects,
/// blocks, the object→block edges, the per-object name-version counts, and the
/// current name bindings. Consumed by [`crate::verify`] to check the invariants.
#[derive(Debug, Clone, Default)]
pub struct IndexSnapshot {
    /// Object CID → stored refcount.
    pub objects: Vec<(Cid, i64)>,
    /// The `blocks` table rows (hash, generation, refcount, available).
    pub blocks: Vec<BlockRow>,
    /// The object→block edges (see [`Edge`]).
    pub object_blocks: Vec<Edge>,
    /// Object CID → number of name-version rows referencing it (its true refcount).
    pub name_refs: Vec<(Cid, i64)>,
    /// Current name → bound object CID (the newest version of each name).
    pub current_names: Vec<(String, Cid)>,
}

/// Schema migrations, applied in order. The DB's `PRAGMA user_version` records
/// how many have run; each migration runs in its own transaction together with
/// the version bump, so a crash mid-migration rolls back cleanly and re-runs.
const MIGRATIONS: &[&str] = &[
    // v1 — the M1a/M1b base. `IF NOT EXISTS` makes it a no-op on a pre-M1c
    // database (which already has these tables, but no recorded version).
    "CREATE TABLE IF NOT EXISTS name_versions (
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
    );",
    // v2 — M1c garbage collection: re-key blocks on (hash, generation) and add
    // the `available` claim flag. Existing rows become generation 0, available.
    "CREATE TABLE blocks_v2 (
        hash       TEXT    NOT NULL,
        generation INTEGER NOT NULL DEFAULT 0,
        refcount   INTEGER NOT NULL,
        available  INTEGER NOT NULL DEFAULT 1,
        PRIMARY KEY (hash, generation)
    );
    INSERT INTO blocks_v2 (hash, generation, refcount, available)
        SELECT cid, 0, refcount, 1 FROM blocks;
    DROP TABLE blocks;
    ALTER TABLE blocks_v2 RENAME TO blocks;",
    // v3 — M1c: record the generation on each object→block edge, so unbind
    // decrements the right (hash, generation). Pre-existing edges are generation 0.
    "ALTER TABLE object_blocks ADD COLUMN generation INTEGER NOT NULL DEFAULT 0;",
    // v4 — M1c audit: stamp when a block was last reserved/minted (Unix millis).
    // The reachability audit only reconciles a drifted refcount once the block
    // has gone untouched past the grace, so a live in-flight reservation (touched
    // just now) is never mistaken for a leaked one. Existing rows start at 0 (the
    // distant past → immediately eligible, which is correct: they predate any
    // in-flight reservation).
    "ALTER TABLE blocks ADD COLUMN touched_at INTEGER NOT NULL DEFAULT 0;",
    // v5 — object/manifest GC (ADR-0030 back-off): give objects the same
    // `available` claim flag blocks have, so the collector can CLAIM an object
    // (available → 0) before unlinking its manifest and a writer's `bind` is
    // refused if it would resurrect a claimed object. Existing objects become
    // available (the normal state).
    "ALTER TABLE objects ADD COLUMN available INTEGER NOT NULL DEFAULT 1;",
];

/// Release the writer's reservations on `reserved` (`refcount -= 1` per
/// `(hash, generation)`) within a transaction — used by [`NameIndex::bind`] when
/// the reservations are redundant: an alias, or a re-bind of the current CID.
async fn release_reservations(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    reserved: &[(Cid, u32)],
) -> Result<()> {
    for (hash, generation) in reserved {
        sqlx::query("UPDATE blocks SET refcount = refcount - 1 WHERE hash = ? AND generation = ?")
            .bind(hash.to_hex())
            .bind(*generation as i64)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

fn hexes_to_cids(hexes: Vec<String>) -> Result<Vec<Cid>> {
    hexes.iter().map(|h| Cid::from_hex(h)).collect()
}

/// The current time in Unix milliseconds — the `touched_at` stamp on a reserve or
/// mint, and the audit's reconcile cutoff. Clamped at 0 (never negative here).
pub(crate) fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn cid_counts(rows: Vec<(String, i64)>) -> Result<Vec<(Cid, i64)>> {
    rows.into_iter()
        .map(|(h, n)| Ok((Cid::from_hex(&h)?, n)))
        .collect()
}

/// Apply any migrations the database has not yet seen, tracked by
/// `PRAGMA user_version`.
async fn migrate(pool: &SqlitePool) -> Result<()> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let target = i as i64 + 1;
        if version < target {
            let mut tx = pool.begin().await?;
            sqlx::raw_sql(sql).execute(&mut *tx).await?;
            // PRAGMA values cannot be bound; `target` is a trusted constant.
            sqlx::query(&format!("PRAGMA user_version = {target}"))
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
    }
    Ok(())
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
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5));
        // A single connection serializes every index operation, so the two
        // reference-counting compare-and-swaps (reserve / claim) and the
        // multi-statement transactions (bind / unbind) linearize exactly as the
        // verified protocol assumes: SQLite's single writer. It also sidesteps
        // WAL's `SQLITE_BUSY_SNAPSHOT`, which a busy timeout cannot retry away.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        migrate(&pool).await?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl NameIndex for SqliteNameIndex {
    #[tracing::instrument(level = "debug", skip_all, fields(name, cid = %cid))]
    async fn bind(&self, name: &str, cid: &Cid, reserved: &[(Cid, u32)]) -> Result<()> {
        let cid_hex = cid.to_hex();
        let mut tx = self.pool.begin().await?;

        let current: Option<String> = sqlx::query_scalar(
            "SELECT cid FROM name_versions WHERE name = ? ORDER BY seq DESC LIMIT 1",
        )
        .bind(name)
        .fetch_optional(&mut *tx)
        .await?;

        // Re-binding the current CID is a no-op for the name — the caller's
        // reservations are then redundant, so release them.
        if current.as_deref() == Some(cid_hex.as_str()) {
            release_reservations(&mut tx, reserved).await?;
            tx.commit().await?;
            return Ok(());
        }

        // Append a new current version.
        let next_seq: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM name_versions WHERE name = ?",
        )
        .bind(name)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO name_versions (name, seq, cid) VALUES (?, ?, ?)")
            .bind(name)
            .bind(next_seq)
            .bind(&cid_hex)
            .execute(&mut *tx)
            .await?;

        // Reserve the object under the claim CAS (ADR-0030). The single-writer
        // index serialises this against the collector's `claim_object`, so the
        // read-then-reserve is atomic: a claimed object (`available = 0`) is
        // refused rather than resurrected over a manifest being unlinked.
        let existing: Option<(i64, i64)> =
            sqlx::query_as("SELECT refcount, available FROM objects WHERE cid = ?")
                .bind(&cid_hex)
                .fetch_optional(&mut *tx)
                .await?;
        let prev_rc = match existing {
            // Claimed by the collector (available = 0) — refuse. The transaction
            // rolls back (the name-version above is undone); `reserved` is left
            // intact for the caller to release or reuse on retry.
            Some((_, 0)) => {
                return Err(StoreError::Conflict(format!(
                    "cannot bind {cid}: object is being collected; retry"
                )));
            }
            Some((rc, _)) => {
                sqlx::query("UPDATE objects SET refcount = refcount + 1 WHERE cid = ?")
                    .bind(&cid_hex)
                    .execute(&mut *tx)
                    .await?;
                rc
            }
            None => {
                sqlx::query("INSERT INTO objects (cid, refcount, available) VALUES (?, 1, 1)")
                    .bind(&cid_hex)
                    .execute(&mut *tx)
                    .await?;
                0
            }
        };

        if prev_rc == 0 {
            // Object going live: hand the reservations off to object→block edges
            // (their refcounts are already bumped — no second increment).
            for (hash, generation) in reserved {
                sqlx::query(
                    "INSERT INTO object_blocks (object_cid, block_cid, generation)
                     VALUES (?, ?, ?) ON CONFLICT DO NOTHING",
                )
                .bind(&cid_hex)
                .bind(hash.to_hex())
                .bind(*generation as i64)
                .execute(&mut *tx)
                .await?;
            }
        } else {
            // Alias: the object's blocks are already held — release the
            // now-redundant reservations.
            release_reservations(&mut tx, reserved).await?;
        }

        tx.commit().await?;
        Ok(())
    }

    async fn resolve(&self, name: &str) -> Result<Option<Cid>> {
        let hex: Option<String> = sqlx::query_scalar(
            "SELECT cid FROM name_versions WHERE name = ? ORDER BY seq DESC LIMIT 1",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
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
        }?;
        Ok(names)
    }

    #[tracing::instrument(level = "debug", skip_all, fields(name))]
    async fn unbind(&self, name: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        // This name's references per object (with multiplicity).
        let refs: Vec<(String, i64)> =
            sqlx::query_as("SELECT cid, COUNT(*) FROM name_versions WHERE name = ? GROUP BY cid")
                .bind(name)
                .fetch_all(&mut *tx)
                .await?;
        sqlx::query("DELETE FROM name_versions WHERE name = ?")
            .bind(name)
            .execute(&mut *tx)
            .await?;

        for (cid_hex, m) in refs {
            let new_rc: i64 = sqlx::query_scalar(
                "UPDATE objects SET refcount = refcount - ? WHERE cid = ? RETURNING refcount",
            )
            .bind(m)
            .bind(&cid_hex)
            .fetch_one(&mut *tx)
            .await?;
            if new_rc == 0 {
                let edges: Vec<(String, i64)> = sqlx::query_as(
                    "SELECT block_cid, generation FROM object_blocks WHERE object_cid = ?",
                )
                .bind(&cid_hex)
                .fetch_all(&mut *tx)
                .await?;
                for (b, generation) in edges {
                    sqlx::query(
                        "UPDATE blocks SET refcount = refcount - 1
                         WHERE hash = ? AND generation = ?",
                    )
                    .bind(&b)
                    .bind(generation)
                    .execute(&mut *tx)
                    .await?;
                }
                // The object is dead: drop its edges now (not at collection) so a
                // later resurrection — possibly onto a different block generation
                // after a GC collision — rebuilds a clean reference set instead of
                // accumulating a stale edge. This keeps the invariant that an edge
                // exists iff its object currently references the block.
                sqlx::query("DELETE FROM object_blocks WHERE object_cid = ?")
                    .bind(&cid_hex)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    async fn history(&self, name: &str) -> Result<Vec<Cid>> {
        let hexes: Vec<String> =
            sqlx::query_scalar("SELECT cid FROM name_versions WHERE name = ? ORDER BY seq DESC")
                .bind(name)
                .fetch_all(&self.pool)
                .await?;
        hexes_to_cids(hexes)
    }

    async fn unreferenced_objects(&self) -> Result<Vec<Cid>> {
        let hexes: Vec<String> = sqlx::query_scalar("SELECT cid FROM objects WHERE refcount = 0")
            .fetch_all(&self.pool)
            .await?;
        hexes_to_cids(hexes)
    }

    async fn unreferenced_blocks(&self) -> Result<Vec<Cid>> {
        let hexes: Vec<String> = sqlx::query_scalar("SELECT hash FROM blocks WHERE refcount = 0")
            .fetch_all(&self.pool)
            .await?;
        hexes_to_cids(hexes)
    }

    async fn snapshot(&self) -> Result<IndexSnapshot> {
        // One transaction so all five reads see a single consistent index state —
        // the oracle and the reachability audit (M1c slice 6) must not observe a
        // half-applied writer or collector. (A read transaction takes its snapshot
        // on the first read and holds it.)
        let mut tx = self.pool.begin().await?;
        let objects: Vec<(String, i64)> = sqlx::query_as("SELECT cid, refcount FROM objects")
            .fetch_all(&mut *tx)
            .await?;
        let blocks: Vec<(String, i64, i64, i64, i64)> =
            sqlx::query_as("SELECT hash, generation, refcount, available, touched_at FROM blocks")
                .fetch_all(&mut *tx)
                .await?;
        let edges: Vec<(String, String, i64)> =
            sqlx::query_as("SELECT object_cid, block_cid, generation FROM object_blocks")
                .fetch_all(&mut *tx)
                .await?;
        let name_refs: Vec<(String, i64)> =
            sqlx::query_as("SELECT cid, COUNT(*) FROM name_versions GROUP BY cid")
                .fetch_all(&mut *tx)
                .await?;
        let current: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, cid FROM name_versions AS nv
             WHERE seq = (SELECT MAX(seq) FROM name_versions WHERE name = nv.name)",
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(IndexSnapshot {
            objects: cid_counts(objects)?,
            blocks: blocks
                .into_iter()
                .map(|(h, g, rc, av, touched)| {
                    Ok(BlockRow {
                        hash: Cid::from_hex(&h)?,
                        generation: g as u32,
                        refcount: rc,
                        available: av != 0,
                        touched_at: touched,
                    })
                })
                .collect::<Result<_>>()?,
            object_blocks: edges
                .into_iter()
                .map(|(o, b, g)| {
                    Ok(Edge {
                        object: Cid::from_hex(&o)?,
                        block: Cid::from_hex(&b)?,
                        generation: g as u32,
                    })
                })
                .collect::<Result<_>>()?,
            name_refs: cid_counts(name_refs)?,
            current_names: current
                .into_iter()
                .map(|(n, c)| Ok((n, Cid::from_hex(&c)?)))
                .collect::<Result<_>>()?,
        })
    }

    async fn reserve_block(&self, hash: &Cid) -> Result<Option<u32>> {
        let reserved: Option<i64> = sqlx::query_scalar(
            "UPDATE blocks SET refcount = refcount + 1, touched_at = ?
             WHERE hash = ? AND available = 1 RETURNING generation",
        )
        .bind(now_millis())
        .bind(hash.to_hex())
        .fetch_optional(&self.pool)
        .await?;
        Ok(reserved.map(|g| g as u32))
    }

    async fn mint_block(&self, hash: &Cid, generation: u32) -> Result<bool> {
        let h = hash.to_hex();
        let affected = sqlx::query(
            "INSERT INTO blocks (hash, generation, refcount, available, touched_at)
             SELECT ?, ?, 1, 1, ?
             WHERE NOT EXISTS (SELECT 1 FROM blocks WHERE hash = ? AND available = 1)
             ON CONFLICT(hash, generation) DO NOTHING",
        )
        .bind(&h)
        .bind(generation as i64)
        .bind(now_millis())
        .bind(&h)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(affected == 1)
    }

    async fn reconcile_block(
        &self,
        hash: &Cid,
        generation: u32,
        to_refcount: i64,
        touched_before: i64,
    ) -> Result<bool> {
        let affected = sqlx::query(
            "UPDATE blocks SET refcount = ?
             WHERE hash = ? AND generation = ? AND refcount > ? AND touched_at <= ?",
        )
        .bind(to_refcount)
        .bind(hash.to_hex())
        .bind(generation as i64)
        .bind(to_refcount)
        .bind(touched_before)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(affected == 1)
    }

    async fn next_generation(&self, hash: &Cid) -> Result<u32> {
        let used: Vec<i64> = sqlx::query_scalar("SELECT generation FROM blocks WHERE hash = ?")
            .bind(hash.to_hex())
            .fetch_all(&self.pool)
            .await?;
        let used: std::collections::HashSet<i64> = used.into_iter().collect();
        let mut generation = 0i64;
        while used.contains(&generation) {
            generation += 1;
        }
        Ok(generation as u32)
    }

    async fn claim_block(&self, hash: &Cid, generation: u32) -> Result<bool> {
        let affected = sqlx::query(
            "UPDATE blocks SET available = 0
             WHERE hash = ? AND generation = ? AND refcount = 0 AND available = 1",
        )
        .bind(hash.to_hex())
        .bind(generation as i64)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(affected == 1)
    }

    async fn release_block(&self, hash: &Cid, generation: u32) -> Result<()> {
        sqlx::query("UPDATE blocks SET refcount = refcount - 1 WHERE hash = ? AND generation = ?")
            .bind(hash.to_hex())
            .bind(generation as i64)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn claimable_blocks(&self) -> Result<Vec<(Cid, u32, bool)>> {
        let rows: Vec<(String, i64, i64)> =
            sqlx::query_as("SELECT hash, generation, available FROM blocks WHERE refcount = 0")
                .fetch_all(&self.pool)
                .await?;
        rows.into_iter()
            .map(|(h, g, a)| Ok((Cid::from_hex(&h)?, g as u32, a != 0)))
            .collect()
    }

    async fn delete_block(&self, hash: &Cid, generation: u32) -> Result<()> {
        sqlx::query(
            "DELETE FROM blocks
             WHERE hash = ? AND generation = ? AND refcount = 0 AND available = 0",
        )
        .bind(hash.to_hex())
        .bind(generation as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn claim_object(&self, cid: &Cid) -> Result<bool> {
        let affected = sqlx::query(
            "UPDATE objects SET available = 0
             WHERE cid = ? AND refcount = 0 AND available = 1",
        )
        .bind(cid.to_hex())
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(affected == 1)
    }

    async fn claimable_objects(&self) -> Result<Vec<(Cid, bool)>> {
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT cid, available FROM objects WHERE refcount = 0")
                .fetch_all(&self.pool)
                .await?;
        rows.into_iter()
            .map(|(c, a)| Ok((Cid::from_hex(&c)?, a != 0)))
            .collect()
    }

    async fn delete_object(&self, cid: &Cid) -> Result<()> {
        let cid_hex = cid.to_hex();
        let mut tx = self.pool.begin().await?;
        // Only a claimed, dead object is deleted (refcount 0 AND unavailable) —
        // symmetric with `delete_block`. A writer that reserved it (refcount > 0)
        // is never deleted; a claim must precede the unlink + delete.
        let deletable: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM objects WHERE cid = ? AND refcount = 0 AND available = 0",
        )
        .bind(&cid_hex)
        .fetch_optional(&mut *tx)
        .await?;
        if deletable.is_some() {
            sqlx::query("DELETE FROM object_blocks WHERE object_cid = ?")
                .bind(&cid_hex)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM objects WHERE cid = ?")
                .bind(&cid_hex)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
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

    /// Mimic a put at the index layer: reserve (or mint) each unique block, then
    /// hand the reservations to `bind`.
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
        s.bind(name, obj, &reserved).await
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
}
