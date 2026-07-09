# FUSE VFS bake-off

A comparison spike: which Rust FUSE crate best fits factor-q's harness-owned
virtual filesystem (ADR-0028, draft). One fixed spec, four **blind**
implementations, one shared harness.

- **Spec:** [`docs/plans/active/2026-07-09-fuse-vfs-bakeoff.md`](../../docs/plans/active/2026-07-09-fuse-vfs-bakeoff.md)
- **Harness (the shared control):** [`harness/run.sh`](harness/run.sh)
- **Implementations:** `impls/<crate>/` — one blind Cargo binary per crate
  (`fuser`, `fuse3`, `easy_fuser`, `fuse-backend-rs`).

## Running

Validate the harness itself (no implementation, real FS only):

```sh
harness/run.sh --baseline-only
```

Run an implementation (mounts it, exercises it, benchmarks vs. a real FS):

```sh
harness/run.sh impls/fuser/target/debug/fuse-vfs-fuser
harness/run.sh impls/fuser/target/debug/fuse-vfs-fuser --heavy   # + the factor-q build rung
```

FUSE mounts work in the ordinary sandbox via setuid `fusermount3`; no elevation
needed (see the spec's step-0 findings).

## Rules

The four implementations are **blind and independent** — each built from the
spec + this harness only, none seeing another or any reference. There is no
reference implementation, by design. The harness is the objective control and
each implementation's own acceptance test.
