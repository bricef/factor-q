# M1c — garbage collection: implementation plan

**Status:** active (2026-06-30). The slice-by-slice build of the online-reclaim
protocol, on branch `m1c-garbage-collection` (PR #1).

The **"what"** and **"why correct"** live elsewhere — the protocol in
[storage garbage collection](../../design/storage-garbage-collection.md), and the
claims, fault map, and model in
[verification](../../design/storage-gc-verification.md) + `storage_gc.tla`. This
doc is the **"how, in what order"**: the implementation slices, the decisions
taken while building, and where each is verified.

**Verification leads.** The invariant oracle + DST harness (slice 1) gate every
later slice; each slice is green on `cargo test` + `cargo clippy --all-features`
+ `cargo fmt --check` (the fq-store `just ci` gate) before commit.

## Slices

| # | Slice | Validates | Status |
|---|---|---|---|
| 1 | Verification harness — oracle (`verify`), `snapshot()`, DST | the net itself | done |
| 2 | CAS deletion primitive (`remove` / `has_block` / `remove_block`) | — | done |
| 3 | `blocks` re-keyed on `(hash, gen)` + `available`; oracle gains I1, I3 | I1, I3 | done |
| 4 | Generation-aware storage + fsync-before-publish durability | I2 | done |
| 5 | Online-reclaim protocol + collector | S1, I1–I4, the race | done |
| 6 | Reachability audit (the strong-fairness backstop) | L1, L4, recovery | **next** |
| 7 | `fq-cas gc` UX | — | |

> This refines the design doc's "What M1c builds": its original slice 4
> (reserve-before-rely) and 5 (collector) are **merged into slice 5 here**, since
> the writer's mint and the collector's claim are interdependent — collisions
> only arise once a collector can claim, so they can't be tested apart.

## Slice 5 sub-slices

The interdependent core, built and committed in order, oracle + DST after each:

- **5a — index ops.** `reserve` (`UPDATE … refcount+1 WHERE hash AND available`),
  `mint` (conditional `INSERT … WHERE NOT EXISTS (available row)`), and `claim`
  (`UPDATE available=false WHERE … refcount=0 AND available`) as `NameIndex`
  methods. Unit test: a reserve and a claim on the same block — exactly one wins,
  the loser sees zero rows affected.
- **5b — write path.** The `Repository` reserve → materialize → bind
  orchestration + `ContentStore::write_block` (generation-aware, fsync). The bind
  hand-off / release.
- **5c — collector.** A `Collector` trait + reference impl: claim → unlink →
  delete over `unreferenced_objects` / `unreferenced_blocks`, in small online
  batches.
- **5d — race + fault tests.** Collector wired into the DST (interleaved GC +
  collision + resurrection); a concurrent-tasks reserve-vs-claim stress test;
  `fail`-point crash-recovery cases at the named steps; a cranked soak run.


**Status: all four done** (through commit `3cc48ff`). 5d landed the
generation-on-collision mint (the wait-free `reserve_or_materialize` loop), the
`chunk` / `write_block` / `write_object` CAS primitives + `next_generation`, and
the concurrent reserve-vs-claim stress test. Two things shifted while building:
the oracle caught a stale-edge bug on collision resurrection — fixed by dropping
a dead object's edges at death (`unbind`) rather than at collection — and the
index pool became a single connection so every CAS linearizes as the verified
model assumes. The `fail`-point crash-recovery cases move to slice 6, where
recovery is the focus; the coarse DST crash step and the collector's orphan-claim
adoption already cover the crash paths.

## Slice 6 sub-slices

The reachability audit — the strong-fairness backstop, built as the **full**
self-healing worker (decision below). Oracle + DST after each, as with slice 5.

- **6a — seams.** An injectable `Clock` (real in prod, controllable in the DST)
  and file enumeration with mtimes (`(hash, generation, mtime)` for blocks,
  `(cid, mtime)` for objects), so the reaper can cross-check disk against the
  index and age files against the grace.
- **6b — reap + finish.** The `Auditor` (mirrors `Collector`): reap orphan
  block/object files (on disk, no index row, older than the mtime grace) and
  finish orphaned claims / dead blocks the online collector missed. A lightweight
  orphaned-claim reset at `open()` for bounded crash recovery (L4).
- **6c — reconcile.** Block-refcount drift: for a block quiescent past the grace,
  reduce a *stored > derived* refcount to the oracle's recomputed truth
  (transactional, re-checked) — the leaked-reservation leak. Alarm, never
  auto-fix, on *stored < derived* (I4) and the forbidden state (S1).
- **6d — fault DST + soak.** Inject leaked reservations / orphaned claims / stale
  files and advance the clock past grace; assert the audit restores every
  invariant (L4). Cranked soak.

Grace default ~15 min, configurable. A periodic scheduler is deferred — slice 7's
CLI and the startup hook cover invocation until a daemon exists.

## Decisions taken while building

- **Hand-off / aliasing accounting.** Reserve bumps a block's refcount
  *unconditionally* (to protect it before any reliance); `bind` keeps those
  reservations as the object→block edges when the object goes live, and
  **releases** them on an alias or idempotent re-bind (the blocks are already
  held), and a failed put releases. So block refcount stays exactly *= the number
  of live objects referencing it*. `bind` branches on the object's prior refcount.
- **`gen` is the smallest free `u32`, not a random token.** The spec allowed a
  random token (no coordination); the impl uses `next_generation` (the smallest
  generation absent for the hash) — deterministic and simpler. The mint loop
  already converges under contention: a refused conditional `INSERT` either
  reserves the peer's freshly-minted row or climbs to the next free generation,
  so no random-token PK-collision retry is needed.
- **Block primitives on the `ContentStore` trait** — `chunk`, `write_block`,
  `write_object`, so the `Repository` owns the write path. The trait defaults
  *error*; only `FilesystemStore` implements them. The matching `RemoteStore`
  RPCs (the uniform in-process/remote contract, as with `remove` in slice 2) are
  deferred to M5 — the in-process `Repository<FilesystemStore>` is the only
  writer until then, so the wire methods aren't yet exercised.
- **The race is tested with concurrent tokio tasks, not `loom`.** Reserve-vs-claim
  is linearised by SQLite's single writer; there is no Rust-level shared memory
  for `loom` to model, so the faithful test is concurrent tasks against a shared
  DB with the oracle as the check.
- **Durability scope.** The block file's data fsync (slice 4) is in; the directory
  fsync that would also make the rename itself crash-durable is left to the
  dedicated crash-consistency tests.

- **Edges are dropped at death, not at collection.** `unbind` deletes a dead
  object's `object_blocks` edges (after decrementing the block refcounts) rather
  than leaving them for `delete_object`. Found via the oracle: resurrecting a
  dead object onto a *fresh* generation (the collision case) otherwise left a
  stale edge at the old, claimed generation, reading as a live reference to a
  reclaimable block. The rule is now: an edge exists iff its object currently
  references that block.
- **The index is a single-connection pool.** `max_connections(1)` + a busy
  timeout, so every reserve / claim / bind / unbind linearizes exactly as the
  verified single-writer model assumes, and WAL's `BUSY_SNAPSHOT` — which a
  deferred read-then-write transaction hits under contention and a busy timeout
  cannot retry — cannot arise. `snapshot()` reads in one transaction for a
  tear-free view (the oracle and the slice-6 audit must not see a half-applied
  operation). Read concurrency is traded for correctness-by-construction;
  revisit if the metadata path ever needs it.

## References

- [storage garbage collection](../../design/storage-garbage-collection.md) — the protocol.
- [verification](../../design/storage-gc-verification.md) + `storage_gc.tla` — claims, fault map, model.
- [storage + vector foundation](2026-06-27-storage-vector-foundation.md) — the parent plan (M1–M5).
