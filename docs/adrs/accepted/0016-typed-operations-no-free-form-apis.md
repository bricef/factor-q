# ADR-0016: Typed Operations Exposed to Agents, No Free-Form Storage APIs

## Status

Accepted

## Context

Agents in factor-q have side-effecting capabilities beyond producing their declared output: they may write to memory, contribute to workflow context, attach annotations to events, reference artifacts, and (for exploratory signatures) write to a scratchpad. The interface through which agents express these side effects is an architectural decision with significant consequences.

The naive approach is a small set of free-form APIs: `memory.write(key, value)`, `annotations.set(key, value)`, `workflow_context.append(entry)`. This is the pattern most agent frameworks adopt because it is simple to implement and gives agents maximum flexibility.

The flexibility is the problem. Free-form APIs make the following failure modes possible:

- An agent stores a 50KB blob in memory because nothing prevents it
- An agent attaches a control-flow-bearing string to annotations, creating an untyped semantic dependency between producer and consumer that bypasses the typed-contracts discipline
- An agent writes prose to workflow context that downstream agents have to interpret as natural language, recreating the prompt-coupling failure mode the architecture was designed to prevent
- An agent writes inconsistent shapes to memory across invocations, making the data unusable for the learning loop or downstream consumers

These failures are not hypothetical. They are the standard outcome of free-form storage APIs in agent systems and are difficult to detect because the system continues to function — just less predictably and less optimizably.

The decision must address: how do agents express side effects in a way that preserves the typed-contracts discipline, prevents misuse, and remains expressive enough for legitimate work?

## Decision

Agents do not have access to free-form storage APIs. All side-effecting writes are exposed as typed, schema-validated operations with constraints baked into their schemas.

Instead of `memory.write(key, value)`, agents see operations like `memory.record_observation(text: string, tags: Tag[])` or `memory.record_decision(decision: DecisionEntry)`.

Instead of `annotations.set(key, value)`, agents see `annotations.add_note(text: string)` (with size constraint) or `annotations.record_confidence(score: number)`.

Instead of `workflow_context.append(entry)`, agents see `workflow_context.add_assumption(assumption: Assumption)`, `workflow_context.add_observation(observation: Observation)`, etc., each with a typed entry shape.

Each typed operation:

- Has a declared schema for its inputs (validated at the MCP boundary)
- Has size constraints baked into the schema where appropriate
- Maps to a specific, named entry type in the underlying storage
- Is part of the agent's declared capability set (per the sandbox model)

## Consequences

### Positive

- Misuse is not expressible. An agent cannot write a 50KB blob to memory because no operation accepts a 50KB blob. An agent cannot create an arbitrarily-typed annotation because annotations are typed.
- The typed-contracts discipline extends to side effects. The same JSON Schema validation that protects payload boundaries protects storage writes.
- The learning loop has stable vocabulary. When the system aggregates over memory writes, workflow context entries, or annotations, the entries have known types — aggregation is meaningful rather than parsing-prose-and-hoping.
- Agent capabilities are introspectable. The set of operations an agent has access to is enumerable, validated, and visible in the graph definition.
- New entry types require deliberate registration. Adding a new memory entry type or workflow context type means defining a schema and a typed operation. This friction is desirable: it surfaces architectural decisions about what kinds of state matter.
- Migration is tractable. When a typed operation's schema evolves, all writes that conform to the old schema are findable; backfill or migration is mechanical.

### Negative

- Authoring a new agent capability requires defining typed operations rather than reusing a generic write API. This is more work upfront.
- Agents cannot store information that doesn't fit any defined typed operation. If a legitimate need arises that no existing operation supports, the operation must be added — agents cannot work around the absence with a generic blob write.
- The set of typed operations grows over time. The runtime registry of operations needs maintenance and occasional pruning of operations that turn out not to be useful.
- Some patterns common in unconstrained agent systems (e.g., "let the agent invent its own categorisation scheme on the fly") are not directly expressible. They must be channelled through typed operations or the exploratory-signature scratchpad mechanism.

### Neutral

- The annotations layer specifically follows a slight variation: the *content* of an annotation can be relatively free-form (an agent can write whatever notes are useful within size bounds), but the *operations* for writing are typed. This matches the architectural intent that annotations are advisory commentary, not control-flow signal.
- The scratchpad mechanism for exploratory signatures has typed entry types but gives agents authoring discretion over content within those types. This is the appropriate relaxation for genuinely exploratory work.

## Alternatives considered

### Free-form APIs with linting

Pro: maximum flexibility, simpler implementation, familiar pattern from other agent frameworks.

Con: linting catches obvious failures but not subtle ones. The control-flow-via-annotations failure mode is structural, not detectable by lint rules. The runtime would need to enforce constraints at write time anyway, which means the typed-operation infrastructure exists — at which point exposing free-form APIs alongside it is a back door that defeats the discipline.

Rejected.

### Typed operations with a generic-blob escape hatch

Pro: typed operations for the common case, generic write available when nothing fits.

Con: the escape hatch becomes load-bearing. Agents will gravitate toward it whenever a typed operation feels constraining, especially under time pressure during agent authoring. Within months, the typed operations become a polite suggestion and the generic blob carries the actual state. The discipline collapses.

Rejected. The architectural rule is that if a need arises, a typed operation should be added — not that a generic fallback should absorb the unfamiliar case.

### Schema-per-write with no typed operations

Pro: every write specifies its schema inline. Maximum flexibility while preserving validation.

Con: agents have to reason about schema choice as part of side-effecting writes, which violates the principle that agents are functions and the runtime makes structural decisions. Schema choice is a signature-design concern, not a runtime-call concern.

Rejected.

## Implications

- The typed-operation registry is a piece of runtime infrastructure that needs deliberate maintenance. Adding operations, deprecating operations, and evolving operation schemas should be first-class workflows.
- Agent authoring must specify which operations the agent has access to, as part of the capability declaration. This composes with the sandbox model.
- Entry-type taxonomies (for memory, workflow context, annotations) are design artifacts that should be reviewed periodically. Operation proliferation without principled curation degrades the value of the typed approach.
- Exploratory signatures' scratchpad is a partial relaxation, not an exception: scratchpad operations are still typed, but the entry-type set may be richer to support open-ended work. The relaxation is in *what kinds of entries exist*, not in *whether entries are typed*.
- The MCP boundary is where typed-operation validation lives. Tool servers presenting these operations must validate against the schemas the runtime registers; the runtime double-checks at the boundary.

## References

- ADR-0013 (memory delegation to MCP services) — establishes that memory is an MCP service; this ADR specifies that the operations exposed are typed, not free-form
- inter-node-contracts-and-event-layers.md — establishes the typed-contracts discipline this ADR extends to side effects
- storage-taxonomy-and-signature-kinds.md — establishes the storage mechanisms whose write APIs this ADR governs
- ADR-0014 (agent harness as reducer) — establishes that the runtime owns dispatch, including dispatch of side-effecting operations
