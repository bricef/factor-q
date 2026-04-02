# factor-q

A single-tenant, self-hosted agent runtime for designing, operating, and evolving multi-agent systems that deliver on large, ongoing projects.

factor-q is not a chatbot or an interactive coding assistant. It is a continuously running, event-driven agent orchestrator where human interaction is one input among many.

## Key properties

- **Event-driven** — every agent interaction is an event on a NATS-based event bus, enabling auditing, replay, and debugging
- **Model-agnostic** — each agent in a graph targets the model best suited to its task, mixing providers freely
- **Headless-first** — runs as a persistent server; CLI, TUI, and other interfaces are clients of the runtime
- **Cost-aware** — budget limits and spending controls built in from the start
- **Extensible** — custom tools via subprocess/MCP; agent definitions as Markdown files, graph definitions as YAML

## Documentation

- [Vision](VISION.md) — what factor-q is and why it exists
- [Architecture](ARCHITECTURE.md) — core subsystems and cross-cutting concerns
- [Phase 1 Plan](docs/plans/active/2026-04-02-phase-1-foundation.md) — current development scope
- [ADRs](docs/adrs/) — architectural decision records ([accepted](docs/adrs/accepted/), [draft](docs/adrs/draft/))

## Technology

- **Runtime:** Rust
- **Event bus:** NATS + JetStream
- **Projections:** SQLite
- **Agent definitions:** Markdown with YAML frontmatter
- **Graph definitions:** YAML with JSON Schema

## Prior Art

- [Crush](research/crush.md) — architecture analysis
- [OpenCode](research/opencode.md) — architecture analysis
- [open-agent.io](https://open-agent.io/)
