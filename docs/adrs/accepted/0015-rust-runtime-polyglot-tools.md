# ADR-0015: Rust Runtime, Polyglot Tools, Language Boundary at Event Bus

## Status

Accepted

## Context

Factor-q is implemented in Rust. The AI ecosystem — model SDKs, MCP server implementations, prompt optimizers (DSPy, GEPA), retrieval and embedding libraries — is predominantly Python, with a 12-18 month lag for equivalent Rust crates that has not been shrinking.

This creates a recurring tension. Every capability factor-q wants to access has a mature Python implementation and an immature (or missing) Rust one. Three options exist:

1. **Full Rust.** Reimplement or wait for Rust equivalents of every needed capability. Maximum coherence, slowest progress, and ongoing tax on every new capability.
2. **Pivot to Python.** Rewrite factor-q in Python to access the ecosystem natively. Loses Rust's runtime guarantees (memory footprint, deterministic resource control, single-binary deployment, sandbox-as-types). Gains LangGraph, DSPy, etc. as native components — but with composition tensions where their opinions conflict (LangGraph state-primary vs Temporal journal-primary, DSPy whole-program introspection vs distributed execution).
3. **Hybrid: runtime in Rust, capabilities accessed through process boundaries.** The Rust runtime owns the long-running, durability-sensitive concerns (event bus, executor, sandbox, cost tracking, schema registry, harness). External capabilities are accessed through well-defined process-level boundaries: MCP servers, subprocess tool dispatch, offline compilation pipelines.

The decision affects every future "should we adopt library X" question and is therefore worth committing to explicitly rather than handling case-by-case.

## Decision

Factor-q adopts the hybrid model. The runtime is Rust. Tool implementations, optimizers, and ecosystem integrations are accessed through process boundaries. The language boundary is at the event level (or, equivalently, at the typed contract level — they are the same boundary expressed differently).

Specifically:

- **Rust owns:** event bus consumer, projection store, agent harness (per ADR-0014), sandbox enforcement, cost tracking, schema registry and validation, repository abstractions for the storage mechanisms (per storage-taxonomy decision), trigger dispatch, the executor loop.
- **Process boundaries access:** tool implementations via MCP or subprocess, prompt optimization via offline DSPy (or equivalent) pipelines that produce compiled artifacts, retrieval and embedding services as MCP servers, model API calls via Rust HTTP clients to provider APIs.
- **The language boundary lives at typed contracts:** every cross-language interaction is a schema-validated message. Tool calls in, tool results out. Compilation inputs in, compiled artifacts out. No shared in-memory state across language boundaries.

## Consequences

### Positive

- The runtime keeps Rust's operational properties: low memory footprint, deterministic resource control, single-binary deployment, type-enforced sandbox, predictable performance for long-running daemons.
- The Python AI ecosystem is accessible without bringing it inside the runtime. MCP is a particularly clean integration: tool authors write Python (or any language), factor-q dispatches via the protocol, no in-process coupling.
- DSPy and prompt optimizers fit naturally as offline tools that consume signatures/verifier-data and produce compiled artifacts. The runtime loads artifacts; it does not host the optimizer.
- Each process boundary is a clean substitution point. A Python tool today can become a Rust tool tomorrow without runtime changes; an MCP server can be replaced with a different implementation behind the same protocol.
- The composition tensions that would arise from running multiple Python frameworks together (LangGraph + DSPy + Temporal) are sidestepped because no two frameworks meet inside factor-q's process. Each operates within its own boundary.

### Negative

- Cross-language debugging is harder. A failure that involves the runtime, an MCP server, and an offline-compiled artifact spans three execution contexts with different debuggers and logging conventions. Investment in cross-process tracing (OpenTelemetry-style) is needed.
- Per-tool startup overhead exists. Subprocess-launched tools pay process-startup cost per invocation; MCP servers amortise this with long-lived processes but require connection management. For very high-frequency tools, this matters.
- Deployment is slightly more complex than pure single-binary: factor-q ships its Rust binary plus expects MCP servers and tool processes to be available. The deployment story is "Rust binary + a documented runtime environment for tools," not "Rust binary alone."
- The runtime cannot directly leverage Python-native capabilities that don't fit a process-boundary model. Anything that requires shared in-memory state with the runtime (e.g., a deeply integrated LangGraph state machine) is inaccessible.
- Python-versioning and dependency-management for tool processes is an operational concern that pure-Rust deployment avoids.

### Neutral

- Performance for tool-heavy workloads is acceptable but bounded by IPC overhead. For factor-q's expected workloads (LLM call latency dominates), this is rarely the bottleneck.
- The hybrid model matches industry patterns (Temporal worker model, Inngest serverless functions, MCP itself), so adopters will recognise the shape.

## Alternatives considered

### Pure Rust (option 1)

Rejected: every new AI capability would either require a Rust reimplementation, a wait for community equivalent, or a custom integration. The project would spend disproportionate effort on infrastructure that already exists elsewhere, with no compensating benefit.

### Pivot to Python (option 2)

Rejected: Python sacrifices the runtime properties that are factor-q's actual differentiation. The composition story for combining LangGraph + DSPy + Temporal is unproven in production, and ad-hoc integration of three opinionated frameworks recreates the runtime-engineering work without the coherence benefit. The "we have access to the ecosystem" win is real but smaller than the loss of runtime properties, given that MCP and process-boundary integration provide most of the access without the in-process coupling.

This was a serious option and deserves explicit acknowledgement: the pivot is *plausible*, it is not *obviously wrong*, and a different project with different priorities might rationally choose it. The rejection is specific to factor-q's positioning as an event-sourced runtime where state, execution, sandbox, cost, and learning are expressed in a single coherent model. A project that prioritised ecosystem access over runtime coherence would correctly choose differently.

## Implications

- MCP is a first-tier integration target. The runtime should make adding new MCP servers low-friction; this is the primary mechanism by which factor-q's capabilities grow.
- Cross-process observability needs early investment. Distributed tracing across the Rust runtime and external tool processes is operational baseline, not an enhancement.
- The Rust runtime should ship with a documented, versioned tool process protocol. Tool authors should be able to write a tool against a stable contract without needing to read runtime source.
- DSPy integration is offline by design. The compilation pipeline (when it runs, where artifacts are stored, how they are loaded) is a Phase 3 concern, but the boundary is clear: signatures and verifier-data go in, compiled artifacts come out, the runtime consumes artifacts at execution time.

## References

- ADR-0013 (memory delegation to MCP services) — first instance of the runtime-plus-MCP pattern this ADR generalises
- ADR-0014 (agent harness as reducer) — establishes that the LLM/tool loop is in Rust; this ADR clarifies what is *not* in Rust
- The conversation and design docs leading to this decision evaluated Pure Python and Pure Rust pivots and converged on the hybrid model as the right shape for factor-q's positioning
