# FUSE VFS bake-off — experiment spec

**Status:** active · **Date:** 2026-07-09

## Purpose

Choose the Rust FUSE crate for factor-q's harness-owned virtual filesystem.
ADR-0028 (draft, PR #53 — tool-scoped isolation + a harness-owned VFS) makes
a FUSE mount the way a host binary (`cargo`, `rustc`, `git`) sees the agent's
workspace. This experiment compares four candidate crates against one fixed
spec, one shared test harness, and one fixed rubric — so the choice rests on
measured fit, not on whichever library we happened to touch first.

The comparison is only trustworthy if it is **blind**: see "Experimental
design" below. This document is written to be implementable from itself alone.

## Background

- The workspace is a `FileSystem`-trait VFS backed by fq-store's CAS; host
  binaries reach it through a FUSE mount (ADR-0028 decision 4). This spike
  isolates and validates the **FUSE-binding** half of that decision.
- The in-memory store used here is a deliberate stand-in for the CAS. The
  experiment measures the *crate*, not the store, so the store is trivial.
- Related design: `docs/design/committed/tool-isolation-model.md` (the tier
  model), `docs/design/aspirational/wasm-posix-sandbox.md` (the sibling WASM
  binding), and design principle 3 (nothing ambient; the agent's filesystem
  reality *is* the mount).

### Step-0 findings (already validated, 2026-07-09)

- **FUSE mounts work in-sandbox.** A `fuser` hello-world mounted
  (`type fuse (ro,nosuid,nodev … user_id=1000)`) and `ls`/`cat`/`grep` read
  through it, with **no** sandbox override — the setuid `fusermount3` (v3.14)
  does the mount; `CapEff: 0` in the caller does not block it. So the harness
  and every implementation run **in the ordinary sandbox**; no `mise exec`.
- **`libfuse3-dev` is not installed.** `fuser`'s default build panics probing
  pkg-config for `fuse3`. Building with `default-features = false` makes
  `fuser` mount via the `fusermount3` binary instead of linking libfuse, and
  builds cleanly. The **link-libfuse vs. fusermount-only** build path is a real
  difference between crates and is scored (see rubric).
- Toolchain: rustc/cargo 1.95; `git`, `just` present; `/dev/fuse` world-rw.

## Experimental design (the integrity of the bake-off)

1. **Four blind, independent implementations.** Each is built from *this spec
   and the shared harness only*. No implementation may see another, and there
   is **no reference implementation** — deliberately. A reference would shape
   the others toward its structure, turning "how well does crate X fit the
   problem" into "how well does crate X fit the reference's approach."
2. **Constant author.** All four are produced by the same model with the same
   prompt (or one peer model), so coding style is held constant and the
   observed differences are attributable to the *crate*, not the author.
3. **Self-verifying.** Each implementation must pass the shared harness — which
   mounts it, exercises it, and benchmarks it — in-sandbox. A passing run is
   real behaviour, not a compile check.
4. **Central, side-by-side scoring.** Once all four exist and pass, one
   reviewer scores them against the fixed rubric with all four in view.

## The implementation contract

Each blind implementation produces a standalone Cargo binary crate.

- **Location & name:** `experiments/fuse-vfs/impls/<crate>/`, package name
  `fuse-vfs-<crate>` (`<crate>` ∈ `fuser`, `fuse3`, `easy_fuser`,
  `fuse-backend-rs`).
- **Dependencies:** *only* the assigned FUSE crate, plus `libc`/std as needed.
  No other VFS/filesystem/store crate. The backing store is a hand-rolled
  in-memory tree — implementing our own backend is the whole point.
- **CLI:** `fuse-vfs-<crate> <mountpoint>` mounts a **read-write** virtual
  filesystem at `<mountpoint>`, backed by an in-memory store that starts
  **empty**. It runs in the foreground (driving the FUSE session) until the
  filesystem is unmounted, then exits `0`. On a fatal mount error it exits
  non-zero with a message on stderr.
  - Prefer the no-libfuse build path if the crate supports it (record which
    path was used and why). If the crate *requires* libfuse, note it — that is
    data for the rubric, not a failure.
  - Auto-unmount is not required; the harness unmounts via `fusermount3 -u`.
- **The store:** an in-memory tree of directories and regular files (byte
  contents). Symlinks, devices, xattrs, and hard links are **out of scope for
  v1** (a tool that needs them may degrade, and the harness avoids them).
  Permissions/uid/gid may be reported as the mounting user with sane modes.

### Operations the implementation must support

The bar is behavioural: **a real host binary operating in the mount must see
the same result it would on a real filesystem.** Concretely, implement the
POSIX operations the harness and its tools exercise:

- **List / stat:** `lookup`, `getattr`, `readdir` (`.`/`..` + entries),
  `statfs` (return plausible non-zero totals — some tools divide by them).
- **Read:** `open`, `read` (honour `offset`/`size`; partial reads at EOF),
  `release`.
- **Write / create:** `create`, `write` (honour `offset`; extend the file),
  `mkdir`, `setattr` (at minimum `size` → truncate/extend; accept `mtime`/mode
  no-op-ok), `flush`, `fsync` (may be no-ops).
- **Delete / move:** `unlink`, `rmdir` (reject non-empty, `ENOTEMPTY`),
  `rename` (within the tree; overwrite semantics as POSIX).
- Return correct errnos for the obvious cases: `ENOENT`, `EEXIST`,
  `ENOTEMPTY`, `EISDIR`/`ENOTDIR`, `ENOSPC` never (in-memory).

### What the implementer records (in the crate's `NOTES.md`)

- Build path used (link libfuse vs. fusermount-only) and whether
  `libfuse3-dev` was needed.
- Async posture: does the crate's API run on a tokio runtime, or is it a
  sync-callback loop bridged to a thread?
- Any operation the crate made awkward, any op it could not support, and any
  place a default impl had to be overridden non-obviously.
- Rough LoC of the implementation (harness reports a normalised count too).

## The shared harness (single-authored control — identical for all)

`experiments/fuse-vfs/harness/run.sh <impl-binary>` mounts the implementation,
runs the rung ladder against the mount, re-runs each rung against a real
`tmpdir` **baseline**, checks correctness (the resulting tree must be
byte-identical to the baseline's), and prints a results table:

```
rung           correct  impl(s)  baseline(s)  ratio
smoke          ok       0.03     0.01         3.0x
many-small     ok       1.84     0.12         15x
large-file     ok       0.42     0.09         4.7x
git            ok       2.10     0.55         3.8x
cargo          ok       9.7      6.1          1.6x
just-ci*       ok       …        …            …
```

Everything is done **through** the mount (populate by copying in, run tools in
the mount, read results out), so the harness is purely external and identical
across implementations — the implementation only has to serve a correct
read-write VFS. `--baseline-only` runs just the real-FS side (validates the
harness with no implementation). `--rungs a,b` selects rungs.

### The rung ladder (progressively harder)

1. **smoke** — `mkdir` a tree; write a file; read it back; `ls`; `stat`;
   `rename`; `rm`; `rmdir`. Exercises every verb once.
2. **many-small** — create ~500 small files across nested dirs; `grep -r`;
   `wc`; `find`; delete half; re-list. Metadata / `readdir` stress.
3. **large-file** — write a ~64 MB file, read it back, `sha256sum`. Throughput.
4. **git** — `git init`; copy a small source tree in; `git add -A`;
   `git commit`; `git status`; `git log`. A real tool doing many stats/reads.
5. **cargo** — copy a tiny crate in; `cargo build`. A heavier real tool
   (compiler I/O patterns).
6. **just-ci** — a bounded factor-q subset in the mount and a bounded command
   (default: `cargo build -p fq-runtime`; full `just ci` behind
   `--heavy`). The realistic top-end workload. Marked `*` — allowed to be the
   most expensive and the last to pass.

Each rung: run against the mount → snapshot the tree (`find | sort` + per-file
`sha256`); run the same rung against a fresh real `tmpdir` → snapshot; assert
the snapshots match; report both wall-times and the ratio. A rung whose trees
differ is a correctness failure for that implementation.

### Harness requirements

- Runs in the ordinary sandbox (FUSE mounts there). Needs `git`, `cargo`,
  `just`, `sha256sum`, `grep`, `find` on `PATH`.
- Robust mount wait (poll the mount table with a timeout), and guaranteed
  cleanup (`fusermount3 -u` + kill + `rm` on any exit path).
- Times are wall-clock; each timed rung is run once warm after a discarded
  warm-up pass where noted, so caching effects don't dominate.

## The rubric (fixed before any implementation)

Each implementation is scored on:

1. **Async / tokio fit** — does the crate integrate with an async runtime, or
   force a sync-callback loop on a bridged thread? (factor-q is tokio.)
2. **Backend ergonomics** — boilerplate to implement the in-memory store; how
   leaky/awkward the trait; inode-and-lookup vs. path-based addressing; how
   naturally a CAS-shaped, content-addressed store would slot behind it.
3. **Operation surface** — what the crate makes you implement vs. provides
   sane defaults for; coverage of what real tools call; any unsupportable op.
4. **Performance** — the harness ratios (FUSE vs. real-FS), weighted toward the
   metadata-heavy rungs (`many-small`, `git`) and throughput (`large-file`),
   with `cargo`/`just-ci` as the realistic end.
5. **Build & dependency footprint** — link-libfuse (needs `libfuse3-dev`) vs.
   fusermount-only; direct + transitive dep count; cold compile time.
6. **Maturity / maintenance / licence** — last release, activity, open issues
   on core ops, docs quality, BSL-1.1 compatibility.
7. **Portability** — Linux-only vs. macOS (macFUSE). Noted; Linux-first for now.
8. **Subjective ergonomics** — one reviewer's side-by-side read of the four
   implementations. The least objective axis; weighted accordingly.

## The candidates

Current understanding (the experiment confirms/corrects it):

- **`fuser`** — the most-used binding; sync callback trait; `default-features
  = false` avoids `libfuse3-dev` (step-0 confirmed). The de-facto baseline.
- **`fuse3`** — async/tokio-native, higher-level API over FUSE3.
- **`easy_fuser`** — aims at a more ergonomic, higher-level API surface;
  posture and async story to be confirmed by the spike.
- **`fuse-backend-rs`** — the rust-vmm lineage (used by virtiofsd / cloud
  hypervisors); a lower-level FUSE transport with a different shape; heavier
  but the most control. Async story to be confirmed.

## Execution

- **Written here (the control):** this spec + the shared harness. No reference
  implementation, by design.
- **Four blind runs:** one task per crate — "implement the contract above for
  `<crate>`; pass the shared harness." Delegable to the dogfood fleet or a peer
  model (e.g. Fable 5, xhigh), same prompt for all four, each blind to the
  others. Each run self-verifies with the harness in-sandbox.
- **Scoring:** central, side-by-side, against the rubric, once all four pass.

## Open questions / notes

- **`just-ci` weight.** Full `just ci` on factor-q may be too slow for a
  comparison loop; the default top rung is `cargo build -p fq-runtime`, with
  full CI behind `--heavy`.
- **Warm vs. cold.** FUSE and page-cache warm-up can dominate small rungs; the
  harness discards a warm-up pass on the timed rungs and should be checked for
  stability across runs.
- **Store is deliberately trivial.** The in-memory tree is a CAS stand-in; a
  crate that is awkward for a simple tree will be worse for a real backend, so
  fit on the simple case is a valid signal.
- **Outcome.** The result feeds back into ADR-0028 (the FUSE-binding choice)
  and, if a crate is clearly best, promotes from draft to a recorded decision.

## Results & verdict (2026-07-09)

Four blind Fable-5 (xhigh) runs, one per crate, each on its own branch
(`experiment/fuse-vfs-<crate>`), no reference implementation, no cross-talk
(worktree-isolated). **All four built, mounted, and passed the full default
ladder** (`smoke` → `many_small` → `large_file` → `git` → `cargo`) on the first
scored harness run, and **none needed `libfuse3-dev`** — every crate mounts via
the setuid `fusermount3` path in-sandbox. So the feasibility question the
bake-off existed to answer is settled four times over; the choice is about
*fit*, not *whether*.

### Performance — one controlled pass, all four back-to-back

Per-crate numbers from the blind runs are **not comparable** (different
worktrees, different machine load). Re-run here in a single controlled sweep,
FUSE-vs-real-FS ratio (lower is better), medians of 3 on the two rungs that
carry signal — the sub-100 ms rungs (`smoke`, `git`) are startup-dominated
noise and omitted:

| Rung (ratio vs real FS) | fuser | fuse3 | easy_fuser | fuse-backend-rs |
|---|---|---|---|---|
| `many_small` (500-file metadata) | 1.5× | 1.3× | 1.6× | **1.0×** |
| `large_file` (64 MB throughput)  | 1.7× | 1.6× | 1.6× | **1.2×** |

All four sit within ~2× of a real in-memory FS — excellent for FUSE, and
**performance is not the differentiator**. `fuse-backend-rs` leads on both,
occasionally beating the real FS on metadata, because it dispatches requests
across several channel threads; the other three cluster in the noise.

### Scorecard (8 axes, `Best` / `Good` / `OK` / `Weak`)

| Axis | fuser | fuse3 | easy_fuser | fuse-backend-rs |
|---|---|---|---|---|
| 1. Async / tokio fit | Weak — sync single-thread loop, bridge + 1-in-flight | **Best** — tokio-native async trait, callbacks `.await` directly | OK — own threadpool / own runtime, not unified | Good — sync but drives N channel threads (concurrent) |
| 2. Backend / CAS ergonomics | OK — you own the inode↔object table | Good — raw-inode or path trait | Good — least boilerplate (~130-line store), but unlinked-open identity loss in PathBuf mode | **Best** — inode+handle addressing maps straight onto CAS objects |
| 3. Operation surface / defaults | OK — `fsync` ENOSYS trap; forgotten reply = hang | Good — safe ENOSYS defaults; must do `readdir`+`readdirplus` | OK — most ops required, ENOSYS preset + macro | **Best** — full surface + INIT capability negotiation |
| 4. Performance | Good (1.5–1.7×) | Good (1.3–1.6×) | Good (1.6×) | **Best** (1.0–1.2×) |
| 5. Build / dep footprint | **Best** — 17 crates, ~23 s | Weak — 44 crates, ~2 min (tokio+futures) | OK — 25–40 crates, ~41 s, build-time codegen | Good — 24 crates, ~26 s |
| 6. Maturity / maintenance / licence | **Best** — canonical crate, MIT | OK — active, async niche | Weak — v0.5.0, API churn, codegen internals | **Best** — crosvm/virtiofsd production pedigree, Apache-2.0+BSD-3 |
| 7. Portability | **Best** — macFUSE support | OK — Linux-first | OK — wraps fuser | OK — Linux-first (virtiofs-oriented) |
| 8. Subjective ergonomics | Honest, predictable, known warts | Clean async, but undocumented `readdir` offset footgun | Magical but opaque (jump-to-def lands in `OUT_DIR`) | Powerful but low-level; read the source |

### Recommendation — primary: **fuse-backend-rs**; fallback: **fuse3**

The two factor-q-specific facts that decide it:

1. **The VFS backs onto fq-store's async, concurrent CAS** (ADR-0026/0028) — the
   FUSE layer calls *async* code, and the store is already `Send+Sync`. The
   "zero-locking" upside of a single-threaded sync loop (fuser) is therefore
   moot for our backend.
2. **The flagship workload is a parallel build** — an agent running `cargo
   build`/`rustc` in its workspace issues many concurrent file reads. A
   one-request-in-flight session loop serialises exactly that. Concurrent
   dispatch is worth real wall-clock here.

**`fuse-backend-rs`** checks the most boxes for *this* backend and *this*
workload: inode+handle addressing that maps directly to content-addressed
objects (axis 2), the best measured performance via concurrent channel threads
(axis 4), full wire-protocol handling incl. INIT capability negotiation (axis
3), and a production pedigree (it powers virtiofsd / Cloud-Hypervisor / Kata /
ChromeOS). It also positions us for a virtiofs/VM-isolation future without
switching FUSE stacks. Costs, all one-time: a lower-level integration (drive a
~25-line channel-thread loop yourself; `block_on` into the async CAS store), the
`set_allow_other(false)` mount gotcha, and 24 transitive crates. Licence
(Apache-2.0 + BSD-3) is BSL-1.1 compatible.

**`fuse3`** is the fallback: if we'd rather the FUSE layer speak async
*natively* — callbacks `.await` the CAS store with no `block_on`, task-per-
request concurrency for free, zero manual thread management — at the price of a
heavier dep tree (~2-min cold build) and two footguns to pin down once (the
undocumented `readdir` offset-resumption contract and the `i64`/`u64` offset
split between `readdir`/`readdirplus`). It becomes primary if the `block_on`
bridge proves awkward in practice.

**Not recommended as primary:**

- **`fuser`** — the canonical, leanest, most-portable (macFUSE), best-documented
  crate, and the *right* choice for a **sync** backend. But its single-threaded,
  one-in-flight session loop serialises the concurrent I/O our flagship workload
  depends on, and every callback still bridges to the async store. Keep it as
  the macOS/portability reference and for any sync-backed mount.
- **`easy_fuser`** — genuinely the least boilerplate (path-addressed; its
  `PathResolver` owns all inode bookkeeping; ~130-line store), but v0.5.0 API
  churn, proc-macro + Jinja-codegen driver internals, and unlinked-open-file
  identity loss in PathBuf mode are too much risk under a load-bearing
  foundation. Revisit if it matures.

**The hinge (state it plainly):** this ranking puts *concurrent-I/O throughput
+ CAS-native addressing* above *raw maturity + leanness*, because the backend is
an async content-addressed store and the workload is parallel builds. **If the
VFS backend turns out sync + single-consumer, `fuser` wins instead** on maturity
and footprint.

### Follow-ups

- Promote the FUSE-binding choice into **ADR-0028** (currently accepted, but the
  binding crate was left open) once the primary is confirmed.
- The four implementation branches + per-crate `NOTES.md` are preserved as the
  evidence trail: `experiment/fuse-vfs-{fuser,fuse3,easy_fuser,fuse-backend-rs}`.
- The blind runs used a *trivial in-memory* store; the real integration wires the
  chosen crate's trait to fq-store's CAS behind the `FileSystem` trait — where
  `fuse-backend-rs`'s inode+handle model and INIT negotiation earn their keep.
