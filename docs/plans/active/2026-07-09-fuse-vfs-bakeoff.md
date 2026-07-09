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
