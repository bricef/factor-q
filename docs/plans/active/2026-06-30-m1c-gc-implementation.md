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
| 5 | Online-reclaim protocol + collector | S1, I1–I4, the race | **next** |
| 6 | Reachability audit (the strong-fairness backstop) | L1, L4, recovery | |
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

## Decisions taken while building

- **Hand-off / aliasing accounting.** Reserve bumps a block's refcount
  *unconditionally* (to protect it before any reliance); `bind` keeps those
  reservations as the object→block edges when the object goes live, and
  **releases** them on an alias or idempotent re-bind (the blocks are already
  held), and a failed put releases. So block refcount stays exactly *= the number
  of live objects referencing it*. `bind` branches on the object's prior refcount.
- **`gen` is a random `u32` token** (per the spec — no coordination); a PK
  collision on mint retries with another.
- **Block primitives on the `ContentStore` trait** (`chunk`, `write_block`) with
  matching `RemoteStore` RPCs — the uniform in-process/remote contract, as with
  `remove` (slice 2).
- **The race is tested with concurrent tokio tasks, not `loom`.** Reserve-vs-claim
  is linearised by SQLite's single writer; there is no Rust-level shared memory
  for `loom` to model, so the faithful test is concurrent tasks against a shared
  DB with the oracle as the check.
- **Durability scope.** The block file's data fsync (slice 4) is in; the directory
  fsync that would also make the rename itself crash-durable is left to the
  dedicated crash-consistency tests.

## References

- [storage garbage collection](../../design/storage-garbage-collection.md) — the protocol.
- [verification](../../design/storage-gc-verification.md) + `storage_gc.tla` — claims, fault map, model.
- [storage + vector foundation](2026-06-27-storage-vector-foundation.md) — the parent plan (M1–M5).
