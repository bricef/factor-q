# ADR-0014: Agent Harness as Reducer with Runtime-Owned Loop

## Status

Accepted

## Context

Factor-q's value proposition rests substantially on durable execution: long-running agentic work that survives crashes, supports replay against modified configurations, and provides cost enforcement and sandbox discipline throughout. The locus of these properties is the agent harness — the loop that constructs LLM requests, dispatches tool calls, and accumulates state across steps within a single agent invocation.

Where this loop lives is an architectural decision with cascading consequences. Three plausible options exist:

1. **Loop in the runtime, agent as configuration.** The Rust runtime owns the LLM/tool loop. An "agent" is a configuration consumed by the runtime: model selection, system prompt (compiled artifact), tool set, termination condition. Tool implementations may be in any language (Rust, Python via subprocess, MCP services).

2. **Loop in the agent process, runtime orchestrates between agents.** Each agent is a long-lived process that owns its own LLM/tool loop, accumulates its own state, and serializes its reducer state to checkpoint. The runtime orchestrates between agent processes via the event bus.

3. **Coarse-grained agents, no mid-agent durability.** Each agent invocation is an atomic unit. No mid-execution checkpointing. Resumability is at the agent boundary only.

Each carries different tradeoffs for durability granularity, language ecosystem access, and runtime coherence.

The decision interacts with prior commitments: typed inter-node contracts (the agent's input and output are schema-validated payloads), event-sourced spine (the runtime is authoritative over event history), cost as runtime enforcement (budgets are tracked in the executor, not delegated), sandbox discipline (capabilities are runtime-enforced, not agent-managed).

## Decision

The agent harness is a reducer that lives in the Rust runtime. An agent invocation is a fold over a sequence of (LLM call → tool dispatch → result accumulation → continuation decision) steps, with reducer state persisted to the event bus after each step.

An "agent" in factor-q is a configuration consumed by this harness:

- A signature (typed input, typed output, intent statement)
- An implementation strategy specification (model, compiled prompt artifact, decoding parameters)
- A declared tool set
- A capability/sandbox declaration
- A cost budget

The harness loop is in Rust. Tool implementations may be in any language and are dispatched by the harness as subprocess calls, MCP tool invocations, or in-process Rust functions. The language boundary is at the tool dispatch boundary, not the loop boundary.

This is option 1 from the context above.

## Consequences

### Positive

- Durable execution semantics are runtime-owned and uniform across all agents. Mid-agent crashes resume from the last persisted reducer state without involving agent-side code.
- Cost enforcement is mechanical: every step touches the harness, every LLM call and tool dispatch is accounted, the budget ceiling is enforced inside the loop with no opportunity for an agent to bypass it.
- Sandbox enforcement is at the loop boundary: the harness validates every tool dispatch against the capability declaration. Agents cannot escape the sandbox because they don't own the dispatch.
- Replay-against-modified-configuration is well-defined: the reducer state plus the modified configuration produces a replayable execution. Configuration changes (different model, different prompt artifact, different tool set) can be evaluated against historical reducer states.
- The event log captures every step in a uniform schema. No per-language variation in what gets persisted.
- Single binary deployment is preserved.

### Negative

- LangGraph and similar Python-native agent frameworks cannot be used as agent internals. Their value proposition (state machines, checkpointing, multi-actor coordination within an agent) overlaps with what the harness provides; using both would mean paying for the same capability twice and creating conflicts over who owns persistence.
- Agent authoring is constrained: agents express their behaviour through configuration (model, prompt, tools, signature) rather than imperative code. Behaviours that don't fit the (LLM call → tool → result → continue) loop shape are awkward or impossible without harness extensions.
- The harness becomes a critical piece of runtime infrastructure that must be maintained correctly. Bugs in the reducer logic affect every agent in the system.
- Some advanced patterns (multi-actor coordination within a single logical agent, complex internal state machines) require either harness extensions or decomposition into multiple agents at the graph level.

### Neutral

- DSPy remains accessible as an *offline* tool: signatures and verifier-`Verdict` data feed DSPy compilation to produce prompt artifacts that the harness loads. The boundary is clean. DSPy does not run inside factor-q's runtime.
- MCP tool servers are a natural fit: the harness dispatches tool calls to MCP servers without caring about their implementation language. This is the primary integration point for the Python ecosystem.
- Exploratory signatures (per the storage-taxonomy-and-signature-kinds design doc) require harness support for sub-agent spawning, which is a defined extension point rather than an arbitrary capability.

## Alternatives considered

### Loop in agent processes (option 2)

Pro: agents could use LangGraph or DSPy natively as their internal logic; richer agent-authoring story.

Con: cross-process checkpointing protocol is non-trivial. Idempotency for LLM calls and tool side effects becomes a per-language concern. Cost enforcement is delegated to agent processes (or duplicated). Sandbox discipline weakens because the runtime cannot see inside agent processes. The duplication with LangGraph's checkpointer creates ongoing tension over who owns persistence. Two implementations of durable execution in the system rather than one.

This option was rejected because the runtime's value proposition *is* durable execution with cost and sandbox discipline. Pushing the loop into agent processes either gives those properties up or rebuilds them in a worse way.

### Coarse-grained agents (option 3)

Pro: simplest possible model. Agents can be anything internally because the runtime treats them as black-box atomic units. Full LangGraph/DSPy compatibility within agents.

Con: a long-running agent that crashes after substantial work loses all of it. Replay-with-modification is at the agent boundary only. Cost enforcement becomes coarse: budgets are checked at agent start, not throughout. Sandbox enforcement is at agent boundaries only.

This option was rejected because long-running autonomous work is a primary use case. A 10-minute agent crashing at minute 9 starting over is a poor user experience and burns money. The fine-grained durability that justifies the runtime requires the harness to live where it can see every step.

## Implications

- The harness is a first-tier piece of runtime infrastructure; its design and testing rigour should match.
- Tool dispatch is the primary integration point for cross-language work; making this boundary clean and well-typed is high leverage.
- Agent authoring tooling will likely be needed to make signature + binding + tool-set declarations easy to express. This is downstream work but worth scoping.
- Configuration-as-code for agents creates a clean substitution boundary between LLM-backed and deterministic implementations of the same signature. This composes with the signatures-as-primitive decision.

## References

- ADR-0013 (memory delegation to MCP services) — establishes the pattern of runtime + MCP for capabilities the runtime doesn't own
- inter-node-contracts-and-event-layers.md — establishes the typed contract boundary the harness operates over
- signatures-and-optimization-hierarchy.md — establishes signatures as primary, agents as bindings
- storage-taxonomy-and-signature-kinds.md — establishes the storage mechanisms the harness writes to
