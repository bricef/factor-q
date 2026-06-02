# Type and Signature Discovery

## Context

factor-q's bedrock primitive is the **signature** — a typed
input schema, a typed output schema, and an intent statement —
with **bindings** (implementations) as a separate, tunable layer
(see [`signatures-and-optimization-hierarchy.md`](./signatures-and-optimization-hierarchy.md)).
[ADR-0015](../adrs/accepted/0015-rust-runtime-polyglot-tools.md)
already commits the Rust runtime to owning a **schema registry
and validation**. This document proposes giving that registry a
*discovery surface*, and sketches where that leads.

The motivation comes from two directions:

- **Off-the-shelf types.** Once agents spawn typed sub-tasks
  (`spawn(definition, seed, [OutputType])`, see
  [`backlog.md`](../plans/backlog.md) → Agent concurrency
  primitives), they need to *name* the types they pass and
  return. Reconstructing `ReportSummary` ad-hoc at every call
  site is wasteful and breaks composability — two agents'
  independently-built `ReportSummary` schemas may not match.
  Agents (and their authors) should grab a shared, named type
  off the shelf.
- **Type-directed discovery.** If signatures are typed and
  registered, the registry can be *queried by type*: "signatures
  returning `ReportSummary`", "signatures taking
  `List(DataSource)` and returning `Graph`". This is Hoogle for
  signatures.

This is the third discovery modality. The codebase already has
two: **fragments** discover by name/tag/version with a reverse
index built from `FragmentRef` edges
([`agent-orchestration-tools.md`](./agent-orchestration-tools.md)),
and **skills** discover by embedding-based *semantic* search
([ADR-0019](../adrs/accepted/0019-skill-format.md)). Type
discovery is the *structural* sibling of skill discovery.

**Out of scope:** this is **not** part of the MCP-client
full-spec plan
([`2026-05-28-mcp-client-full-spec.md`](../plans/active/2026-05-28-mcp-client-full-spec.md)).
It depends on the signature registry existing and on a real
population of signatures to search — neither of which is true
today (single-agent runtime; sub-agents unbuilt). It is captured
here as direction, with a clear near-term/north-star split.

## Two complementary discovery axes

| Axis | Key | Finds | Status |
|---|---|---|---|
| **Semantic** (embeddings) | "*about* X" | skills, memory | planned (phase-2; ADR-0019) |
| **Structural** (type index) | "*shaped like* X → Y" | signatures, bindings | this document |

Both are *runtime* capabilities, not prompt content — ADR-0019's
scaling argument (you cannot list hundreds–thousands of entries
in a prompt) applies identically to signatures. Both fit the same
MCP split worked out for memory: **read one by URI → resource**;
**search → tool**. They are two indexes over registries; nothing
about one precludes the other, and a query may eventually fuse
them ("a signature *about* graph-building that returns `Graph`").

## Near-term shape (cheap, buildable when the registry lands)

1. **Named, versioned types as off-the-shelf artifacts.** Types
   live in the schema registry, addressable and versioned
   (`Name@v`, mirroring the fragment registry's `name@version`).
   Agent/signature definitions `$ref` them instead of inlining
   schemas — DRY, and guaranteed compatibility between a producer
   and consumer that reference the same type.
2. **Nominal type index.** Index signatures by their declared
   input and output *named* types. "Signatures returning
   `ReportSummary`" is a reverse-index lookup — the same shape as
   the fragment reverse index, keyed by type rather than ref. A
   result is a signature **plus its bindings** (which
   implementations exist, if any).
3. **Agent-facing MCP surface.** Read a type or signature by URI
   → **resource** (`type://ns/Name@v`, `signature://ns/name@v`) —
   the off-the-shelf grab, host-injectable. Search-by-type →
   **tool** (model-controlled, structured filters over input
   type, output type, tags). Structural where skill-search is
   semantic.
4. **Access-filtered.** Discovery respects the same per-agent
   access model as skills (ADR-0019's namespace filters): a
   search returns only signatures the caller is permitted to
   use/spawn. An agent cannot discover what it cannot invoke.

This slice serves typed spawning directly: grab `OutputType` off
the shelf, and find *which* signature to spawn for a desired
output without knowing its name.

## Operator surface

The same index is searchable by humans through `fq`, consistent
with the `fq <noun> <verb>` convention
([operator-cli plan](../plans/closed/2026-05-22-operator-cli.md):
`fq invocation list`, `fq workers show`):

- `fq signature search --in 'List(DataSource)' --out Graph`
- `fq signature show <name@v>` — its schemas, intent, and bindings
  (with the verifier history that attests each binding's quality,
  per the optimization doc §7).
- `fq type list [--namespace ...]`

Operators use it to avoid authoring duplicate signatures, to see
which signatures are **unbound** (synthesis candidates, below),
and to inspect a signature's competing bindings. The agent MCP
surface and the operator CLI are two front-ends over one index.

## North star: a typed data-definition language and a type algebra

Today the structural component of a signature is **JSON Schema**
(per the inter-node contracts decision and
[`storage-taxonomy-and-signature-kinds.md`](./storage-taxonomy-and-signature-kinds.md)).
JSON Schema has no principled unification — `anyOf`/`allOf`/
`oneOf`, optional fields, and open-vs-closed objects make
"subtype" ill-defined in general. So *nominal* matching (by type
name) is exact and cheap, but *structural* matching ("anything
shaped like `List(DataSource) → Graph`", modulo names, with
sub/supertype compatibility and parametric types) is approximate
at best.

The Hoogle experience depends on a real type system. The
north-star direction is therefore a **typed data-definition
language** with a principled type algebra — parametric types
(`List(T)`, `Map(K, V)`), structural subtyping, and unification —
so type-directed search becomes principled rather than heuristic.
Whether to **roll our own** IDL or **adopt an external** one
(candidates worth surveying: CUE, Dhall, an ML-style algebra,
protobuf-with-extensions) is an open decision. This is clearly a
way off — it touches the signature-versioning-and-evolution open
question already flagged in the optimization doc — and is
recorded as direction, not commitment. The near-term nominal
index does not block on it.

## Discovery triggers evolution

The most interesting consequence. A type-directed query that
finds **no implementation** — either no matching signature, or a
matching signature with **no binding** — is a natural synthesis
trigger:

> "No implementation found for `List(DataSource) → Graph`.
> Evolve a dedicated implementation for this signature?"

This wires the type DB directly into the self-improvement loop.
An unbound signature is *exactly* what **node-level
optimization** compiles: DSPy-style optimizers (MIPROv2, GEPA)
run **offline** against the signature contract and a verifier to
produce a binding
([`signatures-and-optimization-hierarchy.md`](./signatures-and-optimization-hierarchy.md)
§4, §6, "DSPy as offline optimizer"), and the **shadow-mode
self-improvement loop** evaluates and promotes the candidate
([`shadow-mode-and-self-improvement.md`](./shadow-mode-and-self-improvement.md)).
So the registry is not passive storage — it is the front door to
self-improvement: **a missed query is a demand signal that pulls
a compiled binding into existence.**

Two initiation modes:

- **Agent-initiated** — an agent that needs a `X → Y` it can't
  find requests evolution autonomously. This is itself a budgeted
  capability under the attenuation rules (spawning an evolution
  spends from the agent's budget and must be granted), and it
  honours the no-human invariant: the decision is the agent's,
  the authorization is declarative.
- **Operator-initiated** — a human spots an unbound signature via
  `fq` and requests a compilation.

**Important caveat:** synthesis is expensive and offline (the
compilation pipeline is Phase-3 infrastructure per the
optimization doc). "Evolve on miss" is therefore **asynchronous**
— the missed query records a demand signal; compilation happens
out-of-band; the new binding appears in the registry later. The
discovery surface must not promise a synchronous synthesize-and-
return.

## Relationship to existing decisions

- **Signatures-as-primitive** — this is the discovery surface for
  that primitive. "Agents that return X" is precisely "signatures
  with output X and their bindings."
- **ADR-0015 schema registry** — the substrate this indexes.
- **ADR-0019 skill discovery** — the semantic sibling; reuse its
  access-filtering and tool-not-prompt scaling discipline.
- **Fragment registry** — the name/tag sibling and the
  reverse-index precedent.
- **Typed spawn** (`backlog.md`) — the primary consumer:
  off-the-shelf `OutputType`, and discover-which-to-spawn.
- **ADR-0016 (typed operations, no free-form APIs)** — why
  discovery returns typed signatures, never prose matches.
- **Planned vector substrate** (phase-2 #2) — the semantic axis;
  this is the structural axis over (potentially) the same
  registry.

## Open questions

- **Type representation** — JSON Schema vs. a typed DDL vs. a full
  algebra. The fork that gates structural/Hoogle search.
- **Structural-match semantics** — over whatever representation,
  is a match exact, subtype-compatible, or full unification? What
  does `List(DataSource) → Graph` mean when inputs are partially
  optional?
- **Demand-signal handling** — how missed queries are recorded,
  aggregated, and prioritized to drive synthesis (and deduped so
  one popular gap isn't compiled N times).
- **Structural identity / dedup** — two structurally-equal,
  differently-named types: one canonical type, or two? Affects
  both the registry and match results.
- **Versioning interaction** — bindings are tied to signature
  versions (optimization doc open question); discovery results
  must be version-aware.

## Why this matters

- It closes the discovery loop typed spawning leaves open: a
  generic parent can *find* the signature it needs by type.
- It gives the signature-as-primitive reframing a navigable
  surface — signatures become a searchable library, not a flat
  namespace.
- It turns the registry from passive storage into the **front
  door of self-improvement**: demand (a queried-but-missing
  signature) pulls supply (a compiled binding) into existence.
- It completes the discovery story — the *structural* complement
  to the planned *semantic* search — over one registry, with two
  front-ends (agents via MCP, operators via `fq`).
