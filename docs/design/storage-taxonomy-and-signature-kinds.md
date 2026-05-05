# Storage Taxonomy and Signature Kinds

## Context

This document captures decisions about factor-q's storage architecture and the signature taxonomy that drives it. It builds on the inter-node contracts and signature-as-primitive design documents.

Two related concerns are addressed here. First, factor-q has multiple distinct needs for persisted state — not all of which are well-served by treating "storage" as a single concern. Second, the strict typed-function model of signatures applies cleanly to most work but is genuinely restrictive for open-ended exploratory work; a single discipline cannot cover both well.

The decisions here should land alongside or shortly after the envelope/payload/annotations refactor. They affect the same code paths and the storage repositories the refactor introduces.

## Decisions

### 1. Five storage mechanisms with distinct semantic domains

Factor-q has five storage mechanisms, each with distinct semantics, lifetimes, and access patterns. Conflating them produces real bugs and undermines the typed-contracts discipline established elsewhere.

| Mechanism | Carries | Lifetime | Scope |
|---|---|---|---|
| **Payload** | Typed data driving graph behaviour | Single event | Producer → consumer |
| **Annotations** | Advisory event metadata | Single event | Humans, meta-agents, learning loop |
| **Memory** | Agent's persistent knowledge | Indefinite | Single agent |
| **Workflow context** | Chain-scoped accumulating context | Chain duration | All invocations in a chain |
| **Artifact store** | Large or referenced outputs | Beyond chain, ref-counted | Cross-chain, cross-agent |

Each mechanism has its own repository abstraction in the runtime (e.g., `EventLog`, `MemoryStore`, `ArtifactStore`, `WorkflowContextStore`), with typed operations specific to its semantics. Consumers interact with these typed repositories, never directly with the underlying storage.

The semantic distinctions are non-negotiable:

- Memory is *vertical in time, agent-scoped*. It's where an agent's knowledge accumulates across re-invocations on different tasks.
- Workflow context is *horizontal across a workflow, chain-scoped*. It's where context accumulates across invocations in the same task.
- Annotations are *event-attached, advisory*. They never drive consumer behaviour.
- Payload is *contractual*. It is the only thing that drives graph behaviour.
- Artifacts are *durable, content-addressed*. They live independent of any specific chain.

Putting workflow context in memory contaminates agent knowledge with task-specific noise. Putting agent knowledge in workflow context loses persistence. Putting large outputs in payloads bloats events. Putting decision-driving information in annotations breaks the typed-contracts discipline. The distinctions matter.

### 2. Workflow context is implemented as an event projection

Workflow context is not a separate storage system. It is a projection over chain-scoped events in the existing event log, filtered to a particular subset of context-bearing event types.

Concretely: producers emit context events tagged with a chain's trace ID. The runtime maintains a projection that, given a trace ID, materialises the current workflow context with appropriate filtering (by entry type, recency, or relevance). Consuming invocations receive a projection-snapshot in their input context, not raw events.

This composes with the event-sourced spine, requires no separate storage system, has the right lifetime semantics (workflow context lives with the chain's events), and supports replay naturally.

Workflow context entries are typed, not free-form. The initial entry-type taxonomy:

- `Assumption` — something an upstream invocation took as given
- `Decision` — a choice an upstream invocation made
- `Observation` — something an upstream invocation noticed
- `Constraint` — a constraint that should propagate
- `ArtifactReference` — a pointer to an artifact relevant to the chain

The taxonomy can grow as patterns emerge, but new types require runtime registration; ad-hoc string-typed entries are not permitted. This is the same discipline applied to annotations and is for the same reason: prevent the mechanism from becoming a back door for prose coupling.

### 3. Shared SQLite engine, distinct repositories

The five repository abstractions share a single SQLite database file as their underlying engine. They do not share a unified abstraction layer.

Shared at the engine level: SQLite itself, the connection pool, schema-migration tooling, backup machinery, query logging, serialization conventions (JSON for structured content, JSON Schema for validation, content-addressing for binary blobs).

Not shared: the access patterns, the indexes, the typed operations, the schema definitions of what each mechanism stores. Each repository exposes its own typed interface; consumers see typed operations on their semantic domain, never raw SQLite.

The discipline: cross-mechanism queries go through *application code*, not SQL joins across tables belonging to different mechanisms. SQL-level coupling defeats the optionality the repository pattern is designed to preserve.

This composition gives operational simplicity (one database, one backup story, one migration framework) without sacrificing the semantic distinctions that make the five mechanisms useful as separate concepts.

### 4. Artifacts use SQLite for metadata, filesystem for content

Artifacts deserve a hybrid storage model. The artifact catalog (hash, size, mime type, references, metadata) lives in SQLite as a typed table queryable like any other repository's data. The actual binary content lives on the filesystem at hash-derived paths (e.g., `/var/lib/factor-q/artifacts/sha256/ab/cd/abcdef...`).

This is the standard architecture for content-addressable stores (Git, IPFS, Docker layers all use variants of this pattern). It preserves SQLite's transactional consistency for metadata while letting the filesystem handle what it's good at: streaming arbitrarily-sized bytes.

The artifact store's repository interface presents content-addressed operations (`put(bytes) -> Hash`, `get(Hash) -> Bytes`, `metadata(Hash) -> ArtifactMetadata`) without exposing the storage layout. Consumers see `ArtifactRef { id }` in payloads; the runtime resolves these through the repository.

### 5. Engine swaps preserved through repository abstraction

The repository pattern preserves optionality to swap storage engines per-mechanism as workloads outgrow SQLite:

- Memory at high read volume could move to Redis behind the same `MemoryStore` interface
- Artifact bytes at large volumes could move to S3/MinIO behind the same `ArtifactStore` interface
- The event log already lives in JetStream; SQLite is a projection
- Workflow context could move to a dedicated time-series store if its volume warrants

The architectural commitment is that consumers depend on repository semantics, not on SQLite specifics. Direct SQL access from consumer code is forbidden for exactly this reason.

### 6. Bounded structured data goes in SQLite columns; unbounded content goes through artifacts

Most of factor-q's persisted data is bounded structured JSON: memory entries, annotations, workflow-context entries, payload fields. These are kilobytes, occasionally tens of kilobytes. SQLite handles them well as JSON columns with appropriate indexes.

Genuinely variable-sized content — research reports, generated datasets, conversation transcripts — goes through the artifact store with content-addressing, regardless of nominal size. The `ArtifactRef` in a payload points to where the content actually lives.

The signature designer makes this choice at schema definition time, not the agent at runtime (see §8 below).

### 7. Storage placement is determined by schemas, not by agent judgement

The agent's responsibility is to produce output conforming to its signature. The signature determines storage placement. The agent does not choose where things go.

This is enforced through three patterns in the schema design:

**Always inline.** Output type is the structured type itself (e.g., `Report`). Schema enforces a maximum size at the field level. Used for bounded content. Producers exceeding the size fail validation, which surfaces as a redesign of the signature, not a runtime workaround.

**Always referenced.** Output type is `ArtifactRef<T>`. Used for content that should always be content-addressed (large, deduplicable, or referenced from multiple places). The agent must upload to the artifact store before populating the field.

**Promoted on size.** Output type is `Promoted<T>`, a wrapper that the runtime auto-routes. The agent always produces `T`-shaped data. If under threshold (e.g., 16KB), kept inline in the payload. If over threshold, the runtime automatically uploads to the artifact store and substitutes a reference. Consumers see a uniform type and learn from envelope metadata whether they're reading inline content or a reference.

The third pattern is the default for any signature where size is genuinely unpredictable. It centralises placement in the runtime, removes agent discretion, and gives consumers a uniform interface. Threshold tuning happens without agent code changes.

### 8. Side-effecting writes use typed operations, not free-form APIs

Writes to memory, workflow context, and annotations are exposed to agents as typed operations, not free-form `write(key, anything)` APIs.

Examples: `memory.record_observation(text: string, tags: Tag[])`, `workflow_context.add_assumption(assumption: Assumption)`, `annotations.add_note(text: string)` (with size constraints baked into each schema).

Each typed operation has a schema validated at the MCP boundary. An agent cannot write a 50KB blob to memory because no operation accepts a 50KB blob — the typed operations have constraints. Misuse is not expressible.

The annotations layer specifically: the *content* can be free-form (the agent writes whatever notes are useful within size bounds), but the *operations* for writing are typed and constrained. This matches the no-untyped-escape-hatches discipline.

### 9. Validation feedback handles residual misplacement

When an agent produces output that fails placement constraints — exceeds an inline size limit, references a nonexistent artifact, attempts an unsupported operation — the runtime returns a validation error in the same shape as schema-violation feedback (per the inter-node contracts decision §3).

The agent receives a clear error and retries. Frontier models self-correct on these almost reliably given good error messages. "Field `report` exceeded the 16KB inline limit. Use `report_artifact` (ArtifactRef) for content of this size" is sufficient context for one-iteration recovery.

The discipline: the runtime is the source of truth on placement. The agent is best-effort. Neither is solely responsible.

### 10. Two kinds of signature: constrained and exploratory

Not all signatures want the same discipline. Constraints that are correct for deterministic operations are wrong for genuinely open-ended work, and a single discipline produces either over-constraint or under-constraint depending on the workload.

Factor-q distinguishes two signature kinds, each with explicit treatment in the runtime.

**Constrained signatures** are the architectural default. Typed input, typed output, runtime-managed placement, no agent discretion over storage or sub-agent spawning. The agent is a function: input goes in, output comes out, side effects are bounded by typed operations on memory/workflow-context/annotations. Most signatures are this kind: classification, extraction, summarization, transformation, verification, decision-making.

**Exploratory signatures** have typed input and a typed deliverable, but the agent has explicit latitude inside its execution. It may spawn sub-agents from a declared permitted set. It may write to a per-invocation scratchpad and read from it during execution. It may decide its own decomposition and tool-use ordering. The signature still has a contract — input shape, deliverable shape, cost budget, capability set — but the *how* of producing the deliverable is left to the agent.

A graph composes both kinds. The signature kind is part of the signature declaration: `kind: constrained | exploratory`. Most nodes are constrained; specific exploratory nodes are explicitly marked and get explicit additional capabilities.

### 11. Exploratory signatures preserve key disciplines

Even exploratory signatures retain core architectural properties. What relaxes is mid-execution decision-making; what stays disciplined is the boundary.

Retained for both kinds:

- A signature contract (input schema, output schema, intent statement)
- Event-sourced trace of all activity (sub-agent spawns, scratchpad writes, tool calls)
- Cost budget enforcement (sub-agent spawning consumes from the parent's budget)
- Sandbox constraints (declared capability sets, runtime enforcement)
- Typed deliverables (output validated against schema before emission)
- Verifiability (a verifier can evaluate the deliverable's quality)

Relaxed for exploratory:

- Mid-execution decisions about tool ordering, decomposition, and intermediate artifacts
- Authoring control over scratchpad content
- Discretion over which permitted sub-agents to invoke and when

Exploratory does not mean unbounded. Sub-agent spawning is from a declared set, not arbitrary. The scratchpad has size and structural constraints. The deliverable's schema may be looser than a constrained signature's but is still typed. The freedom is *within* the contract, not over it.

### 12. The scratchpad is a sixth storage mechanism

Adding to the storage taxonomy from §1:

| Mechanism | Carries | Lifetime | Scope |
|---|---|---|---|
| **Scratchpad** | Per-invocation working state for exploratory agents | Single invocation | Single agent invocation |

The scratchpad is implemented as another projection over events, scoped to a single invocation rather than to a chain. Entries are typed, the agent has authoring discretion within the type set, and the scratchpad is archived with the chain's events when the invocation completes.

The scratchpad is not memory (per-agent across invocations) and not workflow context (chain-scoped across invocations). It is invocation-scoped working state, available only to exploratory signatures that have declared it.

### 13. Optimization treatment differs by signature kind

The two-level optimization hierarchy from the signatures-and-optimization-hierarchy design document operates cleanly on constrained signatures because input → output mapping is well-defined and verifier-`Verdict` cleanly attributes credit. Exploratory signatures are murkier: the verifier evaluates the deliverable, but credit assignment for *why* the deliverable was good is harder.

Implications:

- Node-level optimization for constrained signatures uses DSPy-style algorithms (MIPROv2, GEPA) with full effectiveness
- Node-level optimization for exploratory signatures is slower, noisier, and produces less generalisable artifacts; multi-step agent program optimizers (e.g., GEPA's full-program adapter) are appropriate
- Replay-with-modification works cleanly for constrained signatures; for exploratory signatures, modification requires re-execution because the agent's intermediate trajectory cannot be replayed deterministically
- Cost predictability is high for constrained signatures, bounded but variable for exploratory signatures

This is a real tradeoff, not a free lunch. Exploratory signatures are reserved for cases where the variability is genuinely warranted by the work — research, planning, complex synthesis — not used as a default. A graph dominated by exploratory signatures loses most of factor-q's optimization story.

## Implementation notes

### SQLite configuration

Standard configuration for factor-q's SQLite usage:

- WAL mode enabled (concurrent reads alongside writes)
- Foreign keys enabled
- `busy_timeout` configured for the multi-writer case
- JSON columns using `jsonb` representation where SQLite version permits, plain TEXT otherwise
- Indexes on JSON expressions where queries warrant them
- Single database file for the typical deployment; separate files only if a specific repository's IO patterns demand isolation (none currently identified)

### Artifact store path layout

Hash-derived paths with two-level directory split to avoid any single directory holding millions of entries:

```
/var/lib/factor-q/artifacts/sha256/ab/cd/abcdef...
```

Hash function is SHA-256 by default; the metadata table records hash algorithm to permit future migration. Reference counting is via a separate `artifact_references` table keyed by (artifact_hash, referencing_event_id).

### Garbage collection

- Workflow context: archived with chain events when the chain completes; not actively garbage-collected
- Scratchpad: archived with invocation events when the invocation completes
- Memory: indefinite retention; agent-controlled deletion via typed operations
- Artifacts: reference-counted; periodic mark-sweep for orphans (zero references)
- Annotations: retained for the lifetime of their event

### Schema definition for size-aware types

Standard library of size-aware wrapper types:

- `Bounded<T, MaxSize>` — must fit inline; schema enforces max size
- `Promoted<T, Threshold>` — auto-routes inline or to artifact based on threshold
- `Referenced<T>` — always an `ArtifactRef<T>`; agent must upload first
- `Streaming<T>` — for outputs produced incrementally; routes through a separate streaming mechanism (deferred to later design)

Signature designers compose these wrappers based on expected content distribution. The wrapper carries the placement policy.

### Exploratory signature declaration

An exploratory signature explicitly declares its additional capabilities:

```yaml
kind: exploratory
permitted_sub_agents: [research_agent, summarisation_agent]
scratchpad: enabled
cost_budget_usd: 5.00
deliverable_schema: ResearchReport
```

The runtime enforces what is declared. An exploratory agent attempting to spawn a sub-agent not in `permitted_sub_agents` receives a sandbox-violation error.

### Constraints on exploratory usage

The architectural temptation will be to make signatures exploratory because flexibility feels powerful. The discipline:

- Constrained is the default; exploratory requires explicit justification at signature definition time
- A graph review should flag any exploratory signature and ask whether the work genuinely warrants the relaxed discipline
- The proportion of exploratory signatures in a graph is a metric worth tracking; high proportions indicate the architecture is being misused

## Open questions

- **Cross-mechanism transactions.** SQLite's transactional model permits atomic writes across repositories, and this is occasionally desirable (e.g., recording a memory write and an event in the same transaction). The repository abstraction discourages exposing this directly. Whether to provide a typed cross-repository transaction primitive, and how to scope it, is unresolved.
- **Scratchpad sharing.** When an exploratory agent spawns a sub-agent, should the sub-agent see (some projection of) the parent's scratchpad? Inheritance would help context flow; isolation would preserve sub-agent independence. No firm answer yet.
- **Verifier access to scratchpads.** A verifier evaluating an exploratory agent's deliverable could in principle read its scratchpad to assess the reasoning quality. This may be valuable but also breaks the fresh-context discipline that gives verifiers their independent perspective. Likely requires verifier kinds: deliverable-only verifiers (default) and trace-aware verifiers (specifically marked).
- **Streaming outputs.** Some agent outputs are produced incrementally (long-running research, conversational responses). The current architecture is request-response; a streaming mechanism would need its own design, particularly for the consumer side and for replay semantics.
- **Migration to non-SQLite stores.** When and how to swap individual repositories' backing stores is unresolved beyond the principle that the repository pattern preserves the option. Operational triggers (volume, latency, deployment requirements) need to be enumerated.

## Why this matters

The decisions here close several loops left open by the prior design documents:

- Storage is no longer a single concern that all five (now six) mechanisms collapse into; the distinctions are explicit and the engine sharing is at the right layer
- Placement decisions are no longer ambiguous between agent and runtime; the schema decides, the runtime routes, the agent fills in
- The tension between strict typing and exploratory work is acknowledged and addressed structurally, not papered over
- The scratchpad mechanism gives exploratory agents the freedom they genuinely need without giving up the architectural properties that make factor-q tractable

These compose with the inter-node contracts and signatures-as-primitive decisions to give factor-q a coherent storage and signature architecture. The remaining open questions are real but bounded; none are blocking.

The framing to internalise: factor-q has *six* typed semantic domains (payload, annotations, memory, workflow context, scratchpad, artifacts), *one* shared SQLite engine, *two* signature kinds, and *zero* free-form storage APIs exposed to agents. The discipline at every layer is that semantics drive interfaces, interfaces drive operations, operations drive runtime behaviour — and engines are an implementation detail underneath.
