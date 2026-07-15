# ADR-0030: Object/manifest garbage collection uses back-off, not generations

## Status

Accepted (2026-07-15). Refines
[ADR-0023](0023-storage-and-vector-foundation.md) (F2 — GC by refcounting
with an audit backstop) by fixing the object/manifest layer of the reclaim
protocol (issue #173). Builds on the **block** reclaim protocol
([storage garbage collection](../../design/committed/storage-garbage-collection.md),
[`storage_gc.tla`](../../design/committed/storage_gc.tla)); the design and the
model check are in
[storage-gc-objects-verification](../../design/committed/storage-gc-objects-verification.md).

## Context

The online collector reclaims two kinds of thing: **blocks** (content chunks)
and **objects** (a manifest file per CID listing an object's blocks, plus its
index row). The block layer is protected against the writer/collector race by a
compare-and-swap on a shared `available` bit — the collector `CLAIM`s a block
before unlinking its file; a writer `RESERVE`s before relying on it — backed by a
**generation** dimension so a writer that meets a claimed block mints a fresh
generation and never waits.

The object layer shipped with **neither**: the `objects` table is
`(cid, refcount)` with no `available` bit, and the collector unlinks the manifest
*unconditionally*, before the only refcount re-check. A `bind` that aliases a
dead-but-not-yet-collected object bumps its refcount while trusting the manifest
present. So a `bind` races the unlink and can leave a **live name resolving to a
missing manifest** — the object-level forbidden state (S1-obj), a data-corruption
bug. The block model (`storage_gc.tla`) cannot even express S1-obj: it abstracts
objects to a per-block count, with no manifest, object row, or name.

The fix must give objects the block protocol's shape (a claim CAS and
reserve-before-rely). The open question — the decision this ADR records — is what
a writer does when it meets an object the collector has **already claimed**:

- **Generation:** mint a fresh generation (a new manifest at `(cid, g+1)`) and
  never wait, exactly as blocks do.
- **Back-off:** retry the same CID once the collector has finished
  (`DELETE_ROW`), with one manifest path per CID.

## Decision

**Back-off.** Objects get the claim CAS and reserve-before-rely, but **no
generation dimension**. A writer that meets a claimed object backs off and
retries after the collector deletes the row; a `bind`-alias of a claimed object
returns a retryable `StoreError::Conflict` (it holds only block hashes, not the
sizes needed to re-materialize a manifest — the same behaviour it already has
when a *block* it needs is claimed mid-alias).

Both candidate protocols were modelled in TLA⁺ and model-checked with TLC before
this decision (`storage_gc_objects_backoff.tla`, `storage_gc_objects_gen.tla`).

## Rationale

- **Equal safety, verified.** Both eliminate S1-obj across every interleaving of
  writers and the collector, including a crash between any two steps — back-off
  clean across 8,535 states, generation across 226,008. TLC also refuted a
  *no-reservation* sketch of back-off in nine steps, pinning down that the
  **reservation** — not the generation — is what closes S1-obj: objects need
  reserve-before-rely exactly as blocks do.
- **Manifests are not blocks.** Generations earn their complexity on blocks
  because blocks are hot, shared, and contended, so a claimed one must be
  *replaced* wait-free. A manifest is content-addressed — one content per CID,
  forever — cheap, and rarely contended at the exact instant of collection. The
  reason for generations does not transfer.
- **Much less machinery.** Back-off drops the generation axis and the
  `OneAvailable` (I1) invariant, and checks in a ~26× smaller state space. The
  generation variant is, structurally, the block protocol run a second time at
  the object layer.
- **The cost is a bounded, rare wait, not a correctness compromise.** A writer
  waits only when it puts exactly the CID being collected at that instant; the
  strong-fair reachability audit guarantees `DELETE_ROW` occurs, so the wait
  terminates.

## Consequences

- The `objects` table gains an `available` bit; the collector gains
  `CLAIM → UNLINK → DELETE` (unlink only after a successful claim); the writer's
  object path becomes `RESERVE → MATERIALIZE → BIND`, materialize creating the
  row at refcount 1 *with* the manifest so it is protected from that instant.
- `bind`-aliasing a claimed object is a retryable `Conflict` — a caller-visible
  semantic, consistent with the existing block-claimed-mid-alias behaviour.
- One manifest path per CID; no unbounded object-generation tokens to reason
  about, back up, or bound.
- The generation model is **retained** in the tree as the checked alternative and
  as the precise statement of what wait-freedom would have cost, should manifest
  contention ever change the calculus.
- Verification declares a JRE + `tla2tools.jar` as a toolchain dependency (a CI
  job should re-run both models on any change to the object reclaim protocol);
  implementation is verified as a refinement of the back-off model, with the
  deterministic simulation witnessing writer-retry termination and the
  un-fsynced durability refinement (a manifest durable before the row that names
  it) on the real code.
