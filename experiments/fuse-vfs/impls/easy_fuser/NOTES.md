# fuse-vfs-easy_fuser — implementer notes

Blind implementation for the FUSE VFS bake-off
(`docs/plans/active/2026-07-09-fuse-vfs-bakeoff.md`), crate `easy_fuser` v0.5.0.

## Harness result

All five default rungs pass (two consecutive runs):

```
rung         fuse   impl_s   base   base_s   ratio
smoke        ok     0.03     ok     0.05     0.6x
many_small   ok     6.09     ok     2.95     2.1x
large_file   ok     1.69     ok     1.01     1.7x
git          ok     0.38     ok     0.05     7.6x
cargo        ok     0.50     ok     0.36     1.4x
```

(Second run: 1.0x / 2.2x / 2.1x / 9.0x / 1.2x — small rungs are noisy.)
`just_ci` (`--heavy`) was not run.

## Build path

- **fusermount-only; `libfuse3-dev` NOT needed.** easy_fuser wraps `fuser`
  0.16 and inverts fuser's default: linking libfuse is behind an opt-in
  `libfuse` feature (`libfuse = ["fuser/libfuse"]`). The default build mounts
  via the `fusermount3` binary. No `default-features = false` gymnastics were
  needed for the libfuse question itself — but see the feature note below.
- Built with `default-features = false, features = ["parallel"]`. The crate's
  *default* features are `serial + parallel + async` — i.e. by default it
  pulls in **tokio + async-trait + threadpool** and code-generates all three
  drivers. Trimming to one mode is a one-line choice but you must know to
  make it.
- Dependency footprint: 25 crates (normal) / 40 with build-deps. The build
  script renders the driver/trait/mount code from Jinja templates at compile
  time via **askama**, so the build-dep tree includes askama + its proc-macro
  stack, plus serde/basic-toml. Cold release build: ~41 s.

## Async posture

**Sync-callback loop, with an optional tokio mode — I used neither tokio nor
plain serial but the `parallel` mode:** each FUSE callback is dispatched onto
a `threadpool` (N = available parallelism), with a second pool for replies.
The `async` feature generates an `#[async_trait]` variant of the same
handler trait running on a tokio runtime the *crate* owns
(`Runtime::spawn` per request) — it does not embed into an existing runtime;
you'd hand it a thread count and it builds its own. So for factor-q's tokio
process the realistic integration is still "FUSE session on its own
threads", whichever mode is picked; the async mode mainly buys `await`
syntax inside handlers, not runtime unification.

## What the crate gave / made awkward

**The good — path-based addressing is the headline feature.** With
`type TId = PathBuf` the crate's `PathResolver` owns the entire
inode↔path mapping (lookup counts, `forget`, and crucially it re-keys its
mapping on `rename` automatically). The handler never sees an inode: every
op arrives with a root-relative `PathBuf`, and the backend is a plain tree
walk. `lookup`/`create`/`mkdir` return just a `FileAttribute` — the crate
allocates inode numbers. That erased the usual inode-table boilerplate
almost completely; the whole store fits in ~130 lines. It also decides the
`readdir` offset/continuation protocol for you (handler returns the full
`Vec`, the driver windows it, caching partial iterators keyed by offset).

**The awkward:**

- **The trait has almost no default implementations.** Only
  `forget`/`init`/`destroy`/`readdirplus` default; everything else — down to
  `bmap`, `ioctl`, `getlk`/`setlk`, xattrs — is a required method. The
  sanctioned escape hatch is composing `DefaultFuseHandler` (an
  ENOSYS-answering preset) and delegating with the `delegate_fs!` proc-macro
  from a sibling `easy_fuser_macro` crate (re-exported). It works, but "list
  the 14 ops you don't support in a macro invocation" is strictly more
  ceremony than fuser-style provided defaults, and the preset's methods are
  inherent (not a trait), so you can't just `impl`-inherit.
- **v0.5.0 restructured the API** (per-mode preludes,
  `easy_fuser::fuse_parallel::prelude::*`, presets moved, delegation macros
  introduced) — docs/examples on the web lag it; the shipped README is
  current but the examples/ dir in the package is just an ideas file.
- **Path-based mode has semantic edges for a real backend:** unlinked-open
  files lose their identity (the resolver keeps the stale path; the store no
  longer has it), so unlink-then-write-through-fd would EIO — no rung
  exercises it, but a CAS-backed workspace serving real tools eventually
  will (Inode or HybridId mode would be the fix, giving up the free
  mapping). Stateless file handles are natural here; there is no place to
  hang an open-file reference by design in PathBuf mode.
- **Small sharp bits:** `OwnedFileHandle::from_raw` is `unsafe` even for a
  meaningless stateless handle value (the crate's own preset does the same);
  the driver logs every errno reply through `log::warn!` (silent without a
  logger, but ENOENT-per-negative-lookup as *warnings* is odd); the whole
  driver is code-generated from Jinja templates at build time, so
  jump-to-definition lands in `OUT_DIR`.
- **Convention quirk:** `readdir` must include `.`/`..` yourself (the
  MirrorFs preset does), and the driver feeds those names into the path
  mapper as if they were children — harmless in practice, but the mapper
  visibly wasn't designed with them in mind.

**Unsupported / not implemented** (all out of scope per spec, all answered
with ENOSYS via delegation; the tools' fallbacks were exercised by the
rungs): hard `link` (git falls back to rename, cargo/rustc to copy —
observed working in the `git`/`cargo` rungs), `symlink`/`readlink`,
xattrs, POSIX locks (`getlk`/`setlk`; kernel-local flock still works, which
is what cargo uses), `copy_file_range`, `fallocate`, `lseek`
(SEEK_DATA/HOLE), `bmap`, `ioctl`. Nothing the spec required was
unsupportable.

**Non-obvious overrides:** none required beyond the delegation block; but
note `statfs` *must* be implemented (a `DefaultFuseHandler`-delegated statfs
would ENOSYS and `df`/some tools misbehave — the spec's "plausible non-zero
totals" is implemented by hand).

## LoC

- `src/main.rs`: 645 lines total, **546 non-blank/non-comment** (single
  file: store ~130, handler ~360, main ~15, rest comments/imports).
