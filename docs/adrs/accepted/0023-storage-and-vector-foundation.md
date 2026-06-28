# ADR-0023: Storage, extraction, and vector index foundation (Phase 2 pillar #2)

## Status

Accepted (2026-06-27). Records the architecture for Phase 2's storage +
embedding/vector foundation; all ten design forks (F1–F10) are resolved
below. Further refinements are handled by supplementary ADRs — the F9
DB-separation question is settled by
[ADR-0024](0024-separate-databases-storage-foundation.md).

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
- **Retrieval is a pluggable pipeline behind one `search`** (F5):
  `Retriever(s) → Fuser → Reranker → top-k`. Retrievers emit explicitly
  scored candidates (scores are retriever-local, not comparable across
  retrievers); the Fuser reconciles them into one list (normalize /
  rank-fuse, e.g. RRF); the Reranker re-scores jointly (cross-encoder /
  LLM). The pipeline **over-fetches a candidate budget N ≫ k and truncates
  to the caller's `k` *after* reranking** (N = strategy config, k = query).
  v1 = one dense retriever + passthrough fuser + identity reranker.

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

- **Service over `tarpc`.** The store/index runs as a standalone,
  concurrency-safe service whose **`tarpc` trait *is* the contract** (no
  IDL, no codegen). The *same trait* is a direct in-process call in the
  single-binary build and a `tarpc` RPC once extracted — one contract, one
  auth dataflow, no schema duplication. Consumers (the memory and skill MCP
  servers) are Rust. Blob access is **range-based unary** (S3-like), not RPC
  streaming.
- **NATS event spine.** The service emits events (`object.stored`,
  `granted`, …) to NATS for the grant projection, the embed-on-store
  pipeline, and audit
  ([ADR-0011](../accepted/0011-event-bus-and-persistence.md)); bulk bytes go
  over the service API, not NATS.
- **Polyglot plugins via JSON-RPC/stdio.** Embedders and extractors run as
  **plugins over JSON-RPC on stdio** — no codegen, polyglot (Python, or Rust
  `fastembed`/`candle`), reusing the MCP stdio child-process pattern. A
  single-binary all-Rust build is the reference implementation.

### Design stance

Interface-first. **Bake in the extension points that are expensive to
retrofit** — range/streaming, the embedding-space key, ACL-on-name, and an
async shape that works the same in-process and remote — and **defer the
hard implementations** (multimodal extraction, hybrid rerank, distributed
GC, CDC tuning) behind the interface.

## Design forks (resolved)

Stakes flagged how much discussion each needed. "Low" = a default behind
the interface, easily changed.

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

### F3 — Content hash · RESOLVED

**Decision: BLAKE3** — cryptographic + tree-mode (verified range reads,
parallel hashing) + bandwidth-bound on large files; the multi-agent
dedup+ACL surface makes collision resistance worth keeping. Swappable
behind the CID interface.

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

### F5 — Retrieval / ranking interface · RESOLVED

**Decision:** one `RetrievalStrategy::search(query) -> Vec<Hit>` that
consumers call, implemented by a composable pipeline:

- **`Retriever`** (dense / sparse / …) emits scored `Candidate`s; scores
  are **retriever-local and not comparable across retrievers**.
- **`Fuser`** collapses the set-of-sets into one list, reconciling those
  incomparable scores (normalize / rank-fuse, e.g. RRF) and deduping.
- **`Reranker`** re-scores jointly (cross-encoder / LLM) for precision.
- The pipeline **over-fetches a candidate budget N and truncates to the
  query's `k` *after* reranking** — `N` (over-fetch depth) is strategy
  config, `k` (final count) is the caller. Taking top-k before rerank would
  defeat the reranker.

Strategy is configured per index/namespace with an optional per-call
override. `Hit` carries a `ChunkRef` resolving to `(name, CID, offset
range)`. v1 = dense retriever + passthrough fuser + identity reranker; the
sparse/fusion/cross-encoder seams need no consumer change.

### F6 — Service transport · RESOLVED

**Decision:** **`tarpc`** for the service API — a code-first Rust RPC where
the service *trait is the contract* (no `.proto`, no codegen). The same
trait is a direct call in-process and a `tarpc` RPC when extracted; blob
access is range-based unary (S3-like). **NATS remains the event spine**
(store/grant/delete events; bulk bytes do not cross NATS). gRPC/tonic was
rejected for its build-time codegen and proto-evolution cost — its only
edge, a language-neutral IDL, is unneeded while consumers are Rust. Escape
hatch: a JSON-RPC/HTTP facade if polyglot *consumers* are ever required.

### F7 — Embedder / extractor plugin protocol · RESOLVED

**Decision:** **JSON-RPC over stdio** for embedder and extractor plugins —
no codegen, polyglot (Python or Rust), reusing the MCP stdio child-process
pattern the runtime already runs (minimizing format/standard
proliferation). The service manages plugin process lifecycle.

### F8 — Storage CDC algorithm · RESOLVED

**Decision: FastCDC** — fast, good dedup ratios, the modern default for
content-defined chunking; behind the store interface.

### F9 — Vector engine (reference) · RESOLVED

**Decision:** a clean **index interface** with a **sqlite-vec** reference
implementation — consistent with factor-q's existing SQLite projections
([ADR-0011](../accepted/0011-event-bus-and-persistence.md)), minimizing new
storage tech. LanceDB (versioned, multimodal) and Qdrant (server-grade) are
alternative implementations behind the same interface.

**Open follow-up:** whether to **separate the databases** (vectors vs
projections vs the storage index) even under sqlite-vec — parked for a
supplementary discussion/ADR.

### F10 — Embedding-space axes · RESOLVED

**Decision:** vectors are keyed by **all four axes** `(modality, model,
version, chunk strategy)` from the start, so new implementations never force
a schema change. v1 *implements* `model-version + single-vector`;
multimodal and multi-vector (ColBERT-style) are later additions to the same
key.

## Consequences and process

- All ten forks are resolved (F1–F10 above); further refinements — e.g. the
  F9 DB-separation question — are handled by supplementary ADRs.
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
