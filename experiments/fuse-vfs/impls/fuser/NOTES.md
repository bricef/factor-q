# fuse-vfs-fuser — implementer notes

Blind bake-off implementation for the `fuser` crate (v0.15.1).
Spec: `docs/plans/active/2026-07-09-fuse-vfs-bakeoff.md`.

## Build path

- **fusermount-only; no libfuse.** Built with `fuser = { version = "0.15",
  default-features = false }`, which mounts by exec'ing the setuid
  `fusermount3` binary instead of linking libfuse. `libfuse3-dev` was **not**
  needed; the crate never touches pkg-config on this path. This is the crate's
  documented no-libfuse mode, not a workaround.
- Dependency footprint (this path): fuser + libc, transitively `nix`,
  `zerocopy`(+derive), `log`, `smallvec`, `page_size`, `memchr`, `bitflags`,
  `cfg-if` — 17 crates in `cargo build`, cold release build ~23 s.

## Async posture

- **Sync callback loop, no async runtime.** `fuser::mount2` blocks the calling
  thread and dispatches `Filesystem` trait callbacks (`&mut self`) serially
  from a single-threaded session loop. There is no tokio integration in the
  crate; embedding in factor-q would mean `spawn_mount2`/a dedicated thread
  plus channel/`block_on` bridging into the tokio world. All store access here
  is `&mut self` with zero locking, which is pleasant for the toy store but is
  also the ceiling: one in-flight FUSE request at a time.

## What was implemented

`lookup`, `getattr`, `setattr` (size/mode/atime/mtime honoured), `mknod`
(regular files only), `mkdir`, `unlink`, `rmdir` (`ENOTEMPTY`), `rename`
(POSIX overwrite semantics + `RENAME_NOREPLACE`/`RENAME_EXCHANGE`), `open`
(O_TRUNC), `read`, `write` (offset writes, zero-fill holes, extend), `flush`,
`fsync`, `fsyncdir`, `readdir`, `statfs` (fixed plausible geometry), `access`,
`create`, and `link`/`symlink` as explicit `EPERM` (out of scope for v1; the
explicit errno makes tool fallbacks — e.g. git's link→rename — deterministic).
Defaults were fine for `forget`/`open`/`opendir`/`release`/`releasedir`; xattr
ops were left at the crate's `ENOSYS` defaults and nothing in the ladder
minded.

Store: flat `HashMap<u64, Inode>` inode table; dirs hold a `BTreeMap<OsString,
u64>` (deterministic, offset-stable readdir). Unlinked inodes stay in the
table so still-open handles keep working (open-after-unlink is real — cargo
and git rely on it); memory is never reclaimed, which is fine for a spike.

## Awkward spots (crate-attributable)

- **Nothing unsupportable.** Every op the ladder exercises mapped 1:1 onto a
  trait method; no non-obvious default had to be overridden to *pass* — but
  two defaults are traps to know about: `flush` and `fsync` default to
  `ENOSYS` (must override to `ok()` or fsync-ing tools see errors), while
  `open`/`release` default to success.
- **`setattr` is a 15-parameter callback** mirroring the raw FUSE ABI
  (crtime/chgtime/bkuptime included on Linux where they mean nothing). The
  crate does no decomposition into truncate/chmod/utimens — you switch on
  `Option`s yourself. Ugly but mechanical.
- **Inode-and-lookup addressing throughout** (parent ino + name, `u64` inos,
  you manage the table and, implicitly, lookup counts). Fine here since
  `forget` can be ignored for an in-memory store, but a CAS-backed store will
  have to own an ino↔object mapping and its lifetime. The crate gives no help;
  it is a thin, honest mirror of the kernel protocol.
- **`readdir` offset protocol is manual**: you pass "offset of the *next*
  entry" to `reply.add()` and must resume from the kernel-supplied offset with
  a stable iteration order. Easy to get subtly wrong; nothing in the API
  enforces it.
- Reply objects are single-shot and easy to use (`reply.error(errno)` /
  typed success), but nothing stops you forgetting to reply on some branch —
  it's a runtime hang, not a compile error.

## Harness results (this machine, release build)

All five default rungs pass; representative of two runs:

```
rung         fuse   impl_s   base   base_s   ratio
smoke        ok     0.03     ok     0.03     1.0x
many_small   ok     2.60     ok     2.17     1.2x
large_file   ok     1.69     ok     0.94     1.8x
git          ok     0.16     ok     0.07     2.3x
cargo        ok     0.41     ok     0.33     1.2x
```

Small-rung ratios are noisy run-to-run (smoke 1.0x–3.8x); `large_file`
(~64 MB through a single-threaded loop) is the steadiest signal at ~1.8–2.0x.

## Rough LoC

`src/main.rs`: 740 lines total, ~660 non-blank/non-comment (rustfmt-expanded;
a good third is the wide trait signatures and `if let` destructuring of the
inode enum, not logic).
