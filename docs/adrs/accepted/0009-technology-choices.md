# ADR-0009: Technology Choices

## Status

Accepted (language). The event bus and persistence questions this ADR left
open were settled by [ADR-0011](0011-event-bus-and-persistence.md) (NATS +
JetStream, SQLite projection), whose source-of-truth half was in turn
amended by [ADR-0026](0026-event-log-system-of-record.md).

Implementation: complete — the runtime, event bus, executor and work engine
are Rust; `async-nats` and JetStream carry the bus; SQLite carries the
stores. One prediction did not come true and is corrected in Consequences:
the authoring surfaces are declarative text files, not Rust embedded DSLs.

## Context

factor-q needs a host language, an event bus implementation, and a persistence layer. These choices affect the contributor pool, the extension model, the deployment story, and performance characteristics.

## Decision: Rust as the host language

### Rationale

factor-q is a long-running, concurrent server that manages an event bus, multiple agent executors, task scheduling, and persistent state. It also needs embedded DSLs for agent definitions, graph wiring, and sandbox declarations — domain concepts that should be expressible, readable, and hard to get wrong.

**Rust was chosen over Go, TypeScript, and Python for the following reasons:**

**Embedded DSL ergonomics** — this was the deciding factor. factor-q's agent definitions, graph topologies, and sandbox rules are complex enough to warrant domain-specific abstractions. Rust's macro system (both `macro_rules!` and procedural macros), type-state builders, and expressive type system allow embedded DSLs that read like configuration while retaining full compile-time safety. Go lacks macros entirely, limiting embedded DSLs to struct literals and builder functions.

**Type system for domain modelling** — Rust's enums with data (sum types) and exhaustive pattern matching map naturally to agent states, event types, and message patterns. The compiler enforces that every case is handled. Go's interface-based approach lacks exhaustive checking.

**Concurrency and long-running reliability** — Rust's ownership model catches concurrency bugs at compile time. No garbage collector means predictable latency, which matters for time-sensitive triggers (operational alerts). Tokio provides a mature async runtime.

**Single binary deployment** — aligns with the self-hosted model. No runtime dependencies to manage on the target server.

**Performance ceiling** — the event bus may handle high throughput under heavy agent load. Rust provides headroom without architectural changes.

**Extension model** — custom tools and integrations will be authored as subprocesses or MCP servers, decoupling the extension language from the host language. Users can write tools in Python, TypeScript, or any language. The core runtime's language choice does not constrain extension authors.

### Tradeoffs accepted

- **Slower compile times** — Rust's compilation is significantly slower than Go's, which impacts iteration speed during early development when the design is still fluid.
- **Steeper learning curve** — smaller contributor pool and higher onboarding cost for new contributors.
- **Async complexity** — Tokio adds conceptual overhead that Go's goroutines avoid.

These tradeoffs are accepted because the DSL and type-safety benefits compound over the lifetime of the project, while compile-time costs are a fixed tax that tooling (incremental compilation, `cargo check`) mitigates.

## Consequences

- The core runtime, event bus, agent executor, and task engine are implemented in Rust
- Agent definitions and graph wiring were expected to use Rust's macro
  system and type-state patterns as embedded DSLs. **That is not what
  happened.** Both authoring surfaces are declarative text files —
  Markdown + YAML frontmatter ([ADR-0005](0005-agent-definition-format.md))
  and YAML graphs ([ADR-0012](0012-graph-definition-format.md)) — parsed by
  `serde_yaml`. Rust's type system carries the *internal* domain model (the
  agent builder, the `fq-ops` value declarations, the typed op identifiers),
  not the surface a user writes. The embedded-DSL argument was the deciding
  factor for choosing Rust; the thing it decided was then built another way.
  The other reasons above are unaffected
- Custom tools and extensions are language-agnostic, communicating via
  MCP. (The subprocess tool protocol this also predicted was not built —
  see [ADR-0015](0015-rust-runtime-polyglot-tools.md))
- Contributors need Rust proficiency to work on the core — extension authors do not
- Event bus and persistence technology choices were left open here and
  settled by [ADR-0011](0011-event-bus-and-persistence.md), as amended by
  [ADR-0026](0026-event-log-system-of-record.md)
