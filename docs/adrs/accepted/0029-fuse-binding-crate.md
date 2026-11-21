# ADR-0029: FUSE binding crate — `fuse-backend-rs`

## Status

Accepted (2026-07-09). Refines [ADR-0028](0028-tool-scoped-isolation-and-workspace.md),
which decided that the agent workspace is a harness-owned virtual filesystem
over fq-store's CAS, bound per-tool — including a **FUSE mount** for
host-binary tools (`cargo`, `git`, `rustc`) that do real syscalls — but left
*which* Rust FUSE crate open. This ADR fixes that one choice. It is a
lower-stakes, more reversible decision than most ADRs (it is an
implementation-library pick behind the `FileSystem` trait), recorded because
it was made by experiment rather than by argument.

## Context

Four candidate crates — `fuser`, `fuse3`, `easy_fuser`, `fuse-backend-rs` —
were compared by a **blind bake-off**: one written spec and one shared control
harness, no reference implementation, four independent same-prompt
implementations (Fable 5, xhigh), each worktree-isolated and blind to the
others, then scored centrally against a rubric fixed before any code was
written. Methodology, the full 8-axis scorecard, the controlled performance
sweep, and per-crate verdicts live in the experiment record:

- **Spec + methodology + results:** [FUSE VFS bake-off](../../plans/closed/2026-07-09-fuse-vfs-bakeoff.md)
  (see *Results & verdict*).
- **Evidence trail:** branches `experiment/fuse-vfs-fuser`,
  `experiment/fuse-vfs-fuse3`, `experiment/fuse-vfs-easy_fuser`,
  `experiment/fuse-vfs-fuse-backend-rs`, each with its own `NOTES.md`.

All four built, mounted, and passed the full ladder (`smoke` → `many_small`
→ `large_file` → `git` → `cargo`) in-sandbox, and **none needed
`libfuse3-dev`** (all mount via the setuid `fusermount3` path). Feasibility
was never the question; fit was. Two factor-q-specific facts decided it:

1. **The VFS backs onto fq-store's async, concurrent CAS** (ADR-0026/0028) —
   the FUSE layer calls async code and the store is already `Send + Sync`, so
   the "zero-locking" upside of a single-threaded sync callback loop is moot
   for our backend.
2. **The flagship workload is a parallel build** — an agent running
   `cargo`/`rustc` in its workspace issues many concurrent file reads, which a
   one-request-in-flight session loop serialises.

## Decision

1. **`fuse-backend-rs` is the FUSE binding crate** for the harness-owned VFS.
   It fits factor-q's backend and workload best on the rubric:
   - **inode + handle addressing** maps directly onto content-addressed CAS
     objects — no path/kernel-state coupling forced on the store;
   - **concurrent request dispatch** (driven over several channel threads)
     matches the parallel-build workload and gave the best measured
     performance (~1.0–1.2× real-FS on the metadata- and throughput-heavy
     rungs, vs ~1.3–1.7× for the others);
   - the crate handles the **full wire protocol** including INIT capability
     negotiation, dirent packing, and `io::Error` → errno replies;
   - **production pedigree** — it is the rust-vmm/Cloud-Hypervisor backend
     that powers virtiofsd, Kata, and ChromeOS crosvm — which weighed heavily
     for a load-bearing foundation, and positions us for a virtiofs/VM-
     isolation future without switching FUSE stacks.
   Licence (Apache-2.0 + BSD-3-Clause) is BSL-1.1 compatible.

2. **`fuse3` is the recorded fallback.** If the sync-thread `block_on`-into-
   the-async-store bridge proves awkward in practice, `fuse3` is the
   async-native alternative: callbacks `.await` the CAS store directly, task-
   per-request concurrency for free, no manual thread management — at the cost
   of a heavier dependency tree (~2-minute cold build) and two footguns to pin
   down once (the undocumented `readdir` offset-resumption contract and the
   `i64`/`u64` offset split between `readdir`/`readdirplus`).

3. **`fuser` and `easy_fuser` are not chosen.** `fuser` is the canonical,
   leanest, most-portable (macFUSE) crate and remains the right pick for a
   *sync* backend or for macOS support, but its single-threaded, one-in-flight
   session loop serialises the concurrent I/O our workload depends on.
   `easy_fuser` has the least boilerplate but v0.5.0 API churn, proc-macro +
   codegen driver internals, and unlinked-open-file identity loss in its
   path-addressed mode — too much risk under a foundation.

## Consequences

- The VFS integration wires `fuse-backend-rs`'s `FileSystem` trait to
  fq-store's CAS behind factor-q's own `FileSystem` trait; the harness owns
  inode allocation and `nlookup`/`FORGET` bookkeeping, and drives a small
  (~25-line) channel-thread service loop that `block_on`s into the async
  store. This is more plumbing than a minimal binding — accepted as a one-time
  cost for the fit above.
- Known integration gotchas to carry into the implementation: the crate
  defaults `allow_other = true`, which `fusermount3` rejects without
  `user_allow_other` in `/etc/fuse.conf` — call `set_allow_other(false)`; and
  it hard-wires `default_permissions` into the mount options.
- **Revisit if** the VFS backend turns out sync + single-consumer (then
  `fuser` is simpler and leaner), or if agent isolation moves to a virtiofs/VM
  model (then `fuse-backend-rs` is confirmed for a different, vhost-user
  reason). The blind-bake-off harness and branches remain runnable, so a
  re-evaluation is cheap.

## Alternatives considered

The three other crates, above; scored in full against the 8-axis rubric in the
[bake-off results](../../plans/closed/2026-07-09-fuse-vfs-bakeoff.md). No
non-FUSE mechanism was considered here — ADR-0028 already decided FUSE as the
host-binary VFS binding; this ADR only selects the crate.
