# Agent Orchestration Tools — Design Wishlist

A specification for the primitives I'd like available when orchestrating subagents from within a Claude Code–style harness. Grouped by how much each one would change the shape of what's achievable.

## Design philosophy

The guiding principle behind this wishlist is that **common orchestration patterns should be expressible inside the subagent system, not re-implemented by hand in the orchestrator's conversation every time.** The current model assumes the top-level agent is the orchestrator and subagents are one-shot workers; this forces every fan-out, join, retry, and review loop to consume the orchestrator's context and attention turn-by-turn. The tools below push those patterns down into the harness.

Three cross-cutting points worth calling out before the tool list:

1. **The graph is the primitive; sugar tools are fragments.** The underlying model the runtime executes is a directed graph of agent nodes connected by typed edges. Cycles are allowed — back-edges with guards express loops and convergence conditions. Every other tool in this spec (`AgentSpawn`, `AgentMap`, `AgentLoop`) is a canonical graph shape shipped as a fragment in the standard library namespace; the runtime compiles each call down to a graph instance and executes it with the same engine. New canonical patterns are added by publishing fragments, not by modifying the runtime. See the [Foundation](#foundation) section for `AgentGraph` and the [fragment library](#fragment-library).
2. **Handles over blocking.** Spawning anything — a single agent, a map, a full graph — returns a handle. Blocking is an explicit separate operation (`AgentWait`). Handles are durable and independent of the calling session (see [Durable handles](#durable-handles)).
3. **Subagent recursion should be the default, not the exception.** Currently only the `general-purpose` agent type has the `Agent` tool in its allowlist, so every tree-shaped workload has to thread through it at every branch point even when a more specialised agent would fit the leaves. Any agent type whose job benefits from delegation should be able to spawn further agents — or compose full graphs — optionally with a depth limit.
4. **No confabulation where data exists.** `AgentList`, `AgentPeek`, and the introspection shape of handles exist in service of this principle — any state the runtime tracks that a caller might reasonably ask about is exposed, not left to inference. Canonical statement in [`design-principles.md`](design-principles.md#2-no-confabulation-where-data-exists).

---

## Foundation

The runtime executes one kind of thing: a graph of agent nodes. `AgentGraph` is the substrate; `AgentSpawn`, `AgentMap`, and `AgentLoop` are sugar that compiles to graph instances. Two other concerns live at this layer because they're orthogonal to which sugar you reached for: durable handles (how running work is identified and persisted) and result sinks (how completed work surfaces its output to the outside world).

### `AgentGraph`

A directed graph of agent nodes with typed edges. Not a DAG — cycles are first-class, guarded by edge predicates and bounded by iteration caps.

```
AgentGraph({
  nodes: [
    {
      id: string,                  // unique within this graph
      kind: "agent" | "map" | "aggregate" | "branch" | "sink" | "subgraph",
      config: NodeConfig,          // depends on kind; see below
    }
  ],
  edges: [
    {
      from: string,                // node id, optionally "node.port" for multi-output nodes
      to: string,
      guard?: string,              // optional predicate over the edge payload; edge traverses only if truthy
    }
  ],
  entry: string,                   // node id where execution begins
  exits: string[],                 // terminal node ids; graph completes when every reachable path terminates here
  max_iterations?: number,         // safety cap across all back-edges; defaults to a sensible ceiling
  budget?: Budget,                 // applies to the graph as a whole (see Budget inheritance)
  notify_on_complete?: SinkSpec,   // see "Result sinks"
})
  → { id: HandleRef, status: "running" }
```

**Node kinds.**

- **`agent`** — runs one agent with a supplied prompt (possibly templated over inbound edge payloads). Output flows out to its outbound edges.
- **`map`** — fans out over an iterable payload; each item runs a specified inner node (or subgraph) with bounded concurrency. Emits a collection.
- **`aggregate`** — gathers inbound payloads (from N parallel edges or a `map`'s output) and produces a single output. Typically an agent node with a templated prompt over the collection, but can also be a pure reducer for simple cases.
- **`branch`** — takes one inbound payload, routes to one of several outbound edges based on a predicate. The `guard` on outbound edges is how selection is expressed.
- **`sink`** — external side effect (see [Result sinks](#result-sinks)). Terminal; no outbound edges.
- **`subgraph`** — nests another `AgentGraph` as a single node. How composition scales; also how graph fragments become reusable.

**Cycles and convergence.** A back-edge (an edge whose `to` is upstream of its `from` in the entry-rooted traversal) is fine. Iteration semantics are expressed by the guard on the back-edge — e.g. "loop while quality score < threshold" — and bounded by `max_iterations`. A cycle with no guard that can exit fails graph validation at submission time.

**Compilation of the sugar tools.** Informally:
- `AgentSpawn` compiles to a graph with one `agent` node, `entry = exit`.
- `AgentMap` compiles to `source → map → aggregate → exit`. The aggregator is optional today; once added (below), it's the default shape.
- `AgentLoop` compiles to `worker ↔ reviewer` with a guarded back-edge from reviewer to worker, plus an exit edge when the reviewer approves.

Callers who need shapes the sugar tools don't cover — dynamic branching, multi-round convergence with aggregation, arbitrary fan-in from heterogeneous sources — drop down to `AgentGraph` directly.

**Why this belongs at the foundation.** Factor-q's vision calls out graph-based agent composition as a core principle. Making the graph the primitive (rather than an optional escape hatch layered over spawn) means there's one execution engine, one event model, one thing to visualise and debug, and one representation that a self-improvement loop can reason about. Natural-language prompts can't be statically analysed, rewritten, or replanned; a graph can.

---

### Durable handles

A handle returned by `AgentSpawn`, `AgentMap`, `AgentLoop`, or `AgentGraph` is opaque, globally-scoped, and persistent.

```
HandleRef = string                 // opaque, globally unique, stable across restarts
```

**Semantics.**

- **Persistent.** Handles are stored in a harness-managed handle store (keyed on handle id), independent of any event stream. Restarting the harness does not invalidate running work; handles remain readable, waitable, peekable, and cancellable.
- **Session-independent.** The agent or client that obtains a handle is not the owner. Any caller with the handle can wait on it, peek at it, cancel it, or subscribe to its completion. This matters for fire-and-forget: the spawning agent can terminate immediately, and a separate process (or a human) can pick up the handle later.
- **Not a stream reference.** Agent executions are logically independent of any particular event stream. One agent may read from several streams and write to several more; one stream may carry traffic from many agents. A handle identifies a piece of running work — it is not and must not be a subject or subscription into an event bus.
- **Scoped lifecycle.** Handles have a retention policy (defaults: running handles retained indefinitely, completed handles retained for N days with their final result, then garbage-collected). `AgentList` surfaces retention state.

**Rationale.** Real fire-and-forget workflows run longer than their calling session. Without durable handles, "fire and forget" is a polite fiction for "fire and politely poll until your session dies." Durable handles plus result sinks (below) together make genuine fire-and-forget possible.

---

### Result sinks

A completed agent's result needs to go somewhere. Sometimes that's back to the caller via `AgentWait`. Often — especially for long-running and fire-and-forget workflows — it's somewhere external: an email, a webhook, an event on a bus, a Slack message, a row appended to a database.

Sinks are declared at spawn time via `notify_on_complete`:

```
SinkSpec = OneOf<{
  bus: { subject: string, payload_shape?: JSONSchema },
  webhook: { url: string, method: "POST" | "PUT", headers?: map, retries?: number },
  email: { to: string[], subject_template: string, body_template: string },
  slack: { channel: string, template: string },
  agent: { type: string, prompt_template: string },   // spawn a follow-up agent with the result in context
  artifact: { name: string, ttl_s?: number },         // write the result to the artifact store under a known name
  multiple: SinkSpec[],                                // fan out to several sinks
}>
```

**Semantics.**

- **At-least-once delivery.** Sinks are retried with backoff on failure; the runtime surfaces sink failures as events but does not block graph completion on a sink that permanently fails (configurable per sink type).
- **Side-effect boundary.** Sinks are the *only* place non-agent side effects are allowed to cross the runtime boundary. This keeps the rest of the system pure with respect to the outside world, which makes replay and debugging tractable.
- **Applicable at multiple layers.** `notify_on_complete` can be declared on `AgentSpawn`, `AgentMap`, `AgentLoop`, `AgentGraph`, *and* on individual `sink` nodes within a graph. The outer-spawn notification fires when the whole graph completes; node-level sinks fire when their node completes.
- **Authentication and credentials.** Sink types that require credentials (webhook auth, Slack tokens, SMTP credentials) resolve them against the agent's sandbox-scoped environment, not from a global credential store. An agent can only emit to sinks it has credentials for — consistent with factor-q's container-style isolation model.

**Why sinks are a foundational concern, not a tool.** Every spawn-level primitive needs a "where does the answer go?" answer. Building it into spawn metadata means fire-and-forget is expressible uniformly across every compositional shape, not re-invented per tool.

---

### Fragment library

Graphs are data. Named, versioned, parameterised graphs are reusable data — **graph fragments**. The sugar tools (`AgentSpawn`, `AgentMap`, `AgentLoop`, and any others added over time) are implemented as fragments in a standard library namespace; they are the registry's first entries, not special cases in the runtime. The same mechanism that delivers `std.map` to new users lets an organisation publish `org.research.tournament` without modifying the core.

A fragment is:

```
Fragment = {
  name: string,                        // e.g. "std.map", "org.research.scatter_gather"
  version: string,                     // semver and/or content hash (see versioning, below)
  graph: AgentGraphDefinition,         // the parameterised graph body
  parameter_schema: JSONSchema,        // what callers must supply
  result_schema: JSONSchema,           // what the fragment emits
  metadata: {
    description: string,
    author?: string,
    tags?: string[],
    requires?: FragmentRef[],          // dependencies on other fragments
  },
}

FragmentRef = string                   // "name@version" — version may be a semver range or a content hash
```

Fragments reference other fragments through `subgraph` nodes in their graph body, resolved against the registry at compile time.

**Relationship to skills.** factor-q's skill system (AgentSkills format, per VISION.md) addresses how *a single agent* should approach a task — prompt instructions plus scoped tool configurations. Fragments address how *a composition of agents* should collaborate — graph topology plus connection contracts. Both live in registries, both are versioned and namespaced, both are packaged for reuse; the artefacts are distinct and the two systems should not be collapsed. A fragment may, and often will, reference agent types that have skills attached — but the fragment registry doesn't carry skills, and the skill registry doesn't carry graphs.

#### Registry tools

Dedicated tools for publishing, resolving, and discovering fragments. These are administrative — used during authoring and at graph compile time, rarely inside a hot loop.

```
FragmentPublish({
  name: string,
  version: string,
  graph: AgentGraphDefinition,
  parameter_schema: JSONSchema,
  result_schema: JSONSchema,
  metadata: FragmentMetadata,
  replaces?: FragmentRef,              // optional; supersedes this ref in the registry
})
  → { ref: FragmentRef, canonical_hash: string }
```
Validates the graph (type-checks edges against node parameter/result schemas, rejects unresolvable fragment refs, verifies no unguarded cycles), computes the canonical content hash, and stores the fragment.

```
FragmentResolve({ ref: FragmentRef })
  → {
      name, version, graph, parameter_schema, result_schema,
      metadata, canonical_hash,
      resolved_dependencies: FragmentRef[],   // transitive closure, fully pinned
    }
```
Returns the full fragment record with all transitive dependencies resolved to specific versions — the "lockfile view" for a given reference. Deterministic for a given registry state.

```
FragmentList({
  namespace?: string,                  // e.g. "std", "org.research"
  tag?: string,
  author?: string,
  include_deprecated?: boolean,
})
  → [FragmentSummary]
```
Discovery. Returns summaries (name, latest version, description, tags) rather than full records.

```
FragmentInspect({ ref: FragmentRef })
  → {
      ...full fragment record,
      dependents: FragmentRef[],       // other fragments that depend on this one
      usage: { spawn_count_30d: number, last_spawned_at: timestamp },
    }
```
Full metadata plus usage telemetry, useful before deprecating or modifying widely-used fragments.

```
FragmentDeprecate({
  ref: FragmentRef,
  successor?: FragmentRef,             // recommend callers migrate here
  reason?: string,
})
  → ack
```
Marks a version as deprecated without deleting it. Callers still resolving the deprecated version get a warning event on the bus; existing dependents continue to work.

**Explicitly out of scope for the first pass:** a full package-manager transport (publish to a remote registry, pull from upstream, mirror, vendor). The initial registry is local to the factor-q instance. Remote federation is future work.

#### Open design questions

These are flagged inline rather than resolved because they interact with each other and with factor-q's wider architecture.

- **Versioning scheme.** Content-addressed (hash-only) is the safest for correctness — a `FragmentRef` uniquely identifies a specific graph definition, full stop. Semver is more ergonomic for humans but invites drift (a "patch" release that subtly changes behaviour). The likely answer is *both*: every fragment has a canonical content hash, semver tags are layered on top as mutable aliases that resolve to a hash at compile time. A project-level lockfile records the resolved hashes.
- **Resolution policy.** When does a `FragmentRef` with a semver range get resolved to a concrete hash — at publication time of the dependent fragment, at graph compile time, or at execution time? Each has cost/correctness trade-offs. Recommend compile-time with a lockfile (familiar model from language package managers), but this interacts with how runtime-hot-reloading of fragments should behave.
- **Trust and signing.** For user-published fragments in a multi-tenant or federated future, signing is essential. Out of scope for the local-only first pass but the data model should leave room for a signature block on each published version.
- **Namespacing convention.** Reverse-DNS (`org.example.pattern`) is boring and correct; short names (`std.map`) are ergonomic. Recommend reverse-DNS for anything outside `std.*`, and reserve `std.*` for fragments shipped with the runtime itself.
- **Fragment testing.** Fragments benefit from being testable in isolation (sample inputs, expected outputs or invariants). Could be a `FragmentTest` tool, could be a convention in metadata (`tests: [{input, expected}]`), or could just fall out of normal test infrastructure. Worth a pass once the first non-trivial user fragment exists.
- **Deprecation and removal.** Deprecating is safe; removing is not — any running graph or durable handle may depend on the fragment. The registry probably needs to retain deprecated versions indefinitely, with removal only permitted once no dependents remain (verifiable via the reverse index built from `FragmentRef` edges).

---

## Tier 1 — Primitives used constantly

These replace patterns I currently compose by hand on every complex task.

### `AgentSpawn`

Replaces the current foreground/background split with a uniform handle-returning call.

```
AgentSpawn({
  type: string,                  // agent type to spawn
  prompt: string,                // the brief
  model?: ModelSelector,         // optional model override (see "Model selection")
  isolation?: "worktree",        // optional git worktree isolation
  budget?: Budget,               // see Tier 3
  output_schema?: JSONSchema,    // see "Typed outputs" below
  notify_on_complete?: SinkSpec, // see "Result sinks"
  parent_hint?: string,          // optional label for parent_id tracking
})
  → { id: HandleRef, status: "running" }
```

**Semantics.** Non-blocking. Returns immediately with a handle. The caller uses `AgentWait` or `AgentPeek` to read results or check progress.

**Failure modes.** If the `type` is unknown, or the prompt exceeds a hard input cap, fail synchronously with a structured error — do not silently truncate.

---

### `AgentWait`

Explicit join with configurable semantics. Replaces the implicit "foreground blocks / background notifies" model.

```
AgentWait({
  ids: string[],
  mode: "all" | "any" | "n",
  n?: number,                    // required if mode === "n"
  timeout_s?: number,
})
  → {
      completed: [{ id, result, tokens_used, elapsed_ms }],
      still_running: string[],
      timed_out: string[],
      failed: [{ id, error }],
    }
```

**Semantics.** Blocks the calling agent until the join condition is met or the timeout fires. `still_running` on return lets the caller decide whether to keep waiting, cancel, or proceed with partial results.

**Rationale.** This is the single biggest gap today. Every "spawn 5, wait for all, synthesise" pattern is currently orchestrated by eyeballing background-completion notifications and mentally tracking which ones are still outstanding.

---

### `AgentMap`

Templated fan-out with a concurrency cap. The most common pattern in practice.

```
AgentMap({
  type: string,
  items: any[],
  prompt_template: string,       // supports {{item}} and {{index}} interpolation
  model?: ModelSelector,         // optional model override for all spawned agents
  max_concurrency: number,
  per_item_budget?: Budget,
  output_schema?: JSONSchema,
  aggregate?: {                  // optional aggregation step; see below
    type: string,
    prompt_template: string,     // {{results}} is the array of per-item outputs
    model?: ModelSelector,
    output_schema?: JSONSchema,
  },
  on_failure: "skip" | "abort" | "retry_n",
  retry_n?: number,              // required if on_failure === "retry_n"
  notify_on_complete?: SinkSpec,
})
  → {
      results: [{ item, result?: any, error?: Error }],
      aggregate?: any,           // present iff `aggregate` was configured
    }
```

**Aggregation step.** When `aggregate` is supplied, the map fans out as before, then the aggregator runs once with all per-item results in context and emits a synthesised output. This covers the map-reduce shape directly and is what "fan-out → evaluate" usually wants. Without `aggregate`, the caller must do the synthesis step itself (either in the orchestrator or by chaining another spawn), which for fire-and-forget workflows means nothing picks up the slack. With `aggregate`, the map emits a single top-level result suitable for passing to a sink.

**Semantics.** Spawns up to `max_concurrency` agents at a time, feeding each one item from `items` through the template. Blocks until all items are resolved (or `on_failure: "abort"` trips).

**Why first-class:** doing this by hand means composing N parallel `Agent` calls in a single message, then manually correlating results to inputs in the next turn. The concurrency cap also matters — without it, a 200-item list spawns 200 parallel agents.

---

### `AgentCancel`

Stops a running agent cleanly.

```
AgentCancel({ id: string, reason?: string })
  → { final_state: "cancelled" | "already_done", partial_result?: any }
```

**Semantics.** Sends a cancellation signal to the agent; the agent gets one chance to flush a partial result or summary before termination. Idempotent — cancelling an already-completed agent is a no-op.

**Rationale.** Today a runaway agent has to be waited out. With concurrency this becomes a serious throughput problem.

---

## Tier 2 — Patterns that are currently too expensive to run

These enable workflows that technically work today but are prohibitively expensive because every iteration round-trips through the orchestrator's context.

### `AgentLoop`

Worker/reviewer loops that iterate inside the subagent system without filling the orchestrator's context with intermediate drafts.

```
AgentLoop({
  worker: { type, prompt_template, model? },     // receives {{previous_output}} and {{review}} on iterations > 1
  reviewer: { type, prompt_template, model? },   // receives {{worker_output}}, must return approval signal
  stop_when: "reviewer_approves" | "max_iterations",
  max_iterations: number,
  pass_context: "full" | "summary" | "critique_only",
  approval_schema?: JSONSchema,          // schema the reviewer must return; defaults to {approved: bool, critique: string}
  notify_on_complete?: SinkSpec,
})
  → {
      final_result: any,
      iterations: number,
      stopped_because: "approved" | "max_iterations" | "error",
      review_trace_ref?: ArtifactRef,    // full iteration history, stored as artifact
    }
```

**Semantics.** Runs worker → reviewer → worker → reviewer until the reviewer returns `approved: true` or `max_iterations` is hit. The orchestrator sees only the final result and a summary, not every intermediate draft.

**Why this matters:** the current way to do this costs a full orchestrator turn per iteration, which eats context fast. After 3–4 rounds it becomes prohibitive. Pushing the loop into the harness means the orchestrator's context cost is O(1) regardless of iteration count.

---

### Artifact store — `ArtifactWrite`, `ArtifactRead`

Pass refs between agents instead of bodies.

```
ArtifactWrite({ name?: string, content: string | bytes, ttl_s?: number })
  → { ref: ArtifactRef }

ArtifactRead({ ref: ArtifactRef })
  → { content: string | bytes }
```

**Semantics.** Agents that produce large outputs (reports, generated code, analysis results) can write them to the artifact store and return only the ref. Downstream agents receive the ref in their prompt and fetch the content themselves. The orchestrator's context only ever holds pointers.

**Failure modes.** Expired refs (past TTL) return a structured "gone" error rather than silently missing.

**Rationale.** Today if agent A produces a 40KB report and agent B needs it, I read A's full output into my context and paste it into B's prompt, doubling context cost at each hop. In a pipeline of N agents, that's O(N²) in the worst case.

---

### Model selection via `model`

Not a separate tool — a parameter on `AgentSpawn`, `AgentMap`, and on each `worker` / `reviewer` sub-config inside `AgentLoop`.

```
ModelSelector = string
  // Either a tier alias — "fast" | "default" | "deep"
  // Or a concrete model id — e.g. "claude-haiku-4-5", "claude-sonnet-4-6", "claude-opus-4-7"
```

**Semantics.** The agent definition is still the source of truth for which model to run by default and which models are permitted. The spawn-time `model` parameter is an **override**, not the primary selector. It must satisfy the agent's declared `allowed_models` list (or pass without restriction if the agent opts into `allowed_models: ["*"]`). If the override is rejected, the spawn fails synchronously with a structured error — the harness does not silently fall back to the default.

**Tier aliases vs. concrete ids.** Tier aliases (`"fast"`, `"default"`, `"deep"`) are resolved at the runtime level to concrete model ids via the factor-q config. This keeps graphs portable across model churn and providers: a graph that spawns with `model: "deep"` keeps working when today's Opus is tomorrow's legacy. Concrete ids are also accepted for callers that genuinely need a specific model (e.g. A/B comparisons, reproducibility, exercising a feature only available on one model).

**Why it earns its place.** Routing decisions often depend on context the agent definition doesn't have: the orchestrator may know this particular input is trivial (use `"fast"`), or that budget headroom is low (downgrade one tier), or that this is the critical path of a long workflow (use `"deep"`). Pushing that routing into the agent definition would force an explosion of near-duplicate agent variants per complexity level. Pushing it into every caller would scatter routing logic everywhere. Making it a spawn-time parameter, constrained by the agent's declared allow-list, concentrates routing decisions where the context lives while keeping the agent's contract honest.

**Interaction with `AgentLoop`.** Worker and reviewer naturally benefit from different models — cheap-propose + expensive-critique, or the inverse (expensive-draft + fast-sanity-check). Each sub-config carries its own `model`, so a single `AgentLoop` can mix tiers without additional plumbing.

**Interaction with `Budget`.** Overriding the model changes the expected cost curve. Either the caller must pass a `budget` that reflects the chosen model, or the runtime must re-derive default budgets from the chosen model's pricing. The latter is friendlier; the former is explicit. Recommendation: derive automatically, but emit a warning event when the override pushes expected cost above the agent definition's declared default budget by more than 2×.

---

### Typed outputs via `output_schema`

Not a separate tool — a parameter on `AgentSpawn`, `AgentMap`, and `AgentLoop`.

```
AgentSpawn({ ..., output_schema: JSONSchema })
  → { id }

// Later:
AgentWait(...) → { completed: [{ id, result: <parsed JSON matching schema> }] }
```

**Semantics.** The agent is constrained to return JSON matching the schema; the harness parses and validates before returning. On validation failure, the agent gets one retry with the validation error included in its context; a second failure is returned to the caller as a structured error.

**Rationale.** Turns agent results from prose-to-parse into structured data the orchestrator can compose directly. Particularly valuable for `AgentMap` aggregations where ad-hoc parsing across N results is error-prone.

---

## Tier 3 — Supervision and observability

Currently impossible to answer basic operational questions like "what's running right now?" and "how much has this cost so far?"

### `AgentList`

```
AgentList({ status?: "running" | "done" | "failed" | "cancelled" })
  → [{
      id,
      type,
      parent_id,                 // who spawned this one
      status,
      tokens_in,
      tokens_out,
      elapsed_ms,
      cost_usd,
      started_at,
    }]
```

**Semantics.** Snapshot of all agents in the current session (or spawn tree, if scoped). `parent_id` is the key field — being able to see the full spawn tree makes recursive flows debuggable.

---

### Budgets on spawn

A `Budget` type used by `AgentSpawn`, `AgentMap`, and `AgentLoop`:

```
Budget = {
  max_tokens?: number,
  max_duration_s?: number,
  max_cost_usd?: number,
  on_exceed: "cancel" | "soft_warn",
}
```

**Semantics.** Hard stops when exceeded (under `cancel`), or a structured warning attached to the result (under `soft_warn`).

**Inheritance rule.** A parent's budget caps the **sum** of its descendants, not each one independently. A recursive fan-out that blows its total budget stops spawning new children. Without this, recursive `AgentMap` calls can explode cost invisibly.

---

### `AgentPeek`

Streaming-ish progress without blocking.

```
AgentPeek({ id: string, last_n_events?: number })
  → {
      status,
      recent_tool_calls: [{ tool, args_summary, timestamp }],
      recent_text: string,              // last bit of assistant text
      estimated_progress?: number,      // 0..1 if the agent reports it
      tokens_used: number,
    }
```

**Semantics.** Peek at what a running agent is doing without blocking on completion. Useful for deciding whether a long-running agent is making useful progress or has wandered off and should be cancelled.

---

## Tier 4 — Nice to have

Lower priority but would smooth rough edges.

### `AgentPool`

Named pool with a standing concurrency cap; submit work to it instead of re-specifying `max_concurrency` per call.

```
PoolCreate({ name, max_concurrency, default_budget? }) → { pool_id }
PoolSubmit({ pool_id, type, prompt, ... }) → { id }
PoolDrain({ pool_id }) → waits for all submitted work
```

### Result caching

Deterministic cache keyed on `{type, prompt, tool_state_hash}`, with an opt-in flag on `AgentSpawn`. Same inputs shouldn't pay twice within a session.

### `AgentMessage` (generalised `SendMessage`)

The current harness has a `SendMessage` primitive for resuming an agent. Make it explicit and documented:

```
AgentMessage({ id, message }) → { ack, estimated_resume_delay_ms? }
```

### Inter-agent messaging

Deliberately listed last because the design trade-off is real: letting sibling agents talk to each other without routing through the orchestrator enables some patterns but makes reasoning about data flow much harder. The "everything routes through the orchestrator" model has real clarity benefits and I'd think carefully before giving it up. If included, it should be opt-in per pool or per spawn tree, not a global capability.

---

## Summary of the shape change

| Today | With these primitives |
|---|---|
| No shared substrate; every compositional pattern is bespoke | `AgentGraph` as the underlying execution model; sugar tools compile to it |
| New canonical patterns require runtime changes | Fragment library: sugar tools ship as standard-library fragments; user fragments extend the sugar layer without runtime changes |
| Handles die with the calling session | Durable handles persisted independently of event streams |
| Results only reachable by polling `AgentWait` | `notify_on_complete` sinks (bus / webhook / email / agent / artifact) make fire-and-forget real |
| Foreground blocks, background notifies | Uniform handles + explicit `AgentWait` |
| Parallelism = "however many tool calls I put in one message" | `AgentMap` with `max_concurrency`, pools |
| Fan-out has no built-in synthesis step | `AgentMap` with an optional `aggregate` node — map-reduce in one call |
| Review loops cost one orchestrator turn per iteration | `AgentLoop` iterates inside the harness |
| Large outputs bloat orchestrator context at every hop | Artifact refs, O(1) context cost per hop |
| No way to cancel a runaway | `AgentCancel` |
| No visibility into what's running or what it's cost | `AgentList`, `AgentPeek`, budget accounting |
| Only `general-purpose` can spawn further agents | Any agent type can recurse (optionally depth-capped) |
| Unbounded cost on recursive fan-outs | Budget inheritance down the spawn tree |
| Model choice fixed in agent definition | Spawn-time `model` override constrained by the agent's allow-list, with tier aliases for portability |
| Cycles are special-cased inside `AgentLoop` only | Cycles are a property of edges in `AgentGraph`; guards and iteration caps make them tractable at every compositional layer |

The common thread: move common orchestration patterns into harness-provided primitives so they don't consume the orchestrator's context and attention for every iteration.

---

## Future extensions

Ideas that are probably worth building eventually but don't need a full spec yet. Kept here so they aren't lost.

- **Capability matching on model override.** Today the `allowed_models` list is the only gate on spawn-time model selection. A stricter version would let agent definitions declare capability requirements (extended thinking, prompt caching tier, tool-use flavour, context window size) and let the runtime reject an override that picks a model lacking a required capability — instead of relying on the author to keep `allowed_models` in sync with the real feature needs.
- **Auto-routing via `model: "auto"`.** A lightweight router (heuristic or a small model) picks the tier per spawn based on prompt length, declared complexity, or recent outcome signals from the projection. Natural extension once tier aliases are in; removes the need for callers to encode routing heuristics by hand.
- **Shared-state primitives beyond artifacts.** Artifacts cover "one agent writes, others read." Some patterns need finer-grained shared state — counters, sets of seen items, distributed locks — without the overhead of routing through the orchestrator or a full KV tool. Worth designing only once a concrete use case demands it.
- **Graph visualisation and live view.** `AgentList` plus the graph representation gives enough to render the spawn tree and in-flight graph instances. A live visualiser (client-side, not a runtime primitive) would make debugging deep recursive flows much easier, and is the natural UI surface for the graph-as-primitive model.
- **Composition-level error semantics.** A short explicit table specifying how `on_failure` (in `AgentMap`), `stop_when` (in `AgentLoop`), and `on_exceed` (in `Budget`) propagate across nested compositions. The shapes are sound individually; the cross-product needs its own pass before anyone writes deeply-nested graphs in anger.
- **Remote fragment registries / federation.** The initial fragment library is local to the factor-q instance. Mirroring, publishing to a shared upstream, and vendoring fragments from other organisations are the obvious extensions once multiple factor-q instances need to share patterns.
- **Reactive triggers as graph entry points.** Graphs currently assume an imperative "submit and run" entry. Binding a graph's entry to a reactive trigger (cron, webhook, bus subject, file watcher) makes it live — a standing pipeline rather than a one-off. Aligns with factor-q's existing trigger model.
- **Fragment testing framework.** Fragments benefit from being testable in isolation — sample inputs, expected outputs or invariants. Could be a `FragmentTest` tool, could be convention-based via metadata, could just fall out of normal test infrastructure. Worth a pass once the first non-trivial user fragment exists.
