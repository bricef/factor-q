# Storage GC — object/manifest reclaim: the forbidden state, two protocols, and their model check

Companion to [storage garbage collection](storage-garbage-collection.md) and
[storage-gc-verification](storage-gc-verification.md). Those cover the **block**
reclaim protocol. This note covers the **object/manifest** layer above it —
issue #173 — where a distinct forbidden state lives that the block model cannot
express, and where there is a genuine protocol fork to decide before code.

**Verification leads implementation, as for blocks:** state the invariant,
model-check both candidate protocols, pick one, *then* build. The models are
[`storage_gc_objects_backoff.tla`](storage_gc_objects_backoff.tla) and
[`storage_gc_objects_gen.tla`](storage_gc_objects_gen.tla); reproduce every
number below with the commands in [Reproduce](#reproduce).

## The forbidden state the block model cannot see

The block model ([`storage_gc.tla`](storage_gc.tla)) abstracts objects to a
single per-block counter, `objects[b] : Nat` — "how many bound objects reference
this block." Its safety invariant is about **block files**: a referenced block
has its file. There is no *manifest* in that model, no *object row* with its own
lifecycle, and no *name*. So the object-level forbidden state is not merely
unchecked there — it is **inexpressible**:

> **S1-obj — no live name over a missing manifest.** For every object a live
> name resolves to, its manifest file exists. Equivalently, `get` on a
> resolvable name never fails with a missing manifest.

The manifest is the little content-addressed record listing an object's blocks;
`get` and alias-`bind` both read it. If a live name resolves to a CID whose
manifest has been unlinked, `get` corrupts rather than cleanly missing. That is
S1-obj, and it is what these models add the object layer to check.

### How the race arises (the shape #173 reported)

Blocks are protected by a compare-and-swap on a shared `available` bit: the
collector `CLAIM`s a block (flip `available` off, conditional on `refcount = 0`)
before it unlinks the file, and a writer `RESERVE`s (bump `refcount`, conditional
on `available`) before it relies on the block. One of the two CASes wins; the
loser falls back. Objects, in the shipped code, have **neither** — the `objects`
table is `(cid, refcount)` with no `available` bit, and the collector unlinks the
manifest *unconditionally*, before the only refcount re-check:

```
for cid in unreferenced_objects():   # refcount = 0
    content.remove(cid)               # (1) unlink manifest — UNCONDITIONAL
    index.delete_object(cid)          # (2) delete row — guarded (refcount==0)
```

and `bind`-aliasing an existing object bumps the object refcount 0→1 while
*trusting* the manifest present. So a `bind` that resurrects a dead-but-not-yet
collected object races step (1), and ends with a live name over a removed
manifest. The row guard at (2) correctly refuses to delete a resurrected row —
but the manifest is already gone, and no guard covers step (1).

The fix in both candidate protocols is the same in spirit: give objects the
block protocol's shape — a claim CAS, and reserve-before-rely — so the manifest
is protected from the instant it matters until a name takes over. The **fork** is
what a writer does when it meets an object the collector has already claimed.

## The two candidate protocols

Both add an `available` bit to the object row, a `CLAIM → UNLINK → DELETE`
collector sequence that unlinks the manifest only after a successful claim, and a
`RESERVE → MATERIALIZE → BIND` writer path that bumps the object refcount before
relying on the manifest. They differ on one axis.

### Back-off ([`storage_gc_objects_backoff.tla`](storage_gc_objects_backoff.tla))

Objects carry **no generation**. A manifest is content-addressed — one content
per CID, forever — so there is only ever one manifest path per object. A writer
that meets a **claimed** object cannot mint a variant of it; it **backs off** and
retries once the collector's `DELETE_ROW` makes the CID absent, then materializes
it afresh. One manifest path per CID; a writer briefly waits on the collector
under direct contention.

### Generation ([`storage_gc_objects_gen.tla`](storage_gc_objects_gen.tla))

Objects carry a **generation**, exactly as blocks do. A writer that meets a
claimed generation **mints a fresh one** — a new manifest at `(cid, g+1)` — and
never waits, because the claimed generation's row persists (steering the mint to
a higher generation) until the collector deletes it. This makes the object
protocol *structurally identical to the block protocol*: it re-runs
`storage_gc.tla` at the object layer, carrying the same `OneAvailable` (I1)
invariant and the same unbounded generation tokens.

## What TLC found

### Both protocols are safe — but only *with* reserve-before-rely

A first sketch of back-off skipped the reservation: write the manifest, then bind
the name in two steps, on the theory that a content-addressed manifest needs no
CAS. **TLC refuted it in nine steps.** Two puts of one CID interleave with a full
collect cycle: writer B writes the manifest, writer A creates→names→unbinds the
object, the collector claims→unlinks the manifest→deletes the row, and then
writer B's *stale* bind creates a live named object over the now-deleted manifest
— S1-obj. The lesson is precisely why blocks reserve before relying: **the
manifest must be protected by a refcount from the instant it is written until a
name takes over.** Adding the reservation step (materialize creates the row at
refcount 1 *with* the manifest, so the collector cannot claim it) closes it. Both
committed models carry the reservation and are clean:

| Model | Distinct states | Depth | Invariants checked | Time |
|---|---|---|---|---|
| Back-off (safety) | **8,535** | 27 | SafeObj, LiveHasManifest, ClaimedHasNoRefs, RefcountDominates | ~2 s |
| Generation (safety) | **226,008** | 32 | the same **+ OneAvailable** | ~20 s |

Both check `SafeObj` (S1-obj) across every interleaving of two writers and the
collector, including a crash between any two steps (bounded by `MaxCrash`).

### Liveness

Both hold `GCProgress`, `WriterProgress`, and `EventualReclaim` under the same
strong-fair-audit / weak-fair-writer fairness the block model uses — back-off
across 250 distinct states, generation across 2,243 (at the tighter bounds
liveness checking needs; the generation model is isomorphic to `storage_gc.tla`,
whose liveness is separately established at 203,770 states).

One honesty note on `WriterProgress`: in the back-off model a writer that meets a
claimed object yields to idle (the put attempt returns for the caller to retry),
so `WriterProgress` reads as "a put step always resolves," not "a put eventually
succeeds." The stronger property — a writer is never *starved* by an adversarial
collector — is where the two protocols genuinely differ: generation is wait-free
by construction (mint and proceed), while back-off's writer waits for the
collector's `DELETE_ROW`. That wait is bounded because the strong-fair audit
guarantees `DELETE_ROW` occurs, and the contention window (a put of exactly the
CID being collected, right now) is vanishingly rare for manifests. Modelling
writer *intent* to check "eventually succeeds" directly is left to the
deterministic simulation, which drives real retries.

## The choice

The recommendation is **back-off**, unless review weighs writer wait-freedom on
the manifest path as essential:

- **Safety is equal** — both eliminate S1-obj across every interleaving.
- **Back-off is dramatically simpler.** No generation dimension, no
  `OneAvailable` invariant, and a ~26× smaller state space at identical bounds. A
  manifest is content-addressed, so the *reason* blocks need generations —
  wait-free replacement of a hot, contended, shared block — does not transfer:
  manifests are one-per-object, cheap, and rarely contended at the exact instant
  of collection.
- **The cost is a bounded, rare writer wait**, not a correctness compromise. The
  generation variant buys wait-freedom the manifest path does not need, at the
  price of re-running the entire block protocol — generation tokens, convergence,
  I1 — a second time.

In short: blocks earn their generations by being hot and shared; manifests do
not, so back-off gives objects the *safety* of the block protocol without its
*machinery*. The generation model is retained as the checked alternative and as
the precise statement of what wait-freedom would cost.

### A note on the `bind`-alias loser

Under either protocol, a `bind` that aliases an object the collector has just
claimed cannot resurrect it (the claim CAS denies it) and cannot re-materialize
the manifest itself — `bind` holds only block hashes, not the sizes a manifest
needs. So the aliasing writer returns a retryable `StoreError::Conflict`, exactly
as it already does when a *block* it needs was claimed mid-alias. Only `put`
(which holds the bytes) can re-materialize; in the back-off model that is the
retry after `DELETE_ROW`, in the generation model the fresh-generation mint.

## The fault map (object layer)

Mirrors the block fault map; every step leans to retention.

| Step | Crash (`kill -9`) | I/O error | Lost un-fsynced write |
|---|---|---|---|
| `RESERVE` | leaked reservation → object **retained**; audit reconciles when quiescent | put fails; no change | index txn atomic |
| `MATERIALIZE` (manifest + row) | manifest with no row → orphan → reaper (mtime grace) reclaims | put fails → `RELEASE` | manifest **must** be fsync'd before the row that names it; else a crash loses a named object's manifest (the object analogue of the block I2 refinement) |
| `BIND` | reservation leaked → retained; audit reconciles | put fails → `RELEASE` | index txn atomic |
| `CLAIM` | object `CLAIMED`, manifest present → next GC / audit resumes or resets to available | GC retries | claim lost → object stays available (safe) |
| `UNLINK` | `CLAIMED` row, no manifest, refcount 0 → audit / next GC deletes the row | GC retries; object retained | **unlink durable before `DELETE_ROW` commits** |
| `DELETE_ROW` | reclaimed | GC retries | row-delete lost → object stays `CLAIMED` → re-reclaimed (safe) |

Concurrency, as for blocks, is the central axis: `RESERVE` vs `CLAIM` on the same
object must linearise — exactly one wins. The `bind`-alias vs `CLAIM` race is the
S1-obj attack, checked directly by the models.

## Verification strategy

| Technique | Covers | When |
|---|---|---|
| **TLA⁺ / TLC** — both candidate models, exhaustive over interleavings + crash | S1-obj, structural invariants, GC liveness; the protocol choice | *before* code; re-run on any protocol change |
| **Deterministic simulation + nemesis** — real code, seeded, injected crashes / conflicts | S1-obj, recovery, writer *retry* termination | continuous |
| **Adversarial `bind`-vs-collect** — the hand-crafted worst interleaving | S1-obj as an attack | dedicated |
| **Audit as oracle** — after every op, assert S1-obj + zero drift | all | every test |
| **Crash-consistency / fsync** — manifest durable before its row | the `MATERIALIZE` refinement | dedicated |

The models check design-level safety and liveness. The un-fsynced durability
refinement (manifest before row) and writer-retry *termination* (a put with
intent eventually stores) are the deterministic simulation's to witness on the
real code, exactly as the block layer splits its obligations.

## Reproduce

Requires a JRE and `tla2tools.jar` (declared toolchain dependency — no in-repo
checker is maintained for this layer):

```sh
cd docs/design/committed
# Safety (both protocols):
java -cp tla2tools.jar tlc2.TLC storage_gc_objects_backoff.tla
java -cp tla2tools.jar tlc2.TLC storage_gc_objects_gen.tla
# Liveness (FairSpec + temporal properties):
java -cp tla2tools.jar tlc2.TLC -config storage_gc_objects_backoff_liveness.cfg storage_gc_objects_backoff.tla
java -cp tla2tools.jar tlc2.TLC -config storage_gc_objects_gen_liveness.cfg     storage_gc_objects_gen.tla
```

Each `.tla` auto-uses its same-named `.cfg` for safety; the liveness configs are
passed explicitly. A CI job with a JRE + `tla2tools` should re-run these on any
change to the object reclaim protocol.

## References

- [Storage garbage collection](storage-garbage-collection.md) — the block
  protocol these object models sit above.
- [storage-gc-verification](storage-gc-verification.md) — the block layer's
  claims, fault map, and TLC results this note mirrors.
- [`storage_gc.tla`](storage_gc.tla) — the block model the generation variant
  re-instantiates.
- [ADR-0023](../../adrs/accepted/0023-storage-and-vector-foundation.md) F2 — the
  GC decision (refcounting + audit backstop).
