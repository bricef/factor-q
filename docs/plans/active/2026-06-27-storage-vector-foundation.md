# Plan: Storage + vector foundation (Phase 2 pillar #2) — implementation

## Status

Active (2026-06-27). Implements the design in
[ADR-0023](../../adrs/accepted/0023-storage-and-vector-foundation.md) +
[ADR-0024](../../adrs/accepted/0024-separate-databases-storage-foundation.md).
Design forks and implementation decisions are resolved.

**Progress:** M1a (the CAS) is implemented and conformance-tested in
`services/fq-store` — the `ContentStore` trait, the BLAKE3/FastCDC
filesystem backend, and the reusable property-based conformance suite (see
`docs/guide/implementing-a-storage-backend.md`), plus the `fq-cas`
standalone CLI. The **tarpc service boundary** (F6) is also proven early on
M1a — `fq-cas serve` / `--server` with a `RemoteStore` client that re-runs
the conformance suite *over the wire*, validating "same contract,
in-process and distributed" ahead of the harder layers. Next: M1b (storage
index DB).

## Goal

A runnable, single-binary **content storage + semantic index service** that
the Memory (#3) and Skill registry (#4) pillars build on. It stores
arbitrary files (content-addressed, deduplicated), extracts embeddable
representations from them, indexes those for semantic search, and gates
everything with namespace/file access control — behind a `tarpc` trait that
is the same contract in-process and when later extracted to its own
process.

## Context — where we are

- The **design is complete**: three layers (content store → extraction →
  index), three separate SQLite DBs, BLAKE3 + FastCDC CAS, two-level
  refcount GC, event-sourced grants + capability tokens, `tarpc` service +
  JSON-RPC/stdio plugins, and the `Retriever → Fuser → Reranker` search
  pipeline. See ADR-0023/0024 for every decision and its rationale.
- **Nothing is built yet** — no storage crate, no embedding/vector deps.
- factor-q is a Rust workspace (`services/fq-runtime/`) on NATS + SQLite
  ([ADR-0011](../../adrs/accepted/0011-event-bus-and-persistence.md)); this
  service reuses both (NATS event spine, SQLite stores).

## Scope

**In:** the content store, access control, a text extraction path, a
local-embedding index, semantic search (dense-only), the `tarpc` service +
Rust SDK, and the plugin protocol — enough that Memory and Skills can
consume it.

**Out (interfaces/seams only, no implementation):** multimodal
extraction/embedding (audio/video/image); hybrid (sparse/BM25) retrieval +
cross-encoder rerank; non-filesystem storage backends (S3, …);
non-sqlite-vec vector engines (Qdrant/LanceDB); distributed deployment
(the `tarpc` seam exists, but v1 runs in-process); groups/roles (the
`Principal` slot exists). The Memory service (#3), Skill registry (#4), and
context-window management (#5) are separate pillars that *consume* this.

## The work (milestones)

Each milestone is independently shippable and testable.

### M1 — Layer-1 content store (CAS) + storage index + GC

Built as three independently testable, separately useful components.

#### M1a — Content-addressed store (CAS)

**Deliverable:** a content-addressed blob store — write bytes → CID, read
by CID (+ range), with content dedup. Useful standalone, and it is the pure
**K/V** access path for files that don't need indexing.

**Build:** the `ContentStore` `tarpc` trait (the contract); filesystem
backend — FastCDC chunking → BLAKE3 block hashes → blocks as sharded files;
objects as a Merkle tree of blocks (object CID = BLAKE3 root); streaming
write (chunk on the fly) and range read (assemble from blocks).

#### M1b — Storage index DB

**Deliverable:** the mutable name layer over M1a — names → CID + version
history, with the bidirectional index and refcounts.

**Build:** the storage-index DB (SQLite #1); name resolution (current +
history); the `name → object → block` and reverse indexes; object + block
refcount maintenance, transactional with name updates.

#### M1c — Garbage collection

**Deliverable:** unreferenced objects/blocks are reclaimed online, with the
store available throughout.

**Build:** two-level reference counting over M1b's indexes; a pluggable
`Collector` trait; the background reconciliation-audit worker.

**M1 depends on:** nothing (M1b builds on M1a; M1c on M1b).

### M2 — Access control (grants + capability tokens)

**Deliverable:** namespace/file grants with delegation, revocation, and
default-deny cross-agent; capability tokens minted and verified.

**Build:**

- Grant events on NATS (`granted` / `revoked` / `delegated`) + their
  schemas; the grant projection DB (SQLite #2), rebuildable from the log.
- Capability tokens (**biscuit**): mint from the projection (private key),
  verify at the op boundary (public key only), with Datalog authorization
  and offline attenuation — the uniform mint→verify dataflow in-process now
  and distributed later.
- `Principal` (extensible; `Agent` only), verbs
  (`read`/`write`/`delete`/`list`/`grant`), prefix grants over hierarchical
  namespaces, attenuation enforcement, default-deny.
- Wire `can(Principal, Op, Resource)` into the `ContentStore` ops.

**Depends on:** M1 (interfaces to gate).

### M3 — Layer-2 extraction

**Deliverable:** a stored file yields a cached, embeddable representation;
the plugin protocol is proven.

**Build:**

- The extractor plugin protocol — JSON-RPC over stdio, with managed plugin
  process lifecycle (reusing the MCP stdio pattern).
- A reference extractor: UTF-8 text passthrough (PDF/others are later
  plugins). Representations are stored as CAS objects keyed by
  `(source CID, extractor, version)`.

**Depends on:** M1.

### M4 — Layer-3 index (embedding + retrieval)

**Deliverable:** store text → (opt-in) embedded → semantic `search` returns
relevant chunks resolvable to `(name, CID, offset)`, ACL-filtered.
Non-indexed files remain available as pure K/V.

**Build:**

- The embedder plugin protocol (same JSON-RPC/stdio mechanism) + a Rust
  reference embedder (`fastembed`, local model).
- A versioned chunk strategy (reference: fixed-size + overlap or recursive)
  producing chunks as offset ranges into representations.
- Vector-index DB (SQLite #3, sqlite-vec): chunks + vectors keyed by
  embedding space `(modality, model, version, chunk strategy)`; v1
  implements model-version + single-vector.
- The embed-on-store pipeline, **opt-in per object/namespace**: a NATS
  event → embed worker → index (async, eventually consistent).
- The `RetrievalStrategy::search` pipeline: `Retriever → Fuser → Reranker`
  with v1 = dense + passthrough + identity, and the over-fetch-N /
  truncate-to-k-after-rerank plumbing.

**Depends on:** M1, M3.

### M5 — Service wiring + SDK

**Deliverable:** a runnable single-binary service + Rust SDK; the
foundation Memory and Skills build on.

**Build:**

- Compose the three layers; open the three DBs; run the GC worker, the
  embed pipeline, and the plugin manager.
- The `tarpc` service endpoint (in-process direct call now, RPC-ready).
- NATS event emission (`object.stored`, `granted`, …) for audit, the
  projections, and the embed pipeline.
- The Rust SDK (client library) for consumers.
- Integration tests: store → search → authorize; the `tarpc` path; GC;
  re-embedding/backfill.

**Depends on:** M1–M4.

## Sequencing

M1 is the base. After it, **M2 and M3 can proceed in parallel**; M4 needs
M3; M5 integrates all. By the end of M4 the service is functionally
complete and Memory (#3) / Skills (#4) can start consuming it; M5 hardens
and packages it.

## Implementation decisions

- **Service placement: a new `services/fq-store`** — factor-q will have
  multiple services, so the storage service is its own service from the
  start, not folded into `fq-runtime`.
- **Storage index (M1b): implicit dotted-path namespaces.** The index keys
  on a single hierarchical name string (`research.papers.doc1`); namespaces
  are *any prefix*, not first-class objects. Access control (M2) matches
  these strings via prefix and glob grants (`research.papers.*`, and
  `system.agents.<id>.files.*` for harness-only scopes). The `object→block`
  edges live **in the index DB** (denormalized from the CAS manifest) so the
  two-level refcounts stay transactional; `NameStore` is a **trait + SQLite
  reference impl**, reusing the M1a conformance pattern. To source those
  edges, the `ContentStore` trait gains `blocks(cid)` — an object's dedup
  units (`[cid]` for non-chunking backends). *(M1b)*
- **Capability tokens: biscuit** (`biscuit-auth`) — offline attenuation +
  public-key verification + Datalog authorization; matches the
  build-for-distribution stance and accommodates the deferred groups/roles
  as Datalog rules. *(M2)*
- **Indexing is opt-in** per object/namespace (or content type) — embedding
  everything on store is expensive; non-indexed files are still stored and
  served as pure **K/V**, and indexing is requested explicitly (ties to
  ADR-0021 budget). *(M4)*
- **The embedder is an interface/plugin seam** — a local reference model
  (`fastembed`, `bge-small`/`MiniLM` class) behind the embedder interface so
  it can be reviewed/augmented/swapped later. *(M4)*
- **Both the in-process and stdio-plugin paths are exercised** — an
  in-process Rust reference *and* the JSON-RPC/stdio plugin protocol, so the
  plugin seam is proven, not just the in-process path. *(M3/M4)*
- **Chunk strategy** stays a versioned, swappable component (ADR-0023 layer
  3); the reference default (fixed+overlap or recursive) is settled in M4.

## Deferred — captured for the future

Multimodal extraction/embedding; hybrid + cross-encoder retrieval;
alternative storage backends (S3) and vector engines (Qdrant/LanceDB);
distributed deployment; groups/roles; per-namespace GC retention policies
(ADR-0023 F1); convergent encryption / encryption-at-rest.

## Risks and what we'll learn

- **CAS + GC correctness** is the trickiest part (refcount/crash
  consistency); the audit backstop is the safety net, but M1 needs careful
  property tests (store-twice-dedups, delete-then-GC-reclaims,
  crash-mid-write).
- **`tarpc` ergonomics** at the trait boundary — confirm the
  same-trait-in-process-and-remote story holds in practice.
- **Embedding cost + latency** on the store path — validates the
  opt-in-indexing instinct.
- **sqlite-vec** maturity/perf at the scale we need — informs when the
  alternative-engine seam gets exercised.

## Closing condition

A single-binary storage+index service with the three separate DBs;
store/get/range/name/delete with dedup + working GC; grants/delegation/
revocation + capability tokens + default-deny + prefix grants; a text
document stored → auto-extracted → embedded → semantic search returning
ACL-filtered, source-resolvable chunks; the `tarpc` trait + Rust SDK; all
tests green through the CI gate.

## References

- [ADR-0023](../../adrs/accepted/0023-storage-and-vector-foundation.md),
  [ADR-0024](../../adrs/accepted/0024-separate-databases-storage-foundation.md)
  — the design.
- [ADR-0013](../../adrs/accepted/0013-memory-as-mcp-service.md) (memory),
  [ADR-0019](../../adrs/accepted/0019-skill-format.md) (skills),
  [ADR-0021](../../adrs/accepted/0021-mcp-cost-control-and-memory-boundary.md)
  (cost), [ADR-0011](../../adrs/accepted/0011-event-bus-and-persistence.md)
  (event bus) — consumers and context.
- [Phase 2 plan](2026-04-11-phase-2-mcp-and-memory.md) — the parent.
