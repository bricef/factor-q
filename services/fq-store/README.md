# fq-store

Content-addressed storage and semantic index for
[factor-q](../../README.md) (Phase 2 pillar #2). This crate is the storage
substrate the memory and skill services build on, and it ships **`fq-cas`**,
a standalone CLI over the content store.

**Status: M1a** — the content-addressed store (CAS). The storage index,
garbage collection, and access-control layers are in progress (see
[the plan](../../docs/plans/active/2026-06-27-storage-vector-foundation.md)).

## `fq-cas` — the content store CLI

Store arbitrary files — deduplicated, addressed by the BLAKE3 hash of their
content — and read them back by id.

### Install

```sh
# pre-built binary (once a release is published)
curl -fsSL https://raw.githubusercontent.com/bricef/factor-q/main/install.sh | sh

# or build from source
cargo build --release --features cli --bin fq-cas    # -> ./target/release/fq-cas
```

### Use

```sh
fq-cas put file.bin                        # store a file -> prints its content id (cid)
echo hi | fq-cas put -                     # store from stdin
fq-cas get <cid>                           # read content to stdout
fq-cas get <cid> -o out.bin                # ...or to a file
fq-cas get <cid> --offset 0 --length 64    # a byte range
fq-cas has <cid>                           # present? (exit 0 / 1)
fq-cas size <cid>                          # byte size
fq-cas metrics                             # objects/blocks, sizes, dedup ratio (--json too)
```

The store lives under `--root` (env `FQ_CAS_ROOT`, default `./.fq-cas`).
Run `fq-cas --help` for the full surface.

### Distributed mode

The same store can run as a network service, so a client can talk to a
remote (or, later, production) instance — useful for debugging and backups:

```sh
fq-cas serve --bind 127.0.0.1:9000           # run the server (one terminal)
fq-cas --server 127.0.0.1:9000 put file      # client: every command works remotely
fq-cas --server 127.0.0.1:9000 metrics
```

> The server is **unauthenticated** for now (capability tokens land in M2),
> so keep it on localhost.

## Library

`fq-store` is also a library. The core is the `ContentStore` trait
(`put` / `get` / `get_range` / `has` / `size` / `stats`), with a
BLAKE3 + FastCDC filesystem backend (`fs::FilesystemStore`) and a `tarpc`
network client (`service::RemoteStore`, behind the `service` feature).

Every backend is held to one bar: the shared, property-based **conformance
suite**. See
[Implementing a storage backend](../../docs/guide/implementing-a-storage-backend.md).

## Performance & observability

The CAS sits far below LLM-call latency on the agent path, so it is not a
bottleneck there — but the seams to ask perf questions later are in place:

- **Tracing.** Every operation is `tracing`-instrumented (off unless a
  subscriber is attached, so zero-cost by default). `fq-cas` wires one up on
  stderr gated by `RUST_LOG` — `RUST_LOG=fq_store=debug fq-cas put file`
  prints per-op spans with span-close timings, leaving stdout clean.
- **Benchmarks.** `cargo bench` runs throughput baselines (`benches/throughput.rs`,
  on-demand, not CI). Rough dev-hardware baseline: `get` ~400 MiB/s, `put`
  ~80 MiB/s (many small block files + atomic renames), small-op latency
  ~0.2–2 ms — orders of magnitude under a multi-second LLM call. Bulk
  ingestion is the path where `put` throughput would matter, if it ever does.

## Features

- `cli` — the `fq-cas` binary (pulls in `clap` and `service`).
- `service` — the `tarpc` server and `RemoteStore` client.

Library consumers that only need the in-process store enable neither.

## License

[BUSL-1.1](../../LICENSE) — see the repository root.
