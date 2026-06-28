# Plan: Storage + vector foundation (Phase 2 pillar #2) — implementation

## Status

Draft for review (2026-06-27). Implements the design in
[ADR-0023](../../adrs/accepted/0023-storage-and-vector-foundation.md) +
[ADR-0024](../../adrs/accepted/0024-separate-databases-storage-foundation.md).
All design forks are closed; this plan sequences the build.

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

**Deliverable:** a working content store — `put`/`get`/range-read/`name`/
`delete` with content dedup and functioning GC.

**Build:**

- The `ContentStore` `tarpc` trait — the contract (names, objects,
  streaming write, range read, delete).
- Filesystem backend: FastCDC chunking → BLAKE3 block hashes → blocks as
  sharded files; objects as a Merkle tree of blocks; object CID = BLAKE3
  root. Streaming write (chunk on the fly), range read (assemble from
  blocks).
- Storage-index DB (SQLite #1): names → CID + version history, object →
  blocks, the bidirectional index, object + block refcounts.
- GC: two-level refcounting (transactional with name updates), a pluggable
  `Collector` trait, and the background reconciliation-audit worker.
- A new workspace crate (placement is an open question, below).

**Depends on:** nothing.

### M2 — Access control (grants + capability tokens)

**Deliverable:** namespace/file grants with delegation, revocation, and
default-deny cross-agent; capability tokens minted and verified.

**Build:**

- Grant events on NATS (`granted` / `revoked` / `delegated`) + their
  schemas; the grant projection DB (SQLite #2), rebuildable from the log.
- Capability token: format, mint (from the projection), verify — the
  uniform mint→verify dataflow used in-process now and distributed later.
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

**Deliverable:** store text → auto-embedded → semantic `search` returns
relevant chunks resolvable to `(name, CID, offset)`, ACL-filtered.

**Build:**

- The embedder plugin protocol (same JSON-RPC/stdio mechanism) + a Rust
  reference embedder (`fastembed`, local model).
- A versioned chunk strategy (reference: fixed-size + overlap or recursive)
  producing chunks as offset ranges into representations.
- Vector-index DB (SQLite #3, sqlite-vec): chunks + vectors keyed by
  embedding space `(modality, model, version, chunk strategy)`; v1
  implements model-version + single-vector.
- The embed-on-store pipeline: NATS event → embed worker → index (async,
  eventually consistent).
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

## Open implementation questions (resolve during the build)

- **Crate/service placement** — a crate in the `fq-runtime` workspace
  (in-process consumption now) vs a new `services/fq-store` from the start.
  Leaning: a crate now with clean module boundaries, extract later (matches
  the ADR's single-binary-extractable stance). *(M1)*
- **Capability token format** — custom signed token vs `biscuit`
  (Datalog-attenuable, Rust) vs macaroons vs a JWT profile. Attenuation +
  delegation favor biscuit; weigh dependency + complexity. *(M2)*
- **Auto-index vs opt-in indexing** — embedding *everything* on store is
  expensive; likely **opt-in per namespace/object** (or per content type)
  so cost is controlled (ties to ADR-0021's budget concerns). *(M4)*
- **Reference embedding model + chunk strategy defaults** — e.g.
  `bge-small`/`MiniLM` via `fastembed`; fixed+overlap vs recursive chunking.
  *(M4)*
- **Plugin vs in-process for the reference extractor/embedder** —
  stdio-plugin from the start (proves the protocol) vs in-process Rust first
  (simpler) with the plugin path added once. Leaning: in-process reference +
  the plugin protocol both in M3/M4 so the seam is exercised. *(M3/M4)*

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
