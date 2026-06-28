# ADR-0023: Storage, extraction, and vector index foundation (Phase 2 pillar #2)

## Status

Draft — in progress (2026-06-27). This ADR records the agreed
architecture for Phase 2's storage + embedding/vector foundation and
enumerates the **open forks** to resolve step by step. Each fork is
closed either by updating this draft or by a supplementary ADR; this ADR
is accepted once the forks are settled.

Foundation for [ADR-0013](../accepted/0013-memory-as-mcp-service.md)
(memory MCP service) and the skill registry
([ADR-0019](../accepted/0019-skill-format.md)). Resolves the storage /
embedding boundary deferred by
[ADR-0021](../accepted/0021-mcp-cost-control-and-memory-boundary.md) §4.

## Context

Phase 2 pillar #2 is the shared semantic-search substrate that both the
memory and skill registry services build on. It must:

- Store **arbitrary files** — text, but also video, audio, PDF, and
  proprietary formats — and retrieve them by name or ID. The substrate is
  "S3, but content-addressed/deduplicated", not a text store.
- Provide **semantic search**, which for non-text means an extraction
  step exists between raw bytes and embeddings.
- Be **interface-first**: a clear internal contract with a filesystem
  reference implementation, so the backend can be swapped later without
  touching consumers.

## Nomenclature

To keep the two kinds of chunking (and the layers) from blurring:

| Term | Meaning |
|---|---|
| **Object** | A complete stored byte-sequence (a "file"), addressed by its content ID. |
| **Content ID (CID)** | The content-address of an object (a hash; see fork F3). |
| **Block** | A **storage-layer** content-defined-chunking (CDC) unit. Objects split into blocks for sub-file dedup; an object's CID is the Merkle root over its blocks. |
| **Name** | A mutable identifier mapping to a CID (like an S3 key or git ref). |
| **Namespace** | An access-control + organizational scope holding names (per-agent by default). **Hierarchical** (dotted, e.g. `research.papers`), so grants can target a subtree. |
| **Principal** | An access-control subject (grantee/grantor). Extensible typed reference; v1 implements `Agent(id)` only. |
| **Representation** | A layer-2 extraction output (extracted text, transcript, …), itself stored as an object, keyed by `(source CID, extractor, extractor version)`. |
| **Chunk** | An **embedding-layer** retrieval unit: an offset range over a representation — not a stored copy. |
| **Embedding space** | The identity of a comparable vector family: `(modality, model, model version, chunk strategy)`. |
| **Vector** | The embedding of a chunk within an embedding space. |

**Block ≠ chunk.** Blocks are a storage dedup concern; chunks are a
retrieval concern. They are never the same unit.

## Decided architecture

### Three layers

1. **Content store** — arbitrary bytes by CID, dedup, names/namespaces,
   ACL, streaming/range.
2. **Representation/extraction** — `object → embeddable form` (PDF→text,
   audio→transcript, image→features). Pluggable, versioned; outputs are
   themselves objects (so extraction is done once and dedup'd). The
   pluggable middle tier is how capabilities are augmented progressively.
3. **Index/embedding** — chunks the representation, embeds, stores and
   searches vectors.

### Layer 1 — content store

- **Availability contract:** a name resolves to its **current** content,
  and that current content stays available. The contract makes **no
  promise about prior versions** — history retention is a GC policy
  (default: keep-all), not a guarantee, so consumers must not assume old
  versions persist.
- Objects are a **Merkle tree of CDC blocks** for sub-file dedup of large
  binaries (a 2 GB video must not be one opaque blob).
- **Streaming + range** reads/writes are first-class (large media; also
  serves chunk range-reads from layer 3).
- **CID = BLAKE3** (proposed — fork F3): cryptographic, tree-mode (verified
  range reads, parallel hashing), already bandwidth-bound on large files.
- **ACL attaches to names/namespaces, not blobs** — dedup means one object
  backs many logical files. Default-deny across agents.
- **Pluggable backend**, filesystem reference implementation.

### Garbage collection

- **Persist-what's-named; reclaim the rest.** GC runs **async and online**
  (the store stays available during GC) over the name index.
- **The current version of every name is a GC root** (the contract);
  historical versions are roots only while a namespace's retention policy
  keeps them (default: keep-all).
- **Default: two-level reference counting** (objects + blocks) over the
  bi-directional index — transactional with name updates where the backend
  supports it, with a periodic reachability audit as the backstop (F2).
  Naive stop-the-world mark-and-sweep is explicitly *not* the mechanism.
- **GC is pluggable** (a `Collector` interface) so alternatives can be
  benchmarked and swapped.
- This same persist-and-reclaim model **generalizes to migration**:
  superseded representations and vectors (old extractor/chunker/model
  versions) become unreferenced and are reclaimed once the new ones are
  active.

### Layer 2 — representation / extraction

- Extractors are **pluggable and versioned**; a representation is keyed by
  `(source CID, extractor, extractor version)` and stored as a CAS object
  (extract once, cache, dedup).
- New modalities/quality = add an extractor; existing objects re-extract
  lazily or via backfill.

### Layer 3 — index / embedding

- A **chunk is an offset range over a representation** (not a copied
  blob); retrieval range-reads the text from the representation object. No
  chunk-text duplication.
- **Chunk boundaries are versioned** (`chunk strategy`) exactly like the
  model — re-chunk implies re-embed.
- **Vectors are keyed by embedding space** `(modality, model, version,
  chunk strategy)`; only same-space vectors are comparable.
- **Progressive re-embedding**: backfill a new embedding space in the
  background, then flip an "active space" pointer (optionally serve both
  during transition). Old spaces become GC-eligible.
- **Retrieval + ranking are a pluggable strategy** — naive first, refined
  later (fork F5).

### Access control

- **Source of truth = event-sourced grant claims, projected to a permission
  state** ([ADR-0011](../accepted/0011-event-bus-and-persistence.md)'s
  pattern): a grant / delegate / revoke is an event; current permissions
  are a projection — giving audit, time-travel, and trivial revocation.
- **Subjects are `Principal`s** — an extensible typed reference. v1
  implements `Agent(id)`; `Group` / `Service` / roles slot in later as a
  non-breaking projection + schema addition (the slot is kept; the feature
  is not built in v1).
- **Resources are names or namespaces**, and a grant may target a
  **namespace prefix** over the hierarchical tree (e.g. `research.*`) —
  covering the skill access-pattern model without group machinery.
- **Verbs:** `read / write / delete / list / grant`. **`grant` is
  delegatable** — a holder may delegate ≤ their own access on a resource;
  the projection enforces attenuation (no escalation). Default-deny.
- **Wire mechanism = scoped, short-TTL capability tokens, built in from the
  start.** A token is minted from the projection and verified at the
  operation boundary; the **same mint→verify dataflow runs in-process and
  distributed** (differing only in transport), so token mechanics and
  process-separation are never conflated during fault-finding.
- The **dedup existence side-channel** (one agent can confirm another holds
  identical bytes) is **accepted** for this use case — noted, not
  mitigated.

### Deployment

- **Service + SDK.** The store/index runs as a standalone, concurrency-safe
  service; consumers (the memory and skill MCP servers) use a client SDK.
  We may ship it inside a single binary at first, but the **module
  boundaries are kept clean so it extracts cleanly into its own service**.
- **Polyglot embedders.** Embedders run as **plugins**, potentially in
  different languages (Python's ecosystem, or Rust `fastembed`/`candle`),
  behind a defined plugin protocol (fork F7). A single-binary all-Rust
  build is the **reference implementation**.

### Design stance

Interface-first. **Bake in the extension points that are expensive to
retrofit** — range/streaming, the embedding-space key, ACL-on-name, and an
async shape that works the same in-process and remote — and **defer the
hard implementations** (multimodal extraction, hybrid rerank, distributed
GC, CDC tuning) behind the interface.

## Open forks

Stakes flag how much discussion each needs. "Low" = a default behind the
interface, easily changed.

### F1 — Name version retention · RESOLVED

**Decision:** versioned names; the **contract guarantees only the current
version**; history is retained by **GC policy**, default **unbounded**.

A name maps to an ordered history of `(CID, timestamp)`; the latest is
current. The availability contract covers the current version only —
history is a default GC policy (keep-all), **not** a guarantee, so
consumers must not depend on old versions being retrievable. Per-namespace
retention policies (`keep all`, `keep last N`, `keep last T`,
`current-only`) are a **future** GC feature; the default stays unbounded
until storage pressure motivates trimming.

### F2 — GC algorithm · RESOLVED

**Decision:** reference counting as the online primary — transactional with
the name-index update where the backend supports it — plus a periodic
reachability **audit** as a universal backstop. Pluggable `Collector`
interface so alternatives (epoch, online mark-sweep, log-structured) can be
benchmarked later.

- **Two-level refcount:** objects (refs from name versions) and blocks
  (refs from objects); an object reaching zero decrements its blocks. Safe
  because `name → object → block` is acyclic — no cycles to confound
  refcounting.
- **Crash safety:** refcount deltas are transactional with name updates
  where the backend allows (filesystem reference impl via atomic rename; a
  future DB backend via real transactions); the audit recomputes truth from
  the indexes and corrects drift, online and off the hot path.

### F3 — Content hash · low

- **(a) BLAKE3** — crypto + tree-mode + bandwidth-bound (recommended).
- **(b) Non-crypto + self-built Merkle** (XXH3/XXH128 per block) — marginal
  speed, no crypto, ACL-collision risk.
- **(c) KangarooTwelve** — Keccak tree hash, crypto, slower than BLAKE3.

Lean: (a). Behind the interface, so swappable on benchmark evidence.

### F4 — Grant / delegation mechanism · RESOLVED

**Decision:** event-sourced grant claims → permission projection as the
source of truth, with scoped short-TTL **capability tokens as the wire
mechanism from the start** (one mint→verify dataflow in-process and
distributed). `grant` is a **delegatable** verb with attenuation enforced
by the projection. Subjects are an **extensible `Principal`** (v1: `Agent`
only). Grants target names or **namespace prefixes** over the hierarchical
namespace tree. **Roles and groups are deferred** — the `Principal`/verb
model keeps the slot; the feature is not built in v1. Full model in the
Access control section above.

### F5 — Retrieval / ranking interface · high

- **(a) Single stage** — vector top-k + metadata filter.
- **(b) Multi-stage pipeline** — retrieve → fuse (dense + sparse/BM25) →
  rerank (cross-encoder).
- **(c) Pluggable strategy DAG.**

Lean: define a **pipeline** interface (so dense-only is the v1
implementation of a retrieve→fuse→rerank shape). Needs an API-design pass.

### F6 — Service transport · high

- **(a) NATS** — consistent with the event bus
  ([ADR-0011](../accepted/0011-event-bus-and-persistence.md)); streaming
  large blobs over it needs care.
- **(b) gRPC** — natural streaming, strong typing.
- **(c) HTTP** — simplest, S3-like.

Lean: undecided; gRPC suits streaming, NATS suits ecosystem fit. Tie to F7.

### F7 — Embedder / extractor plugin protocol · high

- **(a) Subprocess (stdio) protocol.**
- **(b) gRPC plugin.**
- **(c) Reuse the polyglot-tool boundary**
  ([ADR-0015](../accepted/0015-rust-runtime-polyglot-tools.md)).

Lean: a defined plugin contract so Python and Rust embedders both work;
mechanism tied to F6.

### F8 — Storage CDC algorithm · low

- **(a) FastCDC** (fast, good dedup — recommended), **(b) Rabin
  fingerprint**, **(c) Gear/Buzhash**, **(d) fixed-size** (poor dedup).

Lean: (a), behind the store interface.

### F9 — Vector engine (reference + path) · low

- **(a) sqlite-vec** — embedded, simplest reference.
- **(b) LanceDB** — embedded, multimodal + **versioned** (aligns with
  progressive re-embedding).
- **(c) Qdrant** — server, production.

Lean: sqlite-vec or LanceDB for the reference; LanceDB's versioning is
attractive. Behind the index interface.

### F10 — Embedding-space axes for v1 · low

Key vectors by `(modality, model, version, chunk strategy)`. Implement
**model-version + single-vector** first; multimodal and multi-vector
(ColBERT-style) later.

## Consequences and process

- This draft is the **design agenda**: forks are resolved one at a time,
  high-stakes first (F1, F4, F5, F6/F7), folding decisions back here or
  into supplementary ADRs. The ADR is **accepted once the forks close**.
- The v1 traits must already carry the expensive-to-retrofit shape
  (range/streaming, embedding-space key, ACL-on-name, async/remote-ready).
- Once layer 1 + a minimal layer 3 exist, Memory (pillar #3) and Skills
  (pillar #4) can proceed largely in parallel on top.

## References

- Prior art to mine while resolving forks: **Perkeep** (claims/permanodes,
  ACL), **git** (immutable objects + mutable refs + reachability GC),
  **casync/restic** (CDC), **IPFS** (Merkle-DAG), **LlamaIndex** (Node /
  chunk model), **LanceDB** (versioned multimodal vectors), **Unstructured**
  (extraction), **Vespa/ColBERT** (hybrid + multi-vector retrieval).
- [Phase 2 plan](../../plans/active/2026-04-11-phase-2-mcp-and-memory.md).
