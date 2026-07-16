# Storage concurrency verification — exhaustive interleaving with an oracle at every seam

How we verify the **code**, not just the design, of the storage layer's
concurrent protocols. The TLA⁺ models
([storage-gc-verification](storage-gc-verification.md),
[storage-gc-objects-verification](storage-gc-objects-verification.md)) check the
*design* exhaustively over interleavings; this note defines the runtime method
that checks the *implementation* against the same state space — and the phased
plan to build it, driven by issue #173.

It is deliberately general. GC (blocks and objects) is the first instance, but
the method is the standard for every concurrent storage protocol that follows —
the async embed-on-store pipeline, extraction, and anything in layer 2+ that has
a writer racing a background worker.

## Why this note exists: the #173 lesson

The object/manifest race (#173) shipped despite a strong verification stack —
an invariant oracle, a seeded deterministic simulation, a conformance suite, and
a TLA⁺ model. It slipped through not because an invariant was missing — the
oracle (`verify.rs`) already checks "every live name's manifest is present" — but
because of three **coverage** gaps:

1. **Model scope.** The block TLA model abstracted objects to a per-block count,
   so the object-manifest race was inexpressible at the design level. *(Closed:
   `storage_gc_objects_backoff.tla` / `_gen.tla`.)*
2. **Interleaving coverage — the actual culprit.** The concurrency test races
   writers against the collector, but only via `put`/`unbind` on shared *blocks*
   (the reserve-vs-claim *block* race), using real `tokio::spawn` + timing
   (nondeterministic, unreproducible). It never drove `bind`-**aliasing**, the
   exact path that resurrects a dead object over a collected manifest. That
   interleaving was never scheduled.
3. **Observation window.** The oracle is asserted at *quiescence* (end of the
   concurrent churn, or between sequential ops), so a *transient* violation that
   heals before the next snapshot is invisible. (#173's corrupted state happens
   to be stable, so this was not the #173 miss — but it is a standing blind spot
   for the class.)

The lesson is not "add invariants." It is: **an oracle invariant is only as good
as the test that drives the system into a state that could violate it — and for
a concurrency invariant that means scheduling the adversarial interleaving,
deterministically and exhaustively, with the oracle checked at every step.**

## The method

### 1. Failpoint seams at every protocol step

Each protocol step becomes an individually schedulable, individually failable
point (the "design-for-testability seams" the GC verification doc calls for, via
the `fail` crate). For GC:

- **writer:** `RESERVE → MATERIALIZE → BIND` (or `RELEASE`)
- **collector:** `CLAIM → UNLINK → DELETE_ROW`
- **fault points:** each step may be told to error, and a crash may fire between
  any two steps.

These seams are the shared substrate for everything below and for the existing
deterministic simulation — nothing here is throwaway.

### 2. Exhaustive interleaving, deduped by *state* (not schedule)

A deterministic driver enumerates every interleaving of the seams' steps and
every injected fault, for a **bounded scenario** (e.g. 2 writers + 1 collector, 2
CIDs, small refcounts — the TLA bounds). The critical design point:

> **Dedupe by an abstract-state projection, not by schedule.** After each step,
> project the real (index + content) state onto the model's abstract tuple —
> per CID `(exists, refcount, available, manifest-present)` and per block
> `(refcount, available)` — and skip any interleaving that re-reaches a
> seen projection.

State-dedup is what makes this cheap and exhaustive at once: the reachable
*state* space is the TLA count (**~8,500** for back-off at these bounds), whereas
the *schedule* space is orders of magnitude larger. Deduping by state converges
to the former — exhaustive coverage of every reachable state, no seeds, well
under 10⁴ states, milliseconds to run.

**Why bespoke, not `shuttle`.** `shuttle` is the off-the-shelf systematic
concurrency tester for async Rust, and it is genuinely useful — it exercises the
*real tokio scheduler* and can catch primitive-level races an abstract projection
elides. But it explores **schedules**, not application **states**: it cannot
dedupe by our state, so its cost is the (much larger) interleaving count, its
exhaustive DFS is tractable only for tiny scenarios, and beyond that it is a
seeded/randomized bug-finder — not the exhaustive, no-seeds, conformance-shaped
prover we want. So: build the state-deduped checker as the correctness artifact;
keep `shuttle` as an optional complementary real-scheduler fuzzer, not the
primary. (Prior art for state-deduped Rust model checking: `stateright` — but it
models an abstraction; the value here is driving the *real* trait
implementations, which makes it a conformance suite, below.)

### 3. The oracle at every seam — split always vs at-rest

Running the oracle after *every* step is the move that turns "did we reach a bad
state" into "no reachable interleaving reaches one." But the oracle as written is
an **at-rest** oracle: `ObjectRefcountDrift` asserts `refcount == name_refs`,
which false-fires at a seam where a writer has RESERVED but not yet BOUND (a
legal in-flight reservation makes `refcount = name_refs + 1`). So the oracle
splits:

- **Always-invariants — asserted at every seam:** `SafeObj` (live name ⇒
  manifest present, and every referenced block file present), refcount
  **dominance** (`refcount >= name_refs`; excess is reservations), `I1` (one
  available generation), `I3` (claimed ⇒ refcount 0), no negative refcounts.
- **At-rest invariants — asserted only at scenario end:** the refcount
  **equality** (`refcount == name_refs`), true only with nothing in flight.

Concretely: change `ObjectRefcountDrift` to a dominance check for the per-seam
pass and keep the equality for the end-of-scenario pass. This mirrors the TLA
invariants, which are already shaped this way (`RefcountDominates` is `>=`, never
`==`).

### 4. Trait-generic → a reusable conformance suite

The checker is parameterised over the `ContentStore` / `NameIndex` traits, so
"survive exhaustive GC-vs-writer interleaving with the oracle clean at every
seam" becomes a suite **any** store implementation must pass — run in-process and
over the wire, exactly as `conformance.rs` already runs the functional
conformance suite against the `tarpc` remote. A future backend (an alternative
index, a different CAS) inherits the correctness bar for free.

### Scope of the guarantee

Exhaustive **within bounds** — the small-scope hypothesis, identical to the TLA
model's scope limit. It is not a global proof for unbounded inputs; it is
*complete for the bounded scenario* rather than a sample of it. Paired with the
TLA model (same bounds, design level) it gives design and implementation both
checked against one state space — a substantial step beyond seeded sampling, and
the strongest guarantee available short of a full refinement proof.

## Phased implementation plan

Verification leads implementation, as for the block layer: the failing test is
the design artifact that drives the fix.

### Phase 1 — seams + the red regression test

- Add `fail`-crate seams at the writer and collector steps above.
- Write the adversarial `bind`-alias-vs-collect test that drives the #173
  interleaving through the seams.
- **Acceptance:** the test reaches S1-obj on today's code — it goes **red** —
  proving both the bug and that the test catches it. Add the always/at-rest
  oracle split so the seam assertion is sound.

### Phase 2 — implement back-off, turn it green

- Implement [ADR-0030](../../adrs/accepted/0030-object-manifest-gc-back-off.md)'s
  back-off protocol (objects gain an `available` bit; collector
  `CLAIM → UNLINK → DELETE`; writer `RESERVE → MATERIALIZE → BIND`) as a
  refinement of `storage_gc_objects_backoff.tla`.
- **Acceptance:** the Phase 1 test is green; the existing block DST, conformance,
  and concurrency suites stay green; `just ci` (store) passes.

### Phase 3a — targeted exhaustive interleaving checker ✅

Landed in `tests/gc_exhaustive.rs`. A deterministic BFS drives the *real* index
and content primitives as discrete steps — a writer (`RESERVE → MATERIALIZE →
BIND`, mirroring `put`) and the collector (`CLAIM → UNLINK → DELETE`, mirroring
`collect`'s object arm) — enumerates every interleaving, and asserts the
always-invariants (`check_index_in_flight`) after **every** step. It dedupes by an
abstract-state projection (object kind, manifest present, name bound, each
process's PC), so it converges to the reachable-*state* count, not the far larger
*schedule* count; each successor replays its schedule from a fresh store (the
store is not cheaply snapshot-restored), which the dedup keeps bounded.

**Results (met the acceptance):** back-off is clean across **13 distinct states**
with no reachable S1-obj, no seeds, ~2 s — and the meta-check, a **sabotaged
collector** that unlinks without the claim CAS (reverting the fix's core
protection), *reaches* S1-obj across 20 states, proving the clean result is a
real guarantee, not a vacuous pass. Runs in the hermetic store `test` phase (no
`failpoints` feature — it calls the primitives directly rather than pausing real
`put`/`collect`).

Deliberately minimal scope — one writer, one collector, one object, no crash
injection — since it targets exactly the #173 object/manifest race (and would
have caught the put-path hole automatically). The `Proc`/step-machine structure
extends directly to the **Phase 3b** generalisations: a second writer, the alias
path, the block arm, and a `Crash` process that resets in-flight PCs (the model's
`Crash` action). Those, plus `error` injection at steps, are the next increments.

### Phase 3b — generalise to conformance ✅

Landed. The checker is now generic over a `StoreBackend` (a `BlockStore` +
`NameIndex` pair): implement `fresh` and the same exhaustive interleaving bar
applies — the reusable **correctness contract** for future backends. The single
`FsSqlite` backend exists today. A second scenario, the **alias** writer
(`RESERVE → BIND`, no manifest write), joins the `put` writer; each is checked
clean under back-off and non-vacuous under the sabotaged collector.

**It earned its keep immediately:** the exhaustive alias sweep found a *third*
S1-obj hole that review and the hand-written regressions missed — `reserve_object`
minted an absent object fresh even for an alias, so an alias whose target was
fully collected mid-flight created a live name over no manifest. Fixed with
`reserve_object(cid, create_if_absent)` (see
[storage-gc-objects-verification](storage-gc-objects-verification.md)). This is
the payoff the method predicted: the code-level exhaustive checker mechanically
finding the interleaving a human missed.

**One correction to the plan.** "Run over `tarpc`" does not apply to *this*
checker: the GC interleaving protocol lives at the local index + manifest layer,
and the `tarpc` service is content-only (no `write_object`, no `NameIndex`).
Over-the-wire coverage stays with the functional `conformance.rs` (content
conformance); the interleaving checker is the *local* contract.

Object reconcile is now in place (#243: `reconcile_object` plus the audit's
Phase 1 object arm, the twin of the block reconcile), so a crash-leaked object
reservation is healed past the grace rather than flagged by the at-rest oracle —
and the audit's alarm moved to the in-flight oracle, so a live in-flight reserve
is tolerated (dominance) instead of raising a spurious drift alarm. Still
deferred (documented): the `Crash` process itself — the crash-injection harness
that kills a `put` at a seam, reopens the index, and asserts the audit heals it
(#248) — plus a second concurrent writer, the block arm, and per-step error
injection. The `Proc`/step-machine structure extends to all of them.

### A standing discipline (for layer 2+)

Every oracle invariant gets a negative test that *reaches* its violation — and
for a concurrency invariant, that means a scheduled interleaving, not a
hand-mutated snapshot. A small meta-test enumerates each `Violation` variant and
asserts some test induces it (coverage of the oracle itself). Extraction and the
embed-on-store pipeline adopt the same seams-plus-exhaustive-interleaving method
from their first slice.

## References

- [storage-gc-verification](storage-gc-verification.md) — the block layer's
  claims, fault map, and TLA⁺ results.
- [storage-gc-objects-verification](storage-gc-objects-verification.md) — the
  object layer's two models and the back-off vs generation comparison.
- [ADR-0030](../../adrs/accepted/0030-object-manifest-gc-back-off.md) — the back-off
  decision this plan implements; refines
  [ADR-0023](../../adrs/accepted/0023-storage-and-vector-foundation.md) (F2).
- `services/fq-store/src/verify.rs` — the invariant oracle to split.
- `services/fq-store/src/conformance.rs` — the functional conformance pattern the
  interleaving suite extends.
- Method prior art: TigerBeetle / FoundationDB deterministic simulation;
  `shuttle` and `loom` (schedule exploration); `stateright` (state-deduped Rust
  model checking); the `fail` crate for step seams.
