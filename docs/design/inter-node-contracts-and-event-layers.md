# Inter-Node Contracts and Event Structure

## Context

factor-q's vision commits to a graph-based agent topology, but the graph executor and its inter-node protocol are not yet implemented. The decisions captured here define the shape of that protocol. They should land before the graph executor is built — once nodes start exchanging data on the bus, retrofitting the contract shape requires either historical event migration or accepting partial replay breakage.

The driving observation is that the value of multi-invocation orchestration comes from path-independence: a verifier or downstream agent operating in fresh context, on the *output* of an upstream agent rather than its reasoning trace, gets a genuinely independent perspective. That value is preserved or destroyed at the inter-node boundary. Free-form prose handoffs lose it; typed handoffs preserve it.

A single forward pass — even with extended thinking — cannot truly abandon its initial framing, because everything subsequently produced is conditioned on what came before. Multi-invocation breaks this path-dependence only when each invocation operates on a clean prior. The contract between nodes is what makes that cleanliness possible or impossible.

## Decisions

### 1. Inter-node contracts are typed by default

Every node in a graph declares input and output schemas. Schemas are mandatory, not optional. The trivial schema `{"type": "string"}` is a runtime-provided default for nodes that genuinely produce or consume free-form text — the goal is not to force structure on prose, but to force *acknowledgement* of what shape is being produced.

JSON Schema is the schema language, for three reasons:

- It is already used for graph definitions in YAML
- It compiles to native Rust types via `schemars` / `typify`
- It is what frontier models produce reliably when given function-calling interfaces

Optional schemas are rejected as a design choice. A two-tier system where typed and untyped nodes coexist forces every consumer to handle the untyped case, which means typed contracts cannot be relied upon, which defeats the purpose. The cost of "schemas required" at the type-definition level is small. The cost of retrofitting is large. The cost of never having them properly is that the self-improvement loop (Q20 → Q200) has no solid substrate to operate on.

### 2. Validation runs at both ends

The producing agent's output is validated against its declared output schema before publishing the event. The consuming agent's executor validates the same payload against the consumer's declared input schema on read. Both schemas are recorded on the event envelope.

The double check catches schema drift between definition and runtime behaviour, and the recorded schemas allow replay against modified graphs to check compatibility honestly rather than guessing.

### 3. Schema-violation is a feedback signal, not a fatal error

When an LLM-backed node produces output that fails its declared schema, the executor feeds the validation error back to the model as a tool-style error and lets it retry, with a bounded retry budget. This mirrors the existing sandbox-violation pattern. Frontier models self-correct schema violations reliably when shown the validator output. Crashing the task is the wrong default.

### 4. Schemas are versioned from day one

Every schema carries `schema_version: 1`. Schemas will change. Versioned events with versioned schemas are tractable; unversioned ones become an archaeological dig. The migration story for v1 → v2 is a separate concern and can be deferred, but the version field cannot.

### 5. The event model has three layers

Every event on the bus has three structurally distinct layers, each with different write permissions, read audiences, and rules.

| Layer       | Written by       | Read by                                  | Mutability                                              |
|-------------|------------------|------------------------------------------|---------------------------------------------------------|
| Envelope    | Runtime          | Everyone                                 | Immutable, closed schema                                |
| Payload     | Producing agent  | Consuming agents                         | Validated against producer + consumer schemas           |
| Annotations | Producing agent  | Humans, meta-agents, learning loop       | **Never read by consuming agents**                      |

**Envelope** holds system-generated metadata: event ID, parent event ID, trace ID, agent ID, schema ID, schema version, timestamp, cost data, sandbox info, retry counts. It is a strict typed struct with no extension mechanism — system metadata is a closed set; if a new field is needed, the runtime grows.

**Payload** is the typed contract between graph nodes. It is the only thing that drives graph behaviour.

**Annotations** are advisory, model-generated commentary: free-form notes, self-reported confidence, reasoning traces, sources considered. They are a `Map<string, JsonValue>` with a runtime registry of well-known keys. Unknown keys are permitted and logged.

The asymmetry between envelope and annotations is deliberate. Envelope is closed because invariants live there. Annotations are open because that is where novelty happens.

### 6. The annotation barrier is enforced by the runtime

The single rule that makes the three-layer model work: **the executor strips annotations from the input context when building the prompt for a consuming agent.** A consuming agent sees the payload and selected envelope fields, never the annotations from upstream events.

This rule cannot be enforced by guidance or convention. Without runtime enforcement, annotations become a structured-bypass channel for cross-node coupling — producers attach `notes`, consumers read `notes`, control flow happens through prose, and the typed contract becomes fictitious. The runtime must be the source of this discipline.

The reasoning-trace case matters specifically: fresh-context verification only works if the verifier does not see the producer's reasoning. If reasoning leaks via annotations into a downstream agent's input, the path-independence that justifies multi-invocation in the first place is lost.

### 7. Where data goes — the placement test

For any new field, the test is:

> If this field disappeared, would graph correctness change?

- **Yes** → payload (typed, validated, semantically load-bearing)
- **No, but humans / meta-agents benefit from seeing it** → annotations
- **No, and it's about the system rather than the work** → envelope

Specific calls following this test:

- *Self-reported confidence* → annotations. LLM confidence is poorly calibrated and should not gate downstream behaviour. Calibrated confidence should come from a separate verifier node producing a typed `Verdict`.
- *Reasoning traces / chain-of-thought* → annotations, with strict barrier enforcement.
- *Tool call summaries* → envelope (system-level, queryable).
- *Provenance / sources used* → payload, as a typed `Citation[]` field, when downstream nodes need them. Sources merely *considered* but not used belong in annotations.
- *"Why I made this choice"* → annotations. Always. The moment a consumer reads "why" to decide what to do, two agents are coupled through prose.

### 8. Verifiers are a first-class typed shape

A verifier is a node with the signature:

```
(typed_artifact) -> Verdict { pass | fail | score; feedback?; route? }
```

Two properties make this distinct from "another agent that happens to evaluate things":

**Fresh-context discipline by construction.** A verifier subscribes to its target's output event and consumes only the payload, never the producer's annotations or upstream context. The bus model already supports this; making verifier a distinct shape ensures it.

**Verdict is the substrate for evals.** A verifier with a known-correct judgment over a fixed test set is, by definition, an eval. Same shape, different binding. Making `Verdict` a canonical type means the learning loop has something measurable to optimize against. Without it, automated workflow improvement is unbounded drift — and the path from Q20 to Q200 depends on that loop being grounded in something measurable.

### 9. Substitution is a design goal

A node declares its input and output schemas. Whether the implementation is an LLM call, a deterministic Rust function, an MCP tool call, or a smaller distilled model is an implementation detail of the node, not a structural property of the graph.

This is the same "define-once, derive-interfaces" pattern factor-q already uses elsewhere — the node *is* the typed operation. A classification agent that today is a Sonnet call should be replaceable with a Haiku call, then a fine-tuned local model, then a deterministic function, with no graph changes. Required schemas make this swap mechanical. Without them it is a refactor.

Cost is the practical motivator: the gap between frontier-model and cheap-model pricing is wide enough that substitution is the largest cost lever in the system. Without typed contracts, that lever cannot be pulled.

## Implementation notes

### Annotation registry

The runtime maintains a registry of well-known annotation keys with documented semantics. Suggested initial set:

- `notes: string` — free-form commentary from the producing agent
- `confidence: number` — self-reported confidence, advisory only, never read by consumers
- `reasoning: string` — chain-of-thought or working, never read by consumers
- `sources_considered: Citation[]` — sources looked at but not directly used in payload
- `flags: string[]` — agent-emitted markers for downstream human review

The registry can grow as patterns emerge. Unknown keys are permitted but logged. The registry's value is giving the learning loop a stable vocabulary to aggregate over without locking out experimentation.

### Output schema escape hatch

Every output schema may include an optional `notes: string` field for prose that does not fit the structured shape. This field carries no semantic weight downstream — consumers must not condition on it. Its purpose is to prevent the temptation to break the schema "just this one case." If a field starts being consumed semantically, it has earned promotion to a real typed field.

### Human-in-the-loop nodes

Strict typed schemas on human input are the wrong default. The pattern: a human-input node has a free-form string input and produces a typed output via an explicit interpretation step (LLM-backed, with the next node's input schema as the target). The interpretation is a separate graph node, not hidden behaviour, because that is where intent drift lives and visibility matters.

### Intra-agent memory vs annotations

An agent reading its own prior state across invocations is intra-agent state, not cross-agent coupling, and should route through the memory MCP service (per ADR-0013) rather than annotations. Annotations are about events; memory is about state. Conflating the two creates a third storage mechanism with unclear semantics.

### Build the layer split before the graph executor exists

Even if the only annotation supported initially is `notes`, the envelope/payload/annotations split should be present in the event type from the start. Adding the structure later requires migrating historical events or accepting partial replay breakage. Defining empty fields up front costs nothing.

## Open questions

- **Schema migration.** v1 → v2 schema evolution mechanics are deferred. The version field must exist from day one; the migration tooling can come later.
- **Adapter nodes.** When two nodes have non-matching schemas but are semantically composable, an adapter is an explicit graph node. The conventions for how adapter nodes are authored (handwritten, generated, LLM-backed with the target schema as a constraint) need their own pass.
- **Eval set diversity.** The path from Q20 to Q200 depends on automated workflow improvement, which depends on a stable evaluation substrate. Keeping the eval set diverse enough to avoid local optima around dogfooding workloads (factor-q working on factor-q) is an open problem and needs its own design.

## Why this lands now, not later

The graph executor does not yet exist. Once it does, and once agent definitions referencing inter-node contracts ship, retrofitting the contract shape becomes expensive — historical events would need migration, replay against modified graphs would partially break, and convention-based "we'll add types later" does not happen in practice. The cost of getting the shape right at the type-definition level is one afternoon. The cost after Phase 2 is months.

The framing to internalize: factor-q is not a chat orchestrator with optional types. It is a typed dataflow graph where some of the nodes happen to be LLMs. That framing changes a lot of downstream decisions in the right direction.
