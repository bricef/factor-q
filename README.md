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

## Project structure

The repository is organised as a monorepo with independent services, shared content, and deployment infrastructure.

```
factor-q/
├── services/              # Each service is self-contained with its own build system
│   └── fq-runtime/        # Rust workspace — CLI, runtime, tools
│       └── crates/
│           ├── fq-cli/      # `fq` binary (argument parsing, command dispatch)
│           ├── fq-runtime/  # Core library (agent loader, config, event schema)
│           └── fq-tools/    # Built-in tool implementations
│
├── infrastructure/        # Deployment and local dev
│   ├── docker-compose.yml # Local dev orchestration (NATS + services)
│   └── nats/              # NATS server configuration
│
├── agents/                # Agent definitions (.md files with YAML frontmatter)
│   └── examples/          # Sample agents shipped with the project
│
├── skills/                # Skill registry content (AgentSkills format)
│
├── docs/
│   ├── adrs/              # Architectural Decision Records
│   │   ├── accepted/      # Resolved decisions
│   │   ├── draft/         # Open decisions under discussion
│   │   └── deprecated/    # Superseded decisions
│   └── plans/             # Phase plans and roadmaps
│       ├── active/        # Current phase plans
│       └── closed/        # Completed phase plans
│
├── research/              # Analysis of prior art
├── VISION.md              # What factor-q is and why it exists
├── ARCHITECTURE.md        # Core subsystems and cross-cutting concerns
└── README.md
```

### Browsing the repository

Rust build artefacts under `services/fq-runtime/target` can clutter directory listings. To view the project structure cleanly:

```sh
tree -I 'target|.git'
```

## Getting started

Prerequisites: Rust toolchain, Docker, Docker Compose, [just](https://github.com/casey/just).

```sh
# Start infrastructure and build the runtime
just up

# Run the CLI
just fq --help

# Start the runtime in the foreground (brings up NATS, builds, and runs)
just run

# See all available recipes
just
```

## Prior Art

- [Crush](research/crush.md) — architecture analysis
- [OpenCode](research/opencode.md) — architecture analysis
- [open-agent.io](https://open-agent.io/)
