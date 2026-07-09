# fuse-backend-rs — implementer notes (blind run)

Implementation of the bake-off contract
(`docs/plans/active/2026-07-09-fuse-vfs-bakeoff.md`) against
`fuse-backend-rs` v0.14.0 (rust-vmm / virtiofsd lineage).

## Harness result

All five default rungs pass (two consecutive runs):

```
rung         fuse   impl_s   base   base_s   ratio
smoke        ok     0.04     ok     0.03     1.3x
many_small   ok     4.48     ok     4.60     1.0x
large_file   ok     1.32     ok     1.10     1.2x
git          ok     0.11     ok     0.19     0.6x
cargo        ok     0.67     ok     0.63     1.1x
```

(Second run: 0.8x / 1.3x / 1.2x / 2.0x / 1.4x — the sub-second rungs are
noise-dominated; many-small and large-file are stable at ~1.0–1.3x.)

## Build path: fusermount-only, no libfuse, no libfuse3-dev

The crate **never links libfuse** — there is no pkg-config probe and no C
dependency at all. Its `fusedev` transport (a default feature) opens
`/dev/fuse` itself and tries a direct `mount(2)`; on `EPERM` it transparently
falls back to spawning `fusermount3` with the `_FUSE_COMMFD` fd-passing
protocol. In the sandbox (CapEff 0) the fallback path is what runs, and it
works with the stock setuid `fusermount3` — `libfuse3-dev` was **not** needed.

Two mount gotchas found by reading the crate source:

- `FuseSession` defaults `allow_other = true`, and fusermount3 rejects
  `allow_other` unless `/etc/fuse.conf` has `user_allow_other` (it doesn't
  here). Must call `session.set_allow_other(false)` or the mount fails.
- The crate hard-wires `default_permissions` into the mount options, so the
  kernel does mode-bit permission checks; reported modes have to be sane.

## Async posture: sync callback loop, self-driven threads

The primary API is fully synchronous: a `FileSystem` trait of sync callbacks
(chromeos/virtiofsd style), plus a transport you drive yourself. There is
**no built-in run loop**: you call `session.new_channel()` N times and run
`channel.get_request()` → `server.handle_message(...)` loops on threads you
spawn (this impl uses 4 channel threads; the kernel distributes requests
across them). For factor-q's tokio world this means a bridged thread pool —
same posture as a sync-callback crate, except even the *event loop* is your
code (~25 lines).

There is a separate `async-io` feature with an `AsyncFileSystem` trait, but
it is built on **tokio-uring** (io_uring), not plain tokio — it pulls in its
own runtime and is aimed at virtiofs/vhost-user backends, so it was not used
here.

## What the crate makes you do vs. gives you

- Provided: the whole FUSE wire protocol (`Server`), INIT negotiation
  (you return wanted `FsOptions`, it intersects with kernel capabilities),
  request framing, readdir dirent packing, errno replies (any `io::Error`
  returned from a callback becomes the errno reply), mount/unmount incl.
  the fusermount fallback.
- Your job: everything stateful. Inode allocation, `nlookup`/FORGET
  bookkeeping (if you want deleted files to actually free memory), directory
  handles, and the service loop. The trait is inode+handle addressed (u64s
  of your choosing), which fits a CAS-shaped store well — nothing forces
  paths or kernel-managed state on you.

## Awkward / noteworthy

- **Nothing was unsupportable.** Every op in the spec mapped 1:1 onto a trait
  method with the data needed.
- The default `statfs` returns zero totals — overridden to report plausible
  64 GiB totals as the spec requires.
- `readdir`'s `add_entry` callback returns bytes-consumed; `0` means "reply
  buffer full, stop and wait for the next offset". Not documented loudly;
  learned from the server's `add_dirent` source.
- Non-obvious default overrides needed: `flush`/`fsync`/`fsyncdir` default to
  `ENOSYS` (kernel converts to success after the first call, so defaults
  would *work*, but explicit no-ops are cleaner); `open`'s default returns
  no handle (fine), but `create` has no default at all.
- O_APPEND is the filesystem's responsibility when writeback caching is off —
  handled in `write()` by honouring the open flags that arrive with every
  request (so no per-open state table is needed for files at all).
- `ATOMIC_O_TRUNC` + `MAX_PAGES` (1 MiB writes) negotiated in `init()`;
  `KEEP_CACHE`/`CACHE_DIR` open options are safe because nothing mutates the
  tree behind the kernel's back.
- Docs are thin; implementing from rustdoc alone would be rough. The trait
  doc-comments (inherited from crosvm) are good, but the transport/session
  layer basically requires reading the crate source and its in-tree
  passthrough example.

## Footprint

- Rough LoC: 922 total, 806 non-comment/non-blank (single `main.rs`:
  in-memory tree ~430, FileSystem impl ~370, session/main ~80).
- Dependencies: `fuse-backend-rs` + `libc` direct; 24 crates in the resolved
  tree (mio, nix 0.24, vm-memory, vmm-sys-util, caps, radix_trie, thiserror,
  arc-swap, …) — noticeably heavier than a minimal binding, it's built for
  virtiofsd-class daemons.
- Cold release build: ~26 s in the sandbox.
- License: Apache-2.0 AND BSD-3-Clause (BUSL-compatible).
