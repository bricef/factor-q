# Storage garbage collection — online reclaim

How the content store reclaims unreferenced objects and blocks **online** — the
store stays fully available for reads and writes throughout — without ever
losing a block a live object still needs. This elaborates
[ADR-0023](../adrs/accepted/0023-storage-and-vector-foundation.md) fork **F2**
(GC algorithm: two-level reference counting + reachability-audit backstop) into
the concrete concurrency protocol for **M1c** of the
[storage + vector foundation plan](../plans/active/2026-06-27-storage-vector-foundation.md).
M1a (the CAS) and M1b (the storage index) are built; this is the design for the
collector that reclaims what the index reports as unreferenced.

The design is **lock-free, wait-free on the write path, loss-free, and
self-healing toward retention**. It leans on one relaxation and one existing
property of the system.

## What M1b provides

The storage index (`NameIndex`) maintains two-level reference counts
transactionally, and exposes `unreferenced_objects()` / `unreferenced_blocks()`
(refcount 0). So *what* to reclaim is already computed. M1c is *reclaiming it
safely*, plus the audit backstop. M1c also refines *when* block refcounts move
(see "reserve-before-rely" below): from bind-time to write-time reservations.

## The hazard

Blocks are content-addressed, and `put` skips re-writing a block whose file is
already present. That optimisation is the source of the one dangerous race:

1. GC sees block `X` at refcount 0 and prepares to delete its file.
2. Concurrently a `put` chunks new content that produces `X` again, sees `X`'s
   file present, **skips the write**, and is about to reference it.
3. GC deletes `X`'s file.
4. The `put` commits — its object now references a block whose file is gone.
   **Corruption: a live object is missing a block.**

It is fundamentally a cross-store consistency problem: block files live on the
filesystem, refcounts live in SQLite, and no transaction spans both.

## The relaxation: leak/duplicate over lose

We accept **keeping a block that could have been freed** (it is reclaimed on a
later pass) and **transiently holding two copies of the same content**. We do
**not** accept losing a block a live object needs. This turns a consistency
problem into a *conservatism* problem — the safe direction is **retention** — and
unlocks the key lever:

> A writer always holds the bytes, so any block it references can be
> re-materialised. "GC deleted a block I'm writing" is recoverable.

True loss is therefore only possible for a block referenced by an
*already-bound* object (no bytes available at read time) — and those are
refcount ≥ 1, which a correct collector never touches. The whole problem reduces
to two obligations: keep refcount transitions correct, and have writers reserve
a block **before** they rely on its file.

## Prior art

The shape follows established practice:

- **Lock-free reference counting** resolves the load-then-dereference race with a
  compare-and-swap on a `(count, flag)` word (the Treiber/ABA lineage). Our
  "CAS" is a conditional SQL `UPDATE`; the flag is `available`.
- **Grace-period reclamation** — hazard pointers, epoch-based reclamation (Rust's
  `crossbeam-epoch`), RCU/QSBR, Cassandra's `gc_grace_seconds` — defers freeing
  until quiescence proves no one holds a reference. We use a small mtime grace
  only for the orphan reaper.
- **Bazel Remote CAS** evicts blobs anytime and relies on *the client has the
  bytes* (re-upload on miss) — exactly our re-materialisation lever. **git gc**
  and **restic prune** are the cautionary cousins that lean on grace periods and
  exclusivity respectively.

## The design

### Index state

The `blocks` table is keyed by `(hash, gen)` with columns `refcount` and
`available`:

- **`refcount`** — references to this block (reservations + bound-object edges).
- **`available`** — `true` while the block is usable; GC flips it `false` to
  claim the block for deletion.
- **`gen`** — a generation token (random; no coordination), almost always the
  single canonical generation. A second generation appears only transiently,
  during a collision (below).

**Invariant: at most one `available` row per hash** — the current generation.
Any `unavailable` rows are claimed-and-being-reaped.

### Two atomic operations, linearised on SQLite

Both are single conditional `UPDATE`s; SQLite's single writer serialises them, so
for a contended block exactly one wins and the loser sees zero rows affected:

- **Writer reserve:** `UPDATE blocks SET refcount = refcount + 1
  WHERE hash = ? AND available` — bumps the current generation if it is still
  usable.
- **GC claim:** `UPDATE blocks SET available = false
  WHERE hash = ? AND gen = ? AND refcount = 0 AND available` — claims a dead
  block for deletion.

If a writer reserves first, GC's claim finds `refcount > 0` and fails. If GC
claims first, the writer's reserve finds the row `unavailable` and fails. There
is no interleaving in which both succeed.

### Reserve-before-rely (the write path)

`put` reserves each block **before** it relies on the block's file:

1. Chunk → block `X`.
2. **Reserve** `X` (the atomic `UPDATE … WHERE available`).
   - **Success** → the block is now refcount ≥ 1, so GC cannot claim it; the file
     is safe to reuse (skip-write). 
   - **No current row** → this is a new block: write the file and `INSERT
     (hash, gen, refcount 1, available)`.
   - **Reserve failed (row unavailable)** → GC has claimed the current
     generation; take the collision path below.
3. Bind the object, **handing each reservation off** to a permanent object→block
   edge (no second increment). A `put` that fails before binding **releases** its
   reservations (decrement).

Because the reservation precedes any reliance on the file, GC never deletes a
block a writer is about to use in the common case.

### Generation-on-collision

When a writer's reserve fails because the current generation is `unavailable`
(GC claimed it), the writer does **not** wait and does **not** touch the doomed
file. It **mints a new generation**: write the bytes to `blocks/<hash>.<new-gen>`
and `INSERT (hash, new-gen, refcount 1, available)` — conditional on no
`available` row existing, so concurrent minters converge to one (the second
deduplicates onto the first).

GC, meanwhile, unlinks the claimed `blocks/<hash>.<old-gen>` whenever it gets to
it. The two files are **disjoint paths**, so:

> The gap between GC's claim transaction and its `unlink` no longer matters —
> nobody depends on the file GC is deleting.

This is what makes the protocol lock-free *and* wait-free on writes: a collision
is resolved by writing elsewhere, never by blocking. The transient second copy is
the "duplicate over lose" we accepted; it converges as GC reaps the old
generation.

### Generations cost nothing on reads

The generation is **recorded in the manifest** (only for blocks that ever
collided — canonical blocks carry no suffix). A read opens
`blocks/<hash>[.gen]` straight from the manifest with **no index lookup**.

This is sound because a generation only changes for a *dead* block being
resurrected, and **a live object's blocks are never dead** (they hold
refcount ≥ 1), so the generation a manifest froze can never shift under a live
object. New references deduplicate to whatever the index reports as the current
`available` generation; the manifest freezes the one that object used.

### Why it is correct

- **Counter side:** the reserve and the claim linearise on the SQLite writer, so
  a block usable to a writer is never claimable by GC and vice-versa.
- **File side:** a writer only relies on a file it reserved (refcount ≥ 1 →
  unclaimable); on a collision it writes a disjoint generation, so GC's unlink of
  the old generation can land at any time.
- **Liveness invariant:** a live object's blocks are refcount ≥ 1, hence never
  claimed, hence never deleted and never re-generationed — so every live object's
  reads resolve.

## The block lifecycle

| State | `(refcount, available)` | File | Meaning |
|---|---|---|---|
| Absent | no row | — | unknown block |
| Live | `(≥ 1, true)` | present | reserved and/or bound |
| Dead | `(0, true)` | present | no references; reusable or claimable |
| Claimed | `(0, false)` | present (doomed) | GC owns it; will unlink + delete row |

Transitions:

- **Absent → Live** — writer writes the file and inserts the row (new block).
- **Dead → Live** — writer reserve resurrects a dead-but-unclaimed block (reuse
  file).
- **Live → Dead** — last reference released (object unbound, or reservation
  released).
- **Dead → Claimed** — GC claim flips `available`.
- **Claimed → Absent** — GC unlinks the file, then deletes the row.
- **Claimed → (new gen) Live** — a writer that wanted the claimed block mints a
  fresh generation; the claimed generation reaps independently.

## Crash safety and recovery

- **GC orders `unlink` before the row delete.** So "row gone" implies "file
  already gone" — never the reverse.
- Crash after `unlink`, before the row delete → a `claimed` row with no file →
  the next GC pass or the audit removes the row. No live reference points at it
  (it was refcount 0).
- Crash mid-`put` after reserving → a leaked reservation keeps the block
  **retained**; the audit reconciles the count down once quiescent.
- A block stuck `unavailable` because GC died mid-reclaim is reset by
  startup/the audit (or carries a lease), so a writer never waits on a dead GC.
- A brand-new file written before its row is committed is an orphan; the orphan
  reaper skips files younger than an **mtime grace**, so in-flight writes and
  crash-retries are safe.

Every failure mode leans toward retention.

## The reachability audit (backstop)

A periodic worker, the safety net ADR-0023 F2 requires:

- **Reaps orphan files** — block/object files with no row, older than the mtime
  grace.
- **Reconciles refcount drift** — recomputes the true reference set by walking
  `name_versions → objects → object_blocks → blocks`. It asserts
  `refcount ≥ object-derived count` (the excess is in-flight reservations, which
  are normal) and only hard-reconciles blocks that have been quiescent past the
  grace.
- **Alarms on the forbidden state** — a live object whose block file is missing.
  The protocol makes this impossible; if the audit ever sees it, that is a bug to
  investigate, not a routine repair.

## What M1c builds

In dependency order (each slice independently testable, per the M1b playbook):

1. **CAS deletion primitive** — `ContentStore::remove` for objects and blocks,
   plus generation-aware block paths and a cheap existence check.
   Conformance-tested.
2. **`blocks` schema migration** — add `gen` and `available`, re-key on
   `(hash, gen)`.
3. **Reserve-before-rely in the write path** — move the block refcount bump to
   write time; bind hands reservations off to edges; failed puts release.
4. **The `Collector` trait + reference collector** — claim → unlink → delete in
   small batches (online), over the index's unreferenced sets.
5. **The reachability audit** — orphan reaper, drift reconcile, the forbidden-
   state alarm.
6. **UX** — `fq-cas gc` (run a pass, report bytes reclaimed). The always-on
   background worker lands with the service in M5.

## Test plan

The property / integration tests target the protocol directly:

- **reserve-vs-claim linearises** — hammer a block with concurrent reserves and
  GC claims; assert exactly one wins each round and the loser falls back, with no
  corruption.
- **collision mints a new generation** — `put` content re-creating a block GC has
  claimed; assert a new generation is written, the object reads back, and the old
  generation is reaped.
- **delete-then-GC-reclaims** — an object and its now-unreferenced blocks are
  gone after a pass; shared blocks survive while another object references them.
- **crash-mid-reclaim** — interrupt between `unlink` and the row delete; assert no
  live reference dangles and the store recovers.
- **audit reconciles / alarms** — corrupt a refcount and assert reconciliation;
  induce a missing block for a live object and assert it *alarms*.
- **online** — reads and writes succeed throughout a GC pass.

## References

- [ADR-0023](../adrs/accepted/0023-storage-and-vector-foundation.md) — storage
  foundation; fork F2 (GC algorithm) is the parent decision this elaborates.
- [ADR-0024](../adrs/accepted/0024-separate-databases-storage-foundation.md) —
  the storage index is SQLite #1, the single writer this protocol linearises on.
- [Storage + vector foundation plan](../plans/active/2026-06-27-storage-vector-foundation.md)
  — M1c is where this is built.
