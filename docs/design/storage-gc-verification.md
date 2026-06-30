# Storage GC — claims, fault map, and verification

Companion to [storage garbage collection](storage-garbage-collection.md): the
explicit correctness claims for the online-reclaim protocol, the map of failure
states it must recover from, and the verification strategy we run *before and
during* M1c.

The protocol is bedrock, so **verification leads implementation**: state what
must always be true, model-check the design, *then* build slice by slice with
fault injection and the invariants as live oracles. The unifying rule throughout
is **every failure leans to retention — leak a block, never lose one.**

## Vocabulary: the named steps

Every operation is a sequence of named, individually-failable steps. These names
are the axes of the fault map and the targets of fault injection.

- **put (per block):** `RESERVE` → (`WRITE_FILE` | `MINT` on collision) → `BIND`
  (or `RELEASE` on failure)
- **gc (per block):** `CLAIM` → `UNLINK` → `DELETE_ROW`
- **audit:** `REAP_ORPHAN`, `RECONCILE`, `ALARM`

## Claims

### Invariants (hold in every committed state)

- **I1 — one live generation:** at most one row per hash has `available = true`.
- **I2 — live blocks have files:** `refcount > 0` ⟹ the block's file exists.
  Holds across crashes because `WRITE_FILE` / `MINT` **fsync** the block file
  before committing the row that makes refcount positive — an un-fsynced crash
  would otherwise lose the file while the row says refcount 1 (confirmed by the
  model below).
- **I3 — no unlink under reference:** `UNLINK` runs only on a `CLAIMED` block
  (`available = false ∧ refcount = 0`); a claimed block cannot be reserved, so
  refcount stays 0 through the unlink.
- **I4 — counts dominate references:** `refcount ≥` the number of bound objects
  referencing the block; any excess is in-flight reservations.
- **I5 — manifests resolve:** every block a bound object names has `refcount ≥ 1`.

### Safety — the one claim that matters

- **S1 — no lost live block (the forbidden state never occurs):** for every bound
  object and every block in its manifest, the file exists. Equivalently, `get` on
  a resolvable name never fails with a missing block. *Follows from I5 + I2.*

### Liveness

- **L1 — eventual reclaim:** a block that stays dead past the grace is reclaimed.
- **L2 — no writer starvation:** every `put` completes (a collision mints a new
  generation and proceeds; it never blocks).
- **L3 — convergence:** transient duplicate generations are reduced to one.
- **L4 — bounded recovery:** after any crash, the audit restores every invariant
  in bounded work.

## The fault map

Every step must fail and recover cleanly. This is the living map; each cell names
the recovered state, and each gets a test that proves it.

| Step | Crash (`kill -9`) | I/O error | Lost un-fsynced write |
|---|---|---|---|
| `RESERVE` | leaked reservation → block **retained**; audit reconciles when quiescent | put fails; no state change | n/a — index txn is atomic |
| `WRITE_FILE` / `MINT` | orphan file, no row → **reaper** (mtime grace) reclaims | put fails → `RELEASE`s reservation | orphan or absent → reaper / retry; canonical file is never torn (atomic temp + rename) |
| `BIND` | reservations leaked → retained; audit reconciles | put fails → `RELEASE` | index txn is atomic |
| `CLAIM` | block `CLAIMED`, file present → next GC / audit resumes or resets to available | GC retries | claim lost → block stays available (safe) |
| `UNLINK` | `CLAIMED` row, no file, refcount 0 → audit / next GC deletes the row (no live ref) | GC retries; block retained | **requires unlink durable before `DELETE_ROW` commits** — tested under write-reorder faults |
| `DELETE_ROW` | reclaimed | GC retries | row-delete lost → block stays `CLAIMED` → re-reclaimed (safe) |

Concurrency is its own axis: `RESERVE` vs `CLAIM` on the same block must
**linearise** — exactly one wins, the loser falls back. That is the central
concurrency test.

## Verification strategy

Each technique mapped to the claims it covers and when it runs.

| Technique | Covers | When |
|---|---|---|
| **TLA⁺ / TLC model check** — exhaustive over interleavings + crash points | S1, I1–I4, L1–L3 (design level) | *before* code; re-run on any protocol change |
| **Deterministic simulation + nemesis** — seed-reproducible, *real* code, injected crashes / I-O faults / conflicts | S1, all invariants, recovery | continuous; millions of seeds |
| **Fault-matrix failpoints** (`fail` crate, every named step) | the fault map, recovery | per slice |
| **`loom` / `shuttle`** — exhaustive / randomised Rust interleavings | the concurrency (reserve-vs-claim) | per slice |
| **Audit as oracle** — run after every op; assert zero drift + S1 | I1–I5, S1 | every test |
| **Crash-consistency / fsync** — reorder/drop un-fsynced writes, tear files at crash | I2, I3, the `UNLINK` ordering | dedicated |
| **Adversarial reach-the-forbidden-state** — hand-crafted worst interleavings | S1 (as an attack) | dedicated |
| **Soak / chaos** — long randomised workload + `kill -9` + restart + audit | accumulation / rare bugs | CI nightly |
| **Differential** — GC vs no-GC return identical content for every read | GC is observably invisible | per slice |

TLC checks I1–I4 and L1–L3 directly; **I5** is a corollary of I4 (a referenced
block has `refcount ≥ 1 > 0`), and **L4** (recovery in *bounded* work) is a
complexity property covered by the deterministic simulation, not TLC.

### Cross-cutting requirements

- **Reproducibility:** every randomised run logs its seed; a failure replays the
  exact interleaving.
- **Design-for-testability seams:** the filesystem, the clock, the crash point,
  and named failpoints sit behind traits so the simulator and the fault matrix
  can drive them. Baked in from slice 1, not retrofitted.
- **History logging:** record the operation history so a Jepsen-style
  linearizability check (Elle/Knossos) can be added when the service goes
  multi-node (M5). Single-node now ⟹ TLA⁺ + DST + `loom`; full Jepsen later.

## The model and how it was checked

[`storage_gc.tla`](storage_gc.tla) (config [`storage_gc.cfg`](storage_gc.cfg))
models the protocol abstractly — block rows `(refcount, available)` keyed by
`(hash, gen)`, the named steps as actions, a crash that can fire between any two
steps, and a recovering audit — and has TLC check **S1** and **I1–I4** across
every interleaving.

**TLC** model-checks it: safety (`storage_gc.cfg`) is clean across **226,008
states** (S1, I1–I4, crashes bounded by `MaxCrash`). An independent explicit-state
checker, [`storage-gc-check.py`](storage-gc-check.py), encodes the same transition
system and agrees; it was written when TLC's Java was unavailable, and earned its
keep — it found a real gap (a stale "write a fixed generation" decision could
create a second available generation, violating **I1**), now fixed by unifying the
new-block and collision paths into one `Materialize` that re-checks at execution
time. A **CI job** with a JRE + `tla2tools` could re-run TLC on every protocol
change.

Two refinements have since been added to the checker:

- **Un-fsynced crash.** Modelling a crash that also drops files not yet fsynced
  surfaced a concrete requirement — a block file **must be fsync'd before its
  index row is committed**. Without it the checker finds an **I2** violation in
  three steps (`Reserve → Materialize → Crash` loses the just-written,
  not-yet-durable block while its row already says refcount 1); with the fsync
  the model is clean.
- **Liveness — and why the audit is load-bearing.** TLC's per-action fairness
  found what the Python checker's coarser fairness missed: under only *weak*
  fairness, a crash can orphan a generation (claimed, refcount 0, collector idle)
  that the online collector never resumes because it stays busy with live churn —
  a permanent leak that (with bounded generations) even starves a writer.
  Modelling the **reachability audit as strong fairness** — it systematically
  visits every block — and bounding crashes (liveness holds only modulo finitely
  many faults), TLC confirms across **203,770 states** that `GCProgress`,
  `WriterProgress`, and `EventualReclaim` (every dead block, crash-orphans
  included, is eventually reclaimed or reused) all hold. This is the formal
  argument for *why M1c needs the audit*, not just the online collector.

Reproduce: `java -cp tla2tools.jar tlc2.TLC storage_gc.tla` (safety) and the same
with `-config storage_gc_liveness.cfg` (liveness); for the Python cross-check,
`CRASH=unfsynced SYNC=0|1` and `MODE=liveness`. The un-fsynced durability
refinement lives only in the Python checker; the `.tla` keeps the clean-crash
model plus a bounded-crash counter for liveness.

## References

- [Storage garbage collection](storage-garbage-collection.md) — the protocol this
  verifies.
- [ADR-0023](../adrs/accepted/0023-storage-and-vector-foundation.md) F2 — the GC
  decision (refcounting + audit backstop).
- Method prior art: FoundationDB / TigerBeetle deterministic simulation; Jepsen
  (concurrent histories + nemesis, Elle/Knossos); TLA⁺ / TLC; `loom` / `shuttle`
  for Rust; the `fail` crate for failpoints.
