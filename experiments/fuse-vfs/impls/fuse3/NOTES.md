# fuse-vfs-fuse3 — implementer notes (blind run)

Implementation of the bake-off contract
(`docs/plans/active/2026-07-09-fuse-vfs-bakeoff.md`) for the **`fuse3`** crate
(v0.9.0), written blind from the spec + shared harness only.

## Harness result

All default rungs pass; two consecutive runs for stability:

```
rung         fuse   impl_s   base   base_s   ratio        rung         fuse   impl_s   base   base_s   ratio
smoke        ok     0.05     ok     0.06     0.8x         smoke        ok     0.03     ok     0.02     1.5x
many_small   ok     3.01     ok     2.70     1.1x         many_small   ok     3.30     ok     2.47     1.3x
large_file   ok     1.61     ok     0.98     1.6x         large_file   ok     1.48     ok     0.90     1.6x
git          ok     0.10     ok     0.07     1.4x         git          ok     0.15     ok     0.06     2.5x
cargo        ok     0.50     ok     0.38     1.3x         cargo        ok     0.67     ok     0.42     1.6x
```

## Build path

- **fusermount-only; libfuse is never linked.** `fuse3` is a pure-Rust
  implementation of the FUSE protocol: it talks to `/dev/fuse` directly and,
  with the `unprivileged` feature, obtains the device fd by exec'ing the
  setuid `fusermount3` binary and receiving the fd over a unix socket
  (`Session::mount_with_unprivileged`). There is no libfuse build path at all —
  no pkg-config probe, no `-lfuse3`.
- `libfuse3-dev` is **not installed** on this box and was **not needed**.
  `ldd` on the release binary shows only `libc`/`libm`/`libgcc`.
- Features used: `tokio-runtime` (pick one of two runtimes) + `unprivileged`.

## Async posture

- **Tokio-native, fully async.** The `raw::Filesystem` trait is an async trait
  (via `trait-make`, i.e. RPITIT + `Send` futures), `Session::mount*` are
  `async fn`s, and the returned `MountHandle` is itself a `Future` that
  resolves when the filesystem is unmounted — `main` is
  `runtime.block_on(async { session.mount_with_unprivileged(...).await?.await })`.
  This is the best-case shape for factor-q's tokio runtime: no bridging thread,
  the backend's async methods run directly on the runtime.
- **Requests are dispatched concurrently** — the session spawns a task per
  request — so the backend must be `Send + Sync` and do its own locking from
  day one (I used a single `std::sync::Mutex<Store>`; no awaits are held across
  it). Good for a real CAS backend, but it means even a toy store needs a lock.
- **The dependency contract cannot be met literally.** The spec allows "only
  the FUSE crate + libc/std", but with fuse3 you must add `tokio` yourself to
  start the runtime (the crate does not re-export it), and `futures-util` (or
  equivalent) to construct the `Stream`s its readdir replies require. I added
  `tokio` (rt-multi-thread only) and `futures-util` and count them as the
  crate's own runtime surface, not extra VFS crates. Recorded as rubric data.

## Awkward spots

- **readdir offset semantics are the big trap.** The session does *not* skip
  already-delivered entries: the implementation must resume the listing from
  the requested `offset` itself and set each entry's `offset` field to the
  index of the *next* entry. Nothing in the docs says so — I found it by
  reading the session source. Getting it wrong yields duplicated or infinite
  directory listings. Also inconsistent types: `readdir` takes `offset: i64`,
  `readdirplus` takes `offset: u64`.
- **Both readdir and readdirplus must be implemented.** The session advertises
  `FUSE_DO_READDIRPLUS` when the kernel supports it, so `ls` arrives as
  readdirplus; the plain readdir path still exists for other callers. The
  reply types want a `Stream` of entries — for an in-memory Vec that's
  `futures_util::stream::iter`, plus an `#[allow(refining_impl_trait)]` if you
  name the concrete stream type instead of mirroring the trait's opaque
  `impl Stream … + 'a` signature.
- **Negotiated kernel flags change your obligations silently.** fuse3 enables
  `FUSE_ATOMIC_O_TRUNC` when available, so `open` receives `O_TRUNC` and must
  truncate (a setattr-only truncate impl would miss it). Discovered by reading
  the init handshake in the session source.
- **`ReplyOpen { fh: 0 }` means "stateless IO"** — an implicit convention: fh 0
  is not a valid handle. I allocate nonzero fhs for files (to track
  unlink-while-open) and use 0 for directories.
- **Defaults are safe.** Everything I didn't implement (xattrs, lseek
  SEEK_DATA/HOLE, copy_file_range, ioctl, poll, …) returns `ENOSYS`, which the
  kernel degrades gracefully for. `init` must be implemented (returns
  `max_write`); `destroy` is a required empty method.
- **Two API levels; I used the lower one.** fuse3 also ships
  `path::PathFilesystem` — a path-addressed trait the crate's own docs
  recommend first; it bridges all inode bookkeeping via an internal
  ~1.2k-line inode↔path table. I chose the inode-based `raw::Filesystem`
  deliberately: the harness's git/cargo rungs are rename-heavy, and I did not
  want a bridge layer I can't debug between the kernel and the store. For a
  CAS-shaped backend the path variant is a genuinely interesting ergonomic
  option (rubric: addressing model), but it is unmeasured here.

## Unsupported / not implemented (out of scope per spec)

- Symlinks, hard links, device nodes, xattrs (spec: out of scope for v1).
- `RENAME_EXCHANGE` (returns `EINVAL`); `RENAME_NOREPLACE` is handled.
- Nothing the harness exercises was unsupportable by the crate.

## Size / footprint

- **LoC:** 829 total in `src/main.rs`; 716 non-blank/non-comment.
- **Deps:** 4 direct (`fuse3`, `libc`, `tokio`, `futures-util`); 44 crates in
  the full tree. Cold release build ~2 min on this box (release, incl. tokio).
