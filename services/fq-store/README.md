# fq-store

Content-addressed storage and semantic index for
[factor-q](../../README.md) (Phase 2 pillar #2). This crate is the storage
substrate the memory and skill services build on, and it ships **`fq-cas`**,
a standalone CLI over the content store.

**Status: M1a–M1c + M2** — the content-addressed store (CAS), the storage
index (names, version history, two-level reference counts), garbage collection
(M1c), and access control (M2: grants, capability tokens, the op-boundary
gate). The gate is a library API driven by the CLI's operator paths; remote
exposure of the named service is M5 (see
[the plan](../../docs/plans/active/2026-06-27-storage-vector-foundation.md) and
the [access-control guide](../../docs/guide/access-control.md)).

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

### Named objects

The commands above are content-addressed (you get back a cid). The `object`
subcommand group adds the **name layer** (M1b) — store and read content by a
hierarchical dotted name, with version history:

```sh
fq-cas object put research.papers.doc1 paper.pdf   # store + name -> prints the cid
fq-cas object get research.papers.doc1 -o out.pdf  # read by name (--offset/--length too)
fq-cas object ls research.papers                   # list a namespace (prefix; empty = all)
fq-cas object resolve research.papers.doc1         # the cid a name points at
fq-cas object history research.papers.doc1         # version cids, newest first
fq-cas object bind alias <cid>                     # alias a name to an existing cid
fq-cas object rm research.papers.doc1              # remove a name (and its history)
```

Re-`put`ting a name keeps the prior version (history is keep-all); `rm` drops
a name's references, after which its object becomes reclaimable by GC (M1c).
Named operations are local — `--server` addresses the CID-level CAS only.

### Distributed mode

The same store can run as a network service, so a client can talk to a
remote (or, later, production) instance — useful for debugging and backups:

```sh
fq-cas serve --bind 127.0.0.1:9000           # run the server (one terminal)
fq-cas --server 127.0.0.1:9000 put file      # client: every command works remotely
fq-cas --server 127.0.0.1:9000 metrics
```

> This CID-level `serve` is **unauthenticated** — the M2 access-control gate
> sits at the named `Repository` layer, not on this endpoint. Keep it on
> localhost; token-gated remote exposure of the named service is M5's charter.

## Library

`fq-store` is also a library. The core is the `ContentStore` trait
(`put` / `get` / `get_range` / `has` / `size` / `stats`), with a
BLAKE3 + FastCDC filesystem backend (`fs::FilesystemStore`) and a `tarpc`
network client (`service::RemoteStore`, behind the `service` feature).

Every backend is held to one bar: the shared, property-based **conformance
suite**. See
[Implementing a storage backend](../../docs/guide/implementing-a-storage-backend.md).

### Names, versioning & the index (M1b)

Above the content store sits the **name layer**. `NameIndex` (a trait, with
a SQLite reference impl `SqliteNameIndex`) maps hierarchical dotted-path
names (`research.papers.doc1`) to CIDs, keeping version history and
**two-level reference counts** (objects and blocks) maintained
transactionally. `Repository` composes a `ContentStore` with a `NameIndex` into
the user-facing API:

```rust
let repo = Repository::new(
    FilesystemStore::new(cas_dir),
    SqliteNameIndex::open(index_db).await?,
);
let cid = repo.put("research.papers.doc1", bytes).await?; // store + name
let doc = repo.get("research.papers.doc1").await?;        // resolve + read
repo.bind("alias", &cid).await?;                          // many names, one object
repo.delete("research.papers.doc1").await?;               // unname -> GC candidate
```

The refcounts identify what is reclaimable; **GC (M1c)** does the reclaiming.

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
